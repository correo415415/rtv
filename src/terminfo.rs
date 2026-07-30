//! Detección adaptativa del tamaño de celda del terminal.
//!
//! v0.3 — cambios importantes respecto a v0.2:
//!
//!   * **Nunca sondeamos con CSI 16t/14t en Windows**. Windows Terminal, cmd
//!     y ConEmu NO responden a esas queries; el sondeo timeout-ea 100 ms cada
//!     vez y además puede leer bytes de otras teclas → dims degeneradas y el
//!     "video se ve tumbado, 2 fps". En Windows aplicamos heurística directa
//!     que además ES CORRECTA para Consolas / Cascadia Mono (8×16 - 10×20).
//!
//!   * **En Unix solo sondeamos si TERM/TERM_PROGRAM indican una terminal
//!     que sabemos que responde** (Kitty, WezTerm, Ghostty, foot, Konsole,
//!     iTerm2). Cualquier otra cae a heurística.
//!
//!   * **Timeout bajísimo (20 ms)** y con `read()` no bloqueante desde el
//!     principio: si la terminal ni empieza a contestar en 20 ms es que no
//!     soporta la query, y esperar más solo pierde tiempo.
//!
//!   * **Se llama UNA sola vez al arrancar y se cachea**. Los resizes
//!     reusan el mismo `CellPx`; no re-sondean.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct CellPx {
    pub w: u32,
    pub h: u32,
    pub source: CellPxSource,
}

#[derive(Debug, Clone, Copy)]
pub enum CellPxSource {
    Csi16t,
    Csi14t,
    Heuristic,
}

impl CellPxSource {
    pub fn short(&self) -> &'static str {
        match self {
            CellPxSource::Csi16t => "csi16",
            CellPxSource::Csi14t => "csi14",
            CellPxSource::Heuristic => "heur",
        }
    }
}

/// Timeout del sondeo. En Unix el roundtrip es local (PTY) y 20 ms
/// sobran. En Windows la respuesta atraviesa conpty (WT ⇄ conhost ⇄
/// proceso) y puede tardar bastante más; como el sondeo corre UNA sola
/// vez al arrancar, 150 ms no se notan y evitan falsos negativos.
const PROBE_TIMEOUT_MS: u64 = if cfg!(windows) { 150 } else { 20 };

/// Devuelve el tamaño de celda, con las reglas descritas en el docstring del
/// módulo. `cols`/`rows` son el tamaño lógico del terminal (por si CSI 14t
/// nos devuelve el área total y hay que dividir).
pub fn probe_cell_px(cols: u16, rows: u16) -> CellPx {
    if !terminal_supports_pixel_query() {
        return heuristic();
    }
    // Ahora sí, intentamos CSI 16t (timeout ultra-corto).
    if let Some((h, w)) = query_and_parse(b"\x1b[16t", b't', 6, PROBE_TIMEOUT_MS) {
        if w > 0 && h > 0 && w < 200 && h < 200 {
            return CellPx {
                w,
                h,
                source: CellPxSource::Csi16t,
            };
        }
    }
    // Intento 2: CSI 14t (tamaño total del área de texto).
    if let Some((total_h, total_w)) = query_and_parse(b"\x1b[14t", b't', 4, PROBE_TIMEOUT_MS) {
        if cols > 0 && rows > 0 && total_w > 0 && total_h > 0 {
            let cw = (total_w / cols as u32).max(1);
            let ch = (total_h / rows as u32).max(1);
            if cw < 200 && ch < 200 {
                return CellPx {
                    w: cw,
                    h: ch,
                    source: CellPxSource::Csi14t,
                };
            }
        }
    }
    heuristic()
}

/// Heurística por defecto para terminales que no responden a queries.
/// 8×16 es el tamaño típico de una celda con una fuente monoespaciada 10pt
/// (Consolas, Cascadia Mono, Menlo, DejaVu Sans Mono, ...).
fn heuristic() -> CellPx {
    CellPx {
        w: 8,
        h: 16,
        source: CellPxSource::Heuristic,
    }
}

