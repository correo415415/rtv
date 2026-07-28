//! Demuxer + decodificador de vídeo. Corre en un hilo dedicado y
//! empuja frames (RGB24 ya reescalados al tamaño de la terminal)
//! a un canal acotado, que el renderer consume.
//!
//! v0.5 — cambios importantes:
//!
//!   * `Serial` en lugar de `generation`: cada seek incrementa un
//!     contador. Los frames que salgan del pipeline con serial viejo
//!     se descartan silenciosamente.
//!
//!   * **hr-seek con drop-until-target-PTS** (equivalente a mpv
//!     `--hr-seek-framedrop=yes`, on por defecto en mpv): tras
//!     `av_seek_frame` con `AVSEEK_FLAG_BACKWARD`, FFmpeg posiciona
//!     al keyframe anterior al target. El decoder rehidrata desde
//!     ese keyframe hacia adelante, pero DESCARTA en el propio hilo
//!     todos los frames cuyo PTS < target_pts. Así el primer frame
//!     que llega al player ES el frame del target, sin tener que
//!     mostrar los intermedios ni acumular retraso.
//!
//!   * Ya no marcamos `last_pts_ms` — el reloj lo lleva el player
//!     desde `vidclk.set_pts` en cada frame renderizado.

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender, TrySendError};
use ffmpeg_the_third as ffmpeg;
use ffmpeg::format::{input, Pixel};
use ffmpeg::media::Type;
use ffmpeg::software::scaling::{context::Context as SwsCtx, flag::Flags};
use ffmpeg::util::frame::video::Video as VideoFrame;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Un frame RGB24 listo para renderizar. `width` y `height` son
/// en píxeles (no en columnas/filas). PTS en segundos. `serial` es
/// el serial en el que fue producido — el player descarta frames
/// con serial distinto del actual (residuo tras seek).
pub struct RgbFrame {
    pub width: u32,
    pub height: u32,
    pub pts: f64,
    pub serial: i32,
    pub data: Vec<u8>,
}

pub struct DecoderHandle {
    pub rx: Receiver<RgbFrame>,
    pub duration: f64,
    pub source_size: (u32, u32),
    /// Frames por segundo estimados del stream (avg_frame_rate).
    pub fps: f64,
    pub eof: Arc<AtomicBool>,
    seek_tx: Sender<SeekReq>,
    resize_tx: Sender<(u32, u32)>,
    /// Serial del decoder de vídeo. Se incrementa en cada seek.
    /// El player lo lee para saber qué frames son válidos.
    pub serial: Arc<AtomicI32>,
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

pub struct SeekReq {
    pub target_secs: f64,
    pub serial: i32,
}

impl DecoderHandle {
    /// Encola un seek al hilo decoder. Devuelve el nuevo serial.
    /// El caller (player) DEBE también bumpear el serial del reloj
    /// ANTES de llamar esto (o al mismo tiempo). Ver `MasterClock::set`.
    pub fn seek(&self, target_secs: f64) -> i32 {
        let new_serial = self.serial.fetch_add(1, Ordering::AcqRel) + 1;
        // Drenar frames antiguos del canal para bajar latencia.
        while self.rx.try_recv().is_ok() {}
        // Canal sin límite + send: un try_send podría descartar el
        // último seek de una ráfaga y dejar vídeo y audio en targets
        // distintos.
        let _ = self.seek_tx.send(SeekReq {
            target_secs,
            serial: new_serial,
        });
        new_serial
    }

    pub fn current_serial(&self) -> i32 {
        self.serial.load(Ordering::Acquire)
    }

