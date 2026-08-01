//! source.rs — resolución de la ENTRADA antes de abrir nada.
//!
//! rtv acepta tres tipos de entrada:
//!   * ruta local (el comportamiento de siempre),
//!   * URL http/https DIRECTA a un medio (libavformat trae los
//!     protocolos de red integrados: http, https si el build lleva TLS,
//!     HLS .m3u8, DASH…),
//!   * página de un sitio de vídeo (YouTube, Twitch, Vimeo…): se delega
//!     en `yt-dlp` (si está instalado) para convertirla en la URL
//!     directa del stream, igual que hace mpv con su ytdl_hook.
//!
//! yt-dlp NO va embebido en el binario. Podría hacerse legalmente (su
//! licencia es Unlicense, dominio público), pero es un programa Python:
//! habría que incrustar el zipapp (~3 MB, exige python3 en el sistema)
//! o el binario PyInstaller (~30 MB por plataforma) y quedaría
//! CONGELADO: YouTube cambia cada pocas semanas y un yt-dlp desfasado
//! deja de extraer. Un yt-dlp del sistema se actualiza solo
//! (pip/pkg/winget). Se busca en $RTV_YTDLP y en el PATH.
//!
//! DOBLE INPUT (preparado, no activo por defecto): los formatos altos
//! de YouTube son DASH con vídeo y audio en streams SEPARADOS. rtv ya
//! usa un demuxer propio para vídeo (decoder.rs) y otro para audio
//! (audio.rs), así que reproducirlos solo requiere pasar la URL de
//! audio al pipeline de audio: eso es `MediaSource::audio`, que
//! player.rs ya enchufa. Con `--ytdl-format "bv*+ba/b"` se activa
//! (experimental); el default "b" pide un formato muxed (una URL).

use anyhow::{anyhow, bail, Context as _, Result};
use ffmpeg_the_third as ffmpeg;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Entrada ya resuelta, lista para abrir con `open()`.
pub struct MediaSource {
    /// Lo que abre libavformat para el VÍDEO (y el audio, si el formato
    /// es muxed): ruta local o URL directa del stream.
    pub video: PathBuf,
    /// Entrada SEPARADA solo-audio (streams DASH partidos que devuelve
    /// yt-dlp con formatos "bv*+ba"). El pipeline de audio la abre con
    /// su propio demuxer. None = el audio va dentro de `video`.
    pub audio: Option<PathBuf>,
    /// Título legible (el que extrae yt-dlp). Para `--info`.
    pub title: Option<String>,
}

/// ¿Es una URL http/https? (case-insensitive, como hace curl).
pub fn is_url(s: &str) -> bool {
    let l = s.get(..8).unwrap_or(s).to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://")
}

/// Host de una URL, en minúsculas y sin userinfo/puerto.
/// "https://user@WWW.YouTube.com:443/watch?v=x" → "www.youtube.com".
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let auth = rest.split(['/', '?', '#']).next()?;
    let no_user = auth.rsplit_once('@').map(|(_, h)| h).unwrap_or(auth);
    let host = no_user.split(':').next()?.to_ascii_lowercase();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// Sitios que se resuelven con yt-dlp automáticamente. La lista es corta
/// a propósito (los grandes); para cualquier otro sitio soportado por
/// yt-dlp está `--ytdl`, que fuerza la resolución.
fn is_ytdl_host(host: &str) -> bool {
    const SITES: &[&str] = &[
        "youtube.com",
        "youtu.be",
        "twitch.tv",
        "vimeo.com",
        "dailymotion.com",
        "dai.ly",
    ];
    SITES
        .iter()
        .any(|s| host == *s || host.ends_with(&format!(".{s}")))
}

/// Resuelve el argumento de entrada:
///   * ruta local → tal cual,
///   * URL de sitio de vídeo (o cualquier URL con `--ytdl`) → yt-dlp,
///   * cualquier otra URL → directa a libavformat.
pub fn resolve(arg: &str, force_ytdl: bool, ytdl_format: &str) -> Result<MediaSource> {
    if !is_url(arg) {
        return Ok(MediaSource {
            video: PathBuf::from(arg),
            audio: None,
            title: None,
        });
    }
    let site = host_of(arg).map(|h| is_ytdl_host(&h)).unwrap_or(false);
    if force_ytdl || site {
        return ytdl_resolve(arg, ytdl_format);
    }
    Ok(MediaSource {
        video: PathBuf::from(arg),
        audio: None,
        title: None,
    })
}

/// Abre una entrada con libavformat. Para URLs añade opciones de red
/// razonables (reconexión ante cortes, timeout de conexión). Drop-in
/// replacement de `ffmpeg::format::input` en todos los demuxers de rtv.
pub fn open(media: &Path) -> Result<ffmpeg::format::context::Input, ffmpeg::Error> {
    let s = media.to_string_lossy();
    if is_url(&s) {
        let mut opts = ffmpeg::Dictionary::new();
        // Reconexión automática si el servidor corta a mitad (CDNs).
        opts.set("reconnect", "1");
        opts.set("reconnect_streamed", "1");
        opts.set("reconnect_delay_max", "5");
        // Sin timeout, un servidor mudo congelaría el arranque. µs.
        opts.set("rw_timeout", "15000000");
        ffmpeg::format::input_with_dictionary(media, opts)
    } else {
        ffmpeg::format::input(media)
    }
}

