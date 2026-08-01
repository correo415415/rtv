//! audio.rs — v0.5: audio clock estilo ffplay.
//!
//! Cambios frente a v0.4:
//!   * El callback de cpal ya NO llama a `advance()` sumando µs
//!     muestra a muestra. En su lugar, cada vez que consume un
//!     bloque de muestras del ring, hace un ÚNICO `audclk.set_pts()`
//!     con el PTS de la última muestra emitida MENOS el
//!     `playback_delay` estimado (samples aún en el buffer del
//!     driver + el bloque que acaba de emitir). Este `pts` refleja
//!     lo que el usuario está oyendo en tiempo real.
//!   * El decoder de audio etiqueta cada `AudioChunk` con
//!     `(serial, first_pts)` donde `first_pts` es el PTS del primer
//!     sample. El callback lo usa para calcular el PTS running.
//!   * Serial: al hacer seek, el player bumpea `audclk.serial` ANTES
//!     de encolar el seek al decoder. Los chunks con serial viejo
//!     que aún estén en el ring se silencian y NO actualizan el reloj.
//!   * Pause: `cpal::Stream::pause()` al nivel OS (WASAPI/ALSA/CoreAudio)
//!     como antes. El callback rellena con ceros como defensa 2ª.

use anyhow::{anyhow, Context, Result};
#[cfg(feature = "cpal-audio")]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender, TrySendError};
use ffmpeg_the_third as ffmpeg;
use ffmpeg::format::sample::Type as SampleType;
use ffmpeg::format::Sample as SampleFormat;
use ffmpeg::media::Type as MediaType;
use ffmpeg::software::resampling::context::Context as SwrCtx;
use ffmpeg::util::frame::audio::Audio as AudioFrame;
use ffmpeg::{ChannelLayout, ChannelLayoutMask};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::audio_backend::AudioChunk;
#[cfg(feature = "cpal-audio")]
use crate::audio_backend::SinkFeeder;
use crate::clock::FfClock;

/// Mensajes de control al hilo audio-decoder.
enum AudioMsg {
    /// Seek: target + serial nuevo.
    Seek { target_secs: f64, serial: i32 },
    /// Cambio de pista en runtime: stream del contenedor + punto de
    /// reproducción actual. El hilo reabre el decoder sobre el stream
    /// nuevo, recrea el resampler y aterriza en `at_secs` con recorte
    /// sample-accurate — mismo mecanismo que un seek.
    Switch { stream_index: usize, at_secs: f64, serial: i32 },
}

pub struct AudioHandle {
    stop: Arc<AtomicBool>,
    volume: Arc<AtomicU8>,
    msg_tx: Sender<AudioMsg>,
    pub clock: Arc<FfClock>,
    pub has_audio: bool,
    #[allow(dead_code)] // API informativa (diagnóstico/futuros usos)
    pub sample_rate: u32,
    #[allow(dead_code)]
    pub channels: u16,
    /// Índice del stream de audio con el que ARRANCÓ el pipeline
    /// (best o el pedido por --aid/--alang). Informativo: el player
    /// lo usa para posicionar el ciclado de pistas.
    pub track_index: Option<usize>,
    decoder_join: Option<thread::JoinHandle<()>>,
    sink: Option<SinkRuntime>,
    /// Backend activo ("cpal" | "pulse" | "none") — verbose/diagnóstico.
    pub backend_name: &'static str,
}

/// Runtime del sink de salida activo.
enum SinkRuntime {
    #[cfg(feature = "cpal-audio")]
    Cpal(cpal::Stream),
    #[cfg(feature = "pulse")]
    Pulse(crate::audio_backend::pulse::PulseRuntime),
}

impl AudioHandle {
    pub fn set_volume(&self, v: i32) {
        let clamped = v.clamp(0, 200) as u8;
        self.volume.store(clamped, Ordering::Relaxed);
    }

