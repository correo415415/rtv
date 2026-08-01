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
mod audio_backend;
mod clock;
mod decoder;
mod hwdec;
mod info;
mod input;
mod player;
mod renderer;
mod source;
mod subs;
mod terminfo;
mod tracks;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "rtv", version, about = "Reproductor de vídeo de terminal (Rust)")]
struct Cli {
    /// Fichero de vídeo, URL http/https directa, o página de un sitio
    /// de vídeo (YouTube, Twitch, Vimeo…; requiere yt-dlp instalado).
    path: String,

    /// NO reproducir: mostrar información del fichero (formato,
    /// duración, calidad, pistas de audio/subtítulos, capítulos…).
    #[arg(long)]
    info: bool,

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

    /// Backend de salida de audio: auto | cpal | pulse | none.
    /// `auto` elige según plataforma (en Termux prueba pulse primero).
    #[arg(long, default_value = "auto")]
    audio_backend: String,

    /// Subtítulos. SIN esta opción no se muestran subtítulos.
    /// `--sub` (sin valor) usa la pista de texto embebida del
    /// contenedor; `--sub fichero.srt|.ass` usa el fichero externo.
    #[arg(long, num_args = 0..=1, default_missing_value = "", value_name = "FICHERO")]
    sub: Option<String>,

    /// Desactivar subtítulos (redundante ahora: es el comportamiento
    /// por defecto sin --sub; se mantiene por compatibilidad)
    #[arg(long)]
    no_subs: bool,

    /// Pista de audio inicial por índice 1-based dentro de las pistas
    /// de audio (`--aid 2` = segunda pista), como mpv.
    #[arg(long, value_name = "N")]
    aid: Option<usize>,

    /// Pista de audio inicial por idioma ("eng", "spa", "en"...).
    #[arg(long, value_name = "IDIOMA")]
    alang: Option<String>,

    /// Pista de subtítulos embebida inicial por índice 1-based dentro
    /// de las pistas de texto del contenedor. Implica subtítulos ON.
    #[arg(long, value_name = "N")]
    sid: Option<usize>,

    /// Pista de subtítulos embebida inicial por idioma. Implica
    /// subtítulos ON.
    #[arg(long, value_name = "IDIOMA")]
    slang: Option<String>,

    /// Decode por hardware: auto | none | vaapi | cuda | qsv | d3d11va |
    /// dxva2 | videotoolbox | vulkan | drm | vdpau. `auto` prueba los
    /// hwaccels de la plataforma y cae a software si ninguno funciona.
    #[arg(long, default_value = "auto")]
    hwdec: String,

    /// Forzar resolución con yt-dlp para CUALQUIER URL (sitios que no
    /// están en la lista automática pero que yt-dlp soporta).
    #[arg(long)]
    ytdl: bool,

    /// Formato que se pide a yt-dlp (sintaxis de su opción -f). El
    /// default "b" pide el mejor formato muxed (una sola URL).
    /// Experimental: "bv*+ba/b" pide vídeo+audio en streams separados
    /// (doble input).
    #[arg(long, default_value = "b", value_name = "FMT")]
    ytdl_format: String,

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
    // Ídem --audio-backend.
    let audio_backend = match audio::BackendPref::parse(&cli.audio_backend) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };

    // Resolver la entrada ANTES de silenciar stderr: yt-dlp puede tardar
    // y fallar, y sus errores deben verse. Para rutas locales y URLs
    // directas esto no ejecuta nada (solo clasifica el argumento).
    let src = match source::resolve(&cli.path, cli.ytdl, &cli.ytdl_format) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    };

    // Silenciar libav antes de tocar nada. Ver comentarios largos en el
    // repositorio para el razonamiento (tres capas de log).
    // Con --info NO se redirige stderr: el proceso no pinta en el
    // terminal y el usuario debe VER "no se pudo abrir ..." si falla
    // (solo se acallan los logs internos de libav).
    if !cli.verbose {
        ffmpeg_the_third::util::log::set_level(ffmpeg_the_third::util::log::Level::Quiet);
        unsafe {
            ffmpeg_the_third::sys::av_log_set_level(ffmpeg_the_third::sys::AV_LOG_QUIET);
            ffmpeg_the_third::sys::av_log_set_callback(None);
        }
        if !cli.info {
            silence_stderr();
        }
    }

    ffmpeg_the_third::init()?;

    // --info: inspección sin reproducción. Sale antes de tocar el
    // terminal (ni raw mode ni alt screen): la salida es pipeable.
    if cli.info {
        // Para URLs, el "nombre" útil es el título de yt-dlp o la URL
        // original que tecleó el usuario (no la URL kilométrica del CDN).
        let display = if source::is_url(&cli.path) {
            Some(src.title.as_deref().unwrap_or(cli.path.as_str()))
        } else {
            None
        };
        if let Err(e) = info::print_info(&src.video, display) {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    if cli.verbose {
        eprintln!("hwaccels disponibles: {:?}", hwdec::available_types());
    }

    // Semántica de --sub:
    //   * ausente            → SIN subtítulos (SubMode::Off)
    //   * `--sub` (vacío)    → pista embebida del contenedor
    //   * `--sub fichero`    → fichero externo .srt/.ass
    // `--no-subs` fuerza Off en cualquier caso (compatibilidad).
    // `--sid/--slang` implican subtítulos embebidos ON aunque no se
    // pase `--sub` (sería absurdo pedir una pista y no verla).
    let sub_mode = if cli.no_subs {
        player::SubMode::Off
    } else {
        match cli.sub.as_deref() {
            None => {
                if cli.sid.is_some() || cli.slang.is_some() {
                    player::SubMode::Embedded
                } else {
                    player::SubMode::Off
                }
            }
            Some("") => player::SubMode::Embedded,
            Some(p) => player::SubMode::File(PathBuf::from(p)),
        }
    };

    player::run(player::Config {
        path: src.video,
        audio_path: src.audio,
        forced_backend: cli.backend,
        scale: cli.scale.clamp(0.1, 1.0),
        loop_video: cli.loop_video,
        show_stats: cli.stats,
        no_audio: cli.no_audio,
        audio_backend,
        hw_pref,
        sub_mode,
        aid: cli.aid,
        alang: cli.alang,
        sid: cli.sid,
        slang: cli.slang,
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