// ------------------------------------------------------------- yt-dlp --

/// Localiza el ejecutable de yt-dlp: $RTV_YTDLP si está definido, si no
/// "yt-dlp" del PATH (en Windows resuelve yt-dlp.exe solo).
fn ytdlp_command() -> Command {
    match std::env::var_os("RTV_YTDLP") {
        Some(p) if !p.is_empty() => Command::new(p),
        _ => Command::new("yt-dlp"),
    }
}

/// Convierte una URL de página en URL(s) directas de stream con yt-dlp.
fn ytdl_resolve(url: &str, format: &str) -> Result<MediaSource> {
    eprintln!("[rtv] resolviendo con yt-dlp…");
    let out = ytdlp_command()
        .args([
            "--no-warnings",
            "--no-playlist",
            "--socket-timeout",
            "20",
            "-f",
            format,
            "--print",
            "title",
            "--print",
            "urls",
            "--",
            url,
        ])
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow!(
                    "yt-dlp no está instalado (necesario para URLs de sitios \
                     de vídeo). Instálalo con pip/pkg/winget o apunta \
                     $RTV_YTDLP al ejecutable."
                )
            } else {
                anyhow!("no se pudo ejecutar yt-dlp: {e}")
            }
        })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = err.lines().rev().take(4).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        bail!("yt-dlp falló ({}):\n{}", out.status, tail.join("\n"));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let (title, urls) =
        parse_ytdl_output(&stdout).context("salida de yt-dlp no reconocida")?;
    let mut it = urls.into_iter();
    let video = PathBuf::from(it.next().ok_or_else(|| anyhow!("yt-dlp no devolvió URLs"))?);
    let audio = it.next().map(PathBuf::from);
    if audio.is_some() {
        eprintln!(
            "[rtv] formato con vídeo y audio en streams separados \
             (doble input, experimental)"
        );
    }
    Ok(MediaSource {
        video,
        audio,
        title: Some(title),
    })
}

/// Parsea la salida de `--print title --print urls`: línea 1 = título,
/// resto = URLs (1 = muxed; 2 = vídeo + audio separados).
fn parse_ytdl_output(out: &str) -> Result<(String, Vec<String>)> {
    let mut lines = out.lines().map(str::trim).filter(|l| !l.is_empty());
    let title = lines
        .next()
        .ok_or_else(|| anyhow!("salida vacía"))?
        .to_string();
    let urls: Vec<String> = lines
        .filter(|l| is_url(l))
        .map(str::to_string)
        .collect();
    if urls.is_empty() {
        bail!("no hay URLs en la salida");
    }
    if urls.len() > 2 {
        bail!("yt-dlp devolvió {} URLs (¿playlist?); esperaba 1 o 2", urls.len());
    }
    Ok((title, urls))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_detection() {
        assert!(is_url("http://a.com/v.mp4"));
        assert!(is_url("HTTPS://a.com/v.mp4"));
        assert!(!is_url("video.mp4"));
        assert!(!is_url("/ruta/con/http://dentro"));
        assert!(!is_url("ftp://a.com/v.mp4")); // libavformat lo abriría,
                                               // pero no lo tratamos como red
    }

    #[test]
    fn hosts() {
        assert_eq!(
            host_of("https://user@WWW.YouTube.com:443/w?v=x").as_deref(),
            Some("www.youtube.com")
        );
        assert_eq!(host_of("http://a.com").as_deref(), Some("a.com"));
        assert_eq!(host_of("nota-url"), None);
    }

    #[test]
    fn ytdl_hosts() {
        assert!(is_ytdl_host("youtube.com"));
        assert!(is_ytdl_host("www.youtube.com"));
        assert!(is_ytdl_host("music.youtube.com"));
        assert!(is_ytdl_host("youtu.be"));
        assert!(!is_ytdl_host("notyoutube.com")); // sufijo con punto, no substring
        assert!(!is_ytdl_host("example.com"));
    }

    #[test]
    fn ytdl_output_muxed() {
        let (t, u) = parse_ytdl_output("Mi vídeo\nhttps://cdn/x.mp4\n").unwrap();
        assert_eq!(t, "Mi vídeo");
        assert_eq!(u, vec!["https://cdn/x.mp4"]);
    }

    #[test]
    fn ytdl_output_split() {
        let (t, u) =
            parse_ytdl_output("Título\nhttps://cdn/video\nhttps://cdn/audio\n").unwrap();
        assert_eq!(t, "Título");
        assert_eq!(u.len(), 2);
    }

    #[test]
    fn ytdl_output_bad() {
        assert!(parse_ytdl_output("").is_err());
        assert!(parse_ytdl_output("solo título sin urls\n").is_err());
    }

    #[test]
    fn resolve_local_and_direct() {
        let s = resolve("video.mp4", false, "b").unwrap();
        assert_eq!(s.video, PathBuf::from("video.mp4"));
        assert!(s.audio.is_none() && s.title.is_none());
        let s = resolve("https://cdn.example.com/v.mp4", false, "b").unwrap();
        assert_eq!(s.video, PathBuf::from("https://cdn.example.com/v.mp4"));
    }
}
