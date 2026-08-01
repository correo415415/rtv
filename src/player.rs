//! Loop principal del reproductor — v0.5.
//!
//! Motor de sincronización rediseñado siguiendo ffplay.c:
//!
//!   * Dos `FfClock`: `audclk` (actualizado por el callback de cpal
//!     con el PTS de la muestra que SE OYE, con `playback_delay`
//!     compensado) y `vidclk` (actualizado en cada frame mostrado).
//!   * Reloj maestro = `audclk` si hay audio, `vidclk` si no.
//!   * `compute_target_delay(last_duration, video_pts, master_now)`:
//!     misma lógica que ffplay, con thresholds `MIN=40ms`, `MAX=100ms`,
//!     `FRAMEDUP=100ms`, `NOSYNC=10s`.
//!   * Seek "hr-seek" atómico: `master.set(target)` bumpea seriales
//!     de ambos relojes; el decoder de vídeo usa `AVSEEK_FLAG_BACKWARD`
//!     y descarta frames hasta llegar a `target_pts` (drop-until-target-PTS
//!     estilo `--hr-seek-framedrop=yes` de mpv). El audio hace lo mismo:
//!     drena su ring y arranca desde el nuevo PTS.
//!   * Nada de "advance por muestra" ni de acumular µs: cada
//!     `set_pts()` es una asignación directa `pts + wall_elapsed`.

