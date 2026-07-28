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
    cursor, execute,
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
use crate::terminfo::{self, CellPx};

pub struct Config {
    pub path: PathBuf,
    pub forced_backend: Option<String>,
    pub scale: f32,
    pub loop_video: bool,
    pub show_stats: bool,
    pub no_audio: bool,
}

struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    fn enter(so: &mut Stdout) -> Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(so, EnterAlternateScreen, cursor::Hide)?;
        Ok(Self { active: true })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut so = stdout();
        let _ = execute!(so, cursor::Show, LeaveAlternateScreen);
        let _ = write!(so, "\x1b[0m");
        let _ = so.flush();
        let _ = terminal::disable_raw_mode();
    }
}

fn hud_rows_for(cols: u16, rows: u16) -> u16 {
    if rows >= 24 && cols >= 100 {
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

    // --- Audio (opcional) ---
    let mut audio_handle: Option<AudioHandle> = if cfg.no_audio {
        None
    } else {
        match audio::spawn(&cfg.path, audclk_pre.clone()) {
            Ok(h) if h.has_audio => Some(h),
            _ => None,
        }
    };
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

    // Decoder vídeo.
    let mut hud_lines = hud_rows_for(cols, rows);
    let (dst_w0, dst_h0) =
        terminfo::adaptive_target_pixels(backend, cols, rows, cell_px, cfg.scale, hud_lines);
    let dec = decoder::spawn(&cfg.path, dst_w0, dst_h0)?;

    let (mut dst_w, mut dst_h, mut col_ox, mut row_oy) = compute_layout(
        backend,
        dec.source_size,
        cols,
        rows,
        cell_px,
        cfg.scale,
        hud_lines,
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

    // Log de sincronía opcional (para tests de integración):
    // RTV_SYNC_LOG=/ruta/fichero → una línea por frame mostrado:
    //   wall_s master_s video_pts_s avdiff_ms dropped_win
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
                Cmd::SeekRel(delta) => {
                    let now = master.now();
                    if let Some(log) = sync_log.as_mut() {
                        let _ = writeln!(
                            log,
                            "# SEEK wall={:.4} delta={:+.1} now={:.3} anchored={}",
                            wall_now_f64(),
                            delta,
                            now,
                            master.master_anchored(),
                        );
                        let _ = log.flush();
                    }
                    // Clamp: dejamos 0.5 s de margen antes del final para
                    // no aterrizar en EOF exacto (pantalla congelada).
                    let max_t = (dec.duration - 0.5).max(0.0);
                    let target = (now + delta).max(0.0).min(max_t);
                    // ORDEN ATÓMICO:
                    //   (1) master.set(target) → bumpea serial en audclk
                    //       Y vidclk; cualquier chunk/frame en vuelo con
                    //       serial viejo será descartado por callback/player.
                    //   (2) audio.seek(target) → decoder audio salta y
                    //       recorta samples hasta el target exacto.
                    //   (3) dec.seek(target)   → decoder vídeo salta con
                    //       keyframe<=target + drop-until-target-PTS.
                    master.set(target);
                    dec.seek(target);
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
                Cmd::Resize(c, r) => {
                    cols = c.max(4);
                    rows = r.max(3);
                    hud_lines = hud_rows_for(cols, rows);
                    let (nw, nh, nox, noy) = compute_layout(
                        backend,
                        dec.source_size,
                        cols,
                        rows,
                        cell_px,
                        cfg.scale,
                        hud_lines,
                    );
                    dst_w = nw;
                    dst_h = nh;
                    dec.resize(dst_w, dst_h);
                    col_ox = nox;
                    row_oy = noy;
                    force_full_redraw = true;
                }
                Cmd::None => {}
            }
        }

        // 2) Pausa: dormimos un pelín y actualizamos HUD. Si hay un
        //    seek pendiente de visualizar, sacamos UN frame del decoder
        //    (el del target) y lo pintamos sin salir de la pausa.
        if master.is_paused() {
            if show_one_frame_paused {
                if let Ok(frame) = dec.rx.recv_timeout(Duration::from_millis(200)) {
                    if frame.serial == dec.current_serial() {
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
                        let mut sol = so.lock();
                        if force_full_redraw {
                            let _ = write!(&mut sol, "\x1b[2J\x1b[H");
                            force_full_redraw = false;
                            renderer_.reset_layout_cache();
                        }
                        let _ = renderer_.draw(&mut sol, &frame, cols, rows, col_ox, row_oy);
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
                                "{:.4} {:.4} {:.4} {:+.1} {}",
                                wall_now_f64(),
                                m,
                                frame.pts,
                                (frame.pts - m) * 1000.0,
                                frames_dropped_win,
                            );
                        }
                    }
                }
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
            draw_hud_dispatch(
                &mut so,
                cols,
                rows,
                hud_lines,
                &*master,
                dec.duration,
                volume,
                backend.name(),
                cell_px,
                dst_w,
                dst_h,
                fps_shown_now,
                fps_dec_now,
                dropped_last_win,
                using_audio,
                cfg.show_stats,
                true,
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
                std::thread::sleep(Duration::from_millis(4));
                continue;
            }
        }

        // 3) Obtener siguiente frame (con timeout corto para poder
        //    seguir procesando input y HUD si el decoder está lento).
        let frame = match dec.rx.recv_timeout(Duration::from_millis(50)) {
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
        };

        // 4) Descartar frames con serial obsoleto (residuo tras seek
        //    reciente que ya no aplica).
        let cur_serial = dec.current_serial();
        if frame.serial != cur_serial {
            continue;
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
                let mut sol = so.lock();
                if force_full_redraw {
                    let _ = write!(&mut sol, "\x1b[2J\x1b[H");
                    force_full_redraw = false;
                    renderer_.reset_layout_cache();
                }
                let _ = renderer_.draw(&mut sol, &frame, cols, rows, col_ox, row_oy);
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
                    "{:.4} {:.4} {:.4} {:+.1} {}",
                    wall_now_f64(),
                    m,
                    frame.pts,
                    (frame.pts - m) * 1000.0,
                    frames_dropped_win,
                );
            }
            draw_hud_dispatch(
                &mut so,
                cols,
                rows,
                hud_lines,
                &*master,
                dec.duration,
                volume,
                backend.name(),
                cell_px,
                dst_w,
                dst_h,
                fps_shown_now,
                fps_dec_now,
                dropped_last_win,
                using_audio,
                cfg.show_stats,
                false,
            );
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
        let master_diff = frame.pts - master.now();
        if master_diff.is_finite() && master_diff < -AV_SYNC_THRESHOLD_MAX {
            frames_dropped_win += 1;
            last_shown_pts = frame.pts;
            continue;
        }

        // Si aún no toca mostrar, dormimos justo hasta frame_timer.
        if frame_timer > now_wall {
            let sleep_s = (frame_timer - now_wall).min(0.5);
            if sleep_s > 0.0005 {
                std::thread::sleep(Duration::from_secs_f64(sleep_s));
            }
        }

        // 6) Dibujar el frame + HUD.
        {
            let mut sol = so.lock();
            if force_full_redraw {
                let _ = write!(&mut sol, "\x1b[2J\x1b[H");
                force_full_redraw = false;
                renderer_.reset_layout_cache();
            }
            let _ = renderer_.draw(&mut sol, &frame, cols, rows, col_ox, row_oy);
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
                "{:.4} {:.4} {:.4} {:+.1} {}",
                wall_now_f64(),
                m,
                frame.pts,
                (frame.pts - m) * 1000.0,
                frames_dropped_win,
            );
        }

        draw_hud_dispatch(
            &mut so,
            cols,
            rows,
            hud_lines,
            &*master,
            dec.duration,
            volume,
            backend.name(),
            cell_px,
            dst_w,
            dst_h,
            fps_shown_now,
            fps_dec_now,
            dropped_last_win,
            using_audio,
            cfg.show_stats,
            false,
        );
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
) {
    let (l1, l2) = format_hud_lines(
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
    let mut sol = so.lock();
    if hud_lines == 2 {
        let _ = renderer::draw_hud_two_lines(&mut sol, cols, rows, &l1, &l2);
    } else {
        let _ = renderer::draw_hud(&mut sol, cols, rows, &l1);
    }
    let _ = sol.flush();
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

    let bar_w = if cols >= 120 {
        40
    } else if cols >= 80 {
        24
    } else if cols >= 60 {
        16
    } else {
        8
    };
    let filled = if duration > 0.0 {
        ((t / duration) * bar_w as f64).round() as usize
    } else {
        0
    }
    .min(bar_w);
    let bar = "█".repeat(filled) + &"░".repeat(bar_w - filled);
    let audio_tag = if using_audio { "🔊" } else { "🔇" };

    if hud_lines == 2 {
        let line1 = format!(
            " {} [{}] {}/{} · vol {} {} · {} {}×{} (cell {}×{} {}) · {:5.1} fps ({:.0} dec, {} drop)",
            flag,
            bar,
            fmt_time(t),
            fmt_time(duration),
            volume,
            audio_tag,
            backend_name,
            frame_w,
            frame_h,
            cell.w,
            cell.h,
            cell.source.short(),
            fps_shown,
            fps_decoded,
            dropped,
        );
        let line2 = " q=salir · ␣=pausa · ←/→=seek ±5s · ↑/↓=vol ±5".to_string();
        (line1, line2)
    } else if show_stats {
        let line = format!(
            " {} [{}] {}/{} · vol {} {} · {} · {:5.1} fps ({:.0} dec, {} drop) · q=salir",
            flag,
            bar,
            fmt_time(t),
            fmt_time(duration),
            volume,
            audio_tag,
            backend_name,
            fps_shown,
            fps_decoded,
            dropped,
        );
        (line, String::new())
    } else if cols >= 60 {
        let line = format!(
            " {} [{}] {}/{} · vol {} {} · {} · {:5.1} fps · q=salir · ␣=pausa · ←/→=seek",
            flag,
            bar,
            fmt_time(t),
            fmt_time(duration),
            volume,
            audio_tag,
            backend_name,
            fps_shown,
        );
        (line, String::new())
    } else {
        let line = format!(
            " {} {}/{} · {:.0} fps · q",
            flag,
            fmt_time(t),
            fmt_time(duration),
            fps_shown,
        );
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
