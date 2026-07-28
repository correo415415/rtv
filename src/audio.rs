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
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender, TrySendError};
use ffmpeg_the_third as ffmpeg;
use ffmpeg::format::sample::Type as SampleType;
use ffmpeg::format::{input, Sample as SampleFormat};
use ffmpeg::media::Type as MediaType;
use ffmpeg::software::resampling::context::Context as SwrCtx;
use ffmpeg::util::frame::audio::Audio as AudioFrame;
use ffmpeg::{ChannelLayout, ChannelLayoutMask};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::clock::FfClock;

/// Petición de seek al hilo audio-decoder: target + serial nuevo.
struct SeekMsg {
    target_secs: f64,
    serial: i32,
}

/// Bloque de muestras con serial + PTS del primer sample.
struct AudioChunk {
    samples: Vec<f32>,
    /// Serial en el que se produjo. El player bumpea el serial en
    /// cada seek → chunks con serial viejo son residuo.
    serial: i32,
    /// PTS (segundos) del primer sample del chunk.
    first_pts: f64,
}

pub struct AudioHandle {
    stop: Arc<AtomicBool>,
    volume: Arc<AtomicU8>,
    seek_tx: Sender<SeekMsg>,
    pub clock: Arc<FfClock>,
    pub has_audio: bool,
    pub sample_rate: u32,
    pub channels: u16,
    decoder_join: Option<thread::JoinHandle<()>>,
    pub stream: Option<cpal::Stream>,
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
        let _ = self.seek_tx.send(SeekMsg {
            target_secs,
            serial,
        });
    }

    pub fn pause_stream(&self) {
        if let Some(s) = self.stream.as_ref() {
            if let Err(e) = s.pause() {
                eprintln_verbose(&format!("cpal pause falló: {e}"));
            }
        }
    }
    pub fn play_stream(&self) {
        if let Some(s) = self.stream.as_ref() {
            if let Err(e) = s.play() {
                eprintln_verbose(&format!("cpal play falló: {e}"));
            }
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.decoder_join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for AudioHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn spawn<P: AsRef<Path>>(path: P, clock: Arc<FfClock>) -> Result<AudioHandle> {
    let path = path.as_ref().to_owned();

    let ictx = input(&path).with_context(|| format!("abriendo {:?}", path))?;
    let audio_stream = ictx.streams().best(MediaType::Audio);
    if audio_stream.is_none() {
        return Ok(no_audio(clock));
    }
    let audio_idx = audio_stream.as_ref().unwrap().index();
    let codec_params: ffmpeg::codec::Parameters = {
        let src_ref = audio_stream.as_ref().unwrap().parameters();
        let mut owned = ffmpeg::codec::Parameters::new();
        unsafe {
            ffmpeg::sys::avcodec_parameters_copy(owned.as_mut_ptr(), src_ref.as_ptr());
        }
        owned
    };
    let time_base = audio_stream.as_ref().unwrap().time_base();
    drop(ictx);

    let host = cpal::default_host();
    let device = match host.default_output_device() {
        Some(d) => d,
        None => return Ok(no_audio(clock)),
    };
    let supported = match device.default_output_config() {
        Ok(s) => s,
        Err(e) => {
            eprintln_verbose(&format!("cpal default_output_config falló: {e}"));
            return Ok(no_audio(clock));
        }
    };
    let out_sample_rate = supported.sample_rate().0;
    let out_channels: u16 = 2;
    let stream_config = cpal::StreamConfig {
        channels: out_channels,
        sample_rate: cpal::SampleRate(out_sample_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    let (samples_tx, samples_rx) = bounded::<AudioChunk>(64);
    let samples_rx_for_drain = samples_rx.clone();

    let stop = Arc::new(AtomicBool::new(false));
    let volume = Arc::new(AtomicU8::new(100));
    let (seek_tx, seek_rx) = unbounded::<SeekMsg>();

    let decoder_join = {
        let stop = stop.clone();
        let path2 = path.clone();
        let tb_num = f64::from(time_base.numerator());
        let tb_den = f64::from(time_base.denominator());
        thread::Builder::new()
            .name("rtv-audio-decoder".into())
            .spawn(move || {
                let _ = audio_decode_loop(
                    path2,
                    audio_idx,
                    codec_params,
                    tb_num,
                    tb_den,
                    out_sample_rate,
                    out_channels,
                    samples_tx,
                    samples_rx_for_drain,
                    seek_rx,
                    stop,
                );
            })?
    };

    // Estado del callback (owned por la closure).
    let stream = {
        let stop_cb = stop.clone();
        let clock_cb = clock.clone();
        let volume_cb = volume.clone();
        // Log de depuración opcional del callback (RTV_AUDIO_DEBUG=/ruta).
        let mut dbg_log: Option<std::io::BufWriter<std::fs::File>> = std::env::var(
            "RTV_AUDIO_DEBUG",
        )
        .ok()
        .and_then(|p| std::fs::File::create(p).ok().map(std::io::BufWriter::new));
        let dbg_origin = std::time::Instant::now();
        let mut dbg_count: u64 = 0;
        // Estado local del callback:
        let mut leftover: Vec<f32> = Vec::new();
        let mut leftover_offset = 0usize;
        let mut leftover_serial: i32 = 0;
        let mut leftover_first_pts: f64 = 0.0;
        // Muestras dentro del chunk actual ya emitidas (per-channel).
        let mut samples_emitted_in_chunk: usize = 0;
        // Estimación SUAVIZADA de la latencia de salida. El tamaño del
        // buffer del callback puede alternar (p.ej. PulseAudio pide
        // 25 ms / 50 ms alternos); usar el tamaño del callback ACTUAL
        // como estimación metía un diente de sierra de ±25 ms en el
        // reloj de audio que el vídeo perseguía (patrón -80/-40/+10 ms
        // en el sync-log). Un EMA converge a la media estable y el
        // reloj queda liso — el offset constante residual es idéntico
        // para todos los frames y el vídeo lo sigue sin jitter.
        let mut latency_ema: f64 = 0.0;

        let build = device.build_output_stream(
            &stream_config,
            move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
                // ---- Salida silenciosa ante stop / pause ----
                if stop_cb.load(Ordering::Relaxed) {
                    out.fill(0.0);
                    return;
                }
                if clock_cb.paused.load(Ordering::Acquire) != 0 {
                    out.fill(0.0);
                    return;
                }
                let vol_pct = volume_cb.load(Ordering::Relaxed) as f32 / 100.0;
                // Serial válido AHORA (único serial compartido por reloj
                // y pipeline). Se lee una vez al principio: si un seek
                // ocurre a mitad de callback, el `set_pts` final será
                // rechazado por el guard de serial del reloj.
                let current_serial = clock_cb.current_serial();

                let mut filled = 0usize;
                // PTS del PRIMER sample válido emitido en esta llamada
                // y su offset (en frames por-canal) dentro de `out`.
                // El primer sample de `out` sale por el DAC en
                // `ts.playback` — con eso anclamos el reloj.
                let mut first_pts_emitted: Option<(f64, usize)> = None;

                while filled < out.len() {
                    // Chunk actual con serial viejo → DESCARTAR AL
                    // INSTANTE (sin "reproducir" su duración como
                    // silencio, que retrasaba el audio fresco tras un
                    // seek en decenas de ms).
                    if leftover_offset < leftover.len() && leftover_serial != current_serial {
                        leftover_offset = leftover.len();
                        continue;
                    }
                    // Chunk agotado → traer otro.
                    if leftover_offset >= leftover.len() {
                        match samples_rx.try_recv() {
                            Ok(chunk) => {
                                leftover = chunk.samples;
                                leftover_offset = 0;
                                leftover_serial = chunk.serial;
                                leftover_first_pts = chunk.first_pts;
                                samples_emitted_in_chunk = 0;
                            }
                            Err(_) => {
                                // Underrun: silencio.
                                out[filled..].fill(0.0);
                                break;
                            }
                        }
                        continue;
                    }
                    // Emisión válida.
                    if first_pts_emitted.is_none() {
                        let pts_here = leftover_first_pts
                            + samples_emitted_in_chunk as f64 / out_sample_rate as f64;
                        first_pts_emitted =
                            Some((pts_here, filled / out_channels as usize));
                    }
                    let take = (out.len() - filled).min(leftover.len() - leftover_offset);
                    for i in 0..take {
                        out[filled + i] = leftover[leftover_offset + i] * vol_pct;
                    }
                    filled += take;
                    leftover_offset += take;
                    samples_emitted_in_chunk += take / out_channels as usize;
                }

                // ---- Actualizar audclk con COMPENSACIÓN DE LATENCIA ----
                if let Some((pts_first, frame_offset)) = first_pts_emitted {
                    // cpal nos da:
                    //   playback = ts.playback  (cuándo el PRIMER frame
                    //                            de `out` sale por el DAC)
                    //   callback = ts.callback  (ahora)
                    //   delay = playback - callback  (>0 normalmente)
                    //
                    // El primer sample VÁLIDO emitido está en el frame
                    // `frame_offset` del buffer, así que sonará en
                    //   playback + frame_offset/rate.
                    // Por tanto el PTS que se OYE en este instante es:
                    //   pts_heard_now = pts_first
                    //                   - frame_offset/rate
                    //                   - delay
                    // (la versión anterior usaba el PTS del ÚLTIMO
                    // sample emitido sin restar la duración del buffer
                    // → el reloj de audio corría ADELANTADO ~1 buffer
                    // (5–40 ms) de forma sistemática).
                    let ts = info.timestamp();
                    let reported_delay = ts
                        .playback
                        .duration_since(&ts.callback)
                        .map(|d| d.as_secs_f64())
                        .unwrap_or(0.0);
                    // Algunos backends (p.ej. ALSA→Pulse, null sinks,
                    // ciertos drivers) reportan delay=0 aunque el
                    // buffer que estamos rellenando NO sonará hasta que
                    // drene el período en curso. Estimación mínima
                    // robusta: la duración de UN período del callback
                    // (ffplay hace lo mismo con audio_hw_buf_size).
                    let buf_period_secs =
                        (out.len() / out_channels as usize) as f64 / out_sample_rate as f64;
                    // Clamp superior: tras un underrun (ring vacío por
                    // CPU starving) PulseAudio puede reportar delays
                    // absurdos (>1 s) — sin límite, el reloj de audio
                    // saltaba SEGUNDOS hacia atrás y el vídeo entraba
                    // en free-run persiguiendo un master roto.
                    let raw_delay = reported_delay.max(buf_period_secs).min(0.5);
                    if latency_ema == 0.0 {
                        latency_ema = raw_delay;
                    } else {
                        latency_ema = 0.9 * latency_ema + 0.1 * raw_delay;
                    }
                    let delay_secs = latency_ema;
                    let offset_secs = frame_offset as f64 / out_sample_rate as f64;
                    let pts_being_heard = (pts_first - offset_secs - delay_secs).max(0.0);
                    clock_cb.set_pts(pts_being_heard, current_serial);
                    if let Some(log) = dbg_log.as_mut() {
                        use std::io::Write as _;
                        dbg_count += 1;
                        let _ = writeln!(
                            log,
                            "{:.4} cb#{} buf={} pts_first={:.4} rep_delay={:.4} set={:.4}",
                            dbg_origin.elapsed().as_secs_f64(),
                            dbg_count,
                            out.len(),
                            pts_first,
                            reported_delay,
                            pts_being_heard,
                        );
                    }
                }
            },
            |err| eprintln_verbose(&format!("cpal stream error: {err}")),
            None,
        );
        match build {
            Ok(s) => match s.play() {
                Ok(()) => Some(s),
                Err(e) => {
                    eprintln_verbose(&format!("cpal play falló: {e}"));
                    None
                }
            },
            Err(e) => {
                eprintln_verbose(&format!("cpal build_output_stream falló: {e}"));
                None
            }
        }
    };

    let has_audio = stream.is_some();

    Ok(AudioHandle {
        stop,
        volume,
        seek_tx,
        clock,
        has_audio,
        sample_rate: out_sample_rate,
        channels: out_channels,
        decoder_join: Some(decoder_join),
        stream,
    })
}

fn no_audio(clock: Arc<FfClock>) -> AudioHandle {
    let stop = Arc::new(AtomicBool::new(true));
    let (seek_tx, _seek_rx) = bounded::<SeekMsg>(1);
    AudioHandle {
        stop,
        volume: Arc::new(AtomicU8::new(100)),
        seek_tx,
        clock,
        has_audio: false,
        sample_rate: 48000,
        channels: 2,
        decoder_join: None,
        stream: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn audio_decode_loop(
    path: PathBuf,
    audio_idx: usize,
    codec_params: ffmpeg::codec::Parameters,
    tb_num: f64,
    tb_den: f64,
    out_sample_rate: u32,
    out_channels: u16,
    samples_tx: Sender<AudioChunk>,
    samples_rx_for_drain: Receiver<AudioChunk>,
    seek_rx: Receiver<SeekMsg>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let mut ictx = input(&path)?;
    let dec_ctx = ffmpeg::codec::context::Context::from_parameters(codec_params)?;
    let mut decoder = dec_ctx.decoder().audio()?;

    // Log de depuración del hilo decoder (RTV_AUDIO_DEC_DEBUG=/ruta).
    let mut dec_log: Option<std::io::BufWriter<std::fs::File>> =
        std::env::var("RTV_AUDIO_DEC_DEBUG").ok().and_then(|p| {
            std::fs::File::create(p).ok().map(std::io::BufWriter::new)
        });
    let dec_origin = std::time::Instant::now();

    let in_sample_rate = decoder.rate();
    // AVChannelLayout DUEÑO (desligado del borrow del decoder) para
    // poder recrear el resampler tras cada seek.
    let in_ch_layout_raw: ffmpeg::sys::AVChannelLayout =
        decoder.ch_layout().to_owned().into_owned();
    let in_format: SampleFormat = decoder.format();

    let out_format = SampleFormat::F32(SampleType::Packed);
    let mk_out_layout = move || {
        if out_channels == 1 {
            ChannelLayout::MONO
        } else {
            ChannelLayout::STEREO
        }
    };

    let mut swr = SwrCtx::get2(
        in_format,
        ChannelLayout::from(&in_ch_layout_raw),
        in_sample_rate,
        out_format,
        mk_out_layout(),
        out_sample_rate,
    )
    .map_err(|e| anyhow!("swresample init: {e}"))?;

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

        // Procesar seeks pendientes ANTES de leer paquete siguiente.
        let mut seeked_to: Option<SeekMsg> = None;
        while let Ok(msg) = seek_rx.try_recv() {
            seeked_to = Some(msg);
        }
        if let Some(msg) = seeked_to {
            current_serial = msg.serial;
            let target = msg.target_secs;
            // Unidades: `Input::seek` → avformat_seek_file con
            // stream_index=-1 → timestamps en AV_TIME_BASE (µs).
            // OJO con el rango: `..ts` (exclusivo) produce
            // max_ts = ts-1 < ts y avformat_seek_file devuelve EINVAL
            // SIN MOVER el demuxer → los seeks hacia atrás dejaban el
            // audio donde estaba (los hacia delante los enmascaraba el
            // trim). Con `..=ts` es (INT64_MIN, ts, ts) = keyframe<=ts,
            // exactamente como ffplay.
            let ts = (target * f64::from(ffmpeg::ffi::AV_TIME_BASE)) as i64;
            let _ = ictx.seek(ts, ..=ts);
            decoder.flush();
            // Recrear el resampler: su FIFO interno puede contener
            // samples pre-seek que saldrían etiquetados con el PTS
            // nuevo (audio viejo sonándo tras el salto). Recrearlo es
            // barato y garantiza estado limpio.
            if let Ok(new_swr) = SwrCtx::get2(
                in_format,
                ChannelLayout::from(&in_ch_layout_raw),
                in_sample_rate,
                out_format,
                mk_out_layout(),
                out_sample_rate,
            ) {
                swr = new_swr;
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
                if s.index() != audio_idx {
                    continue;
                }
                p
            }
            Some(Err(_)) => continue,
            None => {
                let _ = decoder.send_eof();
                drain_audio(
                    &mut decoder,
                    &mut swr,
                    &mut in_frame,
                    &samples_tx,
                    &seek_rx,
                    &stop,
                    current_serial,
                    out_channels,
                    out_sample_rate,
                    in_sample_rate,
                    &mut running_pts,
                    &mut trim_until_pts,
                );
                // Reset del decoder para poder reusarlo tras un seek
                // hacia atrás (send_eof lo deja en estado draining).
                decoder.flush();
                at_eof = true;
                continue;
            }
        };

        let _ = decoder.send_packet(&pkt);

        while decoder.receive_frame(&mut in_frame).is_ok() {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            // PTS del frame decodificado (si es válido).
            let pkt_pts = in_frame.pts().unwrap_or(ffmpeg::sys::AV_NOPTS_VALUE);
            if pkt_pts != ffmpeg::sys::AV_NOPTS_VALUE {
                running_pts = pkt_pts as f64 * tb_num / tb_den;
            }

            // El PTS del primer sample de SALIDA de esta conversión:
            // el resampler puede tener buffer interno de la conversión
            // anterior, cuyo audio es ANTERIOR al frame actual.
            let delay_in = swr
                .delay()
                .map(|d| d.input as f64 / in_sample_rate as f64)
                .unwrap_or(0.0);
            let out_first_pts = running_pts - delay_in;

            let samples = match resample_frame(&mut swr, &in_frame, out_channels, out_sample_rate, in_sample_rate) {
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
                if send_with_stop(&samples_tx, chunk, &stop, &seek_rx).is_err() {
                    return Ok(());
                }
            }
            // Avanzar running_pts para el próximo frame por si viene
            // sin PTS (algunos codecs lo hacen). Se avanza por la
            // duración del frame de ENTRADA (timeline del media).
            running_pts += in_frame.samples() as f64 / in_sample_rate as f64;
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
    seek_rx: &Receiver<SeekMsg>,
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
            if send_with_stop(samples_tx, chunk, stop, seek_rx).is_err() {
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
/// el ring llega un SEEK (seek_rx no vacío), descartamos el chunk y
/// devolvemos Ok para que el loop principal procese el seek YA — sin
/// esto, con el stream pausado (ring lleno, callback sin consumir) el
/// hilo se quedaba bloqueado y los seeks en pausa no se aplicaban.
fn send_with_stop(
    tx: &Sender<AudioChunk>,
    mut chunk: AudioChunk,
    stop: &Arc<AtomicBool>,
    seek_rx: &Receiver<SeekMsg>,
) -> std::result::Result<(), ()> {
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(());
        }
        match tx.try_send(chunk) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(c)) => {
                if !seek_rx.is_empty() {
                    // Seek pendiente: este chunk ya es residuo.
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