    pub fn resize(&self, w: u32, h: u32) {
        while self.rx.try_recv().is_ok() {}
        let _ = self.resize_tx.try_send((w, h));
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        while self.rx.try_recv().is_ok() {}
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for DecoderHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn spawn<P: AsRef<Path>>(path: P, dst_w: u32, dst_h: u32) -> Result<DecoderHandle> {
    let path = path.as_ref().to_owned();
    let ictx = input(&path).with_context(|| format!("abriendo {:?}", path))?;

    let stream = ictx
        .streams()
        .best(Type::Video)
        .ok_or_else(|| anyhow!("no se encontró stream de vídeo"))?;
    let video_stream_index = stream.index();
    let time_base = stream.time_base();
    let duration = if stream.duration() > 0 {
        stream.duration() as f64 * f64::from(time_base.numerator())
            / f64::from(time_base.denominator())
    } else {
        ictx.duration() as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE)
    };

    // fps medio del stream, para el fallback de frame-duration del player.
    let afr = stream.avg_frame_rate();
    let fps = if afr.numerator() > 0 && afr.denominator() > 0 {
        f64::from(afr.numerator()) / f64::from(afr.denominator())
    } else {
        30.0
    };

    let codec_params = stream.parameters();
    let dec_ctx = ffmpeg::codec::context::Context::from_parameters(codec_params)?;
    let decoder = dec_ctx.decoder().video()?;

    let src_w = decoder.width();
    let src_h = decoder.height();
    let src_fmt = decoder.format();

    let (tx, rx) = bounded::<RgbFrame>(2);
    let (seek_tx, seek_rx) = unbounded::<SeekReq>();
    let (resize_tx, resize_rx) = bounded::<(u32, u32)>(4);
    let stop = Arc::new(AtomicBool::new(false));
    let eof = Arc::new(AtomicBool::new(false));
    let serial = Arc::new(AtomicI32::new(0));

    let stop_th = stop.clone();
    let eof_th = eof.clone();
    let serial_th = serial.clone();

    let join = thread::Builder::new()
        .name("rtv-decoder".into())
        .spawn(move || {
            let _ = decode_loop(
                path,
                video_stream_index,
                decoder,
                src_w,
                src_h,
                src_fmt,
                dst_w,
                dst_h,
                tx,
                seek_rx,
                resize_rx,
                stop_th,
                eof_th.clone(),
                serial_th,
            );
            eof_th.store(true, Ordering::Relaxed);
        })?;

    Ok(DecoderHandle {
        rx,
        duration,
        source_size: (src_w, src_h),
        fps,
        eof,
        seek_tx,
        resize_tx,
        serial,
        stop,
        join: Some(join),
    })
}

#[allow(clippy::too_many_arguments)]
fn decode_loop(
    path: std::path::PathBuf,
    video_idx: usize,
    mut decoder: ffmpeg::decoder::Video,
    src_w: u32,
    src_h: u32,
    src_fmt: Pixel,
    dst_w0: u32,
    dst_h0: u32,
    tx: Sender<RgbFrame>,
    seek_rx: Receiver<SeekReq>,
    resize_rx: Receiver<(u32, u32)>,
    stop: Arc<AtomicBool>,
    eof: Arc<AtomicBool>,
    serial_atomic: Arc<AtomicI32>,
) -> Result<()> {
    let mut ictx = input(&path)?;
    let time_base = ictx
        .stream(video_idx)
        .ok_or_else(|| anyhow!("stream desapareció"))?
        .time_base();
    let tb_num = f64::from(time_base.numerator());
    let tb_den = f64::from(time_base.denominator());

    let mut dst_w = dst_w0.max(2);
    let mut dst_h = dst_h0.max(2);

    let mut sws = match SwsCtx::get(
        src_fmt,
        src_w,
        src_h,
        Pixel::RGB24,
        dst_w,
        dst_h,
        Flags::FAST_BILINEAR,
    ) {
        Ok(c) => c,
        Err(_) => {
            eof.store(true, Ordering::Relaxed);
            return Ok(());
        }
    };

    let mut frame = VideoFrame::empty();
    let mut rgb = VideoFrame::empty();
    // Serial que el hilo cree que está procesando ahora mismo. Cuando
    // recibe un SeekReq, actualiza current_serial + target_pts.
    let mut current_serial: i32 = 0;
    // Si es Some(target), estamos en modo "drop hasta llegar a este PTS".
    // Empieza a Some(target) tras un seek y pasa a None cuando el primer
    // frame con pts>=target se emite (o al recibir un frame sin PTS válido).
    let mut drop_until_pts: Option<f64> = None;
    // ¿Hemos llegado a EOF? En vez de matar el hilo, lo "aparcamos"
    // esperando un seek (necesario para seeks tras el final y --loop).
    let mut at_eof = false;

    'outer: loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // Procesar seeks pendientes: nos quedamos SOLO con el último,
        // porque cada uno es absoluto.
        let mut latest_seek: Option<SeekReq> = None;
        while let Ok(req) = seek_rx.try_recv() {
            latest_seek = Some(req);
        }
        if let Some(req) = latest_seek {
            current_serial = req.serial;
            // IMPORTANTE (unidades): `Input::seek` llama a
            // `avformat_seek_file(ctx, -1, min, ts, max, 0)` con
            // stream_index = -1, y en ese caso FFmpeg interpreta los
            // timestamps en AV_TIME_BASE (microsegundos), NO en el
            // time_base del stream. Antes se pasaban ticks del stream
            // (p.ej. 1/15360) → el demuxer aterrizaba decénas de
            // segundos ANTES del target y el drop-until-target tenía
            // que decodificar minutos de vídeo → seeks lentísimos y
            // A/V desincronizado. Con rango `..ts` el demuxer elige el
            // keyframe óptimo <= target (equivalente a
            // AVSEEK_FLAG_BACKWARD para hr-seek).
            // Rango INCLUSIVO `..=ts`: con `..ts` (exclusivo) el
            // max_ts quedaba en ts-1 < ts y avformat_seek_file
            // devolvía EINVAL sin mover el demuxer — el "seek" sólo
            // funcionaba hacia delante gracias al drop-until-target
            // (decodificando segundos de más) y hacia atrás NO
            // funcionaba en absoluto.
            let ts_target =
                (req.target_secs * f64::from(ffmpeg::ffi::AV_TIME_BASE)) as i64;
            let _ = ictx.seek(ts_target, ..=ts_target);
            decoder.flush();
            drop_until_pts = Some(req.target_secs);
            at_eof = false;
            eof.store(false, Ordering::Relaxed);
            // No emitimos nada más de la iteración vieja.
            continue;
        }

        // Aparcados en EOF: dormir y volver a mirar seeks/stop.
        if at_eof {
            thread::sleep(Duration::from_millis(25));
            continue;
        }

        while let Ok((w, h)) = resize_rx.try_recv() {
            dst_w = w.max(2);
            dst_h = h.max(2);
            if let Ok(new_sws) = SwsCtx::get(
                src_fmt,
                src_w,
                src_h,
                Pixel::RGB24,
                dst_w,
                dst_h,
                Flags::FAST_BILINEAR,
            ) {
                sws = new_sws;
            }
        }

        let pkt = match ictx.packets().next() {
            Some(Ok((s, p))) => {
                if s.index() != video_idx {
                    continue;
                }
                p
            }
            Some(Err(_)) => continue,
            None => {
                let _ = decoder.send_eof();
                drain(
                    &mut decoder,
                    &mut sws,
                    &mut frame,
                    &mut rgb,
                    &tx,
                    &stop,
                    tb_num,
                    tb_den,
                    current_serial,
                    &serial_atomic,
                    &mut drop_until_pts,
                );
                eof.store(true, Ordering::Relaxed);
                // NO salimos del hilo: nos aparcamos esperando un
                // posible seek hacia atrás o el restart de --loop.
                at_eof = true;
                continue 'outer;
            }
        };

        let _ = decoder.send_packet(&pkt);

        while decoder.receive_frame(&mut frame).is_ok() {
            if stop.load(Ordering::Relaxed) {
                break 'outer;
            }
            // Si mientras decodificábamos ha llegado OTRO seek con serial
            // más nuevo, descartamos: son frames del segmento intermedio.
            if serial_atomic.load(Ordering::Acquire) != current_serial {
                continue;
            }

            let pts_ticks = frame.pts().unwrap_or(0);
            let pts_secs = pts_ticks as f64 * tb_num / tb_den;

            // hr-seek framedrop: si aún no llegamos al target, drop.
            // Damos margen de 1 frame (~33 ms a 30fps): si sobrepasamos
            // el target por poco, mostramos ese frame, que es lo más
            // cercano posible al punto solicitado.
            if let Some(target) = drop_until_pts {
                if pts_secs + 0.020 < target {
                    // Aún no hemos llegado. Descartamos silenciosamente.
                    continue;
                }
                // Llegamos: desactivamos el drop y mostramos éste.
                drop_until_pts = None;
            }

            if frame.width() != src_w || frame.height() != src_h {
                if let Ok(new_sws) = SwsCtx::get(
                    frame.format(),
                    frame.width(),
                    frame.height(),
                    Pixel::RGB24,
                    dst_w,
                    dst_h,
                    Flags::FAST_BILINEAR,
                ) {
                    sws = new_sws;
                }
            }
            if sws.run(&frame, &mut rgb).is_err() {
                continue;
            }

            let out = build_rgb_frame(&rgb, pts_secs, current_serial);
            if send_with_stop(&tx, out, &stop, &serial_atomic, current_serial).is_err() {
                break 'outer;
            }
        }
    }