    /// Encola un seek al thread audio-decoder. IMPORTANTE: el orden
    /// desde el player es:
    ///   (1) `master.set(t)` — bumpea audclk.serial + vidclk.serial.
    ///   (2) `audio.seek(t)` — el hilo audio-decoder flushea y salta.
    ///   (3) `decoder.seek(t)` — el hilo video-decoder flushea y salta.
    /// Como el serial ya está bumpeado en (1), cualquier chunk viejo
    /// que salga del ring durante (2)/(3) es silenciado por el callback.
    pub fn seek(&self, target_secs: f64) {
        // El serial de referencia es el del RELOJ, que el player acaba
        // de bumpear con `master.set(target)` ANTES de llamarnos. Así
        // el pipeline de audio comparte exactamente el mismo serial que
        // el reloj: los chunks pre-seek (serial viejo) se descartan en
        // el callback y los post-seek (serial nuevo) anclan el reloj.
        let serial = self.clock.current_serial();
        // Canal sin límite: un try_send sobre canal acotado podía
        // DESCARTAR el último seek de una ráfaga (→→→←←) y dejar el
        // audio aterrizado en un target distinto del vídeo → offset
        // A/V constante de ±5 s tras la ráfaga.
        let _ = self.msg_tx.send(AudioMsg::Seek {
            target_secs,
            serial,
        });
    }

    /// Cambia la pista de audio EN CALIENTE. El player debe haber
    /// bumpeado los seriales (`master.set(now)`) ANTES de llamar —
    /// igual que con `seek` — para que los chunks de la pista vieja
    /// que sigan en el ring se silencien y no toquen el reloj.
    pub fn switch_track(&self, stream_index: usize, at_secs: f64) {
        let serial = self.clock.current_serial();
        let _ = self.msg_tx.send(AudioMsg::Switch {
            stream_index,
            at_secs,
            serial,
        });
    }

    pub fn pause_stream(&self) {
        match self.sink.as_ref() {
            #[cfg(feature = "cpal-audio")]
            Some(SinkRuntime::Cpal(s)) => {
                if let Err(e) = s.pause() {
                    eprintln_verbose(&format!("cpal pause falló: {e}"));
                }
            }
            // pulse: la API simple no tiene pausa nativa — el feeder
            // emite silencio mientras clock.paused está activo (misma
            // defensa 2ª que ya tenía el callback de cpal).
            _ => {}
        }
    }
    pub fn play_stream(&self) {
        match self.sink.as_ref() {
            #[cfg(feature = "cpal-audio")]
            Some(SinkRuntime::Cpal(s)) => {
                if let Err(e) = s.play() {
                    eprintln_verbose(&format!("cpal play falló: {e}"));
                }
            }
            _ => {}
        }
    }

