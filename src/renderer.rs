//! Render en terminal — backends Kitty / HalfBlocks / ASCII.
//!
//! Cambios v0.2:
//!   * `fit_aspect` corregido para no perder pixel al alinear con la celda
//!     (redondeo hacia abajo al múltiplo de la celda para evitar
//!     letterbox "roto").
//!   * `reset_layout_cache` para forzar clear al resize/seek.
//!   * Los píxeles/celda vienen del `CellPx` detectado, no fijos.
//!
//! Cambios v0.6 (resize robusto):
//!   * **Recorte a los límites de la terminal en TODOS los backends**:
//!     durante un resize llegan frames con dims "viejas" (más grandes
//!     que la terminal recién encogida) — antes se escribían fuera de
//!     pantalla → basura visual, scroll fantasma y pánicos. Ahora
//!     `draw()` recibe el área útil (cols × filas sin el HUD) y cada
//!     backend recorta filas y columnas que no caben. Los frames con
//!     dims desfasadas se muestran recortados durante los ~1-2 frames
//!     que tarda el decoder en aplicar las dims nuevas — sin perder
//!     el colchón de pre-decode ni el sync.
//!   * `set_cell_px` para que el recorte de Kitty sepa cuántos
//!     píxeles ocupa cada celda.

use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use std::io::{StdoutLock, Write};

use crate::decoder::RgbFrame;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Kitty,
    Iterm2,
    Sixel,
    HalfBlocks,
    Ascii,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Backend::Kitty => "kitty",
            Backend::Iterm2 => "iterm2",
            Backend::Sixel => "sixel",
            Backend::HalfBlocks => "blocks",
            Backend::Ascii => "ascii",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "kitty" => Backend::Kitty,
            "iterm2" | "iterm" => Backend::Iterm2,
            "sixel" => Backend::Sixel,
            "blocks" | "halfblocks" | "half" => Backend::HalfBlocks,
            "ascii" => Backend::Ascii,
            _ => return None,
        })
    }
}

/// Detección heurística por variables de entorno.
pub fn detect_backend(forced: Option<&str>) -> Backend {
    if let Some(f) = forced {
        if let Some(b) = Backend::from_str(f) {
            return b;
        }
    }
    let term = std::env::var("TERM").unwrap_or_default();
    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let kitty = std::env::var("KITTY_WINDOW_ID").is_ok()
        || term.contains("kitty")
        || term_program.eq_ignore_ascii_case("ghostty")
        || term_program.eq_ignore_ascii_case("wezterm");
    if kitty {
        return Backend::Kitty;
    }
    Backend::HalfBlocks
}

/// Ajusta (w, h) manteniendo aspect ratio del vídeo dentro del área destino.
/// Devuelve las nuevas dims + offsets en píxeles (para centrar el letterbox).
///
/// Extra: alinea a múltiplo de `cell_w`/`cell_h` en el eje "corto" para que
/// el letterbox coincida exactamente con celdas del terminal — evita franjas
/// negras "medias" que ensucian visualmente.
pub fn fit_aspect(
    src: (u32, u32),
    dst: (u32, u32),
    align_w: u32,
    align_h: u32,
) -> ((u32, u32), (u32, u32)) {
    let (sw, sh) = (src.0 as f32, src.1 as f32);
    let (dw, dh) = (dst.0 as f32, dst.1 as f32);
    if sw <= 0.0 || sh <= 0.0 {
        return ((dst.0.max(2), dst.1.max(2)), (0, 0));
    }
    let ar_src = sw / sh;
    let ar_dst = dw / dh;
    let (mut w, mut h) = if ar_src > ar_dst {
        (dw as u32, ((dw / ar_src).max(2.0)) as u32)
    } else {
        (((dh * ar_src).max(2.0)) as u32, dh as u32)
    };
    // Alinear (floor) al múltiplo de la celda para que ox/oy también caigan
    // en frontera de celda.
    if align_w > 1 {
        w = (w / align_w).max(1) * align_w;
    }
    if align_h > 1 {
        h = (h / align_h).max(1) * align_h;
    }
    w = w.min(dst.0).max(2);
    h = h.min(dst.1).max(2);
    let ox = (dst.0.saturating_sub(w)) / 2;
    let oy = (dst.1.saturating_sub(h)) / 2;
    // También alineamos ox/oy a la celda.
    let ox = if align_w > 1 { (ox / align_w) * align_w } else { ox };
    let oy = if align_h > 1 { (oy / align_h) * align_h } else { oy };
    ((w, h), (ox, oy))
}

