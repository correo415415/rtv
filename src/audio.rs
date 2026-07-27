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
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use ffmpeg_the_third as ffmpeg;
use ffmpeg::format::sample::Type as SampleType;
use ffmpeg::format::{input, Sample as SampleFormat};
use ffmpeg::media::Type as MediaType;
use ffmpeg::software::resampling::context::Context as SwrCtx;
use ffmpeg::util::frame::audio::Audio as AudioFrame;
use ffmpeg::ChannelLayout;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::clock::FfClock;

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
    seek_tx: Sender<f64>,
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
        let _ = self.seek_tx.try_send(target_secs);
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
    let (seek_tx, seek_rx) = bounded::<f64>(4);

    let decoder_join = {
        let stop = stop.clone();
        let clock_dec = clock.clone();
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
                    clock_dec,
                );
            })?
    };

    // Estado del callback (owned por la closure).
    let stream = {
        let stop_cb = stop.clone();
        let clock_cb = clock.clone();
        let volume_cb = volume.clone();
        // Estado local del callback:
        let mut leftover: Vec<f32> = Vec::new();
        let mut leftover_offset = 0usize;
        let mut leftover_serial: i32 = -1;
        let mut leftover_first_pts: f64 = 0.0;
        // Muestras dentro del chunk actual ya emitidas (per-channel).
        let mut samples_emitted_in_chunk: usize = 0;

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
                let current_serial = clock_cb.current_serial();

                let mut filled = 0usize;
                // Cuando emitimos audio válido, guardamos el PTS del
                // ÚLTIMO sample emitido en esta llamada. Con eso
                // haremos un único `set_pts` al final.
                let mut last_pts_emitted: Option<f64> = None;

                while filled < out.len() {
                    // Chunk actual con serial viejo → silenciar y saltar
                    // toda su porción pendiente sin tocar el reloj.
                    if leftover_offset < leftover.len() && leftover_serial != current_serial {
                        let take = (out.len() - filled).min(leftover.len() - leftover_offset);
                        out[filled..filled + take].fill(0.0);
                        filled += take;
                        leftover_offset += take;
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
                    let take = (out.len() - filled).min(leftover.len() - leftover_offset);
                    for i in 0..take {
                        out[filled + i] = leftover[leftover_offset + i] * vol_pct;
                    }
                    filled += take;
                    leftover_offset += take;
                    let take_per_ch = take / out_channels as usize;
                    samples_emitted_in_chunk += take_per_ch;

                    // PTS del último sample emitido = first_pts del chunk
                    // + (samples_emitted_in_chunk - 1) / sample_rate.
                    let last_sample_offset =
                        (samples_emitted_in_chunk.saturating_sub(1)) as f64
                            / out_sample_rate as f64;
                    last_pts_emitted = Some(leftover_first_pts + last_sample_offset);
                }

                // ---- Actualizar audclk con COMPENSACIÓN DE LATENCIA ----
                if let Some(pts_last) = last_pts_emitted {
                    // `playback_delay_samples` = muestras que ya
                    // pasamos al driver pero AÚN NO HA REPRODUCIDO. Es
                    // el equivalente de `audio_hw_buf_size` en ffplay.
                    // cpal expone eso vía `info.timestamp()`.
                    //
                    //   playback = ts.playback   (cuándo saldrá al DAC)
                    //   callback = ts.callback   (ahora, cuando se
                    //                             ejecuta el callback)
                    //   delay = playback - callback  (>0 normalmente)
                    //
                    // Entonces el "PTS que se OYE ahora" es:
                    //   pts_now_being_heard = pts_last - delay
                    //
                    // Con eso, `audclk.now() = pts_now_being_heard +
                    // wall.elapsed_since_set`, que es exactamente lo
                    // que oye el usuario.
                    let ts = info.timestamp();
                    let delay_secs = ts
                        .playback
                        .duration_since(&ts.callback)
                        .map(|d| d.as_secs_f64())
                        .unwrap_or(0.0);
                    let pts_being_heard = (pts_last - delay_secs).max(0.0);
                    clock_cb.set_pts(pts_being_heard, current_serial);
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
    let (seek_tx, _seek_rx) = bounded::<f64>(1);
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
    seek_rx: Receiver<f64>,
    stop: Arc<AtomicBool>,
    clock: Arc<FfClock>,
) -> Result<()> {
    let mut ictx = input(&path)?;
    let dec_ctx = ffmpeg::codec::context::Context::from_parameters(codec_params)?;
    let mut decoder = dec_ctx.decoder().audio()?;

    let in_sample_rate = decoder.rate();
    let in_ch_layout = decoder.ch_layout().to_owned();
    let in_format: SampleFormat = decoder.format();

    let out_format = SampleFormat::F32(SampleType::Packed);
    let out_layout = if out_channels == 1 {
        ChannelLayout::MONO
    } else {
        ChannelLayout::STEREO
    };

    let mut swr = SwrCtx::get2(
        in_format,
        in_ch_layout,
        in_sample_rate,
        out_format,
        out_layout,
        out_sample_rate,
    )
    .map_err(|e| anyhow!("swresample init: {e}"))?;

    let mut in_frame = AudioFrame::empty();
    let mut out_frame = AudioFrame::empty();

    // PTS running: lo actualizamos cada vez que decoder produce un
    // frame con PTS válido; los subsecuentes suman n_samples/rate.
    let mut running_pts: f64 = 0.0;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // Procesar seeks pendientes ANTES de leer paquete siguiente.
        let mut seeked_to: Option<f64> = None;
        while let Ok(target) = seek_rx.try_recv() {
            seeked_to = Some(target);
        }
        if let Some(target) = seeked_to {
            let ts = (target * (tb_den / tb_num)) as i64;
            let _ = ictx.seek(ts, ..ts);
            decoder.flush();
            // Vaciar ring: aunque el callback silenciaría por serial,
            // preferimos que llegue audio fresco cuanto antes.
            while samples_rx_for_drain.try_recv().is_ok() {}
            running_pts = target;
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
                    &mut out_frame,
                    &samples_tx,
                    &stop,
                    &clock,
                    out_channels,
                    out_sample_rate,
                    &mut running_pts,
                );
                break;
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

            if swr.run(&in_frame, &mut out_frame).is_err() {
                continue;
            }
            let samples = extract_f32_interleaved(&out_frame, out_channels);
            if samples.is_empty() {
                continue;
            }
            let n_per_ch = samples.len() / out_channels as usize;

            let chunk = AudioChunk {
                samples,
                serial: clock.current_serial(),
                first_pts: running_pts,
            };
            if send_with_stop(&samples_tx, chunk, &stop).is_err() {
                return Ok(());
            }
            // Avanzar running_pts para el próximo frame por si viene
            // sin PTS (algunos codecs lo hacen).
            running_pts += n_per_ch as f64 / out_sample_rate as f64;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn drain_audio(
    decoder: &mut ffmpeg::decoder::Audio,
    swr: &mut SwrCtx,
    in_frame: &mut AudioFrame,
    out_frame: &mut AudioFrame,
    samples_tx: &Sender<AudioChunk>,
    stop: &Arc<AtomicBool>,
    clock: &Arc<FfClock>,
    out_channels: u16,
    out_sample_rate: u32,
    running_pts: &mut f64,
) {
    while decoder.receive_frame(in_frame).is_ok() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if swr.run(in_frame, out_frame).is_err() {
            continue;
        }
        let samples = extract_f32_interleaved(out_frame, out_channels);
        if samples.is_empty() {
            continue;
        }
        let n_per_ch = samples.len() / out_channels as usize;
        let chunk = AudioChunk {
            samples,
            serial: clock.current_serial(),
            first_pts: *running_pts,
        };
        if send_with_stop(samples_tx, chunk, stop).is_err() {
            break;
        }
        *running_pts += n_per_ch as f64 / out_sample_rate as f64;
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

fn send_with_stop(
    tx: &Sender<AudioChunk>,
    mut chunk: AudioChunk,
    stop: &Arc<AtomicBool>,
) -> std::result::Result<(), ()> {
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(());
        }
        match tx.try_send(chunk) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(c)) => {
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