/// ¿Es esta terminal una de las conocidas que responde a `CSI 16 t` / `14 t`?
///
/// Lista blanca conservadora: solo activamos el sondeo cuando estamos MUY
/// seguros de que va a responder. Si dudamos, heurística.
fn terminal_supports_pixel_query() -> bool {
    // Windows: SOLO Windows Terminal. WT moderno (el único con sixel en
    // Windows) SÍ responde a CSI 16t/14t (microsoft/terminal#8581); las
    // consolas legacy (conhost, cmd) no, y sondearlas solo quema el
    // timeout. WT exporta WT_SESSION en todos sus perfiles.
    //
    // Esto arregla el "vídeo descentrado y pequeño" en WT con sixel: la
    // heurística 8×16 subestima la celda real (p.ej. Cascadia Mono a
    // tamaños/DPI típicos es 9×19-12×24) → rtv creía llenar el ancho
    // (offset 0) pero la imagen quedaba arriba-izquierda ocupando ~80%.
    #[cfg(windows)]
    {
        return std::env::var("WT_SESSION").is_ok()
            || std::env::var("WT_PROFILE_ID").is_ok();
    }

    #[cfg(not(windows))]
    {
        let term = std::env::var("TERM").unwrap_or_default();
        let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();

        // Kitty: env var propia y TERM=xterm-kitty.
        if std::env::var("KITTY_WINDOW_ID").is_ok() {
            return true;
        }
        // Windows Terminal vía WSL: mismo caso que en Windows nativo.
        if std::env::var("WT_SESSION").is_ok() {
            return true;
        }
        if term.contains("kitty") {
            return true;
        }
        // WezTerm: env var WEZTERM_EXECUTABLE, o TERM_PROGRAM=WezTerm.
        if std::env::var("WEZTERM_EXECUTABLE").is_ok()
            || term_program.eq_ignore_ascii_case("WezTerm")
        {
            return true;
        }
        // Ghostty
        if term_program.eq_ignore_ascii_case("ghostty") {
            return true;
        }
        // iTerm2
        if term_program.eq_ignore_ascii_case("iTerm.app") {
            return true;
        }
        // foot
        if term == "foot" || term == "foot-extra" {
            return true;
        }
        // Konsole
        if std::env::var("KONSOLE_VERSION").is_ok() {
            return true;
        }
        // xterm moderno: responde a CSI 14t, no siempre a 16t. Lo permitimos
        // porque el timeout es tan bajo (20 ms) que el coste es mínimo.
        if term.starts_with("xterm") {
            return true;
        }
        false
    }
}

/// Envía `query` a stdout y lee la respuesta con timeout `timeout_ms`.
/// Solo compila código real en Unix; en Windows nunca se llama (guardado por
/// `terminal_supports_pixel_query`).
#[cfg(unix)]
fn query_and_parse(
    query: &[u8],
    terminator: u8,
    expected_prefix: u32,
    timeout_ms: u64,
) -> Option<(u32, u32)> {
    let mut out = std::io::stdout();
    out.write_all(query).ok()?;
    out.flush().ok()?;

    // Poner stdin en no bloqueante — ANTES de leer. Restaurar al salir.
    set_stdin_nonblock(true);
    let _guard = NonblockGuard;

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut buf = Vec::with_capacity(32);
    let mut handle = std::io::stdin().lock();

    while Instant::now() < deadline {
        let mut byte = [0u8; 1];
        match handle.read(&mut byte) {
            Ok(1) => {
                buf.push(byte[0]);
                if byte[0] == terminator {
                    break;
                }
                // Sanity: si el buffer crece demasiado sin terminador, la
                // terminal está enviando input real y no la respuesta de
                // sondeo → aborta.
                if buf.len() > 40 {
                    return None;
                }
            }
            Ok(_) => std::thread::sleep(Duration::from_millis(1)),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(_) => break,
        }
    }

    parse_response(&buf, terminator, expected_prefix)
}