    /// Parada cooperativa con join ACOTADO (500 ms), espejo del fix de
    /// `DecoderHandle::stop`. El hilo audio-decoder puede estar:
    ///   * dormido en el backoff de `send_with_stop` (sale al ver el
    ///     flag en <4 ms), o
    ///   * bloqueado dentro de FFmpeg (send_packet/receive_frame) o de
    ///     un canal lleno cuyo consumidor (callback cpal) ya no drena
    ///     porque el stream fue pausado/parado — irrecuperable por
    ///     flag. En ese caso se le suelta (detach): el proceso está
    ///     saliendo y el SO recoge el hilo. Nunca colgamos la salida.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // El writer pulse bloquea en pa_simple_write como mucho
        // ~tlength (100 ms): join acotado propio dentro de stop().
        #[cfg(feature = "pulse")]
        if let Some(SinkRuntime::Pulse(rt)) = self.sink.as_mut() {
            rt.stop();
        }
        if let Some(j) = self.decoder_join.take() {
            let deadline = std::time::Instant::now() + Duration::from_millis(500);
            loop {
                if j.is_finished() {
                    let _ = j.join();
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    drop(j); // detach de último recurso
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

impl Drop for AudioHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Preferencia de backend de salida (--audio-backend).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BackendPref {
    /// Termux/Android: pulse→cpal; resto: cpal→pulse.
    Auto,
    Cpal,
    Pulse,
    /// Sin audio (equivale a --no-audio).
    NoAudio,
}

impl BackendPref {
    pub fn parse(s: &str) -> Result<BackendPref> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(BackendPref::Auto),
            "cpal" => Ok(BackendPref::Cpal),
            "pulse" | "pulseaudio" => Ok(BackendPref::Pulse),
            "none" | "off" => Ok(BackendPref::NoAudio),
            other => Err(anyhow!(
                "--audio-backend inválido: {other:?} (valores: auto|cpal|pulse|none)"
            )),
        }
    }
}

/// ¿Estamos corriendo dentro de Termux? (app Android, prefix propio).
fn is_termux() -> bool {
    std::env::var_os("TERMUX_VERSION").is_some()
        || std::env::var("PREFIX")
            .map(|p| p.contains("com.termux"))
            .unwrap_or(false)
}

/// Plan de sink elegido (conexión abierta, aún sin arrancar).
enum SinkPlan {
    #[cfg(feature = "cpal-audio")]
    Cpal(cpal::Device, cpal::StreamConfig),
    #[cfg(feature = "pulse")]
    Pulse(crate::audio_backend::pulse::PulseSink),
}

fn try_cpal_plan(out_channels: u16) -> Option<(SinkPlan, u32)> {
    #[cfg(feature = "cpal-audio")]
    {
        let host = cpal::default_host();
        let device = host.default_output_device()?;
        let supported = match device.default_output_config() {
            Ok(s) => s,
            Err(e) => {
                eprintln_verbose(&format!("cpal default_output_config falló: {e}"));
                return None;
            }
        };
        let rate = supported.sample_rate().0;
        let config = cpal::StreamConfig {
            channels: out_channels,
            sample_rate: cpal::SampleRate(rate),
            buffer_size: cpal::BufferSize::Default,
        };
        return Some((SinkPlan::Cpal(device, config), rate));
    }
    #[cfg(not(feature = "cpal-audio"))]
    {
        let _ = out_channels;
        None
    }
}

fn try_pulse_plan(out_channels: u16) -> Option<(SinkPlan, u32)> {
    #[cfg(feature = "pulse")]
    {
        // 48 kHz: nativo de PulseAudio en Android/Termux y estándar
        // de facto; swresample normaliza cualquier pista a esto.
        match crate::audio_backend::pulse::PulseSink::try_open(48000, out_channels) {
            Ok(s) => return Some((SinkPlan::Pulse(s), 48000)),
            Err(e) => {
                eprintln_verbose(&format!("pulse no disponible: {e}"));
                return None;
            }
        }
    }
    #[cfg(not(feature = "pulse"))]
    {
        let _ = out_channels;
        None
    }
}

/// `start_track`: índice del stream de audio del contenedor con el
/// que arrancar (de `--aid`/`--alang`); `None` = pista "best" de
/// FFmpeg. Si el índice no es un stream de audio válido se cae a
/// "best" silenciosamente.
pub fn spawn<P: AsRef<Path>>(
    path: P,
    clock: Arc<FfClock>,
    start_track: Option<usize>,
    backend: BackendPref,
) -> Result<AudioHandle> {
    let path = path.as_ref().to_owned();

    let ictx = crate::source::open(&path).with_context(|| format!("abriendo {:?}", path))?;
    let requested = start_track.filter(|&i| {
        ictx.stream(i)
            .map(|s| s.parameters().medium() == MediaType::Audio)
            .unwrap_or(false)
    });
    let audio_idx = match requested.or_else(|| ictx.streams().best(MediaType::Audio).map(|s| s.index()))
    {
        Some(i) => i,
        None => return Ok(no_audio(clock)),
    };
    drop(ictx);

    // --- Selección del backend de salida ---
    // Una preferencia explícita NO cae a otro backend (fallo → sin
    // audio, con el motivo en --verbose). `Auto` prueba en orden.
    let out_channels: u16 = 2;
    let plan = match backend {
        BackendPref::NoAudio => None,
        BackendPref::Cpal => try_cpal_plan(out_channels),
        BackendPref::Pulse => try_pulse_plan(out_channels),
        BackendPref::Auto => {
            if is_termux() {
                // cpal (AAudio/NDK) no funciona en un proceso de
                // consola Termux: pulse primero.
                try_pulse_plan(out_channels).or_else(|| try_cpal_plan(out_channels))
            } else {
                try_cpal_plan(out_channels).or_else(|| try_pulse_plan(out_channels))
            }
        }
    };
    let Some((plan, out_sample_rate)) = plan else {
        return Ok(no_audio(clock));
    };

    let (samples_tx, samples_rx) = bounded::<AudioChunk>(64);
    let samples_rx_for_drain = samples_rx.clone();

    let stop = Arc::new(AtomicBool::new(false));
    let volume = Arc::new(AtomicU8::new(100));
    let (msg_tx, msg_rx) = unbounded::<AudioMsg>();

    let decoder_join = {
        let stop = stop.clone();
        let path2 = path.clone();
        thread::Builder::new()
            .name("rtv-audio-decoder".into())
            .spawn(move || {
                let _ = audio_decode_loop(
                    path2,
                    audio_idx,
                    out_sample_rate,
                    out_channels,
                    samples_tx,
                    samples_rx_for_drain,
                    msg_rx,
                    stop,
                );
            })?
    };

    // --- Arranque del sink elegido ---
    // Toda la lógica del reloj de audio (descarte por serial, EMA de
    // latencia, limitador de tasa) vive en SinkFeeder (audio_backend.rs)
    // y es IDÉNTICA para ambos backends.
    let (sink, backend_name): (Option<SinkRuntime>, &'static str) = match plan {
        #[cfg(feature = "cpal-audio")]
        SinkPlan::Cpal(device, stream_config) => {
            let mut feeder = SinkFeeder::new(
                stop.clone(),
                clock.clone(),
                volume.clone(),
                samples_rx,
                out_sample_rate,
                out_channels,
            );
            let build = device.build_output_stream(
                &stream_config,
                move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
                    // cpal reporta: playback = cuándo el PRIMER frame
                    // de `out` sale por el DAC; callback = ahora.
                    let ts = info.timestamp();
                    let reported_delay = ts
                        .playback
                        .duration_since(&ts.callback)
                        .map(|d| d.as_secs_f64())
                        .unwrap_or(0.0);
                    feeder.fill(out, reported_delay);
                },
                |err| eprintln_verbose(&format!("cpal stream error: {err}")),
                None,
            );
            match build {
                Ok(s) => match s.play() {
                    Ok(()) => (Some(SinkRuntime::Cpal(s)), "cpal"),
                    Err(e) => {
                        eprintln_verbose(&format!("cpal play falló: {e}"));
                        (None, "none")
                    }
                },
                Err(e) => {
                    eprintln_verbose(&format!("cpal build_output_stream falló: {e}"));
                    (None, "none")
                }
            }
        }
        #[cfg(feature = "pulse")]
        SinkPlan::Pulse(psink) => {
            let rt = psink.start(
                stop.clone(),
                clock.clone(),
                volume.clone(),
                samples_rx,
            );
            (Some(SinkRuntime::Pulse(rt)), "pulse")
        }
    };

    let has_audio = sink.is_some();

    Ok(AudioHandle {
        stop,
        volume,
        msg_tx,
        clock,
        has_audio,
        sample_rate: out_sample_rate,
        channels: out_channels,
        track_index: Some(audio_idx),
        decoder_join: Some(decoder_join),
        sink,
        backend_name,
    })
}

fn no_audio(clock: Arc<FfClock>) -> AudioHandle {
    let stop = Arc::new(AtomicBool::new(true));
    let (msg_tx, _msg_rx) = bounded::<AudioMsg>(1);
    AudioHandle {
        stop,
        volume: Arc::new(AtomicU8::new(100)),
        msg_tx,
        clock,
        has_audio: false,
        sample_rate: 48000,
        channels: 2,
        track_index: None,
        decoder_join: None,
        sink: None,
        backend_name: "none",
    }
}

/// Estado del decode de UNA pista de audio (decoder + resampler +
/// parámetros de entrada). Se reconstruye entero al cambiar de pista
/// en runtime: cada pista puede tener codec, sample_rate y layout
/// distintos — el resampler siempre normaliza al formato FIJO del
/// sink cpal (f32 interleaved, out_sample_rate, out_channels), así
/// que el stream de salida NO se toca.
struct TrackState {
    decoder: ffmpeg::decoder::Audio,
    tb_num: f64,
    tb_den: f64,
    in_sample_rate: u32,
    in_ch_layout_raw: ffmpeg::sys::AVChannelLayout,
    in_format: SampleFormat,
    swr: SwrCtx,
}

fn mk_out_layout(out_channels: u16) -> ChannelLayout<'static> {
    if out_channels == 1 {
        ChannelLayout::MONO
    } else {
        ChannelLayout::STEREO
    }
}

