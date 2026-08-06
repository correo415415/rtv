//! audio_backend.rs — feeder compartido de los sinks de audio +
//! backend PulseAudio (Termux / fallback Linux).
//!
//! El corazón del reloj de audio (consumo del ring de AudioChunks,
//! descarte por serial, EMA de latencia, limitador de tasa) vivía
//! dentro de la closure del callback de cpal. Para soportar Termux
//! (donde cpal no funciona: su backend AAudio requiere el NDK y un
//! contexto de app Android) se extrae aquí como `SinkFeeder`,
//! compartido por AMBOS backends:
//!
//!   * cpal  (audio.rs): el callback llama a `feeder.fill(out, delay)`.
//!   * pulse (aquí): un hilo writer llama a `feeder.fill(buf, delay)`
//!     y bloquea en `pa_simple_write`.
//!
//! El backend pulse carga libpulse-simple con dlopen (libloading) en
//! runtime: CERO dependencia de build o de arranque — si la lib o el
//! servidor no existen, `PulseSink::try_open` devuelve Err y el caller
//! degrada (a cpal o a no_audio). En Termux el audio real pasa por
//! PulseAudio (`pkg install pulseaudio` + `pulseaudio --start`).

use crossbeam_channel::Receiver;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

use crate::clock::FfClock;

/// Bloque de muestras con serial + PTS del primer sample. (Movido
/// desde audio.rs para que ambos backends lo compartan.)
pub struct AudioChunk {
    pub samples: Vec<f32>,
    /// Serial en el que se produjo. El player bumpea el serial en
    /// cada seek → chunks con serial viejo son residuo.
    pub serial: i32,
    /// PTS (segundos) del primer sample del chunk.
    pub first_pts: f64,
}

/// Estado persistente del consumidor del ring de audio. UNA instancia
/// por stream de salida, poseída por el callback/hilo del backend.
///
/// `fill()` reproduce EXACTAMENTE la semántica del callback histórico
/// de cpal (v0.5): descarte instantáneo de chunks con serial viejo,
/// silencio en underrun, EMA de la latencia reportada, clamp 0.5 s,
/// mínimo de un período del buffer, limitador de tasa ×1.02 con
/// dt=0 si los callbacks pararon >250 ms, y un único `set_pts` por
/// llamada con el PTS que se ESTÁ OYENDO.
pub struct SinkFeeder {
    stop: Arc<AtomicBool>,
    clock: Arc<FfClock>,
    volume: Arc<AtomicU8>,
    samples_rx: Receiver<AudioChunk>,
    out_sample_rate: u32,
    out_channels: u16,

    // --- estado del chunk en curso ---
    leftover: Vec<f32>,
    leftover_offset: usize,
    leftover_serial: i32,
    leftover_first_pts: f64,
    samples_emitted_in_chunk: usize,

    // --- estimación de latencia + limitador de tasa ---
    latency_ema: f64,
    rate_lim: Option<(f64, std::time::Instant)>,
    rate_lim_serial: i32,

    // --- log de depuración opcional (RTV_AUDIO_DEBUG=/ruta) ---
    dbg_log: Option<std::io::BufWriter<std::fs::File>>,
    dbg_origin: std::time::Instant,
    dbg_count: u64,
}

impl SinkFeeder {
    pub fn new(
        stop: Arc<AtomicBool>,
        clock: Arc<FfClock>,
        volume: Arc<AtomicU8>,
        samples_rx: Receiver<AudioChunk>,
        out_sample_rate: u32,
        out_channels: u16,
    ) -> Self {
        let dbg_log = std::env::var("RTV_AUDIO_DEBUG")
            .ok()
            .and_then(|p| std::fs::File::create(p).ok().map(std::io::BufWriter::new));
        SinkFeeder {
            stop,
            clock,
            volume,
            samples_rx,
            out_sample_rate,
            out_channels,
            leftover: Vec::new(),
            leftover_offset: 0,
            leftover_serial: 0,
            leftover_first_pts: 0.0,
            samples_emitted_in_chunk: 0,
            latency_ema: 0.0,
            rate_lim: None,
            rate_lim_serial: i32::MIN,
            dbg_log,
            dbg_origin: std::time::Instant::now(),
            dbg_count: 0,
        }
    }

