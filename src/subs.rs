//! Subtítulos softsub (SRT / ASS) — externos o embebidos en el
//! contenedor.
//!
//! Diseño (cero impacto en el rendimiento del vídeo):
//!
//!   * **Externos** (`--sub fichero.srt|.ass`): parser puro-Rust, se
//!     cargan enteros al arrancar (síncrono: son ficheros de KBs).
//!
//!   * **Embebidos** (stream de subtítulos del contenedor): un hilo
//!     de fondo abre SU PROPIO contexto de demux, marca todos los
//!     streams menos el de subtítulos con `AVDISCARD_ALL` (el demuxer
//!     de MP4/MKV usa el índice para saltarse los paquetes de
//!     audio/vídeo → solo lee los samples de subtítulos, no el
//!     fichero entero) y decodifica TODOS los eventos de una pasada
//!     con `avcodec_decode_subtitle2`. Los eventos van entrando en un
//!     `Vec` compartido; en segundos está completo incluso para
//!     películas largas.
//!
//!   * El player consulta `SubTrack::query(t)` una vez por refresco:
//!     búsqueda binaria por tiempo sobre el Vec ordenado — O(log n),
//!     nanosegundos. Sin canales, sin sincronización con el decoder
//!     de vídeo, sin tocar los relojes.
//!
//! El texto se normaliza a texto plano: fuera tags ASS `{\...}`,
//! `\N`→salto de línea, fuera tags HTML de SRT (`<i>`, `<b>`...).

use anyhow::{anyhow, Result};
use ffmpeg_the_third as ffmpeg;
use ffmpeg::media::Type;
use parking_lot::Mutex;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct SubEvent {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

/// Pista de subtítulos activa. `events` se mantiene ORDENADO por
/// `start` (el hilo de carga inserta en orden; los externos se
/// ordenan tras el parseo).
pub struct SubTrack {
    events: Arc<Mutex<Vec<SubEvent>>>,
    #[allow(dead_code)]
    loaded: Arc<AtomicBool>,
    /// Etiqueta para el HUD ("srt", "ass", "sub:eng"...).
    pub label: String,
}

impl SubTrack {
    /// Texto que debe verse en el instante `t` (segundos de media), o
    /// `None` si no hay evento activo. Si varios solapan se concatenan
    /// con salto de línea (raro, pero legal en ASS).
    pub fn query(&self, t: f64) -> Option<String> {
        let evs = self.events.lock();
        if evs.is_empty() {
            return None;
        }
        // Búsqueda binaria del primer evento con start > t; los
        // activos están (por solapes) en una ventana pequeña hacia
        // atrás — 32 eventos cubren cualquier ASS razonable.
        let hi = evs.partition_point(|e| e.start <= t);
        let lo = hi.saturating_sub(32);
        let mut out: Option<String> = None;
        for e in &evs[lo..hi] {
            if e.start <= t && t < e.end {
                match out.as_mut() {
                    Some(s) => {
                        s.push('\n');
                        s.push_str(&e.text);
                    }
                    None => out = Some(e.text.clone()),
                }
            }
        }
        out
    }
}

/// Carga una pista embebida CONCRETA por índice de stream del
/// contenedor (para el ciclado en runtime `j`/`J` y `--sid/--slang`).
pub fn load_embedded_track(media: &Path, stream_index: usize) -> Option<SubTrack> {
    load_embedded(media, Some(stream_index))
}

/// Carga un fichero externo de subtítulos (público para el ciclado
/// embebida↔externa del player).
pub fn load_external_file(path: &Path) -> Option<SubTrack> {
    load_external(path).ok()
}

// ------------------------- externos -------------------------

fn load_external(path: &Path) -> Result<SubTrack> {
    let raw = std::fs::read(path)?;
    // UTF-8 con tolerancia (los SRT viejos vienen en latin-1).
    let text = match String::from_utf8(raw) {
        Ok(s) => s,
        Err(e) => e.into_bytes().iter().map(|&b| b as char).collect(),
    };
    let text = text.trim_start_matches('\u{feff}');
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut events = if ext == "ass" || ext == "ssa" || text.contains("[Events]") {
        parse_ass(text)
    } else {
        parse_srt(text)
    };
    if events.is_empty() {
        return Err(anyhow!("sin eventos de subtítulo en {:?}", path));
    }
    events.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap());
    Ok(SubTrack {
        events: Arc::new(Mutex::new(events)),
        loaded: Arc::new(AtomicBool::new(true)),
        label: if ext == "ass" || ext == "ssa" { "ass" } else { "srt" }.into(),
    })
}