fn open_track(
    ictx: &ffmpeg::format::context::Input,
    stream_idx: usize,
    out_channels: u16,
    out_sample_rate: u32,
) -> Result<TrackState> {
    let stream = ictx
        .stream(stream_idx)
        .ok_or_else(|| anyhow!("stream {stream_idx} no existe"))?;
    if stream.parameters().medium() != MediaType::Audio {
        return Err(anyhow!("stream {stream_idx} no es de audio"));
    }
    // Copia DUEÑA de los parámetros (desligada del borrow de ictx).
    let codec_params: ffmpeg::codec::Parameters = {
        let src_ref = stream.parameters();
        let mut owned = ffmpeg::codec::Parameters::new();
        unsafe {
            ffmpeg::sys::avcodec_parameters_copy(owned.as_mut_ptr(), src_ref.as_ptr());
        }
        owned
    };
    let tb = stream.time_base();
    let dec_ctx = ffmpeg::codec::context::Context::from_parameters(codec_params)?;
    let decoder = dec_ctx.decoder().audio()?;
    let in_sample_rate = decoder.rate();
    let in_ch_layout_raw: ffmpeg::sys::AVChannelLayout =
        decoder.ch_layout().to_owned().into_owned();
    let in_format: SampleFormat = decoder.format();
    let swr = SwrCtx::get2(
        in_format,
        ChannelLayout::from(&in_ch_layout_raw),
        in_sample_rate,
        SampleFormat::F32(SampleType::Packed),
        mk_out_layout(out_channels),
        out_sample_rate,
    )
    .map_err(|e| anyhow!("swresample init: {e}"))?;
    Ok(TrackState {
        decoder,
        tb_num: f64::from(tb.numerator()),
        tb_den: f64::from(tb.denominator()),
        in_sample_rate,
        in_ch_layout_raw,
        in_format,
        swr,
    })
}