use anyhow::Result;
use crossterm::{
    cursor,
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::{stdout, Stdout, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::audio::{self, AudioHandle};
use crate::clock::{
    compute_target_delay, vp_duration, Clock, FfClock, MasterClock, AV_SYNC_THRESHOLD_MAX,
};
use crate::decoder;
use crate::input::{self, Cmd};
use crate::renderer::{self, Renderer};
use crate::subs;
use crate::terminfo::{self, CellPx};
use crate::tracks::{self, TrackInfo};

/// Modo de subtítulos (semántica de la CLI):
///   * `Off`      — sin `--sub`: NO se muestran subtítulos.
///   * `Embedded` — `--sub` sin valor: pista de texto embebida del
///     contenedor (la "best" según FFmpeg), si existe.
///   * `File(p)`  — `--sub fichero`: fichero externo .srt/.ass.
#[derive(Debug, Clone)]
pub enum SubMode {
    Off,
    Embedded,
    File(PathBuf),
}

pub struct Config {
    pub path: PathBuf,
    /// Entrada SEPARADA solo-audio (doble input: streams DASH partidos
    /// de yt-dlp). El pipeline de audio —que ya usa su propio demuxer—
    /// abre esta URL en vez de `path`. None = audio dentro de `path`.
    pub audio_path: Option<PathBuf>,
    pub forced_backend: Option<String>,
    pub scale: f32,
    pub loop_video: bool,
    pub show_stats: bool,
    pub no_audio: bool,
    pub audio_backend: audio::BackendPref,
    pub hw_pref: crate::hwdec::HwPref,
    /// Modo de subtítulos (ver `SubMode`).
    pub sub_mode: SubMode,
    /// Pista de audio inicial: índice 1-based dentro de las pistas de
    /// audio (--aid) / idioma (--alang).
    pub aid: Option<usize>,
    pub alang: Option<String>,
    /// Pista de subtítulos embebida inicial (--sid / --slang).
    pub sid: Option<usize>,
    pub slang: Option<String>,
}

/// Una opción del ciclo de subtítulos (tecla `j`/`J`):
/// Off → [externa si hay] → embebida 1 → embebida 2 → … → Off.
enum SubChoice {
    Off,
    External(PathBuf),
    /// stream_index REAL en el contenedor.
    Embedded(usize),
}

/// Carga la opción de subtítulos elegida. Devuelve (pista, etiqueta
/// para el OSD).
fn load_sub_choice(
    media: &std::path::Path,
    choice: &SubChoice,
    sub_tracks: &[TrackInfo],
) -> (Option<subs::SubTrack>, String) {
    match choice {
        SubChoice::Off => (None, "off".to_string()),
        SubChoice::External(p) => {
            let t = subs::load_external_file(p);
            let label = match &t {
                Some(t) => format!("{} (externo)", t.label),
                None => "error cargando fichero".to_string(),
            };
            (t, label)
        }
        SubChoice::Embedded(sidx) => {
            let t = subs::load_embedded_track(media, *sidx);
            let label = sub_tracks
                .iter()
                .find(|ti| ti.stream_index == *sidx)
                .map(|ti| ti.label())
                .unwrap_or_else(|| "embebida".to_string());
            match &t {
                Some(_) => (t, label),
                None => (None, format!("{label} — error")),
            }
        }
    }
}

struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    fn enter(so: &mut Stdout) -> Result<Self> {
        terminal::enable_raw_mode()?;
        // Captura de ratón: para la barra de progreso clicable. Si el
        // terminal no la soporta, crossterm emite las secuencias
        // igualmente y el terminal las ignora — sin daño.
        execute!(so, EnterAlternateScreen, cursor::Hide, EnableMouseCapture)?;
        // AUTOWRAP OFF (DECAWM): escribir en la última columna de la
        // última fila con wrap activo hace scroll de TODA la pantalla
        // → el vídeo sube una línea, el siguiente frame lo repinta…
        // parpadeo masivo y "texto basura" en terminales pequeñas.
        // Con wrap off, cualquier exceso se recorta en el borde.
        let _ = write!(so, "\x1b[?7l");
        let _ = so.flush();
        Ok(Self { active: true })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut so = stdout();
        // Restaurar autowrap y salir del alt-screen.
        let _ = write!(so, "\x1b[?7h");
        let _ = execute!(so, DisableMouseCapture, cursor::Show, LeaveAlternateScreen);
        let _ = write!(so, "\x1b[0m");
        let _ = so.flush();
        let _ = terminal::disable_raw_mode();
    }
}

/// Caché del último HUD escrito: (cols, rows, hud_lines, l1, l2).
/// Si el contenido no cambia, NO se reescribe la fila → el HUD pasa
/// de reescribirse 25-60 veces/s a ~1 vez/s — adiós parpadeo.
type HudCache = Option<(u16, u16, u16, String, String)>;

/// Ancho de la barra de progreso del HUD según columnas. Única fuente
/// de verdad: la usan `format_hud_lines` (dibujo) y `bar_hitbox`
/// (clicks del ratón) — si divergen, el click aterriza en el sitio
/// equivocado.
fn hud_bar_w(cols: u16) -> usize {
    if cols >= 120 {
        40
    } else if cols >= 80 {
        24
    } else if cols >= 60 {
        16
    } else {
        8
    }
}

/// Hitbox de la barra de progreso en pantalla: `(fila, col_inicio,
/// ancho)` en coordenadas 1-based, o None si el HUD actual no pinta
/// barra (HUD oculto, o línea corta de <60 cols que omite la barra).
///
/// La línea del HUD con barra siempre empieza " ▶ [" / " ⏸ [": espacio
/// (1) + flag (1) + espacio (1) + '[' (1) → la barra ocupa las
/// columnas 5..5+bar_w-1. Con HUD de 2 líneas la barra va en la
/// PENÚLTIMA fila; con 1 línea, en la última.
fn bar_hitbox(cols: u16, rows: u16, hud_lines: u16) -> Option<(u16, u16, u16)> {
    let bar_w = hud_bar_w(cols) as u16;
    match hud_lines {
        2 => Some((rows.saturating_sub(1).max(1), 5, bar_w)),
        1 if cols >= 60 => Some((rows, 5, bar_w)),
        _ => None,
    }
}

fn hud_rows_for(cols: u16, rows: u16) -> u16 {
    // Terminal minúscula: NO hay sitio para un HUD legible — pintarlo
    // truncado a 4-15 columnas solo produce ruido que parpadea encima
    // del vídeo. Mejor ocultarlo y dedicar todas las filas al vídeo.
    if rows < 5 || cols < 16 {
        0
    } else if rows >= 24 && cols >= 100 {
        2
    } else {
        1
    }
}

pub fn run(cfg: Config) -> Result<()> {
    let backend = renderer::detect_backend(cfg.forced_backend.as_deref());
    let (mut cols, mut rows) = terminal::size().unwrap_or((80, 24));

    let mut so = stdout();
    let _guard = TerminalGuard::enter(&mut so)?;

    let cell_px = terminfo::probe_cell_px(cols, rows);

    // --- Relojes independientes audio/vídeo ---
    let audclk_pre = FfClock::new(); // se pasa al audio thread
    // Staleness del reloj de audio: los callbacks de cpal llegan cada
    // 25-100 ms; si pasan >250 ms sin `set_pts` es que el dispositivo
    // NO está consumiendo (stall de arranque de PulseAudio, underrun,
    // EOF del stream de audio). El reloj se congela y `anchored()`
    // pasa a false → el vídeo (esclavo) espera en vez de correr en
    // silencio y luego saltar hacia atrás.
    audclk_pre.set_staleness(0.25);
    let vidclk = FfClock::new();

    // --- Inventario de pistas del contenedor (audio + subs texto) ---
    // Con doble input las pistas de AUDIO viven en el fichero/URL de
    // audio y las de subtítulos en el de vídeo; con input único todo
    // sale del mismo sondeo.
    let audio_media: &PathBuf = cfg.audio_path.as_ref().unwrap_or(&cfg.path);
    let (audio_tracks, sub_tracks) = match &cfg.audio_path {
        None => tracks::probe(&cfg.path),
        Some(ap) => {
            let (at, _) = tracks::probe(ap);
            let (_, st) = tracks::probe(&cfg.path);
            (at, st)
        }
    };

    // --- Audio (opcional) ---
    // Pista inicial: --aid (1-based) / --alang, con fallback a la
    // "best" de FFmpeg si no hay match.
    let start_audio_stream = tracks::select(&audio_tracks, cfg.aid, cfg.alang.as_deref())
        .map(|pos| audio_tracks[pos].stream_index);
    let mut audio_handle: Option<AudioHandle> = if cfg.no_audio
        || cfg.audio_backend == audio::BackendPref::NoAudio
    {
        None
    } else {
        match audio::spawn(
            audio_media,
            audclk_pre.clone(),
            start_audio_stream,
            cfg.audio_backend,
        ) {
            Ok(h) if h.has_audio => {
                // Visible solo con --verbose (stderr va a /dev/null si no).
                eprintln!("[rtv-audio] backend de salida: {}", h.backend_name);
                Some(h)
            }
            _ => None,
        }
    };
    // Posición de la pista de audio activa dentro de `audio_tracks`
    // (para el ciclado con `a`/`#`).
    let mut cur_audio_pos: usize = audio_handle
        .as_ref()
        .and_then(|a| a.track_index)
        .and_then(|si| audio_tracks.iter().position(|t| t.stream_index == si))
        .unwrap_or(0);
    let using_audio = audio_handle.as_ref().map(|a| a.has_audio).unwrap_or(false);

    // MasterClock: elige audclk o vidclk como maestro.
    let master: Arc<MasterClock> = if using_audio {
        MasterClock::with_audio(audclk_pre.clone(), vidclk.clone())
    } else {
        MasterClock::video_only(vidclk.clone())
    };

    // Volumen inicial.
    let mut volume: i32 = 100;
    if let Some(a) = audio_handle.as_ref() {
        a.set_volume(volume);
    }

    // Subtítulos softsub (externos --sub o embebidos). La carga de
    // los embebidos corre en un hilo propio (demux solo-subs).
    //
    // CICLO de pistas (tecla `j`/`J`): Off → [externa] → embebidas.
    // El estado es (sub_choices, sub_choice_idx); al ciclar se
    // recarga la pista elegida (los eventos cargan en ms–s en un
    // hilo propio, sin tocar vídeo/audio/relojes).
    let mut sub_choices: Vec<SubChoice> = vec![SubChoice::Off];
    if let SubMode::File(p) = &cfg.sub_mode {
        sub_choices.push(SubChoice::External(p.clone()));
    }
    for t in &sub_tracks {
        sub_choices.push(SubChoice::Embedded(t.stream_index));
    }
    let mut sub_choice_idx: usize = match &cfg.sub_mode {
        SubMode::Off => 0,
        SubMode::File(_) => 1,
        SubMode::Embedded => {
            if sub_tracks.is_empty() {
                0
            } else {
                // --sid/--slang eligen pista concreta; sin ellos, la
                // primera pista de texto del contenedor.
                let pos = tracks::select(&sub_tracks, cfg.sid, cfg.slang.as_deref()).unwrap_or(0);
                1 + pos // +1 por el Off inicial (sin externa en este modo)
            }
        }
    };
    let mut sub_track: Option<subs::SubTrack> = if sub_choice_idx == 0 {
        None
    } else {
        load_sub_choice(&cfg.path, &sub_choices[sub_choice_idx], &sub_tracks).0
    };
    let sub_rows_for = |rows: u16, has: bool| -> u16 {
        if has && rows >= 8 {
            2
        } else {
            0
        }
    };
    let mut sub_rows = sub_rows_for(rows, sub_track.is_some());
    // Caché del último texto de subtítulo pintado (anti-parpadeo,
    // misma filosofía que HudCache): (cols, rows, fila_inicial, texto).
    let mut sub_cache: Option<(u16, u16, u16, String)> = None;

    // OSD transitorio del HUD (feedback al ciclar pistas): texto +
    // instante de creación; se muestra ~2.5 s en la línea 1 del HUD
    // y caduca solo (el HudCache detecta el cambio de texto).
    let mut osd: Option<(String, Instant)> = None;

    // Decoder vídeo.
    let mut hud_lines = hud_rows_for(cols, rows);
    let (dst_w0, dst_h0) = terminfo::adaptive_target_pixels(
        backend,
        cols,
        rows,
        cell_px,
        cfg.scale,
        hud_lines + sub_rows,
    );
    let dec = decoder::spawn(&cfg.path, dst_w0, dst_h0, cfg.hw_pref)?;

    // Etiqueta del HUD: "kitty" (sw) o "kitty+vaapi" (decode HW).
    // Se recalcula en cada frame porque el fallback mid-stream puede
    // cambiar hw→sw en caliente (DecoderHandle::hw_state atómico).
    let hud_backend_label = |dec: &decoder::DecoderHandle, base: &str| -> String {
        match dec.hw_name() {
            Some(hw) => format!("{base}+{hw}"),
            None => base.to_string(),
        }
    };

    let (mut dst_w, mut dst_h, _, _) = compute_layout(
        backend,
        dec.source_size,
        cols,
        rows,
        cell_px,
        cfg.scale,
        hud_lines + sub_rows,
    );
    dec.resize(dst_w, dst_h);

    // Frame rate REAL del vídeo (avg_frame_rate del stream) — para la
    // duración "natural" en `compute_target_delay` cuando dos frames
    // consecutivos tengan PTS raros o iguales. Antes era 1/30 fijo, que
    // desincronizaba vídeos a 24/25/50/60 fps en cuanto había un PTS
    // inválido.
    let fallback_frame_dur: f64 = if dec.fps > 1.0 { 1.0 / dec.fps } else { 1.0 / 30.0 };
    let max_frame_dur: f64 = 10.0;

    let mut renderer_ = Renderer::new(backend);
    renderer_.set_cell_px(cell_px.w, cell_px.h);

    // Último frame mostrado — cacheado para redibujo INSTANTÁNEO al
    // redimensionar la terminal (sin esperar al siguiente frame del
    // decoder, que puede tardar si estamos en pausa o en hold). Es un
    // move (no clone): coste cero por frame.
    let mut last_frame: Option<decoder::RgbFrame> = None;

    // Frame PENDIENTE de mostrar (arquitectura ffplay): lo sacamos del
    // canal en cuanto está disponible, pero si aún no toca mostrarlo
    // la espera se hace con `input::wait_event` — interrumpible por
    // teclas y resizes — y volvemos al top del loop. Así un resize se
    // atiende en <1 ms, en vez de esperar a que venza un
    // `thread::sleep` de hasta 500 ms (el resize "no instantáneo").
    let mut pending: Option<decoder::RgbFrame> = None;

    // Caché del último HUD escrito — si el texto no cambia no se
    // reescribe la fila (el 90% de los refrescos), eliminando el
    // parpadeo del HUD en terminales lentas/pequeñas.
    let mut hud_cache: HudCache = None;

    // Estadísticas.
    let mut frames_shown_win: u64 = 0;
    let mut frames_dec_win: u64 = 0;
    let mut frames_dropped_win: u64 = 0;
    let mut last_dec_pts_ms: i64 = -1;
    let mut stats_epoch = Instant::now();
    let mut fps_shown_now: f64 = 0.0;
    let mut fps_dec_now: f64 = 0.0;
    let mut dropped_last_win: u64 = 0;

    let mut force_full_redraw = true;

    // Estado del bucle de refresco (ffplay-style):
    //   * last_shown_pts: PTS del frame renderizado más recientemente.
    //   * frame_timer: momento mural en que "programamos" el siguiente
    //     frame. Se calcula frame a frame: `frame_timer += delay`.
    let mut last_shown_pts: f64 = 0.0;
    let mut frame_timer: f64 = wall_now_f64();
    // Tras un seek estando en pausa, queremos decodificar y MOSTRAR el
    // frame del target (una sola vez) sin salir de la pausa.
    let mut show_one_frame_paused = false;
    // VÍDEO ESCLAVO DEL AUDIO también en arranque/post-seek: mientras
    // el reloj maestro (audio) esté DESANCLADO (congelado esperando el
    // primer chunk del serial nuevo), mostramos UN frame (el del
    // target) y NOS QUEDAMOS QUIETOS. Sin esto, el vídeo avanzaba en
    // free-run contra un reloj congelado (~0.5×–2×) y al anclarse el
    // audio había que dropear/duplicar en ráfaga para resincronizar.
    // Guardamos el serial del vidclk para el que ya mostramos el frame
    // de espera.
    let mut held_frame_serial: Option<i32> = None;
    // Instante en que empezó el hold — válvula de seguridad: si el
    // audio no ancla en un tiempo razonable (p.ej. seek más allá del
    // final del stream de audio), forzamos el anclaje para que el
    // vídeo no se quede congelado para siempre.
    let mut hold_started: Option<Instant> = None;
    // SEEK ESTILO MPV (keyframe landing): al hacer seek NO tocamos el
    // audio todavía. El decoder de vídeo aterriza en el keyframe
    // <= target y emite ESE frame ya (salto instantáneo). Cuando el
    // primer frame post-seek llega, re-apuntamos ambos relojes a su
    // PTS real (retarget, sin bumpear seriales) y ENTONCES pedimos al
    // audio que salte exactamente a ese PTS. Así imagen y sonido
    // arrancan clavados en el mismo instante del media, sin tener que
    // decodificar en silencio todo el GOP hasta el target (que con
    // AV1 4K tardaba varios segundos y desincronizaba todo).
    let mut pending_audio_landing = false;
    // REFINADO DE CALIDAD tras AGRANDAR la terminal: la cola de
    // pre-decode guarda hasta ~2.5 s de frames escalados a las dims
    // VIEJAS (pequeñas). Al encoger no importa (reducir un frame
    // grande se ve bien), pero al agrandar esos frames se upscalean
    // con nearest → borrosos hasta que la cola se vacía. El fix: con
    // debounce de 300 ms tras el último grow, pedir al decoder un
    // `refine_at(now)` — re-seek al punto actual que drena la cola y
    // re-decodifica DESDE AQUÍ con las dims nuevas (drop-until-target
    // exacto, sin salto visual). Los relojes y el audio NO se tocan:
    // el sonido sigue y los frames nítidos entran donde toca.
    let mut refine_deadline: Option<Instant> = None;
    // Serial del refinado EN CURSO: mientras el decoder re-decodifica
    // el GOP para alcanzar al reloj maestro, sus frames llegan "tarde"
    // y el drop estándar de ffplay los tiraría todos — pantalla
    // congelada y borrosa hasta el final del catch-up. En vez de eso
    // los MOSTRAMOS (estilo mpv con hr-seek lento): la imagen se ve
    // nítida en cuanto se decodifica el primer frame refinado y
    // "alcanza" visiblemente al audio. Se limpia al primer frame que
    // llega a tiempo (catch-up completado).
    let mut refine_catchup: Option<i32> = None;

    // Log de sincronía opcional (para tests de integración):
    // RTV_SYNC_LOG=/ruta/fichero → una línea por frame mostrado:
    //   wall_s master_s video_pts_s avdiff_ms dropped_win dec_w dec_h
    // (dec_w/dec_h = dims NATIVAS del decoder, no las del reescalado
    // player-side; sirven para medir la recuperación de calidad tras
    // agrandar la terminal.)
    let mut sync_log: Option<std::io::BufWriter<std::fs::File>> =
        std::env::var("RTV_SYNC_LOG").ok().and_then(|p| {
            std::fs::File::create(p).ok().map(std::io::BufWriter::new)
        });

    // NOTA: NO llamamos a `master.set(0.0)` aquí. Los relojes nacen en
    // pts=0, serial=0 y DESANCLADOS (now() == 0 congelado), igual que
    // los productores (audio serial 0, decoder vídeo serial 0). El
    // primer chunk de audio / frame de vídeo ancla el reloj y arranca
    // el tiempo. Si hiciéramos set(0.0) bumpearíamos los seriales a 1
    // dejando a los productores (serial 0) invalidados para siempre.

    'main: loop {
        // 1) Input.
        let cmds = input::poll_command().unwrap_or_default();
        for cmd in cmds {
            match cmd {
                Cmd::Quit => break 'main,
                Cmd::TogglePause => {
                    if master.is_paused() {
                        master.resume();
                        if let Some(a) = audio_handle.as_ref() {
                            a.play_stream();
                        }
                        frame_timer = wall_now_f64();
                    } else {
                        master.pause();
                        if let Some(a) = audio_handle.as_ref() {
                            a.pause_stream();
                        }
                    }
                }
                Cmd::SeekRel(..) | Cmd::MouseClick(..) => {
                    let now = master.now();
                    // Clamp: dejamos 0.5 s de margen antes del final para
                    // no aterrizar en EOF exacto (pantalla congelada).
                    let max_t = (dec.duration - 0.5).max(0.0);
                    let target = match cmd {
                        Cmd::SeekRel(delta) => (now + delta).max(0.0).min(max_t),
                        Cmd::MouseClick(mc, mr) => {
                            // Solo actúa si el click cae en la BARRA de
                            // progreso del HUD (con 1 celda de gracia a
                            // cada lado: los corchetes '[' ']'). El resto
                            // de la pantalla ignora el ratón.
                            let Some((brow, bcol, bw)) = bar_hitbox(cols, rows, hud_lines)
                            else {
                                continue;
                            };
                            if mr != brow || mc + 1 < bcol || mc > bcol + bw {
                                continue;
                            }
                            if !(dec.duration.is_finite() && dec.duration > 0.0) {
                                continue;
                            }
                            // Posición proporcional dentro de la barra:
                            // celda i de [0, bw) → fracción i/(bw-1)
                            // (la última celda aterriza en el final).
                            let i = mc.saturating_sub(bcol).min(bw.saturating_sub(1));
                            let frac = if bw > 1 {
                                f64::from(i) / f64::from(bw - 1)
                            } else {
                                0.0
                            };
                            (frac * dec.duration).max(0.0).min(max_t)
                        }
                        _ => unreachable!(),
                    };
                    if let Some(log) = sync_log.as_mut() {
                        let _ = writeln!(
                            log,
                            "# SEEK wall={:.4} target={:.3} now={:.3} anchored={}",
                            wall_now_f64(),
                            target,
                            now,
                            master.master_anchored(),
                        );
                        let _ = log.flush();
                    }
                    // ORDEN ATÓMICO:
                    //   (1) master.set(target) → bumpea serial en audclk
                    //       Y vidclk; cualquier chunk/frame en vuelo con
                    //       serial viejo será descartado por callback/player.
                    //   (2) audio.seek(target) → decoder audio salta y
                    //       recorta samples hasta el target exacto.
                    //   (3) dec.seek(target)   → decoder vídeo salta con
                    //       keyframe<=target + drop-until-target-PTS.
                    master.set(target);
                    // Dirección del seek: hacia DELANTE aterriza en el
                    // keyframe >= target (garantiza avance aunque el
                    // GOP sea más largo que el paso del seek — AV1 de
                    // YouTube tiene GOPs de >6 s y con keyframe<=target
                    // el vídeo se quedaba clavado); hacia ATRÁS en el
                    // keyframe <= target, como siempre. Con el ratón
                    // la dirección es relativa a la posición actual.
                    dec.seek_dir(target, target > now);
                    // Descartar el frame en vuelo: su serial ya es viejo.
                    pending = None;
                    // Un seek real drena la cola y re-decodifica con
                    // las dims vigentes: el refinado ya no hace falta.
                    refine_deadline = None;
                    refine_catchup = None;
                    // El audio saltará al PTS de ATERRIZAJE del vídeo
                    // (keyframe <= target) cuando llegue el primer
                    // frame post-seek — ver `pending_audio_landing`.
                    pending_audio_landing = using_audio;
                    // Reseteamos frame_timer para que el próximo frame
                    // se muestre YA (sin arrastre del delay anterior).
                    frame_timer = wall_now_f64();
                    last_shown_pts = target;
                    force_full_redraw = true;
                    if master.is_paused() {
                        // En pausa: mostrar el frame del target una vez.
                        show_one_frame_paused = true;
                    }
                }
                Cmd::VolumeDelta(d) => {
                    volume = (volume + d).clamp(0, 200);
                    if let Some(a) = audio_handle.as_ref() {
                        a.set_volume(volume);
                    }
                }
                Cmd::CycleAudio(dir) => {
                    // Cambio de pista de audio EN CALIENTE, sin cortar
                    // el playback. Mismo protocolo que un seek al
                    // instante actual:
                    //   (1) master.set(now) — bumpea seriales: los
                    //       chunks de la pista vieja que queden en el
                    //       ring se silencian y no tocan el reloj.
                    //   (2) audio.switch_track(stream, now) — el hilo
                    //       reabre el decoder sobre el stream nuevo
                    //       (codec/rate/layout propios), recrea el
                    //       resampler y aterriza en `now` con recorte
                    //       sample-accurate.
                    // El vídeo NO se toca: entra en el hold estándar
                    // (master desanclado → muestra el frame actual y
                    // espera) y al llegar el primer chunk de la pista
                    // nueva el reloj ancla y todo continúa en sync.
                    //
                    // OJO: el OSD informa según las pistas del
                    // CONTENEDOR, no según exista dispositivo de
                    // salida. Sin dispositivo (headless/CI, --no-audio
                    // implícito por fallo de cpal) `audio_handle` es
                    // None, pero el usuario sigue mereciendo el
                    // feedback "Audio [2/2]: spa" / "única pista" al
                    // ciclar — antes cualquier `a` en headless decía
                    // "sin audio" aunque el fichero tuviera 2 pistas.
                    if cfg.no_audio || audio_tracks.is_empty() {
                        osd = Some(("Audio: sin audio".to_string(), Instant::now()));
                    } else if audio_tracks.len() < 2 {
                        let label = audio_tracks
                            .first()
                            .map(|t| t.label())
                            .unwrap_or_else(|| "única".to_string());
                        osd = Some((format!("Audio: {label} (única pista)"), Instant::now()));
                    } else {
                        let n = audio_tracks.len();
                        cur_audio_pos =
                            (cur_audio_pos as i64 + dir as i64).rem_euclid(n as i64) as usize;
                        let track = &audio_tracks[cur_audio_pos];
                        // El switch en caliente solo aplica si hay un
                        // pipeline de audio vivo; sin dispositivo la
                        // selección queda registrada (cur_audio_pos)
                        // y el OSD confirma, sin tocar relojes.
                        if let Some(a) = audio_handle.as_ref() {
                            let now_t = master.now().max(0.0);
                            master.set(now_t);
                            a.switch_track(track.stream_index, now_t);
                            // NO es un seek de vídeo: el decoder sigue y el
                            // aterrizaje del audio ancla el reloj en now_t.
                            pending_audio_landing = false;
                            frame_timer = wall_now_f64();
                            if master.is_paused() {
                                // En pausa el reloj queda re-apuntado; al
                                // reanudar sonará la pista nueva desde aquí.
                                show_one_frame_paused = false;
                            }
                        }
                        osd = Some((
                            format!("Audio [{}/{}]: {}", cur_audio_pos + 1, n, track.label()),
                            Instant::now(),
                        ));
                    }
                }
                Cmd::CycleSubs(dir) => {
                    // Ciclo: Off → [externa --sub] → embebidas → Off.
                    let n = sub_choices.len();
                    if n <= 1 {
                        osd = Some(("Subs: no hay pistas".to_string(), Instant::now()));
                    } else {
                        sub_choice_idx =
                            (sub_choice_idx as i64 + dir as i64).rem_euclid(n as i64) as usize;
                        let (t, label) =
                            load_sub_choice(&cfg.path, &sub_choices[sub_choice_idx], &sub_tracks);
                        sub_track = t;
                        sub_cache = None;
                        osd = Some((
                            format!("Subs [{}/{}]: {}", sub_choice_idx + 1, n, label),
                            Instant::now(),
                        ));
                        // ¿Cambia el layout? (aparecen/desaparecen las
                        // 2 filas reservadas) → recomputar dims y
                        // redibujar YA el último frame (como un resize).
                        let new_sub_rows = sub_rows_for(rows, sub_track.is_some());
                        if new_sub_rows != sub_rows {
                            sub_rows = new_sub_rows;
                            let (nw, nh, _, _) = compute_layout(
                                backend,
                                dec.source_size,
                                cols,
                                rows,
                                cell_px,
                                cfg.scale,
                                hud_lines + sub_rows,
                            );
                            dst_w = nw;
                            dst_h = nh;
                            dec.resize(dst_w, dst_h);
                            hud_cache = None;
                            renderer_.reset_layout_cache();
                            force_full_redraw = true;
                            if let Some(f) = last_frame.as_mut() {
                                rescale_frame_nearest(f, dst_w, dst_h);
                                let vid_rows =
                                    rows.saturating_sub(hud_lines + sub_rows).max(1);
                                let (ox, oy) = offsets_for_frame(
                                    backend, cell_px, f.width, f.height, cols, vid_rows,
                                );
                                let mut sol = so.lock();
                                // Sin 2J manual: reset_layout_cache ya
                                // fuerza el clear DENTRO del batch
                                // sincronizado (?2026) del renderer;
                                // el clear manual fuera del batch era
                                // el parpadeo visible al pulsar `j`.
                                let _ = renderer_.draw(&mut sol, f, cols, vid_rows, ox, oy);
                                let _ = sol.flush();
                                force_full_redraw = false;
                            }
                        }
                    }
                }
                Cmd::Resize(c, r) => {
                    // RESIZE robusto e INSTANTÁNEO: NO tocamos relojes,
                    // ni sync, ni la cola de frames. Solo (1) recalcular
                    // layout, (2) store atómico de las dims nuevas al
                    // decoder y (3) REESCALAR el último frame cacheado a
                    // las dims nuevas (nearest, player-side) y pintarlo
                    // YA — también cuando la terminal CRECE (antes solo
                    // se recortaba al encoger; al agrandar se quedaba
                    // pequeño hasta que el decoder alcanzaba las dims
                    // nuevas, hasta ~2.5 s con la cola llena).
                    cols = c.max(4);
                    rows = r.max(3);
                    hud_lines = hud_rows_for(cols, rows);
                    sub_rows = sub_rows_for(rows, sub_track.is_some());
                    let (nw, nh, _, _) = compute_layout(
                        backend,
                        dec.source_size,
                        cols,
                        rows,
                        cell_px,
                        cfg.scale,
                        hud_lines + sub_rows,
                    );
                    // ¿Creció el área de vídeo? → programar refinado
                    // (debounce 300 ms: en un arrastre solo se refina
                    // al soltar). Al encoger se cancela: reducir los
                    // frames grandes de la cola ya se ve perfecto.
                    let grew =
                        (u64::from(nw) * u64::from(nh)) > (u64::from(dst_w) * u64::from(dst_h));
                    refine_deadline = if grew {
                        Some(Instant::now() + Duration::from_millis(300))
                    } else {
                        None
                    };
                    dst_w = nw;
                    dst_h = nh;
                    dec.resize(dst_w, dst_h);
                    hud_cache = None; sub_cache = None;
                    renderer_.reset_layout_cache();
                    force_full_redraw = true;
                    if let Some(f) = last_frame.as_mut() {
                        rescale_frame_nearest(f, dst_w, dst_h);
                        let vid_rows = rows.saturating_sub(hud_lines + sub_rows).max(1);
                        let (ox, oy) =
                            offsets_for_frame(backend, cell_px, f.width, f.height, cols, vid_rows);
                        let mut sol = so.lock();
                        // El renderer emite UN solo 2J (layout cambió);
                        // el doble clear manual anterior duplicaba el
                        // "flash" por evento de resize.
                        let _ = renderer_.draw(&mut sol, f, cols, vid_rows, ox, oy);
                        let _ = sol.flush();
                        force_full_redraw = false;
                    }
                }
                Cmd::None => {}
            }
        }

        // 1.2) Caducidad del OSD de cambio de pista (~2.5 s): al
        //      volver osd a None el texto del HUD cambia y el
        //      HudCache fuerza el repintado — sin timers extra.
        if osd
            .as_ref()
            .map(|(_, t0)| t0.elapsed() > Duration::from_millis(2500))
            .unwrap_or(false)
        {
            osd = None;
        }

        // 1.5) Disparo del REFINADO de calidad (debounce vencido).
        //      Reproduciendo: re-decode desde un poco por delante del
        //      reloj maestro, para que el primer frame refinado no
        //      llegue ya tarde. En pausa: re-decode del frame en
        //      pantalla y mostrarlo vía show_one_frame_paused (sin
        //      tocar audio ni relojes — pending_audio_landing queda
        //      false, así que el aterrizaje NO re-apunta nada).
        if refine_deadline.map(|t| Instant::now() >= t).unwrap_or(false) {
            refine_deadline = None;
            let max_t = (dec.duration - 0.5).max(0.0);
            if master.is_paused() {
                dec.refine_at(last_shown_pts.min(max_t));
                show_one_frame_paused = true;
                refine_catchup = None;
            } else if using_audio && !master.master_anchored() {
                // En pleno hold post-seek: reintentar en 200 ms (el
                // seek en curso ya decodifica con las dims nuevas,
                // así que normalmente ni hará falta).
                refine_deadline = Some(Instant::now() + Duration::from_millis(200));
            } else {
                // Lead PEQUEÑO (50 ms): el primer frame refinado con
                // pts >= target se muestra en cuanto el maestro lo
                // alcanza. Un lead grande impone ESA congelación de
                // más aunque el decode sea instantáneo; uno pequeño no
                // penaliza el caso lento (los frames tardíos se
                // dropean igual y el punto de re-sincronía es el
                // mismo: cuando el decode alcanza al reloj maestro).
                let target = (master.now() + 0.05).max(0.0).min(max_t);
                refine_catchup = Some(dec.refine_at(target));
                pending = None; // serial obsoleto
            }
        }

        // 2) Pausa: dormimos un pelín y actualizamos HUD. Si hay un
        //    seek pendiente de visualizar, sacamos UN frame del decoder
        //    (el del target) y lo pintamos sin salir de la pausa.
        if master.is_paused() {
            if show_one_frame_paused {
                if let Ok(mut frame) = dec.rx.recv_timeout(Duration::from_millis(200)) {
                    if frame.serial == dec.current_serial() {
                        // Dims NATIVAS del decoder (antes del reescalado
                        // player-side) — es lo que registra el sync-log
                        // para poder medir la recuperación de calidad.
                        let (dec_w, dec_h) = (frame.width, frame.height);
                        if frame.width != dst_w || frame.height != dst_h {
                            rescale_frame_nearest(&mut frame, dst_w, dst_h);
                        }
                        // Aterrizaje del seek en pausa: re-apuntar los
                        // relojes al PTS real y alinear el audio para
                        // que al reanudar suene EXACTAMENTE aquí.
                        if pending_audio_landing {
                            master.retarget(frame.pts);
                            if let Some(a) = audio_handle.as_ref() {
                                a.seek(frame.pts);
                            }
                            pending_audio_landing = false;
                        }
                        let vid_rows = rows.saturating_sub(hud_lines + sub_rows).max(1);
                        let (ox, oy) = offsets_for_frame(
                            backend, cell_px, frame.width, frame.height, cols, vid_rows,
                        );
                        let mut sol = so.lock();
                        if force_full_redraw {
                            // Clear dentro del batch ?2026 del renderer
                            // (vía reset_layout_cache) — sin flash.
                            force_full_redraw = false;
                            hud_cache = None; sub_cache = None;
                            renderer_.reset_layout_cache();
                        }
                        if matches!(renderer_.draw(&mut sol, &frame, cols, vid_rows, ox, oy), Ok(true)) {
                            hud_cache = None; sub_cache = None;
                        }
                        drop(sol);
                        last_shown_pts = frame.pts;
                        show_one_frame_paused = false;
                        // Registrar también este frame en el sync-log:
                        // es el "primer frame post-seek" aunque estemos
                        // en pausa (el test de integración lo mide).
                        if let Some(log) = sync_log.as_mut() {
                            let m = master.now();
                            let _ = writeln!(
                                log,
                                "{:.4} {:.4} {:.4} {:+.1} {} {} {}",
                                wall_now_f64(),
                                m,
                                frame.pts,
                                (frame.pts - m) * 1000.0,
                                frames_dropped_win,
                                dec_w,
                                dec_h,
                            );
                            let _ = log.flush();
                        }
                        last_frame = Some(frame);
                    }
                }
            } else {
                // Espera INTERRUMPIBLE por eventos: una tecla o un
                // resize despiertan al instante (antes: sleep fijo de
                // 20 ms que retrasaba la respuesta en pausa).
                input::wait_event(Duration::from_millis(50));
            }
            let vb = last_frame.as_ref().map(|f| {
                let vid_rows = rows.saturating_sub(hud_lines + sub_rows).max(1);
                video_bottom_row(backend, cell_px, f.width, f.height, cols, vid_rows)
            });
            draw_subs_dispatch(
                &mut so,
                cols,
                rows,
                hud_lines,
                sub_rows,
                vb,
                sub_track.as_ref(),
                last_shown_pts,
                &mut sub_cache,
            );
            draw_hud_dispatch(
                &mut so,
                cols,
                rows,
                hud_lines,
                &*master,
                dec.duration,
                volume,
                &hud_backend_label(&dec, backend.name()),
                cell_px,
                dst_w,
                dst_h,
                fps_shown_now,
                fps_dec_now,
                dropped_last_win,
                using_audio,
                cfg.show_stats,
                true,
                osd.as_ref().map(|(s, _)| s.as_str()),
                &mut hud_cache,
            );
            continue;
        }

        // 2.5) HOLD post-seek/arranque con audio: si ya mostramos el
        //      frame del target y el reloj maestro sigue congelado
        //      (sin audio real aún), esperamos sin consumir más frames.
        if using_audio
            && !master.master_anchored()
            && held_frame_serial == Some(dec.current_serial())
        {
            // Válvula: si el audio no ancla en 1.5 s (seek más allá
            // del final del audio, dispositivo caído…), arrancamos el
            // reloj igualmente para que el vídeo siga.
            if hold_started.map(|t| t.elapsed() > Duration::from_millis(1500)).unwrap_or(false) {
                master.master().force_anchor();
            } else {
                // Interrumpible: un resize durante el hold se atiende YA.
                input::wait_event(Duration::from_millis(4));
                continue;
            }
        }

        // 3) Obtener siguiente frame: el PENDIENTE de la iteración
        //    anterior (aún no le tocaba mostrarse) o uno nuevo del
        //    canal (timeout corto para seguir procesando input y HUD
        //    si el decoder está lento).
        let mut frame = match pending.take() {
            Some(f) => f,
            None => match dec.rx.recv_timeout(Duration::from_millis(50)) {
                Ok(f) => f,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if dec.eof.load(Ordering::Relaxed) {
                        if cfg.loop_video {
                            master.set(0.0);
                            dec.seek(0.0);
                            pending_audio_landing = using_audio;
                            frame_timer = wall_now_f64();
                            last_shown_pts = 0.0;
                            force_full_redraw = true;
                            continue;
                        }
                        break 'main;
                    }
                    continue;
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break 'main,
            },
        };

        // 4) Descartar frames con serial obsoleto (residuo tras seek
        //    reciente que ya no aplica).
        let cur_serial = dec.current_serial();
        if frame.serial != cur_serial {
            continue;
        }

        // 4.1) Frame con dims VIEJAS (resize en vuelo): reescalado
        //      nearest player-side a las dims nuevas. La cola puede
        //      contener hasta ~2.5 s de frames pre-decodificados con
        //      las dims anteriores; antes se mostraban recortados (al
        //      encoger) o minúsculos (al agrandar) hasta que el
        //      decoder alcanzaba las dims nuevas — el resize "tardaba".
        //      Ahora TODOS los frames se muestran ya al tamaño nuevo.
        //      (dec_w/dec_h conservan las dims NATIVAS para el sync-log.)
        let (dec_w, dec_h) = (frame.width, frame.height);
        if frame.width != dst_w || frame.height != dst_h {
            rescale_frame_nearest(&mut frame, dst_w, dst_h);
        }

        let cur_pts_ms = (frame.pts * 1000.0) as i64;
        if cur_pts_ms != last_dec_pts_ms {
            frames_dec_win += 1;
            last_dec_pts_ms = cur_pts_ms;
        }

        // 4.5) Reloj maestro DESANCLADO (audio aún sin arrancar tras
        //      seek/arranque): mostramos este frame YA (es el frame
        //      del target) y activamos el hold hasta que el audio
        //      ancle el reloj. Así el vídeo "salta de golpe" al punto
        //      pedido y arranca EXACTAMENTE cuando suena el audio.
        if using_audio && !master.master_anchored() {
            // Primer frame post-seek: PTS de aterrizaje real (keyframe
            // <= target). Re-apuntamos relojes y saltamos el audio AHÍ.
            if pending_audio_landing {
                master.retarget(frame.pts);
                if let Some(a) = audio_handle.as_ref() {
                    a.seek(frame.pts);
                }
                pending_audio_landing = false;
            }
            {
                let vid_rows = rows.saturating_sub(hud_lines + sub_rows).max(1);
                let (ox, oy) =
                    offsets_for_frame(backend, cell_px, frame.width, frame.height, cols, vid_rows);
                let mut sol = so.lock();
                if force_full_redraw {
                    // Sin 2J manual fuera del batch: reset_layout_cache
                    // hace que el renderer emita el clear DENTRO del
                    // batch sincronizado (?2026) → sin flash negro.
                    force_full_redraw = false;
                    hud_cache = None; sub_cache = None;
                    renderer_.reset_layout_cache();
                }
                if matches!(renderer_.draw(&mut sol, &frame, cols, vid_rows, ox, oy), Ok(true)) {
                    hud_cache = None; sub_cache = None;
                }
            }
            vidclk.set_pts(frame.pts, vidclk.current_serial());
            last_shown_pts = frame.pts;
            frame_timer = wall_now_f64();
            held_frame_serial = Some(frame.serial);
            hold_started = Some(Instant::now());
            if let Some(log) = sync_log.as_mut() {
                let m = master.now();
                let _ = writeln!(
                    log,
                    "{:.4} {:.4} {:.4} {:+.1} {} {} {}",
                    wall_now_f64(),
                    m,
                    frame.pts,
                    (frame.pts - m) * 1000.0,
                    frames_dropped_win,
                    dec_w,
                    dec_h,
                );
                let _ = log.flush();
            }
            let vb = {
                let vid_rows = rows.saturating_sub(hud_lines + sub_rows).max(1);
                video_bottom_row(backend, cell_px, frame.width, frame.height, cols, vid_rows)
            };
            draw_subs_dispatch(
                &mut so,
                cols,
                rows,
                hud_lines,
                sub_rows,
                Some(vb),
                sub_track.as_ref(),
                frame.pts,
                &mut sub_cache,
            );
            draw_hud_dispatch(
                &mut so,
                cols,
                rows,
                hud_lines,
                &*master,
                dec.duration,
                volume,
                &hud_backend_label(&dec, backend.name()),
                cell_px,
                dst_w,
                dst_h,
                fps_shown_now,
                fps_dec_now,
                dropped_last_win,
                using_audio,
                cfg.show_stats,
                false,
                osd.as_ref().map(|(s, _)| s.as_str()),
                &mut hud_cache,
            );
            last_frame = Some(frame);
            continue;
        }
        // Reloj anclado: si veníamos de un hold, resincronizamos el
        // frame_timer al reloj mural para no arrastrar el tiempo de
        // espera como "deuda", y RE-ANCLAMOS vidclk al frame mostrado:
        // vidclk se seteó al ENTRAR al hold y estuvo extrapolando en
        // vacío todo el hold (no tiene staleness) → sin este re-set,
        // `diff = vidclk.now() - master.now()` salía +[duración del
        // hold], la "espera exacta" dormía 0.5 s (cap) y el vídeo
        // arrancaba tarde tras cada anclaje del audio.
        if held_frame_serial.take().is_some() {
            frame_timer = wall_now_f64();
            hold_started = None;
            vidclk.set_pts(last_shown_pts, vidclk.current_serial());
        }

        // 5) SYNC estilo ffplay: computamos el delay natural entre el
        //    frame previo y éste, y lo ajustamos por el drift respecto
        //    al master.
        let natural_delay = vp_duration(last_shown_pts, frame.pts, fallback_frame_dur, max_frame_dur);
        // Semántica EXACTA de ffplay: diff = vidclk - master, donde
        // vidclk extrapola el PTS del frame EN PANTALLA. Usar el PTS
        // del frame PENDIENTE metía +1 frame de offset sistemático
        // (~-40 ms de sesgo con vídeo a 25 fps).
        let vid_now = vidclk.now();
        let master_now = master.now();
        let diff = if vid_now.is_finite() && master_now.is_finite() {
            vid_now - master_now
        } else {
            0.0
        };
        let target_delay = compute_target_delay(natural_delay, diff);

        // Momento mural en el que "queremos" mostrar este frame.
        frame_timer += target_delay;
        let now_wall = wall_now_f64();

        // ffplay: si el frame_timer se quedó atrás más de
        // AV_SYNC_THRESHOLD_MAX (100 ms), resincronizamos al reloj
        // mural para no arrastrar deuda de tiempo. (Antes el umbral
        // era 10 s → tras cualquier hipo del render el bucle
        // "perseguia" la deuda mostrando frames sin dormir, con el
        // vídeo acelerado y desincronizado del audio.)
        if now_wall - frame_timer > AV_SYNC_THRESHOLD_MAX {
            frame_timer = now_wall;
        }

        // ¿Este frame llega claramente tarde respecto al maestro?
        // → drop (pero frame_timer ya avanzó, no acumulamos deuda).
        //
        // EXCEPCIÓN — catch-up del refinado post-resize: mientras el
        // decoder re-decodifica el GOP para alcanzar al reloj, TODOS
        // sus frames llegan "tarde"; droparlos = pantalla congelada y
        // borrosa hasta el final del catch-up. Los mostramos sin
        // dormir (la calidad se recupera al primer frame nítido y el
        // vídeo "alcanza" al audio de forma visible, estilo mpv). El
        // catch-up acaba con el primer frame que llega a tiempo.
        let master_diff = frame.pts - master.now();
        if master_diff.is_finite() && master_diff < -AV_SYNC_THRESHOLD_MAX {
            if refine_catchup == Some(frame.serial) {
                frame_timer = now_wall; // mostrar YA, sin deuda
            } else {
                frames_dropped_win += 1;
                last_shown_pts = frame.pts;
                // REVERTIR el `frame_timer += target_delay` de arriba:
                // un frame dropeado NO consume slot de presentación.
                // Sin esto, el catch-up post-seek (aterrizar en el
                // keyframe y dropear hasta el target — p.ej. 134
                // frames con GOPs de 10 s) empujaba frame_timer
                // ~5 s HACIA EL FUTURO (134 × 40 ms), y como la
                // resincronización solo cubre el atraso (now - timer
                // > 100 ms), el player quedaba mostrando 1 frame
                // cada 500 ms (el cap del sleep) durante ~10 s tras
                // cada seek hacia atrás: cadencia rota, ráfagas de
                // seeks colapsadas en un solo salto visible y gaps
                // murales >1.5 s entre frames.
                frame_timer -= target_delay;
                continue;
            }
        } else {
            refine_catchup = None; // frame a tiempo: catch-up completado
        }

        // Si aún no toca mostrar, esperamos hasta frame_timer — pero
        // de forma INTERRUMPIBLE: si llega un evento (resize, tecla)
        // devolvemos el frame a `pending`, retrocedemos frame_timer
        // (se re-sumará al reprocesar el frame) y volvemos al top del
        // loop para atender el evento YA. Antes: `thread::sleep` de
        // hasta 500 ms → el resize se atendía al despertar → la
        // sensación de "resize no instantáneo".
        if frame_timer > now_wall {
            let sleep_s = (frame_timer - now_wall).min(0.5);
            if sleep_s > 0.0005 && input::wait_event(Duration::from_secs_f64(sleep_s)) {
                frame_timer -= target_delay;
                pending = Some(frame);
                continue;
            }
        }

        // 6) Dibujar el frame + HUD. El layout (offsets de centrado)
        //    se recalcula POR FRAME a partir de las dims REALES del
        //    frame: durante un resize conviven frames con dims viejas
        //    y nuevas, y cada uno se centra/recorta correctamente sin
        //    perder el colchón de pre-decode ni tocar el sync.
        {
            let vid_rows = rows.saturating_sub(hud_lines + sub_rows).max(1);
            let (ox, oy) =
                offsets_for_frame(backend, cell_px, frame.width, frame.height, cols, vid_rows);
            let mut sol = so.lock();
            if force_full_redraw {
                // Clear dentro del batch ?2026 del renderer
                // (vía reset_layout_cache) — sin flash.
                force_full_redraw = false;
                hud_cache = None; sub_cache = None;
                renderer_.reset_layout_cache();
            }
            if matches!(renderer_.draw(&mut sol, &frame, cols, vid_rows, ox, oy), Ok(true)) {
                hud_cache = None; sub_cache = None;
            }
        }

        // 7) Actualizar vidclk al PTS del frame que ACABAMOS de mostrar.
        //    Si no hay audio, esto es el reloj maestro. Con audio, sirve
        //    para el HUD y para futuros sync-to-slave.
        //    Usamos el serial PROPIO del vidclk como token: el filtrado
        //    de frames obsoletos ya se hizo arriba contra el serial del
        //    decoder (frame.serial != dec.current_serial() → skip), y
        //    los contadores del reloj y del decoder son independientes.
        vidclk.set_pts(frame.pts, vidclk.current_serial());
        last_shown_pts = frame.pts;

        if let Some(log) = sync_log.as_mut() {
            let m = master.now();
            let _ = writeln!(
                log,
                "{:.4} {:.4} {:.4} {:+.1} {} {} {}",
                wall_now_f64(),
                m,
                frame.pts,
                (frame.pts - m) * 1000.0,
                frames_dropped_win,
                dec_w,
                dec_h,
            );
            let _ = log.flush();
        }

        let vb = {
            let vid_rows = rows.saturating_sub(hud_lines + sub_rows).max(1);
            video_bottom_row(backend, cell_px, frame.width, frame.height, cols, vid_rows)
        };
        draw_subs_dispatch(
            &mut so,
            cols,
            rows,
            hud_lines,
            sub_rows,
            Some(vb),
            sub_track.as_ref(),
            frame.pts,
            &mut sub_cache,
        );
        draw_hud_dispatch(
            &mut so,
            cols,
            rows,
            hud_lines,
            &*master,
            dec.duration,
            volume,
            &hud_backend_label(&dec, backend.name()),
            cell_px,
            dst_w,
            dst_h,
            fps_shown_now,
            fps_dec_now,
            dropped_last_win,
            using_audio,
            cfg.show_stats,
            false,
            osd.as_ref().map(|(s, _)| s.as_str()),
            &mut hud_cache,
        );
        last_frame = Some(frame);
        frames_shown_win += 1;

        let el = stats_epoch.elapsed();
        if el >= Duration::from_secs(1) {
            let secs = el.as_secs_f64();
            fps_shown_now = frames_shown_win as f64 / secs;
            fps_dec_now = frames_dec_win as f64 / secs;
            dropped_last_win = frames_dropped_win;
            frames_shown_win = 0;
            frames_dec_win = 0;
            frames_dropped_win = 0;
            stats_epoch = Instant::now();
        }
    }

    // Cleanup.
    // Sync-log: flush + fsync EXPLÍCITOS antes de salir. El log se
    // flushea línea a línea, pero en algunos filesystems (9p/WSL,
    // overlays de sandbox, NFS) los datos de un proceso recién muerto
    // pueden tardar unos ms en ser visibles para un lector externo:
    // el test de integración leía el fichero justo tras wait() y veía
    // 0 filas con el fichero ya completo (flaky ~1/6). sync_all()
    // fuerza los datos a estable ANTES de que exit() sea observable.
    if let Some(mut log) = sync_log.take() {
        let _ = log.flush();
        if let Ok(f) = log.into_inner() {
            let _ = f.sync_all();
        }
    }
    if let Some(mut a) = audio_handle.take() {
        a.stop();
    }
    let _ = dec;
    Ok(())
}

