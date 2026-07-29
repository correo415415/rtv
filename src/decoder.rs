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
//!   * **Seek a keyframe (estilo mpv por defecto)**: tras
//!     `avformat_seek_file` con max_ts=target, FFmpeg posiciona al
//!     keyframe <= target. El decoder emite DESDE ESE KEYFRAME: el
//!     primer frame post-seek aparece en ~1 frame de decode (salto
//!     de golpe), en vez de decodificar en silencio todo el GOP
//!     hasta el target (que con AV1 4K y GOPs de 3.5 s tardaba
//!     varios segundos). El player alinea el AUDIO al PTS real de
//!     aterrizaje del vídeo (frame.pts del primer frame), así que
//!     no hay desincronía: simplemente se aterriza en el keyframe.
//!
//!   * **Decode multi-hilo**: `thread_count=0` (auto) + frame
//!     threading. Sin esto, dav1d/AV1 4K decodificaba en UN hilo a
//!     ~1.2× realtime, starving al hilo de audio → underruns y
//!     saltos del reloj maestro.
//!
//!   * Ya no marcamos `last_pts_ms` — el reloj lo lleva el player
//!     desde `vidclk.set_pts` en cada frame renderizado.
//!
//! v0.6 — resize robusto y súper dinámico:
//!
//!   * **`target_dims` como `AtomicU64`** (w<<32|h): `resize()` es un
//!     store atómico — sin canal, sin drenar la cola de frames
//!     pre-decodificados (que costaba ~2.5 s de colchón en cada
//!     evento de resize → stalls), y con coalescencia gratis en
//!     tormentas de resize (el decoder siempre lee el ÚLTIMO valor).
//!
//!   * **`struct Scaler`**: encapsula `SwsCtx` + frame RGB de salida
//!     y los reconstruye JUNTOS cuando cualquier dimensión/formato
//!     cambia. `SwsCtx::run()` de ffmpeg-the-third dimensiona el
//!     frame de salida UNA sola vez (cuando está vacío) y después
//!     exige que coincida con el contexto: el código viejo recreaba
//!     el contexto pero REUTILIZABA el frame viejo →
//!     `Error::OutputChanged` en todos los `run()` posteriores → el
//!     decoder dejaba de emitir para siempre ("crashea todo" al
//!     redimensionar). En error el Scaler se resetea a `None` y se
//!     reconstruye limpio en la siguiente llamada — nunca queda roto.

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender, TrySendError};
use ffmpeg_the_third as ffmpeg;
use ffmpeg::format::{input, Pixel};
use ffmpeg::media::Type;
use ffmpeg::software::scaling::{context::Context as SwsCtx, flag::Flags};
use ffmpeg::util::frame::video::Video as VideoFrame;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Empaqueta (w, h) en un u64 para el store/load atómico de resize.
#[inline]
fn pack_dims(w: u32, h: u32) -> u64 {
    (u64::from(w) << 32) | u64::from(h)
}

/// Desempaqueta el u64 atómico a (w, h), con mínimo de 2×2 para no
/// pasarle jamás dims degeneradas (0/1) a sws_scale.
#[inline]
fn unpack_dims(v: u64) -> (u32, u32) {
    (((v >> 32) as u32).max(2), ((v & 0xFFFF_FFFF) as u32).max(2))
}

/// Escalador robusto: mantiene el `SwsCtx` Y el frame RGB de salida
/// como una unidad. Si cambian las dims de entrada (mid-stream) o de
/// salida (resize de la terminal), reconstruye AMBOS a la vez — el
/// frame de salida nuevo (vacío) se dimensiona en el primer `run()`
/// con las dims del contexto nuevo, evitando `Error::OutputChanged`.
/// Si `SwsCtx::get`/`run` fallan, queda en `None` y se reintenta en
/// la siguiente llamada: nunca envenena el loop de decode.
struct Scaler {
    sws: Option<SwsCtx>,
    rgb: VideoFrame,
    in_w: u32,
    in_h: u32,
    in_fmt: Pixel,
    out_w: u32,
    out_h: u32,
}

impl Scaler {
    fn new() -> Self {
        Self {
            sws: None,
            rgb: VideoFrame::empty(),
            in_w: 0,
            in_h: 0,
            in_fmt: Pixel::None,
            out_w: 0,
            out_h: 0,
        }
    }