#[allow(clippy::too_many_arguments)]
fn audio_decode_loop(
    path: PathBuf,
    audio_idx: usize,
    out_sample_rate: u32,
    out_channels: u16,
    samples_tx: Sender<AudioChunk>,
    samples_rx_for_drain: Receiver<AudioChunk>,
    msg_rx: Receiver<AudioMsg>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let mut ictx = crate::source::open(&path)?;
    // Índice de la pista ACTIVA (cambia con AudioMsg::Switch).
    let mut active_idx = audio_idx;
    let mut ts = open_track(&ictx, active_idx, out_channels, out_sample_rate)?;

    // Log de depuración del hilo decoder (RTV_AUDIO_DEC_DEBUG=/ruta).
    let mut dec_log: Option<std::io::BufWriter<std::fs::File>> =
        std::env::var("RTV_AUDIO_DEC_DEBUG").ok().and_then(|p| {
            std::fs::File::create(p).ok().map(std::io::BufWriter::new)
        });
    let dec_origin = std::time::Instant::now();

    let mut in_frame = AudioFrame::empty();

    // PTS running: lo actualizamos cada vez que decoder produce un
    // frame con PTS válido; los subsecuentes suman n_samples/rate.
    let mut running_pts: f64 = 0.0;
    // Serial que este hilo está procesando ahora mismo. Los chunks se
    // etiquetan con ÉSTE (no con el del reloj, que puede haber sido
    // bumpeado por el player antes de que procesemos el seek).
    let mut current_serial: i32 = 0;
    // Tras un seek: recortar samples hasta llegar EXACTAMENTE al
    // target. FFmpeg posiciona el demuxer en el paquete anterior al
    // target, así que sin este recorte el audio empezaba ANTES del
    // punto pedido (hasta ~1 s con AAC) → desincronía tras cada seek.
    let mut trim_until_pts: Option<f64> = None;
    // ¿EOF? Aparcamos el hilo esperando seek/stop (para seeks hacia
    // atrás después de terminar y para --loop).
    let mut at_eof = false;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // Procesar mensajes pendientes ANTES de leer paquete siguiente.
        // Coalescencia: de una ráfaga solo importa el ÚLTIMO destino
        // (target, pista) — pero un Switch intermedio SÍ cambia la
        // pista aunque después llegue un Seek.
        let mut land_at: Option<(f64, i32)> = None; // (target, serial)
        let mut switch_to: Option<usize> = None;
        while let Ok(msg) = msg_rx.try_recv() {
            match msg {
                AudioMsg::Seek { target_secs, serial } => {
                    land_at = Some((target_secs, serial));
                }
                AudioMsg::Switch { stream_index, at_secs, serial } => {
                    switch_to = Some(stream_index);
                    land_at = Some((at_secs, serial));
                }
            }
        }
        // Cambio de pista: reabrir decoder+resampler sobre el stream
        // nuevo. Si falla (índice inválido, codec sin decoder…) se
        // conserva la pista actual — el aterrizaje del land_at sigue
        // siendo válido y el audio continúa sin cortarse.
        if let Some(idx) = switch_to {
            if idx != active_idx {
                match open_track(&ictx, idx, out_channels, out_sample_rate) {
                    Ok(new_ts) => {
                        ts = new_ts;
                        active_idx = idx;
                        if let Some(log) = dec_log.as_mut() {
                            use std::io::Write as _;
                            let _ = writeln!(
                                log,
                                "{:.4} SWITCH stream={}",
                                dec_origin.elapsed().as_secs_f64(),
                                idx
                            );
                            let _ = log.flush();
                        }
                    }
                    Err(e) => {
                        eprintln_verbose(&format!("switch_track({idx}) falló: {e}"));
                    }
                }
            }
        }
        if let Some((target, serial)) = land_at {
            current_serial = serial;
            // Unidades: `Input::seek` → avformat_seek_file con
            // stream_index=-1 → timestamps en AV_TIME_BASE (µs).
            // OJO con el rango: `..ts` (exclusivo) produce
            // max_ts = ts-1 < ts y avformat_seek_file devuelve EINVAL
            // SIN MOVER el demuxer → los seeks hacia atrás dejaban el
            // audio donde estaba (los hacia delante los enmascaraba el
            // trim). Con `..=ts` es (INT64_MIN, ts, ts) = keyframe<=ts,
            // exactamente como ffplay.
            let seek_ts = (target * f64::from(ffmpeg::ffi::AV_TIME_BASE)) as i64;
            let _ = ictx.seek(seek_ts, ..=seek_ts);
            ts.decoder.flush();
            // Recrear el resampler: su FIFO interno puede contener
            // samples pre-seek que saldrían etiquetados con el PTS
            // nuevo (audio viejo sonando tras el salto). Recrearlo es
            // barato y garantiza estado limpio.
            if let Ok(new_swr) = SwrCtx::get2(
                ts.in_format,
                ChannelLayout::from(&ts.in_ch_layout_raw),
                ts.in_sample_rate,
                SampleFormat::F32(SampleType::Packed),
                mk_out_layout(out_channels),
                out_sample_rate,
            ) {
                ts.swr = new_swr;
            }
            // Vaciar ring: aunque el callback descartaría por serial,
            // preferimos que llegue audio fresco cuanto antes.
            while samples_rx_for_drain.try_recv().is_ok() {}
            running_pts = target;
            trim_until_pts = Some(target);
            at_eof = false;
            if let Some(log) = dec_log.as_mut() {
                use std::io::Write as _;
                let _ = writeln!(
                    log,
                    "{:.4} SEEK target={:.3} serial={}",
                    dec_origin.elapsed().as_secs_f64(),
                    target,
                    current_serial
                );
                let _ = log.flush();
            }
        }

        if at_eof {
            thread::sleep(Duration::from_millis(25));
            continue;
        }

        let pkt = match ictx.packets().next() {
            Some(Ok((s, p))) => {
                if s.index() != active_idx {
                    continue;
                }
                p
            }
            Some(Err(_)) => continue,
            None => {
                let _ = ts.decoder.send_eof();
                drain_audio(
                    &mut ts.decoder,
                    &mut ts.swr,
                    &mut in_frame,
                    &samples_tx,
                    &msg_rx,
                    &stop,
                    current_serial,
                    out_channels,
                    out_sample_rate,
                    ts.in_sample_rate,
                    &mut running_pts,
                    &mut trim_until_pts,
                );
                // Reset del decoder para poder reusarlo tras un seek
                // hacia atrás (send_eof lo deja en estado draining).
                ts.decoder.flush();
                at_eof = true;
                continue;
            }
        };

        let _ = ts.decoder.send_packet(&pkt);

        while ts.decoder.receive_frame(&mut in_frame).is_ok() {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            // PTS del frame decodificado (si es válido).
            let pkt_pts = in_frame.pts().unwrap_or(ffmpeg::sys::AV_NOPTS_VALUE);
            if pkt_pts != ffmpeg::sys::AV_NOPTS_VALUE {
                running_pts = pkt_pts as f64 * ts.tb_num / ts.tb_den;
            }

            // El PTS del primer sample de SALIDA de esta conversión:
            // el resampler puede tener buffer interno de la conversión
            // anterior, cuyo audio es ANTERIOR al frame actual.
            let delay_in = ts
                .swr
                .delay()
                .map(|d| d.input as f64 / ts.in_sample_rate as f64)
                .unwrap_or(0.0);
            let out_first_pts = running_pts - delay_in;

            let samples = match resample_frame(
                &mut ts.swr,
                &in_frame,
                out_channels,
                out_sample_rate,
                ts.in_sample_rate,
            ) {
                Some(s) => s,
                None => continue,
            };
            let n_per_ch = samples.len() / out_channels as usize;

            if let Some(chunk) = make_trimmed_chunk(
                samples,
                out_first_pts,
                n_per_ch,
                current_serial,
                out_channels,
                out_sample_rate,
                &mut trim_until_pts,
            ) {
                if let Some(log) = dec_log.as_mut() {
                    use std::io::Write as _;
                    let _ = writeln!(
                        log,
                        "{:.4} CHUNK pts={:.3} n={} serial={}",
                        dec_origin.elapsed().as_secs_f64(),
                        chunk.first_pts,
                        chunk.samples.len() / out_channels as usize,
                        chunk.serial
                    );
                }
                if send_with_stop(&samples_tx, chunk, &stop, &msg_rx).is_err() {
                    return Ok(());
                }
            }
            // Avanzar running_pts para el próximo frame por si viene
            // sin PTS (algunos codecs lo hacen). Se avanza por la
            // duración del frame de ENTRADA (timeline del media).
            running_pts += in_frame.samples() as f64 / ts.in_sample_rate as f64;
        }
    }
    Ok(())
}