/// Wall time monotónico en segundos (basado en `Instant::now` desde
/// un origen fijo por proceso). Equivalente a `av_gettime_relative()`.
fn wall_now_f64() -> f64 {
    use once_cell::sync::Lazy;
    static ORIGIN: Lazy<Instant> = Lazy::new(Instant::now);
    ORIGIN.elapsed().as_secs_f64()
}

// -------------------- helpers de layout / HUD --------------------

fn compute_layout(
    backend: renderer::Backend,
    source_size: (u32, u32),
    cols: u16,
    rows: u16,
    cell_px: CellPx,
    scale: f32,
    hud_rows: u16,
) -> (u32, u32, u16, u16) {
    let (avail_w, avail_h) =
        terminfo::adaptive_target_pixels(backend, cols, rows, cell_px, scale, hud_rows);
    let (align_w, align_h) = px_per_cell(backend, cell_px);
    let ((w, h), (ox, oy)) = renderer::fit_aspect(source_size, (avail_w, avail_h), align_w, align_h);
    let (col_ox, row_oy) = px_to_cells(backend, cell_px, ox, oy);
    (w.max(2), h.max(2), col_ox, row_oy)
}

/// Celdas que ocupa un frame de `fw`×`fh` píxeles en este backend, y
/// offsets de centrado dentro de `cols`×`vid_rows` (área sin HUD).
/// Se calcula POR FRAME con las dims reales del frame — clave para
/// que los frames con dims "viejas" durante un resize sigan centrados
/// y recortados correctamente.
fn offsets_for_frame(
    backend: renderer::Backend,
    cell: CellPx,
    fw: u32,
    fh: u32,
    cols: u16,
    vid_rows: u16,
) -> (u16, u16) {
    let (pcx, pcy) = px_per_cell(backend, cell);
    let cw = fw.div_ceil(pcx.max(1)).max(1);
    let ch = fh.div_ceil(pcy.max(1)).max(1);
    let ox = (u32::from(cols).saturating_sub(cw)) / 2;
    let oy = (u32::from(vid_rows).saturating_sub(ch)) / 2;
    (ox.min(u16::MAX as u32) as u16, oy.min(u16::MAX as u32) as u16)
}