pub struct Renderer {
    pub backend: Backend,
    scratch: Vec<u8>,
    b64: String,
    last_layout: Option<(u16, u16, u32, u32, u32, u32)>,
    /// Píxeles por celda del terminal (solo relevante para Kitty:
    /// halfblocks es 1×2 y ascii 1×1 implícitos). Se usa para el
    /// recorte a los límites de la terminal.
    cell_px_w: u32,
    cell_px_h: u32,
    /// Buffer de recorte para Kitty (crop del RGB antes del base64).
    crop_buf: Vec<u8>,
}

impl Renderer {
    pub fn new(backend: Backend) -> Self {
        Self {
            backend,
            scratch: Vec::with_capacity(1 << 20),
            b64: String::with_capacity(1 << 20),
            last_layout: None,
            cell_px_w: 8,
            cell_px_h: 16,
            crop_buf: Vec::new(),
        }
    }

    /// Informa al renderer del tamaño de celda en píxeles (para el
    /// recorte del backend Kitty). Llamar al arrancar y tras resize
    /// si el cell size cambia.
    pub fn set_cell_px(&mut self, w: u32, h: u32) {
        self.cell_px_w = w.max(1);
        self.cell_px_h = h.max(1);
    }

    pub fn reset_layout_cache(&mut self) {
        self.last_layout = None;
    }

    /// Dibuja `frame` con su esquina superior izquierda en la celda
    /// (col_ox, row_oy), RECORTANDO a un área útil de `max_cols` ×
    /// `max_rows` celdas (el área del terminal sin el HUD). Tolera
    /// frames con dims que no cuadran con el layout actual (resize en
    /// vuelo): se pintan recortados en vez de desbordar la pantalla.
    pub fn draw<W: Write>(
        &mut self,
        out: &mut W,
        frame: &RgbFrame,
        max_cols: u16,
        max_rows: u16,
        col_ox: u16,
        row_oy: u16,
    ) -> Result<()> {
        if max_cols == 0 || max_rows == 0 || frame.width == 0 || frame.height == 0 {
            return Ok(());
        }
        // Offsets clampeados al área útil.
        let col_ox = col_ox.min(max_cols.saturating_sub(1));
        let row_oy = row_oy.min(max_rows.saturating_sub(1));

        let layout = (
            max_cols,
            max_rows,
            frame.width,
            frame.height,
            col_ox as u32,
            row_oy as u32,
        );
        if self.last_layout != Some(layout) {
            self.scratch.clear();
            self.scratch.extend_from_slice(b"\x1b[2J\x1b[H");
            out.write_all(&self.scratch)?;
            self.last_layout = Some(layout);
        }

        match self.backend {
            Backend::Kitty => self.draw_kitty(out, frame, max_cols, max_rows, col_ox, row_oy),
            Backend::HalfBlocks | Backend::Iterm2 | Backend::Sixel => {
                self.draw_halfblocks(out, frame, max_cols, max_rows, col_ox, row_oy)
            }
            Backend::Ascii => self.draw_ascii(out, frame, max_cols, max_rows, col_ox, row_oy),
        }
    }