/// Convierte un frame con swresample usando un frame de salida NUEVO
/// con capacidad suficiente para (buffer interno + frame actual).
///
/// IMPORTANTE: el wrapper `SwrCtx::run()` de ffmpeg-the-third sólo
/// asigna el frame de salida si está vacío, y lo dimensiona con
/// `input.samples()` — capacidad que después NUNCA crece porque
/// `nb_samples` queda en "samples convertidos". Con out_rate < in_rate
/// (o tras el primer frame corto de AAC) la salida se trunca y el
/// resto se acumula sin límite en el FIFO interno del resampler:
/// los chunks emitidos representan MENOS tiempo del que avanza su
/// PTS → el reloj de audio corría ~3-4× más rápido que el sonido
/// real y el A/V se desincronizaba en segundos. Creamos un frame
/// nuevo por conversión con capacidad holgada para drenar SIEMPRE
/// todo lo disponible.
fn resample_frame(
    swr: &mut SwrCtx,
    in_frame: &AudioFrame,
    out_channels: u16,
    out_sample_rate: u32,
    in_sample_rate: u32,
) -> Option<Vec<f32>> {
    let in_n = in_frame.samples();
    if in_n == 0 {
        return None;
    }
    // Buffer interno pendiente (en samples de entrada) + frame actual,
    // convertido a rate de salida, con margen.
    let pending_in = swr.delay().map(|d| d.input as usize).unwrap_or(0);
    let cap = ((in_n + pending_in) as u64 * out_sample_rate as u64
        / in_sample_rate.max(1) as u64) as usize
        + 256;
    let mask = if out_channels == 1 {
        ChannelLayoutMask::MONO
    } else {
        ChannelLayoutMask::STEREO
    };
    let mut out_frame = AudioFrame::new(
        ffmpeg::format::Sample::F32(SampleType::Packed),
        cap,
        mask,
    );
    if swr.run(in_frame, &mut out_frame).is_err() {
        return None;
    }
    let samples = extract_f32_interleaved(&out_frame, out_channels);
    if samples.is_empty() {
        None
    } else {
        Some(samples)
    }
}