    /// Rellena `out` (f32 interleaved) desde el ring y actualiza el
    /// reloj de audio. `reported_delay_secs`: latencia de salida que
    /// reporta el backend (cpal: playback−callback; pulse:
    /// pa_simple_get_latency). Puede ser 0 — se aplica el mínimo de
    /// un período del propio buffer.
    ///
    /// Devuelve `true` si emitió audio válido (anchor del reloj).
    pub fn fill(&mut self, out: &mut [f32], reported_delay_secs: f64) -> bool {
        // ---- Salida silenciosa ante stop / pause ----
        if self.stop.load(Ordering::Relaxed) {
            out.fill(0.0);
            return false;
        }
        if self.clock.paused.load(Ordering::Acquire) != 0 {
            out.fill(0.0);
            return false;
        }
        let vol_pct = self.volume.load(Ordering::Relaxed) as f32 / 100.0;
        // Serial válido AHORA (único serial compartido por reloj y
        // pipeline). Se lee una vez al principio: si un seek ocurre a
        // mitad de llamada, el `set_pts` final será rechazado por el
        // guard de serial del reloj.
        let current_serial = self.clock.current_serial();

        let mut filled = 0usize;
        // PTS del PRIMER sample válido emitido en esta llamada y su
        // offset (en frames por-canal) dentro de `out`.
        let mut first_pts_emitted: Option<(f64, usize)> = None;

        while filled < out.len() {
            // Chunk actual con serial viejo → DESCARTAR AL INSTANTE
            // (sin "reproducir" su duración como silencio).
            if self.leftover_offset < self.leftover.len()
                && self.leftover_serial != current_serial
            {
                self.leftover_offset = self.leftover.len();
                continue;
            }
            // Chunk agotado → traer otro.
            if self.leftover_offset >= self.leftover.len() {
                match self.samples_rx.try_recv() {
                    Ok(chunk) => {
                        self.leftover = chunk.samples;
                        self.leftover_offset = 0;
                        self.leftover_serial = chunk.serial;
                        self.leftover_first_pts = chunk.first_pts;
                        self.samples_emitted_in_chunk = 0;
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
                let pts_here = self.leftover_first_pts
                    + self.samples_emitted_in_chunk as f64 / self.out_sample_rate as f64;
                first_pts_emitted = Some((pts_here, filled / self.out_channels as usize));
            }
            let take = (out.len() - filled).min(self.leftover.len() - self.leftover_offset);
            for i in 0..take {
                out[filled + i] = self.leftover[self.leftover_offset + i] * vol_pct;
            }
            filled += take;
            self.leftover_offset += take;
            self.samples_emitted_in_chunk += take / self.out_channels as usize;
        }

        // ---- Actualizar audclk con COMPENSACIÓN DE LATENCIA ----
        let Some((pts_first, frame_offset)) = first_pts_emitted else {
            return false;
        };
        // Algunos backends (ALSA→Pulse, null sinks, ciertos drivers)
        // reportan delay=0 aunque el buffer que estamos rellenando NO
        // sonará hasta que drene el período en curso. Estimación
        // mínima robusta: la duración de UN período del buffer (ffplay
        // hace lo mismo con audio_hw_buf_size).
        let buf_period_secs =
            (out.len() / self.out_channels as usize) as f64 / self.out_sample_rate as f64;
        // Clamp superior: tras un underrun PulseAudio puede reportar
        // delays absurdos (>1 s) — sin límite, el reloj de audio
        // saltaba SEGUNDOS hacia atrás.
        let raw_delay = reported_delay_secs.max(buf_period_secs).min(0.5);
        if self.latency_ema == 0.0 {
            self.latency_ema = raw_delay;
        } else {
            self.latency_ema = 0.9 * self.latency_ema + 0.1 * raw_delay;
        }
        let delay_secs = self.latency_ema;
        let offset_secs = frame_offset as f64 / self.out_sample_rate as f64;
        let mut pts_being_heard = (pts_first - offset_secs - delay_secs).max(0.0);
        // ---- Limitador de tasa: el PTS "que se oye" no puede avanzar
        // más rápido que el tiempo mural (×1.02). Al conectar, Pulse
        // consume ~0.4 s DE GOLPE para su prebuffer reportando delay=0;
        // sin esto el reloj saltaba +0.4 s y el vídeo decode-bound
        // quedaba por detrás para siempre. dt=0 si los callbacks
        // pararon >250 ms (el DAC no consumió en el hueco). ----
        let now_i = std::time::Instant::now();
        if self.rate_lim_serial != current_serial {
            self.rate_lim_serial = current_serial;
            self.rate_lim = None;
        }
        if let Some((prev_pts, prev_wall)) = self.rate_lim {
            let raw_dt = now_i.duration_since(prev_wall).as_secs_f64();
            let dt = if raw_dt > 0.25 { 0.0 } else { raw_dt };
            let cap = prev_pts + dt * 1.02;
            if pts_being_heard > cap {
                pts_being_heard = cap;
            }
        }
        self.rate_lim = Some((pts_being_heard, now_i));
        self.clock.set_pts(pts_being_heard, current_serial);
        if let Some(log) = self.dbg_log.as_mut() {
            use std::io::Write as _;
            self.dbg_count += 1;
            let _ = writeln!(
                log,
                "{:.4} cb#{} buf={} pts_first={:.4} rep_delay={:.4} set={:.4}",
                self.dbg_origin.elapsed().as_secs_f64(),
                self.dbg_count,
                out.len(),
                pts_first,
                reported_delay_secs,
                pts_being_heard,
            );
        }
        true
    }
}

// ============================================================
// Backend PulseAudio (feature `pulse`) — libpulse-simple vía dlopen.
// ============================================================
//
// API simple de PulseAudio: una conexión bloqueante por stream.
// `pa_simple_write` bloquea hasta colocar los bytes en el buffer del
// servidor → el propio write marca el ritmo (pacing), igual que el
// callback de cpal. La latencia real la reporta
// `pa_simple_get_latency` (µs) y se pasa al feeder.
//
// dlopen en runtime (libloading): el binario NO enlaza libpulse — en
// sistemas sin PulseAudio (o sin servidor arrancado) `try_open`
// devuelve Err y el caller prueba el siguiente backend. En Termux:
// `pkg install pulseaudio && pulseaudio --start` y funciona.
#[cfg(feature = "pulse")]
pub mod pulse {
    use super::{AudioChunk, SinkFeeder};
    use crate::clock::FfClock;
    use anyhow::{anyhow, Result};
    use crossbeam_channel::Receiver;
    use std::ffi::{c_char, c_int, c_void, CString};
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    // --- ABI de libpulse-simple (estable desde hace ~15 años) ---
    const PA_STREAM_PLAYBACK: c_int = 1;
    const PA_SAMPLE_FLOAT32LE: c_int = 5;

    #[repr(C)]
    struct PaSampleSpec {
        format: c_int,
        rate: u32,
        channels: u8,
    }

    #[repr(C)]
    struct PaBufferAttr {
        maxlength: u32,
        tlength: u32,
        prebuf: u32,
        minreq: u32,
        fragsize: u32,
    }

    type PaSimpleNew = unsafe extern "C" fn(
        server: *const c_char,
        name: *const c_char,
        dir: c_int,
        dev: *const c_char,
        stream_name: *const c_char,
        ss: *const PaSampleSpec,
        map: *const c_void,
        attr: *const PaBufferAttr,
        error: *mut c_int,
    ) -> *mut c_void;
    type PaSimpleWrite = unsafe extern "C" fn(
        s: *mut c_void,
        data: *const c_void,
        bytes: usize,
        error: *mut c_int,
    ) -> c_int;
    type PaSimpleGetLatency = unsafe extern "C" fn(s: *mut c_void, error: *mut c_int) -> u64;
    type PaSimpleFree = unsafe extern "C" fn(s: *mut c_void);

    struct PulseFns {
        write: PaSimpleWrite,
        get_latency: PaSimpleGetLatency,
        free: PaSimpleFree,
    }

    /// Conexión PulseAudio abierta y lista (aún sin hilo writer).
    pub struct PulseSink {
        pa: *mut c_void,
        fns: PulseFns,
        /// La Library debe vivir mientras los fn pointers se usen.
        _lib: libloading::Library,
        pub sample_rate: u32,
        pub channels: u16,
    }
    // El puntero pa_simple solo lo usa el hilo writer (la API simple
    // no es thread-safe, pero aquí hay UN solo usuario).
    unsafe impl Send for PulseSink {}

    impl PulseSink {
        /// Intenta dlopen(libpulse-simple) + conectar al servidor.
        /// Falla rápido y limpio si no hay lib o no hay servidor.
        pub fn try_open(sample_rate: u32, channels: u16) -> Result<PulseSink> {
            let lib = ["libpulse-simple.so.0", "libpulse-simple.so"]
                .iter()
                .find_map(|n| unsafe { libloading::Library::new(n).ok() })
                .ok_or_else(|| anyhow!("libpulse-simple no encontrada"))?;

            let (new_fn, fns) = unsafe {
                let new_fn: PaSimpleNew = *lib
                    .get::<PaSimpleNew>(b"pa_simple_new\0")
                    .map_err(|e| anyhow!("pa_simple_new: {e}"))?;
                let write: PaSimpleWrite = *lib
                    .get::<PaSimpleWrite>(b"pa_simple_write\0")
                    .map_err(|e| anyhow!("pa_simple_write: {e}"))?;
                let get_latency: PaSimpleGetLatency = *lib
                    .get::<PaSimpleGetLatency>(b"pa_simple_get_latency\0")
                    .map_err(|e| anyhow!("pa_simple_get_latency: {e}"))?;
                let free: PaSimpleFree = *lib
                    .get::<PaSimpleFree>(b"pa_simple_free\0")
                    .map_err(|e| anyhow!("pa_simple_free: {e}"))?;
                (new_fn, PulseFns { write, get_latency, free })
            };

            let ss = PaSampleSpec {
                format: PA_SAMPLE_FLOAT32LE,
                rate: sample_rate,
                channels: channels as u8,
            };
            // tlength ~100 ms: latencia contenida sin riesgo de
            // underrun en dispositivos móviles; el resto por defecto.
            let bytes_per_sec = sample_rate * channels as u32 * 4;
            let attr = PaBufferAttr {
                maxlength: u32::MAX,
                tlength: bytes_per_sec / 10,
                prebuf: u32::MAX,
                minreq: u32::MAX,
                fragsize: u32::MAX,
            };
            let app = CString::new("rtv").unwrap();
            let stream = CString::new("playback").unwrap();
            let mut err: c_int = 0;
            let pa = unsafe {
                new_fn(
                    std::ptr::null(),
                    app.as_ptr(),
                    PA_STREAM_PLAYBACK,
                    std::ptr::null(),
                    stream.as_ptr(),
                    &ss,
                    std::ptr::null(),
                    &attr,
                    &mut err,
                )
            };
            if pa.is_null() {
                return Err(anyhow!(
                    "pa_simple_new falló (err={err}) — ¿servidor PulseAudio arrancado?"
                ));
            }
            Ok(PulseSink {
                pa,
                fns,
                _lib: lib,
                sample_rate,
                channels,
            })
        }

        /// Arranca el hilo writer: consume el ring vía el feeder y
        /// bloquea en pa_simple_write (pacing natural).
        pub fn start(
            self,
            stop: Arc<AtomicBool>,
            clock: Arc<FfClock>,
            volume: Arc<AtomicU8>,
            samples_rx: Receiver<AudioChunk>,
        ) -> PulseRuntime {
            let rate = self.sample_rate;
            let ch = self.channels;
            let stop_thread = stop.clone();
            let join = thread::Builder::new()
                .name("rtv-pulse-writer".into())
                .spawn(move || {
                    let sink = self; // mover la conexión al hilo
                    let mut feeder =
                        SinkFeeder::new(stop_thread.clone(), clock, volume, samples_rx, rate, ch);
                    // Bloques de 20 ms (mismo orden que un callback cpal).
                    let frames = (rate / 50).max(64) as usize;
                    let mut buf = vec![0f32; frames * ch as usize];
                    loop {
                        if stop_thread.load(Ordering::Relaxed) {
                            break;
                        }
                        let mut err: c_int = 0;
                        let lat_us = unsafe { (sink.fns.get_latency)(sink.pa, &mut err) };
                        let lat = if lat_us == u64::MAX {
                            0.0
                        } else {
                            lat_us as f64 / 1e6
                        };
                        feeder.fill(&mut buf, lat);
                        let r = unsafe {
                            (sink.fns.write)(
                                sink.pa,
                                buf.as_ptr() as *const c_void,
                                buf.len() * 4,
                                &mut err,
                            )
                        };
                        if r < 0 {
                            // Servidor caído: salir (el reloj deja de
                            // avanzar → staleness lo congela y el
                            // vídeo espera en vez de correr solo).
                            break;
                        }
                    }
                    unsafe { (sink.fns.free)(sink.pa) };
                })
                .ok();
            PulseRuntime { join }
        }
    }

    /// Hilo writer en marcha. `stop()` con join acotado (el write
    /// bloquea como mucho ~tlength=100 ms) + detach de último recurso
    /// — espejo del patrón de DecoderHandle::stop.
    pub struct PulseRuntime {
        join: Option<thread::JoinHandle<()>>,
    }

    impl PulseRuntime {
        pub fn stop(&mut self) {
            if let Some(j) = self.join.take() {
                let deadline = std::time::Instant::now() + Duration::from_millis(500);
                loop {
                    if j.is_finished() {
                        let _ = j.join();
                        return;
                    }
                    if std::time::Instant::now() >= deadline {
                        drop(j);
                        return;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
}