    fn draw_halfblocks<W: Write>(
        &mut self,
        out: &mut W,
        frame: &RgbFrame,
        max_cols: u16,
        max_rows: u16,
        col_ox: u16,
        row_oy: u16,
    ) -> Result<()> {
        let w = frame.width as usize;
        let h = frame.height as usize;
        let stride = w * 3;
        let data = &frame.data;
        if data.len() < h * stride {
            return Ok(()); // frame corrupto/incompleto: no pintar
        }

        self.scratch.clear();
        let mut last_fg: (u8, u8, u8) = (255, 255, 255);
        let mut last_bg: (u8, u8, u8) = (0, 0, 0);

        // Recorte: filas de celdas y columnas visibles dentro del
        // área útil. 1 celda = 1×2 px en halfblocks.
        let rows = (h / 2).min((max_rows - row_oy) as usize);
        let vis_w = w.min((max_cols - col_ox) as usize);
        for cy in 0..rows {
            let term_row = row_oy as usize + cy + 1;
            let term_col = col_ox as usize + 1;
            write!(&mut self.scratch, "\x1b[{};{}H", term_row, term_col)?;
            let mut first_cell = true;

            let y_top = cy * 2;
            let y_bot = y_top + 1;
            let row_top = &data[y_top * stride..y_top * stride + stride];
            let row_bot = &data[y_bot * stride..y_bot * stride + stride];

            for x in 0..vis_w {
                let i = x * 3;
                let fg = (row_top[i], row_top[i + 1], row_top[i + 2]);
                let bg = (row_bot[i], row_bot[i + 1], row_bot[i + 2]);

                if first_cell || fg != last_fg {
                    write!(&mut self.scratch, "\x1b[38;2;{};{};{}m", fg.0, fg.1, fg.2)?;
                    last_fg = fg;
                }
                if first_cell || bg != last_bg {
                    write!(&mut self.scratch, "\x1b[48;2;{};{};{}m", bg.0, bg.1, bg.2)?;
                    last_bg = bg;
                }
                first_cell = false;
                self.scratch.extend_from_slice(&[0xE2, 0x96, 0x80]);
            }
        }
        self.scratch.extend_from_slice(b"\x1b[0m");
        out.write_all(&self.scratch)?;
        Ok(())
    }

    fn draw_kitty<W: Write>(
        &mut self,
        out: &mut W,
        frame: &RgbFrame,
        max_cols: u16,
        max_rows: u16,
        col_ox: u16,
        row_oy: u16,
    ) -> Result<()> {
        let w = frame.width as usize;
        let h = frame.height as usize;
        let stride = w * 3;
        if frame.data.len() < h * stride {
            return Ok(());
        }

        // Recorte en PÍXELES al área útil: si el frame (dims viejas
        // durante un resize) no cabe, mandamos solo el sub-rectángulo
        // visible. Sin esto la imagen desbordaba el área de vídeo y
        // pisaba el HUD / provocaba scroll.
        let avail_px_w = (max_cols - col_ox) as usize * self.cell_px_w as usize;
        let avail_px_h = (max_rows - row_oy) as usize * self.cell_px_h as usize;
        let vis_w = w.min(avail_px_w).max(1);
        let vis_h = h.min(avail_px_h).max(1);

        let payload: &[u8] = if vis_w == w && vis_h == h {
            &frame.data
        } else {
            self.crop_buf.clear();
            self.crop_buf.reserve(vis_w * vis_h * 3);
            for y in 0..vis_h {
                let s = y * stride;
                self.crop_buf.extend_from_slice(&frame.data[s..s + vis_w * 3]);
            }
            &self.crop_buf
        };

        self.scratch.clear();
        write!(
            &mut self.scratch,
            "\x1b[{};{}H",
            row_oy as usize + 1,
            col_ox as usize + 1
        )?;
        // Borrar imágenes previas para no acumular memoria en el terminal.
        self.scratch.extend_from_slice(b"\x1b_Ga=d,d=A,q=2;\x1b\\");

        self.b64.clear();
        B64.encode_string(payload, &mut self.b64);

        let bytes = self.b64.as_bytes();
        const CHUNK: usize = 4096;
        let mut i = 0;
        let mut first = true;
        while i < bytes.len() {
            let end = (i + CHUNK).min(bytes.len());
            let more = end < bytes.len();
            let m = if more { 1 } else { 0 };
            if first {
                write!(
                    &mut self.scratch,
                    "\x1b_Ga=T,f=24,s={},v={},q=2,m={};",
                    vis_w, vis_h, m
                )?;
                first = false;
            } else {
                write!(&mut self.scratch, "\x1b_Gm={},q=2;", m)?;
            }
            self.scratch.extend_from_slice(&bytes[i..end]);
            self.scratch.extend_from_slice(b"\x1b\\");
            i = end;
        }
        out.write_all(&self.scratch)?;
        Ok(())
    }

