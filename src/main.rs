//! rtv — reproductor de vídeo de terminal ultra-rápido.
//!
//! v0.2:
//!   * FFmpeg vía `ffmpeg-the-third 5.0` (compatible FFmpeg 7.1).
//!   * Audio real con cpal + swresample, audio como master clock.
//!   * Escalado adaptativo: detecta el tamaño real de cada celda del terminal
//!     y ajusta la resolución destino en consecuencia (más grande = más nítido).

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

mod audio;
mod clock;
mod decoder;
mod hwdec;
mod input;
mod player;
mod renderer;
mod subs;
mod terminfo;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "rtv", version, about = "Reproductor de vídeo de terminal (Rust)")]
struct Cli {
    /// Ruta al fichero de vídeo
    path: PathBuf,

    /// Forzar backend de render: kitty | iterm2 | sixel | blocks | ascii
    #[arg(long)]
    backend: Option<String>,

    /// Escala máxima (frac. de la terminal, 0.1..=1.0). Por defecto 1.0
    #[arg(long, default_value_t = 1.0)]
    scale: f32,

    /// Loop infinito
    #[arg(long)]
    loop_video: bool,

    /// Mostrar información de rendimiento en el HUD
    #[arg(long)]
    stats: bool,

    /// Desactivar audio (usa reloj monotónico)
    #[arg(long)]
    no_audio: bool,

    /// Fichero de subtítulos externo (.srt / .ass). Sin él, rtv usa la
    /// pista de subtítulos de texto embebida del contenedor si existe.
    #[arg(long)]
    sub: Option<PathBuf>,

    /// Desactivar subtítulos (ni externos ni embebidos)
    #[arg(long)]
    no_subs: bool,

    /// Decode por hardware: auto | none | vaapi | cuda | qsv | d3d11va |
    /// dxva2 | videotoolbox | vulkan | drm | vdpau. `auto` prueba los
    /// hwaccels de la plataforma y cae a software si ninguno funciona.
    #[arg(long, default_value = "auto")]
    hwdec: String,

    /// Dejar que FFmpeg y sus codecs escriban a stderr (útil para depurar).
    #[arg(long)]
    verbose: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Validar --hwdec ANTES de silenciar stderr: un valor inválido
    // debe verse (exit 2, convención de error de uso de CLI).
    let hw_pref = match hwdec::HwPref::parse(&cli.hwdec) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };

    // Silenciar libav antes de tocar nada. Ver comentarios largos en el
    // repositorio para el razonamiento (tres capas de log).
    if !cli.verbose {
        ffmpeg_the_third::util::log::set_level(ffmpeg_the_third::util::log::Level::Quiet);
        unsafe {
            ffmpeg_the_third::sys::av_log_set_level(ffmpeg_the_third::sys::AV_LOG_QUIET);
            ffmpeg_the_third::sys::av_log_set_callback(None);
        }
        silence_stderr();
    }

    ffmpeg_the_third::init()?;

    if cli.verbose {
        eprintln!("hwaccels disponibles: {:?}", hwdec::available_types());
    }

    player::run(player::Config {
        path: cli.path,
        forced_backend: cli.backend,
        scale: cli.scale.clamp(0.1, 1.0),
        loop_video: cli.loop_video,
        show_stats: cli.stats,
        no_audio: cli.no_audio,
        hw_pref,
        sub_file: cli.sub,
        no_subs: cli.no_subs,
    })
}

// --- Silenciado de stderr (red de seguridad para logs que se escapan) ---

#[cfg(unix)]
fn silence_stderr() {
    use std::os::unix::io::AsRawFd;
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open("/dev/null") {
        unsafe {
            libc_dup2(f.as_raw_fd(), 2);
        }
    }
}

#[cfg(unix)]
unsafe fn libc_dup2(oldfd: i32, newfd: i32) -> i32 {
    extern "C" {
        fn dup2(oldfd: i32, newfd: i32) -> i32;
    }
    dup2(oldfd, newfd)
}

#[cfg(windows)]
fn silence_stderr() {
    use std::ptr;
    #[link(name = "kernel32")]
    extern "system" {
        fn SetStdHandle(nStdHandle: u32, handle: *mut core::ffi::c_void) -> i32;
        fn CreateFileA(
            lpFileName: *const u8,
            dwDesiredAccess: u32,
            dwShareMode: u32,
            lpSecurityAttributes: *mut core::ffi::c_void,
            dwCreationDisposition: u32,
            dwFlagsAndAttributes: u32,
            hTemplateFile: *mut core::ffi::c_void,
        ) -> *mut core::ffi::c_void;
    }
    const STD_ERROR_HANDLE: u32 = 0xFFFFFFF4;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_SHARE_READ: u32 = 0x1;
    const OPEN_EXISTING: u32 = 3;
    const INVALID_HANDLE_VALUE: isize = -1;
    unsafe {
        let h = CreateFileA(
            b"NUL\0".as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null_mut(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        );
        if !h.is_null() && h as isize != INVALID_HANDLE_VALUE {
            SetStdHandle(STD_ERROR_HANDLE, h);
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn silence_stderr() {}