/// Windows: la respuesta del terminal llega por el buffer de entrada de
/// la consola como KEY_EVENTs (con ENABLE_VIRTUAL_TERMINAL_INPUT, que
/// crossterm ya activó con el raw mode — el sondeo corre DESPUÉS de
/// `TerminalGuard::enter`). La leemos con `ReadConsoleInputW` usando
/// `WaitForSingleObject` como timeout real, sin bloquear jamás: si el
/// terminal no contesta, a los `timeout_ms` seguimos con heurística.
/// FFI a mano (3 funciones de kernel32) para no arrastrar `winapi`.
#[cfg(windows)]
fn query_and_parse(
    query: &[u8],
    terminator: u8,
    expected_prefix: u32,
    timeout_ms: u64,
) -> Option<(u32, u32)> {
    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const WAIT_OBJECT_0: u32 = 0;
    const KEY_EVENT: u16 = 0x0001;

    // Layout exacto de KEY_EVENT_RECORD / INPUT_RECORD (wincon.h):
    // INPUT_RECORD = { WORD EventType; <pad 2>; union Event (16 bytes) }.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct KeyEventRecord {
        key_down: i32,
        repeat_count: u16,
        virtual_key_code: u16,
        virtual_scan_code: u16,
        unicode_char: u16,
        control_key_state: u32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct InputRecord {
        event_type: u16,
        _pad: u16,
        event: KeyEventRecord,
    }
    extern "system" {
        fn GetStdHandle(n: u32) -> isize;
        fn WaitForSingleObject(h: isize, ms: u32) -> u32;
        fn ReadConsoleInputW(
            h: isize,
            buf: *mut InputRecord,
            len: u32,
            read: *mut u32,
        ) -> i32;
    }

    let hin = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if hin == 0 || hin == -1 {
        return None;
    }

    let mut out = std::io::stdout();
    out.write_all(query).ok()?;
    out.flush().ok()?;

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = (deadline - now).as_millis().max(1) as u32;
        if unsafe { WaitForSingleObject(hin, remaining) } != WAIT_OBJECT_0 {
            break; // timeout o error → sin respuesta
        }
        // Hay eventos: leerlos (esto también DESENCOLA eventos no-tecla
        // — focus, ratón — que mantendrían el handle señalizado).
        let zero = InputRecord {
            event_type: 0,
            _pad: 0,
            event: KeyEventRecord {
                key_down: 0,
                repeat_count: 0,
                virtual_key_code: 0,
                virtual_scan_code: 0,
                unicode_char: 0,
                control_key_state: 0,
            },
        };
        let mut recs = [zero; 16];
        let mut nread: u32 = 0;
        if unsafe { ReadConsoleInputW(hin, recs.as_mut_ptr(), 16, &mut nread) } == 0 {
            break;
        }
        for r in recs.iter().take(nread as usize) {
            if r.event_type != KEY_EVENT || r.event.key_down == 0 {
                continue;
            }
            let ch = r.event.unicode_char;
            if ch == 0 || ch > 255 {
                continue;
            }
            for _ in 0..r.event.repeat_count.max(1) {
                buf.push(ch as u8);
            }
            if ch as u8 == terminator {
                return parse_response(&buf, terminator, expected_prefix);
            }
            // Sanity: demasiados bytes sin terminador → input real del
            // usuario, no la respuesta del sondeo.
            if buf.len() > 40 {
                return None;
            }
        }
    }
    parse_response(&buf, terminator, expected_prefix)
}

#[cfg(not(any(unix, windows)))]
fn query_and_parse(
    _query: &[u8],
    _terminator: u8,
    _expected_prefix: u32,
    _timeout_ms: u64,
) -> Option<(u32, u32)> {
    None
}

fn parse_response(buf: &[u8], terminator: u8, expected_prefix: u32) -> Option<(u32, u32)> {
    if buf.len() < 6 {
        return None;
    }
    let s = std::str::from_utf8(buf).ok()?;
    let s = s.strip_prefix('\x1b')?;
    let s = s.strip_prefix('[')?;
    let s = s.strip_suffix(terminator as char)?;
    let mut parts = s.split(';');
    let prefix: u32 = parts.next()?.parse().ok()?;
    if prefix != expected_prefix {
        return None;
    }
    let a: u32 = parts.next()?.parse().ok()?;
    let b: u32 = parts.next()?.parse().ok()?;
    Some((a, b))
}

#[cfg(unix)]
struct NonblockGuard;
#[cfg(unix)]
impl Drop for NonblockGuard {
    fn drop(&mut self) {
        set_stdin_nonblock(false);
    }
}

#[cfg(unix)]
fn set_stdin_nonblock(enable: bool) {
    use std::os::unix::io::AsRawFd;
    extern "C" {
        fn fcntl(fd: i32, cmd: i32, arg: i32) -> i32;
    }
    const F_GETFL: i32 = 3;
    const F_SETFL: i32 = 4;
    const O_NONBLOCK: i32 = 0o4000;
    unsafe {
        let fd = std::io::stdin().as_raw_fd();
        let cur = fcntl(fd, F_GETFL, 0);
        if cur < 0 {
            return;
        }
        let new = if enable {
            cur | O_NONBLOCK
        } else {
            cur & !O_NONBLOCK
        };
        fcntl(fd, F_SETFL, new);
    }
}

/// Política de escalado adaptativo. `reserve_bottom_rows` cuenta las filas
/// del HUD (1 o 2). Se descuentan del área útil ANTES de calcular píxeles.
pub fn adaptive_target_pixels(
    backend: crate::renderer::Backend,
    cols: u16,
    rows: u16,
    cell: CellPx,
    scale: f32,
    reserve_bottom_rows: u16,
) -> (u32, u32) {
    use crate::renderer::Backend;
    // Guardar SIEMPRE al menos `reserve_bottom_rows` filas + 0 de margen.
    let usable_rows = rows.saturating_sub(reserve_bottom_rows).max(1);

    let (px_per_col, px_per_row) = match backend {
        Backend::HalfBlocks => (1u32, 2u32),
        Backend::Ascii => (1, 1),
        Backend::Kitty | Backend::Iterm2 | Backend::Sixel => (cell.w.max(1), cell.h.max(1)),
    };

    let w = (cols as u32 * px_per_col) as f32 * scale;
    let h = (usable_rows as u32 * px_per_row) as f32 * scale;
    ((w as u32).max(2), (h as u32).max(2))
}