    fn draw_ascii<W: Write>(
        &mut self,
        out: &mut W,
        frame: &RgbFrame,
        max_cols: u16,
        max_rows: u16,
        col_ox: u16,
        row_oy: u16,
    ) -> Result<()> {
        const GRAD: &[u8] = b" .:-=+*#%@";
        let w = frame.width as usize;
        let h = frame.height as usize;
        let stride = w * 3;
        if frame.data.len() < h * stride {
            return Ok(());
        }
        self.scratch.clear();
        // Recorte: 1 celda = 1×1 px en ascii.
        let vis_h = h.min((max_rows - row_oy) as usize);
        let vis_w = w.min((max_cols - col_ox) as usize);
        for cy in 0..vis_h {
            write!(
                &mut self.scratch,
                "\x1b[{};{}H",
                row_oy as usize + cy + 1,
                col_ox as usize + 1
            )?;
            let row = &frame.data[cy * stride..cy * stride + stride];
            for x in 0..vis_w {
                let i = x * 3;
                let l = (row[i] as u32 * 299 + row[i + 1] as u32 * 587 + row[i + 2] as u32 * 114)
                    / 1000;
                let idx = (l as usize * (GRAD.len() - 1)) / 255;
                self.scratch.push(GRAD[idx]);
            }
        }
        out.write_all(&self.scratch)?;
        Ok(())
    }
}

/// Escribe el HUD en las 1 ó 2 últimas filas del terminal.
///
/// El HUD ahora es ADAPTATIVO al ancho del terminal:
///   * < 60 cols: solo tiempo y fps.
///   * 60..120 cols: barra corta + tiempo + volumen + fps + hint teclas.
///   * >= 120 cols: barra larga + tiempo + volumen + backend + fps detallado.
///
/// El caller elige cuántas filas reservar (`reserve_rows`); si es 2, la primera
/// es una barra de progreso "grande" con detalles y la segunda es la línea de
/// atajos de teclado.
/// Escribe UNA línea del HUD en la fila `row` (1-indexed). Antes de escribir,
/// resetea SGR + limpia la fila entera + reposiciona cursor a col 1 de la
/// fila. La línea se trunca a `cols` caracteres visibles y se rellena con
/// espacios normales para tapar cualquier resto del frame anterior.
pub fn draw_hud_at(out: &mut StdoutLock, cols: u16, row: u16, line: &str) -> Result<()> {
    let content = truncate_to_cells(line, cols as usize);
    let content_chars = content.chars().count();
    // Rellenamos manualmente con espacios hasta `cols` para no depender del
    // pad con formato (que cuenta bytes, no caracteres visibles).
    let pad_needed = cols as usize - content_chars.min(cols as usize);
    // Secuencia: reset SGR → mover cursor → borrar fila → texto → padding → reset.
    // El reset final evita que un color "colgado" contamine el frame siguiente.
    write!(
        out,
        "\x1b[0m\x1b[{};1H\x1b[2K{}{}\x1b[0m",
        row,
        content,
        " ".repeat(pad_needed),
    )?;
    Ok(())
}

/// HUD de una sola línea, en la última fila.
pub fn draw_hud(out: &mut StdoutLock, cols: u16, rows: u16, line: &str) -> Result<()> {
    draw_hud_at(out, cols, rows, line)
}

/// HUD de dos líneas, en las dos últimas filas.
pub fn draw_hud_two_lines(
    out: &mut StdoutLock,
    cols: u16,
    rows: u16,
    line1: &str,
    line2: &str,
) -> Result<()> {
    draw_hud_at(out, cols, rows.saturating_sub(1).max(1), line1)?;
    draw_hud_at(out, cols, rows, line2)?;
    Ok(())
}

/// Trunca por número de CARACTERES visibles (no bytes) — importante para
/// evitar romper secuencias UTF-8 y sobredibujar líneas cuando el HUD tiene
/// caracteres multibyte (█, ░, ▶, ⏸, ·).
fn truncate_to_cells(s: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(s.len());
    let mut count = 0usize;
    for c in s.chars() {
        if count >= max_chars {
            break;
        }
        out.push(c);
        // Aproximación: consideramos que estos caracteres ocupan 1 celda.
        // No manejamos wide chars (CJK) porque el HUD es ASCII-latino.
        count += 1;
    }
    out
}
