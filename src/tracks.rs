//! tracks.rs — inventario de pistas de audio y subtítulos del
//! contenedor, para el cambio de pista en runtime (teclas `a`/`#` y
//! `j`/`J`) y la selección por CLI (`--aid/--alang/--sid/--slang`).
//!
//! Se sondea UNA vez al abrir (demux de cabeceras, sin decodificar
//! nada). Los subtítulos solo listan pistas de TEXTO (SRT/ASS/
//! mov_text/WebVTT…): las bitmap (PGS/dvdsub) no se pueden renderizar
//! como texto en un terminal.

use ffmpeg_the_third as ffmpeg;
use ffmpeg::media::Type;
use std::path::Path;

/// Metadatos de una pista (audio o subtítulos).
#[derive(Debug, Clone)]
pub struct TrackInfo {
    /// Índice REAL del stream en el contenedor (para el demuxer).
    pub stream_index: usize,
    /// Idioma ("eng", "spa"…) o vacío si el contenedor no lo trae.
    pub lang: String,
    /// Título de la pista o vacío.
    pub title: String,
    /// Nombre corto del codec ("aac", "opus", "subrip"…).
    pub codec: String,
}

impl TrackInfo {
    /// Etiqueta compacta para HUD/OSD: "eng (aac)", "Comentario (ac3)",
    /// o el codec a secas si no hay metadatos.
    pub fn label(&self) -> String {
        let name = if !self.lang.is_empty() {
            self.lang.clone()
        } else if !self.title.is_empty() {
            self.title.clone()
        } else {
            return self.codec.clone();
        };
        if self.codec.is_empty() {
            name
        } else {
            format!("{name} ({})", self.codec)
        }
    }
}

/// ¿Es un codec de subtítulos de TEXTO renderizable?
pub fn is_text_sub_codec(id: ffmpeg::codec::Id) -> bool {
    use ffmpeg::codec::Id;
    matches!(
        id,
        Id::SUBRIP | Id::SRT | Id::ASS | Id::SSA | Id::TEXT | Id::MOV_TEXT | Id::WEBVTT
    )
}

/// Enumera (pistas de audio, pistas de subtítulos de texto) del
/// contenedor, en orden de aparición.
pub fn probe(path: &Path) -> (Vec<TrackInfo>, Vec<TrackInfo>) {
    let mut audio = Vec::new();
    let mut subs = Vec::new();
    let Ok(ictx) = ffmpeg::format::input(path) else {
        return (audio, subs);
    };
    for stream in ictx.streams() {
        let params = stream.parameters();
        let medium = params.medium();
        if medium != Type::Audio && medium != Type::Subtitle {
            continue;
        }
        if medium == Type::Subtitle && !is_text_sub_codec(params.id()) {
            continue;
        }
        let md = stream.metadata();
        let info = TrackInfo {
            stream_index: stream.index(),
            lang: md.get("language").unwrap_or("").to_string(),
            title: md.get("title").unwrap_or("").to_string(),
            codec: codec_short_name(params.id()),
        };
        if medium == Type::Audio {
            audio.push(info);
        } else {
            subs.push(info);
        }
    }
    (audio, subs)
}

fn codec_short_name(id: ffmpeg::codec::Id) -> String {
    format!("{id:?}").to_ascii_lowercase()
}

/// Resuelve la pista inicial pedida por CLI dentro de `tracks`:
///   * `id`   — índice 1-based DENTRO de las pistas de ese tipo
///     (`--aid 2` = segunda pista de audio), como mpv.
///   * `lang` — código de idioma; matching flexible case-insensitive
///     por prefijo en cualquier dirección ("en" casa con "eng" y
///     "eng" con "en").
/// Devuelve la POSICIÓN dentro de `tracks` (no el stream_index), o
/// `None` si no hay match (el caller decide el fallback).
pub fn select(tracks: &[TrackInfo], id: Option<usize>, lang: Option<&str>) -> Option<usize> {
    if let Some(n) = id {
        if n >= 1 && n <= tracks.len() {
            return Some(n - 1);
        }
        return None;
    }
    if let Some(l) = lang {
        let l = l.trim().to_ascii_lowercase();
        if l.is_empty() {
            return None;
        }
        return tracks.iter().position(|t| {
            let tl = t.lang.to_ascii_lowercase();
            !tl.is_empty() && (tl == l || tl.starts_with(&l) || l.starts_with(&tl))
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(idx: usize, lang: &str) -> TrackInfo {
        TrackInfo {
            stream_index: idx,
            lang: lang.into(),
            title: String::new(),
            codec: "aac".into(),
        }
    }

    #[test]
    fn select_by_id_is_one_based() {
        let ts = vec![t(1, "eng"), t(2, "spa")];
        assert_eq!(select(&ts, Some(1), None), Some(0));
        assert_eq!(select(&ts, Some(2), None), Some(1));
        assert_eq!(select(&ts, Some(3), None), None);
        assert_eq!(select(&ts, Some(0), None), None);
    }

    #[test]
    fn select_by_lang_prefix() {
        let ts = vec![t(1, "eng"), t(2, "spa")];
        assert_eq!(select(&ts, None, Some("spa")), Some(1));
        assert_eq!(select(&ts, None, Some("en")), Some(0));
        assert_eq!(select(&ts, None, Some("SPA")), Some(1));
        assert_eq!(select(&ts, None, Some("fra")), None);
        assert_eq!(select(&ts, None, Some("")), None);
    }

    #[test]
    fn id_takes_precedence_over_lang() {
        let ts = vec![t(1, "eng"), t(2, "spa")];
        assert_eq!(select(&ts, Some(1), Some("spa")), Some(0));
    }

    #[test]
    fn label_formats() {
        let mut x = t(1, "eng");
        assert_eq!(x.label(), "eng (aac)");
        x.lang.clear();
        x.title = "Director".into();
        assert_eq!(x.label(), "Director (aac)");
        x.title.clear();
        assert_eq!(x.label(), "aac");
    }
}