fn px_per_cell(backend: renderer::Backend, cell: CellPx) -> (u32, u32) {
    match backend {
        renderer::Backend::HalfBlocks => (1, 2),
        renderer::Backend::Ascii => (1, 1),
        _ => (cell.w.max(1), cell.h.max(1)),
    }
}

fn px_to_cells(
    backend: renderer::Backend,
    cell: CellPx,
    px_x: u32,
    px_y: u32,
) -> (u16, u16) {
    let (pcx, pcy) = px_per_cell(backend, cell);
    ((px_x / pcx.max(1)) as u16, (px_y / pcy.max(1)) as u16)
}

/// Fila 1-based de la ÚLTIMA fila de celdas ocupada por el vídeo
/// (offset de centrado vertical + alto del frame en celdas), dentro
/// del área de vídeo `vid_rows`. Sirve para anclar los subtítulos
/// justo debajo de la imagen en vez de al fondo de la terminal.
fn video_bottom_row(
    backend: renderer::Backend,
    cell: CellPx,
    fw: u32,
    fh: u32,
    cols: u16,
    vid_rows: u16,
) -> u16 {
    let (_, pcy) = px_per_cell(backend, cell);
    let ch = fh.div_ceil(pcy.max(1)).max(1).min(u32::from(vid_rows)) as u16;
    let (_, oy) = offsets_for_frame(backend, cell, fw, fh, cols, vid_rows);
    (oy + ch).min(vid_rows)
}