fn parse_srt(text: &str) -> Vec<SubEvent> {
    let mut out = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let l = line.trim();
        if !l.contains("-->") {
            continue;
        }
        let mut parts = l.splitn(2, "-->");
        let (Some(a), Some(b)) = (parts.next(), parts.next()) else { continue };
        let (Some(start), Some(end)) = (parse_srt_time(a.trim()), parse_srt_time(b.trim())) else {
            continue;
        };
        let mut body = String::new();
        for tl in lines.by_ref() {
            if tl.trim().is_empty() {
                break;
            }
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(tl.trim_end());
        }
        let clean = strip_html(&body);
        if !clean.trim().is_empty() {
            out.push(SubEvent { start, end, text: clean });
        }
    }
    out
}

/// "HH:MM:SS,mmm" (SRT) o con '.' como separador de milis.
fn parse_srt_time(s: &str) -> Option<f64> {
    let s = s.split_whitespace().next()?;
    let mut hms = s.splitn(3, ':');
    let h: f64 = hms.next()?.parse().ok()?;
    let m: f64 = hms.next()?.parse().ok()?;
    let rest = hms.next()?;
    let (sec, ms) = match rest.split_once([',', '.']) {
        Some((a, b)) => (a.parse::<f64>().ok()?, b.parse::<f64>().ok()? / 1000.0),
        None => (rest.parse::<f64>().ok()?, 0.0),
    };
    Some(h * 3600.0 + m * 60.0 + sec + ms)
}

fn parse_ass(text: &str) -> Vec<SubEvent> {
    let mut out = Vec::new();
    let mut in_events = false;
    let mut fmt: Vec<String> = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_events = l.eq_ignore_ascii_case("[Events]");
            continue;
        }
        if !in_events {
            continue;
        }
        if let Some(rest) = l.strip_prefix("Format:") {
            fmt = rest.split(',').map(|f| f.trim().to_ascii_lowercase()).collect();
            continue;
        }
        let Some(rest) = l.strip_prefix("Dialogue:") else { continue };
        // El campo Text es el ÚLTIMO y puede contener comas: split en
        // n-1 comas donde n = número de campos del Format.
        let nfields = if fmt.is_empty() { 10 } else { fmt.len() };
        let fields: Vec<&str> = rest.splitn(nfields, ',').collect();
        if fields.len() < nfields {
            continue;
        }
        let idx_of = |name: &str, def: usize| -> usize {
            fmt.iter().position(|f| f == name).unwrap_or(def)
        };
        let si = idx_of("start", 1);
        let ei = idx_of("end", 2);
        let ti = nfields - 1; // text SIEMPRE es el último
        let (Some(start), Some(end)) =
            (parse_ass_time(fields[si].trim()), parse_ass_time(fields[ei].trim()))
        else {
            continue;
        };
        let clean = strip_ass_tags(fields[ti]);
        if !clean.trim().is_empty() {
            out.push(SubEvent { start, end, text: clean });
        }
    }
    out
}

/// "H:MM:SS.cc" (centésimas).
fn parse_ass_time(s: &str) -> Option<f64> {
    let mut hms = s.splitn(3, ':');
    let h: f64 = hms.next()?.parse().ok()?;
    let m: f64 = hms.next()?.parse().ok()?;
    let sec: f64 = hms.next()?.parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + sec)
}

