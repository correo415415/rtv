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

/// Devuelve el tamaño de celda, con las reglas descritas en el docstring del
/// módulo. `cols`/`rows` son el tamaño lógico del terminal (por si CSI 14t
/// nos devuelve el área total y hay que dividir).
pub fn probe_cell_px(cols: u16, rows: u16) -> CellPx {
    if !terminal_supports_pixel_query() {
        return heuristic();
    }
    // Ahora sí, intentamos CSI 16t (timeout ultra-corto).
    if let Some((h, w)) = query_and_parse(b"\x1b[16t", b't', 6, 20) {
        if w > 0 && h > 0 && w < 200 && h < 200 {
            return CellPx {
                w,
                h,
                source: CellPxSource::Csi16t,
            };
        }
    }
    // Intento 2: CSI 14t (tamaño total del área de texto).
    if let Some((total_h, total_w)) = query_and_parse(b"\x1b[14t", b't', 4, 20) {
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
    // Windows: NUNCA sondear. Windows Terminal y consolas legacy no responden.
    #[cfg(windows)]
    {
        return false;
    }

    #[cfg(not(windows))]
    {
        let term = std::env::var("TERM").unwrap_or_default();
        let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();

        // Kitty: env var propia y TERM=xterm-kitty.
        if std::env::var("KITTY_WINDOW_ID").is_ok() {
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

#[cfg(not(unix))]
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