/// Pinta las filas de subtítulos. Cacheado por contenido: solo
/// reescribe cuando el texto (o su posición) cambia — los eventos
/// duran segundos → ~0 coste por refresco. El texto se centra y se
/// recorta al ancho; si tiene más líneas que filas reservadas se
/// muestran las ÚLTIMAS (las más recientes del diálogo).
///
/// Colocación: si el vídeo va letterboxeado (barra negra inferior
/// dentro del área de vídeo), los subtítulos se pegan JUSTO debajo
/// de la imagen (una fila de margen) en vez de quedarse al fondo de
/// la terminal lejos del vídeo. Sin letterbox caen en sus filas
/// reservadas de siempre (encima del HUD).
#[allow(clippy::too_many_arguments)]
fn draw_subs_dispatch(
    so: &mut Stdout,
    cols: u16,
    rows: u16,
    hud_lines: u16,
    sub_rows: u16,
    video_bottom: Option<u16>,
    track: Option<&subs::SubTrack>,
    t: f64,
    cache: &mut Option<(u16, u16, u16, String)>,
) {
    if sub_rows == 0 {
        return;
    }
    let Some(track) = track else { return };
    let text = track.query(t).unwrap_or_default();
    // Fila reservada clásica (justo encima del HUD) = tope inferior.
    let reserved_first = rows.saturating_sub(hud_lines + sub_rows) + 1;
    let first_row = match video_bottom {
        // +2 = una fila en blanco de separación bajo la imagen.
        Some(vb) => (vb + 2).min(reserved_first),
        None => reserved_first,
    };
    let key = (cols, rows, first_row, text);
    if cache.as_ref() == Some(&key) {
        return;
    }
    // Si la posición cambió (p.ej. resize sin 2J de por medio),
    // limpiar las filas de la posición anterior antes de pintar.
    let prev_row = cache.as_ref().map(|(_, _, r, _)| *r);
    let lines: Vec<&str> = key.3.lines().collect();
    let start = lines.len().saturating_sub(sub_rows as usize);
    let mut sol = so.lock();
    if let Some(pr) = prev_row {
        if pr != first_row {
            for i in 0..sub_rows {
                let _ = renderer::draw_hud_at(&mut sol, cols, pr + i, "");
            }
        }
    }
    for i in 0..sub_rows {
        let content = lines.get(start + i as usize).copied().unwrap_or("");
        let centered = center_text(content, cols);
        let _ = renderer::draw_sub_line(&mut sol, cols, first_row + i, &centered);
    }
    let _ = sol.flush();
    *cache = Some(key);
}