/// Quita tags de override ASS `{\...}`, dibujo vectorial y convierte
/// `\N`/`\n` en salto de línea, `\h` en espacio duro.
pub fn strip_ass_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    let mut drawing = false;
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                // Bloque de tags: consumir hasta '}'. Detectar modo
                // dibujo \p1..\p0 (el "texto" son coordenadas).
                let mut tag = String::new();
                for t in chars.by_ref() {
                    if t == '}' {
                        break;
                    }
                    tag.push(t);
                }
                if let Some(i) = tag.rfind("\\p") {
                    let level = tag[i + 2..]
                        .chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect::<String>();
                    if let Ok(n) = level.parse::<u32>() {
                        drawing = n > 0;
                    }
                }
            }
            '\\' => match chars.peek() {
                Some('N') | Some('n') => {
                    chars.next();
                    if !drawing {
                        out.push('\n');
                    }
                }
                Some('h') => {
                    chars.next();
                    if !drawing {
                        out.push(' ');
                    }
                }
                _ => {
                    if !drawing {
                        out.push('\\');
                    }
                }
            },
            _ => {
                if !drawing {
                    out.push(c);
                }
            }
        }
    }
    out.trim().to_string()
}

/// Quita tags HTML sencillos de SRT (<i>, </b>, <font ...>).
fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.trim().to_string()
}

// ------------------------- embebidos -------------------------

/// Línea "payload" de un evento ASS embebido de FFmpeg:
/// `ReadOrder,Layer,Style,Name,MarginL,MarginR,MarginV,Effect,Text`
/// (sin prefijo "Dialogue:"). Devuelve el Text plano.
fn ass_payload_text(s: &str) -> String {
    // Text es el campo 9 → separar en 8 comas.
    let fields: Vec<&str> = s.splitn(9, ',').collect();
    let raw = if fields.len() == 9 { fields[8] } else { s };
    strip_ass_tags(raw)
}

fn load_embedded(media: &Path, want_index: Option<usize>) -> Option<SubTrack> {
    // Sondeo rápido: ¿hay stream de subtítulos de TEXTO? (los bitmap
    // — dvdsub/pgs — no se pueden renderizar como texto; se ignoran).
    let ictx = ffmpeg::format::input(media).ok()?;
    let stream = match want_index {
        Some(i) => {
            let s = ictx.stream(i)?;
            if s.parameters().medium() != Type::Subtitle {
                return None;
            }
            s
        }
        None => ictx.streams().best(Type::Subtitle)?,
    };
    let sidx = stream.index();
    if !crate::tracks::is_text_sub_codec(stream.parameters().id()) {
        return None;
    }
    let lang = stream
        .metadata()
        .get("language")
        .map(|l| l.to_string())
        .unwrap_or_default();
    drop(ictx);

    let events = Arc::new(Mutex::new(Vec::<SubEvent>::new()));
    let loaded = Arc::new(AtomicBool::new(false));
    let media = media.to_owned();
    let ev_th = events.clone();
    let loaded_th = loaded.clone();

    // Hilo de fondo: demux SOLO del stream de subs + decode completo.
    let _ = std::thread::Builder::new()
        .name("rtv-subs".into())
        .spawn(move || {
            let _ = decode_embedded(&media, sidx, &ev_th);
            loaded_th.store(true, Ordering::Release);
        });

    Some(SubTrack {
        events,
        loaded,
        label: if lang.is_empty() {
            "sub".to_string()
        } else {
            format!("sub:{lang}")
        },
    })
}

