//! info.rs — `rtv --info`: inspección del fichero SIN reproducirlo.
//!
//! Muestra: fichero (nombre, tamaño, fecha), contenedor (formato,
//! duración, bitrate, metadatos), pista de vídeo (codec, resolución +
//! etiqueta de calidad, fps, formato de píxel), TODAS las pistas de
//! audio y subtítulos (idioma, título, codec, canales…) y capítulos.
//!
//! Solo demux de cabeceras (como tracks::probe): no se decodifica ni
//! un frame, así que es instantáneo incluso con ficheros enormes.

use anyhow::{Context as _, Result};
use ffmpeg_the_third as ffmpeg;
use ffmpeg::media::Type;
use std::ffi::CStr;
use std::fmt::Write as _;
use std::io::IsTerminal;
use std::path::Path;

/// Punto de entrada de `--info`. Imprime a stdout y devuelve error solo
/// si el fichero no se puede abrir/demuxear.
pub fn print_info(path: &Path) -> Result<()> {
    let ictx = ffmpeg::format::input(path)
        .with_context(|| format!("no se pudo abrir {}", path.display()))?;
    let color = std::io::stdout().is_terminal();
    print!("{}", render(path, &ictx, color));
    Ok(())
}

/// Construye el informe completo como String (separado de print_info
/// para poder testearlo).
fn render(path: &Path, ictx: &ffmpeg::format::context::Input, color: bool) -> String {
    let mut o = String::new();
    let h = |s: &str| {
        if color {
            format!("\x1b[1;36m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    };
    let b = |s: &str| {
        if color {
            format!("\x1b[1m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    };

    // ------------------------------------------------------- Fichero --
    let _ = writeln!(o, "{}", h("Fichero"));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let _ = writeln!(o, "  Nombre:     {}", b(&name));
    let _ = writeln!(o, "  Ruta:       {}", path.display());
    if let Ok(md) = std::fs::metadata(path) {
        let _ = writeln!(o, "  Tamaño:     {}", human_size(md.len()));
        if let Ok(mtime) = md.modified() {
            if let Ok(d) = mtime.duration_since(std::time::UNIX_EPOCH) {
                let _ = writeln!(o, "  Modificado: {} UTC", fmt_epoch(d.as_secs() as i64));
            }
        }
    }

    // ---------------------------------------------------- Contenedor --
    // Se acumulan (etiqueta, valor) y se imprimen al final con la
    // columna alineada al label más largo (compatible_brands rompía la
    // alineación fija de 11 chars).
    let _ = writeln!(o, "\n{}", h("Contenedor"));
    let fmt = ictx.format();
    let mut rows: Vec<(String, String)> = Vec::new();
    rows.push((
        "Formato".into(),
        format!("{} ({})", fmt.name(), fmt.description()),
    ));
    let dur_us = ictx.duration();
    if dur_us > 0 {
        let secs = dur_us as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE);
        rows.push(("Duración".into(), fmt_duration(secs)));
    }
    let br = ictx.bit_rate();
    if br > 0 {
        rows.push(("Bitrate".into(), human_bitrate(br)));
    }
    // Metadatos del contenedor: título y fecha primero (lo que pide la
    // gente), luego el resto por orden alfabético.
    let meta = ictx.metadata();
    let mut kv: Vec<(String, String)> = meta
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    // Matroska guarda las claves en mayúsculas ("ENCODER"); se comparan
    // siempre en minúsculas para que el etiquetado no dependa del mux.
    kv.sort_by(|a, b2| {
        let rank = |k: &str| match k.to_ascii_lowercase().as_str() {
            "title" => 0,
            "creation_time" | "date" => 1,
            _ => 2,
        };
        (rank(&a.0), a.0.to_ascii_lowercase()).cmp(&(rank(&b2.0), b2.0.to_ascii_lowercase()))
    });
    for (k, v) in &kv {
        let key = k.to_ascii_lowercase();
        let label = match key.as_str() {
            "title" => "Título",
            "creation_time" => "Creado",
            "date" => "Fecha",
            "encoder" => "Encoder",
            "artist" => "Artista",
            "comment" => "Comentario",
            "compatible_brands" => "Brands",
            "major_brand" => "Brand",
            "minor_version" => "Versión menor",
            _ => k.as_str(),
        };
        let mut val = match key.as_str() {
            // "20240423" → "2024-04-23"; ISO → "YYYY-MM-DD HH:MM:SS UTC"
            "date" | "creation_time" => pretty_date(v).unwrap_or_else(|| v.clone()),
            // "isomiso2avc1mp41" → "isom, iso2, avc1, mp41"
            "compatible_brands" => pretty_brands(v),
            _ => v.clone(),
        };
        if val.chars().count() > 70 {
            val = format!("{}…", val.chars().take(69).collect::<String>());
        }
        rows.push((label.to_string(), val));
    }
    let pad = rows.iter().map(|(l, _)| l.chars().count()).max().unwrap_or(0);
    for (label, val) in &rows {
        let fill = " ".repeat(pad - label.chars().count());
        let _ = writeln!(o, "  {label}:{fill} {val}");
    }

    // -------------------------------------------------------- Pistas --
    let mut videos = Vec::new();
    let mut audios = Vec::new();
    let mut subs = Vec::new();
    let mut others = 0usize;
    for stream in ictx.streams() {
        match stream.parameters().medium() {
            Type::Video => videos.push(stream),
            Type::Audio => audios.push(stream),
            Type::Subtitle => subs.push(stream),
            _ => others += 1,
        }
    }

    let _ = writeln!(o, "\n{}", h(&format!("Vídeo ({})", videos.len())));
    for (i, s) in videos.iter().enumerate() {
        let p = unsafe { &*s.parameters().as_ptr() };
        let codec = codec_name(s.parameters().id());
        let mut line = format!("  #{} {}", i + 1, b(&codec));
        if p.width > 0 && p.height > 0 {
            let _ = write!(line, "  {}x{}", p.width, p.height);
            if let Some(q) = quality_label(p.height) {
                let _ = write!(line, " ({q})");
            }
        }
        let fps = s.avg_frame_rate();
        if fps.numerator() > 0 && fps.denominator() > 0 {
            let f = f64::from(fps.numerator()) / f64::from(fps.denominator());
            let _ = write!(line, "  {:.3} fps", f);
        }
        if let Some(pix) = pix_fmt_name(p.format) {
            let _ = write!(line, "  {pix}");
        }
        if p.bit_rate > 0 {
            let _ = write!(line, "  {}", human_bitrate(p.bit_rate));
        }
        let _ = writeln!(o, "{line}{}", stream_tags(s));
    }

    let _ = writeln!(o, "\n{}", h(&format!("Audio ({})", audios.len())));
    for (i, s) in audios.iter().enumerate() {
        let p = unsafe { &*s.parameters().as_ptr() };
        let codec = codec_name(s.parameters().id());
        let mut line = format!("  #{} {}", i + 1, b(&codec));
        let nch = p.ch_layout.nb_channels;
        if nch > 0 {
            let _ = write!(line, "  {}", channel_desc(&p.ch_layout, nch));
        }
        if p.sample_rate > 0 {
            let _ = write!(line, "  {} Hz", p.sample_rate);
        }
        if p.bit_rate > 0 {
            let _ = write!(line, "  {}", human_bitrate(p.bit_rate));
        }
        let _ = writeln!(o, "{line}{}", stream_tags(s));
    }

    let _ = writeln!(o, "\n{}", h(&format!("Subtítulos ({})", subs.len())));
    for (i, s) in subs.iter().enumerate() {
        let codec = codec_name(s.parameters().id());
        let kind = if crate::tracks::is_text_sub_codec(s.parameters().id()) {
            ""
        } else {
            "  [bitmap: no renderizable en terminal]"
        };
        let _ = writeln!(o, "  #{} {}{}{}", i + 1, b(&codec), stream_tags(s), kind);
    }
    if others > 0 {
        let _ = writeln!(o, "\n  (+{others} pistas de otros tipos: datos/adjuntos)");
    }

    // ----------------------------------------------------- Capítulos --
    let chapters: Vec<_> = ictx.chapters().collect();
    if !chapters.is_empty() {
        let _ = writeln!(o, "\n{}", h(&format!("Capítulos ({})", chapters.len())));
        for (i, ch) in chapters.iter().enumerate().take(30) {
            let tb = ch.time_base();
            let start = ch.start() as f64 * f64::from(tb.numerator())
                / f64::from(tb.denominator());
            let title = ch.metadata().get("title").unwrap_or("").to_string();
            let _ = writeln!(o, "  {:>2}. [{}] {}", i + 1, fmt_duration(start), title);
        }
        if chapters.len() > 30 {
            let _ = writeln!(o, "  … y {} más", chapters.len() - 30);
        }
    }
    o
}

/// " — spa, Comentarios, [default]" a partir de los metadatos y la
/// disposición de la pista. Vacío si no hay nada que decir.
fn stream_tags(s: &ffmpeg::format::stream::Stream) -> String {
    let md = s.metadata();
    let mut parts = Vec::new();
    if let Some(l) = md.get("language") {
        if !l.is_empty() && l != "und" {
            parts.push(l.to_string());
        }
    }
    if let Some(t) = md.get("title") {
        if !t.is_empty() {
            parts.push(t.to_string());
        }
    }
    let disp = s.disposition();
    if disp.contains(ffmpeg::format::stream::Disposition::DEFAULT) {
        parts.push("[default]".into());
    }
    if disp.contains(ffmpeg::format::stream::Disposition::FORCED) {
        parts.push("[forced]".into());
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("  — {}", parts.join(", "))
    }
}

fn codec_name(id: ffmpeg::codec::Id) -> String {
    format!("{id:?}").to_ascii_lowercase()
}

fn pix_fmt_name(format: i32) -> Option<String> {
    unsafe {
        let p = ffmpeg::sys::av_get_pix_fmt_name(std::mem::transmute::<
            i32,
            ffmpeg::sys::AVPixelFormat,
        >(format));
        if p.is_null() {
            None
        } else {
            Some(CStr::from_ptr(p).to_string_lossy().into_owned())
        }
    }
}

/// "stereo", "5.1", "mono"… vía av_channel_layout_describe; si falla,
/// "N canales".
fn channel_desc(layout: &ffmpeg::sys::AVChannelLayout, nch: i32) -> String {
    let mut buf = [0u8; 64];
    // Cast vía c_char: en x86 c_char = i8 pero en ARM/aarch64 = u8 —
    // un cast a *mut i8 rompía las builds arm de Linux y Termux.
    let n = unsafe {
        ffmpeg::sys::av_channel_layout_describe(
            layout,
            buf.as_mut_ptr() as *mut std::os::raw::c_char,
            buf.len(),
        )
    };
    if n > 0 && (n as usize) < buf.len() {
        if let Ok(s) = std::str::from_utf8(&buf[..n as usize]) {
            let s = s.trim_end_matches('\0');
            if !s.is_empty() && !s.starts_with("unknown") {
                return s.to_string();
            }
        }
    }
    format!("{nch} canales")
}

/// Etiqueta de calidad estándar a partir de la ALTURA del vídeo.
fn quality_label(height: i32) -> Option<&'static str> {
    Some(match height {
        h if h >= 4320 => "8K",
        h if h >= 2160 => "4K",
        h if h >= 1440 => "1440p",
        h if h >= 1080 => "1080p",
        h if h >= 720 => "720p",
        h if h >= 480 => "480p",
        h if h >= 360 => "360p",
        h if h >= 240 => "240p",
        _ => return None,
    })
}

fn human_size(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.2} {} ({bytes} bytes)", U[i])
    }
}

fn human_bitrate(bps: i64) -> String {
    if bps >= 1_000_000 {
        format!("{:.2} Mb/s", bps as f64 / 1e6)
    } else {
        format!("{:.0} kb/s", bps as f64 / 1e3)
    }
}

/// "H:MM:SS.mmm" (o "MM:SS.mmm" si dura menos de una hora).
fn fmt_duration(secs: f64) -> String {
    let total_ms = (secs.max(0.0) * 1000.0).round() as u64;
    let ms = total_ms % 1000;
    let s = (total_ms / 1000) % 60;
    let m = (total_ms / 60_000) % 60;
    let hs = total_ms / 3_600_000;
    if hs > 0 {
        format!("{hs}:{m:02}:{s:02}.{ms:03}")
    } else {
        format!("{m:02}:{s:02}.{ms:03}")
    }
}

/// Epoch (segundos UTC) → "YYYY-MM-DD HH:MM:SS" sin dependencias
/// (algoritmo civil_from_days de Howard Hinnant).
fn fmt_epoch(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}")
}

/// Normaliza las fechas típicas de los metadatos de contenedor.
///
/// * `"20240423"` (QuickTime/`date`) → `"2024-04-23"`.
/// * `"2024-04-23T10:31:02.000000Z"` (ISO 8601 / `creation_time`) →
///   `"2024-04-23 10:31:02 UTC"` (sin la `Z` ni los microsegundos).
///
/// Devuelve `None` si el formato no se reconoce; el llamador muestra
/// entonces el valor tal cual venía.
fn pretty_date(v: &str) -> Option<String> {
    let t = v.trim();
    // "YYYYMMDD" compacto.
    if t.len() == 8 && t.bytes().all(|b| b.is_ascii_digit()) {
        return Some(format!("{}-{}-{}", &t[..4], &t[4..6], &t[6..8]));
    }
    // "YYYY-MM-DD" ya legible: se deja igual (pero se valida).
    if t.len() == 10 && t.as_bytes()[4] == b'-' && t.as_bytes()[7] == b'-' {
        return Some(t.to_string());
    }
    // ISO 8601: "YYYY-MM-DDTHH:MM:SS[.ffffff][Z]".
    if t.len() >= 19 && t.as_bytes().get(10) == Some(&b'T') {
        let (date, time) = (&t[..10], &t[11..19]);
        let utc = t.ends_with('Z');
        return Some(format!("{date} {time}{}", if utc { " UTC" } else { "" }));
    }
    None
}

/// Separa la ristra `compatible_brands` en códigos de 4 caracteres:
/// `"isomiso2avc1mp41"` → `"isom, iso2, avc1, mp41"`. Si la longitud no
/// es múltiplo de 4 (valor raro), se devuelve tal cual.
fn pretty_brands(v: &str) -> String {
    let t = v.trim();
    if t.len() <= 4 || t.len() % 4 != 0 || !t.is_ascii() {
        return t.to_string();
    }
    t.as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_labels() {
        assert_eq!(quality_label(2160), Some("4K"));
        assert_eq!(quality_label(1080), Some("1080p"));
        assert_eq!(quality_label(1088), Some("1080p")); // altura codificada
        assert_eq!(quality_label(720), Some("720p"));
        assert_eq!(quality_label(100), None);
    }

    #[test]
    fn sizes_and_bitrates() {
        assert_eq!(human_size(500), "500 B");
        assert_eq!(human_size(1536), "1.50 KiB (1536 bytes)");
        assert_eq!(human_bitrate(128_000), "128 kb/s");
        assert_eq!(human_bitrate(2_500_000), "2.50 Mb/s");
    }

    #[test]
    fn durations() {
        assert_eq!(fmt_duration(0.0), "00:00.000");
        assert_eq!(fmt_duration(65.5), "01:05.500");
        assert_eq!(fmt_duration(3661.25), "1:01:01.250");
    }

    #[test]
    fn epoch_dates() {
        assert_eq!(fmt_epoch(0), "1970-01-01 00:00:00");
        // 2024-02-29 12:00:00 UTC (bisiesto)
        assert_eq!(fmt_epoch(1_709_208_000), "2024-02-29 12:00:00");
    }

    #[test]
    fn metadata_dates() {
        assert_eq!(pretty_date("20240423"), Some("2024-04-23".into()));
        assert_eq!(pretty_date("2024-04-23"), Some("2024-04-23".into()));
        assert_eq!(
            pretty_date("2024-04-23T10:31:02.000000Z"),
            Some("2024-04-23 10:31:02 UTC".into())
        );
        assert_eq!(
            pretty_date("2024-04-23T10:31:02"),
            Some("2024-04-23 10:31:02".into())
        );
        assert_eq!(pretty_date("ayer"), None);
        assert_eq!(pretty_date("2024"), None);
    }

    #[test]
    fn metadata_brands() {
        assert_eq!(pretty_brands("isomiso2avc1mp41"), "isom, iso2, avc1, mp41");
        assert_eq!(pretty_brands("isom"), "isom"); // uno solo, sin comas
        assert_eq!(pretty_brands("abcde"), "abcde"); // longitud rara
    }
}