    /// Escala `frame` a `dst_w`×`dst_h` RGB24. Devuelve `Some(&rgb)`
    /// o `None` si la conversión no fue posible (se reintentará con
    /// un contexto fresco en la próxima llamada).
    fn scale(&mut self, frame: &VideoFrame, dst_w: u32, dst_h: u32) -> Option<&VideoFrame> {
        let iw = frame.width();
        let ih = frame.height();
        let ifmt = frame.format();
        if iw == 0 || ih == 0 || ifmt == Pixel::None {
            return None;
        }
        let dw = dst_w.max(2);
        let dh = dst_h.max(2);

        let needs_rebuild = self.sws.is_none()
            || iw != self.in_w
            || ih != self.in_h
            || ifmt != self.in_fmt
            || dw != self.out_w
            || dh != self.out_h;

        if needs_rebuild {
            match SwsCtx::get(ifmt, iw, ih, Pixel::RGB24, dw, dh, Flags::FAST_BILINEAR) {
                Ok(ctx) => {
                    self.sws = Some(ctx);
                    // CRÍTICO: frame de salida NUEVO junto al contexto
                    // nuevo. Reutilizar el viejo (ya dimensionado a las
                    // dims anteriores) provoca Error::OutputChanged en
                    // todos los run() posteriores.
                    self.rgb = VideoFrame::empty();
                    self.in_w = iw;
                    self.in_h = ih;
                    self.in_fmt = ifmt;
                    self.out_w = dw;
                    self.out_h = dh;
                }
                Err(_) => {
                    self.sws = None;
                    return None;
                }
            }
        }

        match self.sws.as_mut()?.run(frame, &mut self.rgb) {
            Ok(()) => Some(&self.rgb),
            Err(_) => {
                // Estado potencialmente inconsistente → resetear TODO;
                // la próxima llamada reconstruye limpio.
                self.sws = None;
                None
            }
        }
    }
}

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
    /// Dims destino (w<<32|h) que el decoder lee ANTES de escalar
    /// cada frame. `resize()` es un store atómico: coalescencia
    /// automática en tormentas de resize y cero pérdida de eventos.
    target_dims: Arc<AtomicU64>,
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

    /// Cambia las dims destino del escalado. Lock-free e instantáneo:
    /// NO drena la cola de frames pre-decodificados (el colchón de
    /// ~2.5 s se conserva — los frames "viejos" llevan sus propias
    /// dims y el renderer los recorta), y NO usa canal (una tormenta
    /// de resizes colapsa en el último valor automáticamente).
    pub fn resize(&self, w: u32, h: u32) {
        self.target_dims
            .store(pack_dims(w.max(2), h.max(2)), Ordering::Release);
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
    let mut dec_ctx = ffmpeg::codec::context::Context::from_parameters(codec_params)?;
    // Decode multi-hilo (auto = nº de cores). Crítico para AV1/HEVC
    // 4K por software: con 1 hilo el decode no llega a realtime y
    // además roba CPU al hilo de audio (underruns → saltos de reloj).
    {
        let mut tc = ffmpeg::codec::threading::Config::kind(
            ffmpeg::codec::threading::Type::Frame,
        );
        tc.count = 0; // 0 = auto (todos los cores)
        dec_ctx.set_threading(tc);
    }
    let decoder = dec_ctx.decoder().video()?;

    let src_w = decoder.width();
    let src_h = decoder.height();

    // Cola de pre-decode adaptativa por PRESUPUESTO DE MEMORIA
    // (~48 MB): con frames pequeños (ascii/halfblocks) caben 64 →
    // el decoder acumula ~2.5 s de colchón mientras el audio arranca
    // (PulseAudio puede tardar ~2 s en el primer callback) o tras un
    // seek, absorbiendo el warmup del decode AV1/HEVC 4K. Con frames
    // grandes (kitty a 2K) se limita a 4-8 para no comerse la RAM.
    // Con bounded(2) el decoder se quedaba BLOQUEADO durante el hold
    // de arranque y luego nunca recuperaba el déficit (-580 ms fijos).
    let frame_bytes = (dst_w.max(2) as usize) * (dst_h.max(2) as usize) * 3;
    let cap = (48 * 1024 * 1024 / frame_bytes.max(1)).clamp(4, 64);
    let (tx, rx) = bounded::<RgbFrame>(cap);
    let (seek_tx, seek_rx) = unbounded::<SeekReq>();
    let target_dims = Arc::new(AtomicU64::new(pack_dims(dst_w.max(2), dst_h.max(2))));
    let stop = Arc::new(AtomicBool::new(false));
    let eof = Arc::new(AtomicBool::new(false));
    let serial = Arc::new(AtomicI32::new(0));

    let stop_th = stop.clone();
    let eof_th = eof.clone();
    let serial_th = serial.clone();
    let target_dims_th = target_dims.clone();

    let join = thread::Builder::new()
        .name("rtv-decoder".into())
        .spawn(move || {
            let _ = decode_loop(
                path,
                video_stream_index,
                decoder,
                tx,
                seek_rx,
                target_dims_th,
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
        target_dims,
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
    tx: Sender<RgbFrame>,
    seek_rx: Receiver<SeekReq>,
    target_dims: Arc<AtomicU64>,
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

    let mut scaler = Scaler::new();
    let mut frame = VideoFrame::empty();
    // Serial que el hilo cree que está procesando ahora mismo. Cuando
    // recibe un SeekReq, actualiza current_serial.
    let mut current_serial: i32 = 0;
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
            // Seek a keyframe (mpv-style): NO descartamos frames hasta
            // el target. El primer frame decodificado (el keyframe
            // <= target) SE EMITE tal cual → salto de golpe. El player
            // alineará el audio a su PTS real.
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
                    &mut scaler,
                    &mut frame,
                    &target_dims,
                    &tx,
                    &stop,
                    tb_num,
                    tb_den,
                    current_serial,
                    &serial_atomic,
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

            // Leer las dims destino MÁS RECIENTES justo antes de
            // escalar: el resize se aplica al mismísimo siguiente
            // frame (coalescencia atómica; el Scaler reconstruye si
            // cambia cualquier dimensión de entrada o de salida).
            let (dst_w, dst_h) = unpack_dims(target_dims.load(Ordering::Acquire));
            let out = match scaler.scale(&frame, dst_w, dst_h) {
                Some(rgb) => build_rgb_frame(rgb, pts_secs, current_serial),
                None => continue,
            };
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
    scaler: &mut Scaler,
    frame: &mut VideoFrame,
    target_dims: &Arc<AtomicU64>,
    tx: &Sender<RgbFrame>,
    stop: &Arc<AtomicBool>,
    tb_num: f64,
    tb_den: f64,
    current_serial: i32,
    serial_atomic: &Arc<AtomicI32>,
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
        let (dst_w, dst_h) = unpack_dims(target_dims.load(Ordering::Acquire));
        let out = match scaler.scale(frame, dst_w, dst_h) {
            Some(rgb) => build_rgb_frame(rgb, pts_secs, current_serial),
            None => continue,
        };
        if send_with_stop(tx, out, stop, serial_atomic, current_serial).is_err() {
            break;
        }
    }
}