/// Aplica el recorte post-seek: si el frame decodificado empieza antes
/// del target, descarta los primeros samples para que el chunk emitido
/// empiece EXACTAMENTE (sample-accurate) en el target. Devuelve None si
/// el frame entero cae antes del target.
fn make_trimmed_chunk(
    mut samples: Vec<f32>,
    first_pts: f64,
    n_per_ch: usize,
    serial: i32,
    out_channels: u16,
    out_sample_rate: u32,
    trim_until_pts: &mut Option<f64>,
) -> Option<AudioChunk> {
    let mut chunk_first_pts = first_pts;
    if let Some(target) = *trim_until_pts {
        let end_pts = first_pts + n_per_ch as f64 / out_sample_rate as f64;
        if end_pts <= target {
            // Todo el frame es anterior al target → fuera.
            return None;
        }
        if first_pts < target {
            let skip_per_ch = (((target - first_pts) * out_sample_rate as f64) as usize)
                .min(n_per_ch.saturating_sub(1));
            samples.drain(..skip_per_ch * out_channels as usize);
            chunk_first_pts = first_pts + skip_per_ch as f64 / out_sample_rate as f64;
        }
        *trim_until_pts = None;
    }
    if samples.is_empty() {
        return None;
    }
    Some(AudioChunk {
        samples,
        serial,
        first_pts: chunk_first_pts,
    })
}