/// Centra `s` en `cols` celdas (por anchura unicode real).
fn center_text(s: &str, cols: u16) -> String {
    use unicode_width::UnicodeWidthStr;
    let w = s.width();
    let pad = (cols as usize).saturating_sub(w) / 2;
    let mut out = String::with_capacity(pad + s.len());
    for _ in 0..pad {
        out.push(' ');
    }
    out.push_str(s);
    out
}

#[allow(clippy::too_many_arguments)]
fn draw_hud_dispatch(
    so: &mut Stdout,
    cols: u16,
    rows: u16,
    hud_lines: u16,
    clock: &dyn Clock,
    duration: f64,
    volume: i32,
    backend_name: &str,
    cell: CellPx,
    frame_w: u32,
    frame_h: u32,
    fps_shown: f64,
    fps_decoded: f64,
    dropped: u64,
    using_audio: bool,
    show_stats: bool,
    paused: bool,
    osd: Option<&str>,
    cache: &mut HudCache,
) {
    // Terminal minúscula: HUD oculto — no hay nada legible que pintar
    // y reescribirlo truncado cada frame es la fuente principal del
    // parpadeo con ventanas pequeñas.
    if hud_lines == 0 {
        // Flush pendiente del frame recién dibujado (antes lo hacía
        // este mismo dispatch al escribir el HUD).
        let mut sol = so.lock();
        let _ = sol.flush();
        return;
    }
    let (mut l1, l2) = format_hud_lines(
        clock,
        duration,
        volume,
        backend_name,
        cell,
        frame_w,
        frame_h,
        fps_shown,
        fps_decoded,
        dropped,
        using_audio,
        show_stats,
        paused,
        cols,
        hud_lines,
    );
    // OSD transitorio (cambio de pista): sustituye la línea principal
    // del HUD mientras está activo — entra en la key del caché, así
    // que aparece y desaparece con un solo repintado.
    if let Some(o) = osd {
        l1 = format!(" ▸ {o}");
    }
    // Caché anti-parpadeo: si el HUD no cambió (mismo tamaño de
    // terminal y mismo texto), NO se reescribe la fila. El HUD solo
    // cambia ~1 vez/s (el reloj), pero se despachaba a fps completos
    // (25-60/s): cada reescritura es un borrado+repintado visible en
    // terminales lentas → el "parpadeo del HUD".
    let key = (cols, rows, hud_lines, l1, l2);
    let dirty = cache.as_ref() != Some(&key);
    let mut sol = so.lock();
    if dirty {
        if hud_lines == 2 {
            let _ = renderer::draw_hud_two_lines(&mut sol, cols, rows, &key.3, &key.4);
        } else {
            let _ = renderer::draw_hud(&mut sol, cols, rows, &key.3);
        }
        *cache = Some(key);
    }
    let _ = sol.flush();
}