fn decode_embedded(media: &Path, sidx: usize, out: &Mutex<Vec<SubEvent>>) -> Result<()> {
    let mut ictx = ffmpeg::format::input(media)?;

    // AVDISCARD_ALL en todos los streams menos el de subtítulos: el
    // demuxer usa el índice del contenedor y NO lee los paquetes de
    // audio/vídeo del disco.
    unsafe {
        let fmt = ictx.as_mut_ptr();
        for i in 0..(*fmt).nb_streams as isize {
            let st = *(*fmt).streams.offset(i);
            (*st).discard = if (*st).index as usize == sidx {
                ffmpeg::ffi::AVDiscard::DEFAULT
            } else {
                ffmpeg::ffi::AVDiscard::ALL
            };
        }
    }

    let stream = ictx
        .stream(sidx)
        .ok_or_else(|| anyhow!("stream de subs desapareció"))?;
    let tb = stream.time_base();
    let tb_f = f64::from(tb.numerator()) / f64::from(tb.denominator());
    let ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?;
    let mut dec = ctx.decoder().subtitle()?;

    for pkt in ictx.packets() {
        let Ok((s, pkt)) = pkt else { continue };
        if s.index() != sidx {
            continue;
        }
        let mut sub = ffmpeg::Subtitle::new();
        let Ok(got) = dec.decode(&pkt, &mut sub) else { continue };
        if !got {
            continue;
        }
        let pkt_pts = pkt.pts().or(pkt.dts()).unwrap_or(0) as f64 * tb_f;
        let pkt_dur = pkt.duration() as f64 * tb_f;
        // start/end_display_time van en ms relativos al pts.
        let start = pkt_pts + sub.start() as f64 / 1000.0;
        let end = if sub.end() > 0 && sub.end() != u32::MAX {
            pkt_pts + sub.end() as f64 / 1000.0
        } else if pkt_dur > 0.0 {
            pkt_pts + pkt_dur
        } else {
            start + 3.0 // sin duración conocida: 3 s por defecto
        };

        let mut text = String::new();
        for rect in sub.rects() {
            let piece = match rect {
                ffmpeg::codec::subtitle::Rect::Text(t) => strip_html(t.get()),
                ffmpeg::codec::subtitle::Rect::Ass(a) => ass_payload_text(a.get()),
                _ => String::new(),
            };
            if !piece.trim().is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(piece.trim());
            }
        }
        // Liberar los rects del AVSubtitle (el binding no tiene Drop).
        unsafe { ffmpeg::ffi::avsubtitle_free(sub.as_mut_ptr()) };

        if !text.is_empty() && end > start {
            let mut evs = out.lock();
            // El demux ya viene en orden de pts; inserción ordenada
            // defensiva por si el muxer intercala.
            let pos = evs.partition_point(|e| e.start <= start);
            evs.insert(pos, SubEvent { start, end, text });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srt_basic() {
        let s = "1\n00:00:01,000 --> 00:00:02,500\nHola <i>mundo</i>\nsegunda\n\n2\n00:01:00,000 --> 00:01:01,000\nAdiós\n";
        let evs = parse_srt(s);
        assert_eq!(evs.len(), 2);
        assert!((evs[0].start - 1.0).abs() < 1e-9);
        assert!((evs[0].end - 2.5).abs() < 1e-9);
        assert_eq!(evs[0].text, "Hola mundo\nsegunda");
        assert!((evs[1].start - 60.0).abs() < 1e-9);
    }

    #[test]
    fn ass_basic() {
        let s = "[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:05.00,0:00:07.50,Default,,0,0,0,,{\\i1}Cursiva{\\i0} y \\Nsalto, con coma\n";
        let evs = parse_ass(s);
        assert_eq!(evs.len(), 1);
        assert!((evs[0].start - 5.0).abs() < 1e-9);
        assert!((evs[0].end - 7.5).abs() < 1e-9);
        assert_eq!(evs[0].text, "Cursiva y \nsalto, con coma");
    }

    #[test]
    fn ass_drawing_stripped() {
        assert_eq!(strip_ass_tags("{\\p1}m 0 0 l 100 0{\\p0}visible"), "visible");
    }

    #[test]
    fn embedded_ass_payload() {
        assert_eq!(
            ass_payload_text("1,0,Default,,0,0,0,,Hola,con coma"),
            "Hola,con coma"
        );
    }

    #[test]
    fn query_overlap() {
        let track = SubTrack {
            events: Arc::new(Mutex::new(vec![
                SubEvent { start: 1.0, end: 5.0, text: "a".into() },
                SubEvent { start: 2.0, end: 3.0, text: "b".into() },
            ])),
            loaded: Arc::new(AtomicBool::new(true)),
            label: "srt".into(),
        };
        assert_eq!(track.query(0.5), None);
        assert_eq!(track.query(1.5).as_deref(), Some("a"));
        assert_eq!(track.query(2.5).as_deref(), Some("a\nb"));
        assert_eq!(track.query(4.0).as_deref(), Some("a"));
        assert_eq!(track.query(6.0), None);
    }
}
