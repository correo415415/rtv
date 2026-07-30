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
    // iTerm2: TERM_PROGRAM=iTerm.app en local; LC_TERMINAL=iTerm2 se
    // propaga por ssh (iTerm2 lo exporta y sshd suele aceptar LC_*).
    let lc_terminal = std::env::var("LC_TERMINAL").unwrap_or_default();
    if term_program.eq_ignore_ascii_case("iTerm.app")
        || lc_terminal.eq_ignore_ascii_case("iTerm2")
    {
        return Backend::Iterm2;
    }
    // Sixel: terminales que lo soportan de serie. xterm solo lo activa
    // compilado con --enable-sixel-graphics Y lanzado con -ti vt340 →
    // en ese caso TERM suele ser "xterm-sixel" (o el usuario fuerza
    // --backend sixel). mlterm/foot/contour lo traen siempre.
    if term.contains("sixel")
        || term.starts_with("mlterm")
        || std::env::var("MLTERM").is_ok()
        || term == "foot" || term == "foot-extra"
        || term.starts_with("contour")
        || term_program.eq_ignore_ascii_case("contour")
    {
        return Backend::Sixel;
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
    /// Buffer de recorte para Kitty/iTerm2 (crop del RGB antes del base64).
    crop_buf: Vec<u8>,
    /// Sixel: buffer de índices de paleta (1 byte/px) del frame ya
    /// cuantizado + ditheado.
    sixel_idx: Vec<u8>,
    /// Sixel: máscaras de bits por color de la banda en curso
    /// (layout [color][columna]) para el pase por color.
    sixel_band: Vec<u8>,
    /// Sixel: definición de la paleta fija (se construye una vez).
    sixel_palette: String,
    /// iTerm2: buffer del fichero BMP construido en memoria.
    file_buf: Vec<u8>,
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
            sixel_idx: Vec::new(),
            sixel_band: Vec::new(),
            sixel_palette: build_sixel_palette(),
            file_buf: Vec::new(),
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
    ///
    /// Devuelve `true` si emitió un clear de pantalla completo (2J) —
    /// el caller debe invalidar su caché de HUD porque la fila del
    /// HUD también fue borrada.
    pub fn draw<W: Write>(
        &mut self,
        out: &mut W,
        frame: &RgbFrame,
        max_cols: u16,
        max_rows: u16,
        col_ox: u16,
        row_oy: u16,
    ) -> Result<bool> {
        if max_cols == 0 || max_rows == 0 || frame.width == 0 || frame.height == 0 {
            return Ok(false);
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
        // Synchronized output (DEC 2026): el terminal acumula todo lo
        // que llegue entre ?2026h y ?2026l y lo presenta en UN solo
        // refresco. Elimina el flash negro visible entre el clear (2J)
        // y el repintado del frame (p.ej. al pulsar `j` con cambio de
        // layout, o en resizes). Windows Terminal, kitty, WezTerm,
        // foot, iTerm2… lo soportan; el resto lo ignora sin daño.
        out.write_all(b"\x1b[?2026h")?;
        let mut cleared = false;
        if self.last_layout != Some(layout) {
            out.write_all(b"\x1b[2J\x1b[H")?;
            self.last_layout = Some(layout);
            cleared = true;
        }

        let res = match self.backend {
            Backend::Kitty => self.draw_kitty(out, frame, max_cols, max_rows, col_ox, row_oy),
            Backend::Iterm2 => self.draw_iterm2(out, frame, max_cols, max_rows, col_ox, row_oy),
            Backend::Sixel => self.draw_sixel(out, frame, max_cols, max_rows, col_ox, row_oy),
            Backend::HalfBlocks => {
                self.draw_halfblocks(out, frame, max_cols, max_rows, col_ox, row_oy)
            }
            Backend::Ascii => self.draw_ascii(out, frame, max_cols, max_rows, col_ox, row_oy),
        };
        // Cerrar el batch SIEMPRE, incluso si el backend falló — un
        // ?2026h colgado congelaría la pantalla del terminal.
        out.write_all(b"\x1b[?2026l")?;
        res?;
        Ok(cleared)
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

    /// iTerm2 — protocolo de imágenes inline (OSC 1337 File=).
    ///
    /// Construimos un BMP de 24 bits SIN compresión en memoria (cero
    /// dependencias, coste ~memcpy) y lo mandamos en base64. iTerm2 lo
    /// decodifica con NSImage (BMP soportado nativamente). `width`/
    /// `height` van en CELDAS para que el mapeo a la rejilla del
    /// terminal sea exacto e independiente del factor Retina (los
    /// valores en px del protocolo son "puntos", no píxeles: en
    /// pantallas 2x la imagen saldría al doble de tamaño).
    fn draw_iterm2<W: Write>(
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

        // Recorte en píxeles al área útil (mismo criterio que Kitty).
        let avail_px_w = (max_cols - col_ox) as usize * self.cell_px_w as usize;
        let avail_px_h = (max_rows - row_oy) as usize * self.cell_px_h as usize;
        let vis_w = w.min(avail_px_w).max(1);
        let vis_h = h.min(avail_px_h).max(1);

        // BMP 24bpp: cabecera 14 + DIB 40, filas BGR bottom-up con
        // padding a múltiplo de 4 bytes.
        let row_bytes = (vis_w * 3 + 3) & !3;
        let img_size = row_bytes * vis_h;
        let file_size = 54 + img_size;
        self.file_buf.clear();
        self.file_buf.reserve(file_size);
        let fb = &mut self.file_buf;
        fb.extend_from_slice(b"BM");
        fb.extend_from_slice(&(file_size as u32).to_le_bytes());
        fb.extend_from_slice(&0u32.to_le_bytes()); // reservado
        fb.extend_from_slice(&54u32.to_le_bytes()); // offset de píxeles
        fb.extend_from_slice(&40u32.to_le_bytes()); // tamaño DIB
        fb.extend_from_slice(&(vis_w as i32).to_le_bytes());
        fb.extend_from_slice(&(vis_h as i32).to_le_bytes());
        fb.extend_from_slice(&1u16.to_le_bytes()); // planos
        fb.extend_from_slice(&24u16.to_le_bytes()); // bpp
        fb.extend_from_slice(&0u32.to_le_bytes()); // sin compresión
        fb.extend_from_slice(&(img_size as u32).to_le_bytes());
        fb.extend_from_slice(&2835i32.to_le_bytes()); // 72 dpi
        fb.extend_from_slice(&2835i32.to_le_bytes());
        fb.extend_from_slice(&0u32.to_le_bytes());
        fb.extend_from_slice(&0u32.to_le_bytes());
        for y in (0..vis_h).rev() {
            let row = &frame.data[y * stride..y * stride + vis_w * 3];
            for px in row.chunks_exact(3) {
                fb.extend_from_slice(&[px[2], px[1], px[0]]); // RGB→BGR
            }
            for _ in vis_w * 3..row_bytes {
                fb.push(0);
            }
        }

        self.b64.clear();
        B64.encode_string(&self.file_buf, &mut self.b64);

        let cells_w = vis_w.div_ceil(self.cell_px_w.max(1) as usize);
        let cells_h = vis_h.div_ceil(self.cell_px_h.max(1) as usize);

        self.scratch.clear();
        write!(
            &mut self.scratch,
            "\x1b[{};{}H\x1b]1337;File=inline=1;size={};width={};height={};preserveAspectRatio=0:",
            row_oy as usize + 1,
            col_ox as usize + 1,
            file_size,
            cells_w,
            cells_h,
        )?;
        self.scratch.extend_from_slice(self.b64.as_bytes());
        self.scratch.push(0x07); // BEL — terminador OSC
        out.write_all(&self.scratch)?;
        Ok(())
    }

    /// Sixel — encoder real (DCS `ESC P q … ESC \`).
    ///
    /// Estrategia:
    ///   * Paleta FIJA de 252 registros (cubo RGB 6×7×6, más niveles
    ///     en verde: el ojo es más sensible). Se re-emite en CADA
    ///     frame: xterm usa registros de color PRIVADOS por imagen
    ///     (privateColorRegisters, default on) y sin la paleta cada
    ///     frame saldría en negro.
    ///   * Dithering ORDENADO (Bayer 4×4): sin dependencias seriales
    ///     entre píxeles (a diferencia de Floyd-Steinberg) → barato y
    ///     estable entre frames (el ruido no "hierve").
    ///   * Codificación por bandas de 6 filas: una pasada rellena
    ///     máscaras por color (`sixel_band`, [color][columna]) y cada
    ///     color presente se emite con RLE (`!n`), `$` (CR) entre
    ///     colores y `-` (LF) entre bandas.
    fn draw_sixel<W: Write>(
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

        let avail_px_w = (max_cols - col_ox) as usize * self.cell_px_w as usize;
        let avail_px_h = (max_rows - row_oy) as usize * self.cell_px_h as usize;
        let vis_w = w.min(avail_px_w).max(1);
        let vis_h = h.min(avail_px_h).max(1);

        // --- 1) Cuantización + dithering a índices de paleta ---
        const BAYER4: [u8; 16] = [0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5];
        #[inline(always)]
        fn quant(v: u8, levels: u32, t: u32) -> u32 {
            ((v as u32 * (levels - 1) * 16 + t * 255) / (255 * 16)).min(levels - 1)
        }
        self.sixel_idx.resize(vis_w * vis_h, 0);
        for y in 0..vis_h {
            let row = &frame.data[y * stride..y * stride + vis_w * 3];
            let dst = &mut self.sixel_idx[y * vis_w..(y + 1) * vis_w];
            let by = (y & 3) * 4;
            for x in 0..vis_w {
                let t = BAYER4[by + (x & 3)] as u32;
                let i = x * 3;
                let r = quant(row[i], 6, t);
                let g = quant(row[i + 1], 7, t);
                let b = quant(row[i + 2], 6, t);
                dst[x] = (r * 42 + g * 6 + b) as u8;
            }
        }

        // --- 2) Emisión ---
        self.scratch.clear();
        write!(
            &mut self.scratch,
            "\x1b[{};{}H",
            row_oy as usize + 1,
            col_ox as usize + 1
        )?;
        // DCS: P1=0 (aspect 1:1), P2=1 (bits a 0 → transparentes, no
        // pintan fondo fuera del letterbox), P3=0. Atributos raster
        // "Pan;Pad;Ph;Pv" ayudan al terminal a reservar el área.
        write!(&mut self.scratch, "\x1bP0;1;0q\"1;1;{};{}", vis_w, vis_h)?;
        self.scratch.extend_from_slice(self.sixel_palette.as_bytes());

        // Máscaras por color de la banda: [color][columna] → bits 0-5.
        self.sixel_band.resize(256 * vis_w, 0);
        let mut used: Vec<u16> = Vec::with_capacity(64);
        let mut present = [false; 256];

        let bands = vis_h.div_ceil(6);
        for band in 0..bands {
            let y0 = band * 6;
            let rows_in = (vis_h - y0).min(6);

            used.clear();
            present.fill(false);
            for j in 0..rows_in {
                let src = &self.sixel_idx[(y0 + j) * vis_w..(y0 + j + 1) * vis_w];
                let bit = 1u8 << j;
                for (x, &c) in src.iter().enumerate() {
                    let c = c as usize;
                    if !present[c] {
                        present[c] = true;
                        used.push(c as u16);
                    }
                    self.sixel_band[c * vis_w + x] |= bit;
                }
            }

            for (k, &c) in used.iter().enumerate() {
                write!(&mut self.scratch, "#{}", c)?;
                let rowm = &self.sixel_band[c as usize * vis_w..c as usize * vis_w + vis_w];
                // Recortar ceros finales: '?' (vacío) al final no aporta.
                let mut end = vis_w;
                while end > 0 && rowm[end - 1] == 0 {
                    end -= 1;
                }
                let mut x = 0;
                while x < end {
                    let v = rowm[x];
                    let mut run = 1;
                    while x + run < end && rowm[x + run] == v {
                        run += 1;
                    }
                    let ch = 63 + v;
                    if run >= 4 {
                        write!(&mut self.scratch, "!{}", run)?;
                        self.scratch.push(ch);
                    } else {
                        for _ in 0..run {
                            self.scratch.push(ch);
                        }
                    }
                    x += run;
                }
                if k + 1 < used.len() {
                    self.scratch.push(b'$'); // CR: siguiente color, misma banda
                } else if band + 1 < bands {
                    self.scratch.push(b'-'); // LF: siguiente banda
                }
            }

            // Limpiar solo las filas de colores usados (no las 256).
            for &c in &used {
                self.sixel_band[c as usize * vis_w..c as usize * vis_w + vis_w].fill(0);
            }
        }
        self.scratch.extend_from_slice(b"\x1b\\"); // ST — fin DCS
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
/// Escribe UNA línea del HUD en la fila `row` (1-indexed).
///
/// v0.7 — anti-parpadeo y anti-basura:
///   * El truncado y el padding se calculan por ANCHURA REAL en celdas
///     (unicode-width): los emojis del HUD (🔊, 🔇…) ocupan 2 celdas y
///     con el conteo por caracteres la línea desbordaba `cols` en la
///     ÚLTIMA fila → autowrap → scroll de TODA la pantalla → el frame
///     subía una línea, el siguiente frame repintaba… parpadeo masivo y
///     "texto basura por los lados" en terminales pequeñas.
///   * Sin `\x1b[2K`: borrar la fila y reescribirla produce flicker en
///     terminales lentas. El padding hasta `cols` ya cubre la fila
///     entera, así que la escritura es idéntica visualmente pero
///     atómica (sin estado intermedio en blanco).
/// Paleta fija sixel: cubo RGB 6×7×6 (252 registros). El eje verde
/// lleva 7 niveles (sensibilidad del ojo). Los valores van en escala
/// 0..100 como exige el protocolo (`#n;2;r;g;b`). Se construye una
/// vez y se re-emite en cada frame (los registros de color de xterm
/// son privados por imagen con la config por defecto).
fn build_sixel_palette() -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(252 * 16);
    for r in 0..6u32 {
        for g in 0..7u32 {
            for b in 0..6u32 {
                let idx = r * 42 + g * 6 + b;
                let rr = r * 100 / 5;
                let gg = g * 100 / 6;
                let bb = b * 100 / 5;
                let _ = write!(s, "#{};2;{};{};{}", idx, rr, gg, bb);
            }
        }
    }
    s
}

pub fn draw_hud_at(out: &mut StdoutLock, cols: u16, row: u16, line: &str) -> Result<()> {
    let (content, content_width) = truncate_to_width(line, cols as usize);
    let pad_needed = (cols as usize).saturating_sub(content_width);
    // Secuencia: reset SGR → mover cursor → texto → padding → reset.
    // El reset final evita que un color "colgado" contamine el frame siguiente.
    write!(
        out,
        "\x1b[0m\x1b[{};1H{}{}\x1b[0m",
        row,
        content,
        " ".repeat(pad_needed),
    )?;
    Ok(())
}

/// Línea de subtítulo: como `draw_hud_at` pero con estilo propio —
/// negrita + blanco brillante sobre el fondo de la terminal, que es
/// como los pintan los reproductores de vídeo y lo que hace el texto
/// legible sobre el letterbox (antes salían con el estilo por defecto
/// del terminal, finos y grises, difíciles de leer — ver captura del
/// issue). El padding a ancho completo va SIN estilo para no pintar
/// una franja de fondo.
pub fn draw_sub_line(out: &mut StdoutLock, cols: u16, row: u16, line: &str) -> Result<()> {
    let (content, content_width) = truncate_to_width(line, cols as usize);
    let pad_needed = (cols as usize).saturating_sub(content_width);
    write!(
        out,
        "\x1b[0m\x1b[{};1H\x1b[1;97m{}\x1b[0m{}",
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

/// Trunca por ANCHURA REAL en celdas (unicode-width, no chars ni bytes) y
/// devuelve también la anchura resultante. Crítico para el HUD: 🔊/🔇 son
/// wide (2 celdas) y █/░/▶/⏸/· son 1; contar "1 celda por char" hacía que
/// la línea real desbordara `cols` → autowrap+scroll en la última fila.
fn truncate_to_width(s: &str, max_width: usize) -> (String, usize) {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::with_capacity(s.len());
    let mut width = 0usize;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if width + cw > max_width {
            break;
        }
        out.push(c);
        width += cw;
    }
    (out, width)
}
