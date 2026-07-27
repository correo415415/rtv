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
    None,
}

/// Drena todos los eventos pendientes SIN bloquear. Devuelve la
/// última acción efectiva (si hay resize, se prioriza).
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
                    _ => Cmd::None,
                };
                if !matches!(cmd, Cmd::None) {
                    out.push(cmd);
                }
            }
            Event::Resize(c, r) => out.push(Cmd::Resize(c, r)),
            _ => {}
        }
    }
    Ok(out)
}
