//! Manejo de entrada de teclado (no bloqueante) usando crossterm.
//!
//! Sondeamos con `event::poll(Duration::ZERO)` desde el loop principal
//! para no meter overhead de threads adicionales.

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub enum Cmd {
    Quit,
    TogglePause,
    SeekRel(f64),
    VolumeDelta(i32),
    Resize(u16, u16),
    /// Ciclar pista de AUDIO (+1 = siguiente, -1 = anterior).
    CycleAudio(i32),
    /// Ciclar pista de SUBTÍTULOS (+1 = siguiente, -1 = anterior;
    /// el ciclo incluye "off" y la pista externa de --sub si la hay).
    CycleSubs(i32),
    None,
}

/// Drena todos los eventos pendientes SIN bloquear.
///
/// Los `Resize` se COALESCEN: en una tormenta de resizes (arrastre del
/// ratón) el terminal encola decenas de eventos; procesarlos todos
/// significaba redibujar el frame cacheado + clear de pantalla una vez
/// POR EVENTO → latêencia acumulada y parpadeo. Cada resize es absoluto,
/// así que solo importa el ÚLTIMO.
pub fn poll_command() -> std::io::Result<Vec<Cmd>> {
    let mut out = Vec::new();
    while event::poll(Duration::ZERO)? {
        let ev = event::read()?;
        match ev {
            Event::Key(KeyEvent { code, modifiers, kind, .. }) => {
                if kind != KeyEventKind::Press && kind != KeyEventKind::Repeat {
                    continue;
                }
                let cmd = match (code, modifiers) {
                    (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => Cmd::Quit,
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => Cmd::Quit,
                    (KeyCode::Char(' '), _) => Cmd::TogglePause,
                    (KeyCode::Left, _) => Cmd::SeekRel(-5.0),
                    (KeyCode::Right, _) => Cmd::SeekRel(5.0),
                    (KeyCode::Up, _) => Cmd::VolumeDelta(5),
                    (KeyCode::Down, _) => Cmd::VolumeDelta(-5),
                    // Cambio de pista en runtime (estilo mpv: `#`
                    // cicla audio, `j`/`J` cicla subtítulos). `a`/`A`
                    // como alias más cómodo para audio.
                    (KeyCode::Char('a'), _) | (KeyCode::Char('#'), _) => Cmd::CycleAudio(1),
                    (KeyCode::Char('A'), _) => Cmd::CycleAudio(-1),
                    (KeyCode::Char('j'), _) => Cmd::CycleSubs(1),
                    (KeyCode::Char('J'), _) => Cmd::CycleSubs(-1),
                    _ => Cmd::None,
                };
                if !matches!(cmd, Cmd::None) {
                    out.push(cmd);
                }
            }
            Event::Resize(c, r) => {
                // Coalescencia: sustituir cualquier Resize previo.
                out.retain(|c| !matches!(c, Cmd::Resize(..)));
                out.push(Cmd::Resize(c, r));
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Espera BLOQUEANTE hasta que haya un evento de terminal disponible o
/// venza `timeout`. Devuelve `true` si hay evento pendiente. Es la
/// pieza clave del "resize instantáneo": el player duerme sus esperas
/// inter-frame AQUÍ en vez de en `thread::sleep`, de modo que un
/// resize/tecla interrumpe la espera y se atiende en <1 ms en lugar de
/// esperar a que venza el sleep (hasta 500 ms).
pub fn wait_event(timeout: Duration) -> bool {
    event::poll(timeout).unwrap_or(false)
}