    Ok(())
}

fn build_rgb_frame(rgb: &VideoFrame, pts: f64, serial: i32) -> RgbFrame {
    let stride = rgb.stride(0);
    let w = rgb.width() as usize;
    let h = rgb.height() as usize;
    let expected = w * h * 3;
    let mut buf = vec![0u8; expected];
    let src = rgb.data(0);
    for y in 0..h {
        let s = y * stride;
        let d = y * w * 3;
        let end_s = s + w * 3;
        if end_s > src.len() || d + w * 3 > buf.len() {
            break;
        }
        buf[d..d + w * 3].copy_from_slice(&src[s..end_s]);
    }
    RgbFrame {
        width: rgb.width(),
        height: rgb.height(),
        pts,
        serial,
        data: buf,
    }
}

/// Envía un frame respetando `stop` y aborta si el serial cambia
/// mientras esperamos hueco en el canal (ese frame ya sería residuo).
fn send_with_stop(
    tx: &Sender<RgbFrame>,
    mut frame: RgbFrame,
    stop: &Arc<AtomicBool>,
    serial_atomic: &Arc<AtomicI32>,
    my_serial: i32,
) -> std::result::Result<(), ()> {
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(());
        }
        if serial_atomic.load(Ordering::Acquire) != my_serial {
            // Nuestro serial ya está obsoleto. Descartamos y volvemos
            // al loop principal (Ok — no queremos abortar todo el hilo).
            return Ok(());
        }
        match tx.try_send(frame) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(f)) => {
                frame = f;
                thread::sleep(Duration::from_millis(2));
            }
            Err(TrySendError::Disconnected(_)) => return Err(()),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn drain(
    decoder: &mut ffmpeg::decoder::Video,
    sws: &mut SwsCtx,
    frame: &mut VideoFrame,
    rgb: &mut VideoFrame,
    tx: &Sender<RgbFrame>,
    stop: &Arc<AtomicBool>,
    tb_num: f64,
    tb_den: f64,
    current_serial: i32,
    serial_atomic: &Arc<AtomicI32>,
    drop_until_pts: &mut Option<f64>,
) {
    while decoder.receive_frame(frame).is_ok() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if serial_atomic.load(Ordering::Acquire) != current_serial {
            continue;
        }
        let pts_ticks = frame.pts().unwrap_or(0);
        let pts_secs = pts_ticks as f64 * tb_num / tb_den;
        if let Some(target) = *drop_until_pts {
            if pts_secs + 0.020 < target {
                continue;
            }
            *drop_until_pts = None;
        }
        if sws.run(frame, rgb).is_err() {
            continue;
        }
        let out = build_rgb_frame(rgb, pts_secs, current_serial);
        if send_with_stop(tx, out, stop, serial_atomic, current_serial).is_err() {
            break;
        }
    }
}