/// Reescala un RgbFrame a `dst_w`×`dst_h` con nearest-neighbor.
/// Se usa SOLO en transitorios de resize (frames pre-decodificados con
/// dims viejas y redibujo del frame cacheado): el siguiente frame que
/// salga del decoder ya viene reescalado con FAST_BILINEAR de sws.
/// Coste: O(w·h) con LUT de índices — ~1 ms para 300×90 celdas, nada
/// comparado con el frame budget de 40 ms a 25 fps.
fn rescale_frame_nearest(f: &mut decoder::RgbFrame, dst_w: u32, dst_h: u32) {
    let (sw, sh) = (f.width as usize, f.height as usize);
    let (dw, dh) = (dst_w.max(2) as usize, dst_h.max(2) as usize);
    if sw == 0 || sh == 0 || (sw == dw && sh == dh) || f.data.len() < sw * sh * 3 {
        return;
    }
    // LUT de mapeo columna destino → columna origen (evita el div/mul
    // por píxel en el bucle caliente).
    let mut xmap = Vec::with_capacity(dw);
    for x in 0..dw {
        xmap.push((x * sw / dw).min(sw - 1) * 3);
    }
    let mut out = vec![0u8; dw * dh * 3];
    for y in 0..dh {
        let sy = (y * sh / dh).min(sh - 1);
        let srow = &f.data[sy * sw * 3..sy * sw * 3 + sw * 3];
        let drow = &mut out[y * dw * 3..y * dw * 3 + dw * 3];
        for (x, &sx) in xmap.iter().enumerate() {
            let d = x * 3;
            drow[d] = srow[sx];
            drow[d + 1] = srow[sx + 1];
            drow[d + 2] = srow[sx + 2];
        }
    }
    f.width = dst_w.max(2);
    f.height = dst_h.max(2);
    f.data = out;
}