#[allow(clippy::too_many_arguments)]
fn drain_audio(
    decoder: &mut ffmpeg::decoder::Audio,
    swr: &mut SwrCtx,
    in_frame: &mut AudioFrame,
    samples_tx: &Sender<AudioChunk>,
    msg_rx: &Receiver<AudioMsg>,
    stop: &Arc<AtomicBool>,
    current_serial: i32,
    out_channels: u16,
    out_sample_rate: u32,
    in_sample_rate: u32,
    running_pts: &mut f64,
    trim_until_pts: &mut Option<f64>,
) {
    while decoder.receive_frame(in_frame).is_ok() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let pkt_pts = in_frame.pts().unwrap_or(ffmpeg::sys::AV_NOPTS_VALUE);
        if pkt_pts != ffmpeg::sys::AV_NOPTS_VALUE {
            // OJO: aquí no tenemos tb, el caller mantiene running_pts.
        }
        let delay_in = swr
            .delay()
            .map(|d| d.input as f64 / in_sample_rate as f64)
            .unwrap_or(0.0);
        let out_first_pts = *running_pts - delay_in;
        let in_n = in_frame.samples();
        let samples =
            match resample_frame(swr, in_frame, out_channels, out_sample_rate, in_sample_rate) {
                Some(s) => s,
                None => continue,
            };
        let n_per_ch = samples.len() / out_channels as usize;
        *running_pts += in_n as f64 / in_sample_rate as f64;
        if let Some(chunk) = make_trimmed_chunk(
            samples,
            out_first_pts,
            n_per_ch,
            current_serial,
            out_channels,
            out_sample_rate,
            trim_until_pts,
        ) {
            if send_with_stop(samples_tx, chunk, stop, msg_rx).is_err() {
                break;
            }
        }
    }
}

fn extract_f32_interleaved(frame: &AudioFrame, channels: u16) -> Vec<f32> {
    let n_per_ch = frame.samples();
    if n_per_ch == 0 {
        return Vec::new();
    }
    let n = n_per_ch * channels as usize;
    let bytes = frame.data(0);
    let expected_bytes = n * std::mem::size_of::<f32>();
    if bytes.len() < expected_bytes {
        return Vec::new();
    }
    let mut out = vec![0f32; n];
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.as_mut_ptr() as *mut u8, expected_bytes);
    }
    out
}

/// Envía un chunk respetando `stop`. Si mientras esperamos hueco en
/// el ring llega un mensaje (seek/switch, msg_rx no vacío), descartamos
/// el chunk y devolvemos Ok para que el loop principal lo procese YA —
/// sin esto, con el stream pausado (ring lleno, callback sin consumir)
/// el hilo se quedaba bloqueado y los seeks en pausa no se aplicaban.
fn send_with_stop(
    tx: &Sender<AudioChunk>,
    mut chunk: AudioChunk,
    stop: &Arc<AtomicBool>,
    msg_rx: &Receiver<AudioMsg>,
) -> std::result::Result<(), ()> {
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(());
        }
        match tx.try_send(chunk) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(c)) => {
                if !msg_rx.is_empty() {
                    // Seek/switch pendiente: este chunk ya es residuo.
                    return Ok(());
                }
                chunk = c;
                thread::sleep(Duration::from_millis(4));
            }
            Err(TrySendError::Disconnected(_)) => return Err(()),
        }
    }
}

fn eprintln_verbose(msg: &str) {
    eprintln!("[rtv-audio] {msg}");
}