#[allow(clippy::too_many_arguments)]
fn format_hud_lines(
    clock: &dyn Clock,
    duration: f64,
    volume: i32,
    backend_name: &str,
    cell: CellPx,
    frame_w: u32,
    frame_h: u32,
    fps_shown: f64,
    fps_decoded: f64,
    dropped: u64,
    using_audio: bool,
    show_stats: bool,
    paused: bool,
    cols: u16,
    hud_lines: u16,
) -> (String, String) {
    let t = clock.now().max(0.0).min(duration.max(0.0));
    let flag = if paused { "⏸" } else { "▶" };

    let bar_w = hud_bar_w(cols);
    let filled = if duration > 0.0 {
        ((t / duration) * bar_w as f64).round() as usize
    } else {
        0
    }
    .min(bar_w);
    let bar = "█".repeat(filled) + &"░".repeat(bar_w - filled);
    let audio_tag = if using_audio { "🔊" } else { "🔇" };

    // Bloque de métricas: SOLO con --stats. Antes el HUD de 2 líneas
    // las mostraba siempre (backend, resolución, celda, fps, drops) y
    // el flag "no hacía nada"; ahora el HUD por defecto es limpio
    // (transporte + volumen) y --stats añade la telemetría.
    let stats_block = || {
        format!(
            " · {} {}×{} (cell {}×{} {}) · {:5.1} fps ({:.0} dec, {} drop)",
            backend_name,
            frame_w,
            frame_h,
            cell.w,
            cell.h,
            cell.source.short(),
            fps_shown,
            fps_decoded,
            dropped,
        )
    };

    if hud_lines == 2 {
        let mut line1 = format!(
            " {} [{}] {}/{} · vol {} {}",
            flag,
            bar,
            fmt_time(t),
            fmt_time(duration),
            volume,
            audio_tag,
        );
        if show_stats {
            line1.push_str(&stats_block());
        }
        let line2 =
            " q=salir · ␣=pausa · ←/→=seek ±5s · click barra=ir a · ↑/↓=vol ±5 · a=audio · j=subs"
                .to_string();
        (line1, line2)
    } else if cols >= 60 {
        let mut line = format!(
            " {} [{}] {}/{} · vol {} {}",
            flag,
            bar,
            fmt_time(t),
            fmt_time(duration),
            volume,
            audio_tag,
        );
        if show_stats {
            line.push_str(&stats_block());
            line.push_str(" · q=salir");
        } else {
            line.push_str(" · q=salir · ␣=pausa · ←/→=seek");
        }
        (line, String::new())
    } else {
        let line = if show_stats {
            format!(
                " {} {}/{} · {:.0} fps ({} drop) · q",
                flag,
                fmt_time(t),
                fmt_time(duration),
                fps_shown,
                dropped,
            )
        } else {
            format!(" {} {}/{} · q", flag, fmt_time(t), fmt_time(duration))
        };
        (line, String::new())
    }
}

fn fmt_time(t: f64) -> String {
    if !t.is_finite() || t < 0.0 {
        return "--:--".to_string();
    }
    let s = t as u64;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}
