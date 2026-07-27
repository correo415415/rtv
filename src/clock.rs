//! clock.rs — v0.5: reloj estilo ffplay.c
//!
//! Reescrito siguiendo la implementación de ffplay/mpv (referencia:
//! FFmpeg/fftools/ffplay.c, funciones `set_clock_at`, `get_clock`,
//! `compute_target_delay`, `sync_clock_to_slave`).
//!
//! Idea central: el Clock guarda `pts_drift = pts - time`, no un
//! contador acumulado que hay que “avanzar”. Cuando algo (callback de
//! audio, refresh de vídeo, seek) actualiza el reloj, hace un único
//! `set(pts, serial)` y a partir de ese momento `now()` interpola
//! `pts_drift + time` en tiempo mural. Ya no hay `advance()` sumando
//! µs muestra a muestra — un patrón que era la fuente de todas las
//! races y de la desincronización tras seek.
//!
//! Doble reloj:
//!   * `audclk`: se pone al PTS del último frame de audio JUSTO en el
//!     momento en que el callback de cpal lo emite hacia el hardware.
//!     Compensa el `playback_delay` (samples pendientes en el buffer
//!     del driver) para que “now” refleje lo que el usuario oye.
//!   * `vidclk`: se pone al PTS del último frame de vídeo mostrado.
//!     El player lo actualiza en cada frame renderizado.
//!
//! El reloj maestro es `audclk` si hay audio, `vidclk` si no.
//!
//! El `serial` (equivalente al `seek_epoch` de la v0.4) invalida
//! samples/frames obsoletos tras un seek. Cualquier `set()` con
//! serial != master serial se ignora, y `now()` devuelve NaN
//! (interpretado por el player como “aún no hay reloj útil,
//! muestra el siguiente frame ya”).

use parking_lot::Mutex;
use std::sync::atomic::{AtomicI32, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// Thresholds de ffplay.c — no los cambies sin razón.
pub const AV_SYNC_THRESHOLD_MIN: f64 = 0.04; // 40 ms
pub const AV_SYNC_THRESHOLD_MAX: f64 = 0.10; // 100 ms
pub const AV_SYNC_FRAMEDUP_THRESHOLD: f64 = 0.10;
pub const AV_NOSYNC_THRESHOLD: f64 = 10.0; // reset tras diff > 10 s

pub trait Clock: Send + Sync {
    fn now(&self) -> f64;
    fn pause(&self);
    fn resume(&self);
    fn is_paused(&self) -> bool;
    fn set(&self, t: f64);
}

/// Reloj interno estilo ffplay: `pts_drift + time` en modo play,
/// `pts` en modo pause. Sin acumuladores, sin advance() por muestra.
pub struct FfClock {
    inner: Mutex<FfClockInner>,
    pub paused: AtomicU8, // 0=play, 1=pause
    /// Serial monotónico. El productor (audio callback / video loop)
    /// escribe con su serial actual; si no coincide con éste al
    /// hacer `set_pts`, se ignora (residuo tras seek).
    pub serial: AtomicI32,
}

struct FfClockInner {
    /// PTS "base" (segundos absolutos en el media).
    pts: f64,
    /// `pts - wall_time_at_update` — cuando el reloj corre,
    /// `now = pts_drift + wall_now`. Es el truco de ffplay.
    pts_drift: f64,
    /// Momento mural del último set (para paused clock).
    last_updated: Instant,
    /// PTS congelado durante pause.
    pts_at_pause: f64,
}

impl FfClock {
    pub fn new() -> Arc<Self> {
        let now = Instant::now();
        Arc::new(Self {
            inner: Mutex::new(FfClockInner {
                pts: 0.0,
                pts_drift: 0.0,
                last_updated: now,
                pts_at_pause: 0.0,
            }),
            paused: AtomicU8::new(0),
            serial: AtomicI32::new(0),
        })
    }

    /// Escribe `pts` como nuevo punto de referencia. Sólo si `serial`
    /// coincide con el actual — así invalidamos writes de un decoder
    /// que aún no vio el seek.
    pub fn set_pts(&self, pts: f64, serial: i32) {
        if serial != self.serial.load(Ordering::Acquire) {
            return; // residuo tras seek
        }
        let time = Instant::now();
        let mut g = self.inner.lock();
        g.pts = pts;
        g.pts_drift = pts - time.elapsed_secs_from(g.last_updated);
        g.last_updated = time;
        // Re-derivamos pts_drift limpiamente:
        // pts_drift = pts - now_mural. `now_mural` = 0 desde el
        // inicio de esta llamada. Para no acoplarnos a un origen
        // arbitrario, guardamos el `Instant` en `last_updated` y en
        // `now()` calculamos `pts + last_updated.elapsed()`.
        g.pts_drift = pts;
    }

    /// Bump del serial. Llamar ANTES de tocar el pts.
    pub fn bump_serial(&self) -> i32 {
        let s = self.serial.fetch_add(1, Ordering::AcqRel) + 1;
        s
    }

    pub fn current_serial(&self) -> i32 {
        self.serial.load(Ordering::Acquire)
    }
}

// Extensión práctica para calcular elapsed relativo (evita panics si
// last_updated está en el futuro respecto a `time`, cosa que no debería
// pasar pero curémonos en salud).
trait InstantExt {
    fn elapsed_secs_from(&self, earlier: Instant) -> f64;
}
impl InstantExt for Instant {
    fn elapsed_secs_from(&self, earlier: Instant) -> f64 {
        self.saturating_duration_since(earlier).as_secs_f64()
    }
}

impl Clock for FfClock {
    fn now(&self) -> f64 {
        let g = self.inner.lock();
        if self.paused.load(Ordering::Acquire) != 0 {
            return g.pts_at_pause;
        }
        // now = pts + tiempo mural transcurrido desde el último set_pts.
        // Fórmula equivalente a la de ffplay `pts_drift + av_gettime()`
        // pero con `Instant` que es monotónico y sin origen fijo.
        let elapsed = g.last_updated.elapsed().as_secs_f64();
        g.pts + elapsed
    }

    fn pause(&self) {
        if self.paused.swap(1, Ordering::AcqRel) == 0 {
            // Congelamos el pts efectivo en este instante.
            let g = self.inner.lock();
            let frozen = g.pts + g.last_updated.elapsed().as_secs_f64();
            drop(g);
            self.inner.lock().pts_at_pause = frozen;
        }
    }

    fn resume(&self) {
        if self.paused.swap(0, Ordering::AcqRel) != 0 {
            // Reanudar SIN salto: fijamos pts = pts_at_pause y
            // last_updated = ahora, así `now()` = pts_at_pause al
            // instante inmediato tras resume.
            let now = Instant::now();
            let mut g = self.inner.lock();
            g.pts = g.pts_at_pause;
            g.last_updated = now;
        }
    }

    fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire) != 0
    }

    /// Seek absoluto. Bumpea el serial (invalida writes en vuelo) y
    /// fija el reloj a `t` con el nuevo serial.
    fn set(&self, t: f64) {
        let new_serial = self.bump_serial();
        let now = Instant::now();
        let mut g = self.inner.lock();
        g.pts = t.max(0.0);
        g.last_updated = now;
        g.pts_at_pause = t.max(0.0);
        // Serial ya bumpeado atómicamente arriba.
        let _ = new_serial;
    }
}

// -------------------- Master clock chooser --------------------

/// Wrap dos `FfClock` (audio + video). El master se elige por
/// existencia de audio, con fallback a video. Expone `Clock` para
/// que el player siga usándolo como antes.
pub struct MasterClock {
    audclk: Option<Arc<FfClock>>,
    vidclk: Arc<FfClock>,
    /// Estado local de pausa que se propaga a ambos relojes.
    paused: AtomicU8,
}

impl MasterClock {
    pub fn with_audio(audclk: Arc<FfClock>, vidclk: Arc<FfClock>) -> Arc<Self> {
        Arc::new(Self {
            audclk: Some(audclk),
            vidclk,
            paused: AtomicU8::new(0),
        })
    }
    pub fn video_only(vidclk: Arc<FfClock>) -> Arc<Self> {
        Arc::new(Self {
            audclk: None,
            vidclk,
            paused: AtomicU8::new(0),
        })
    }
    pub fn audclk(&self) -> Option<&Arc<FfClock>> {
        self.audclk.as_ref()
    }
    pub fn vidclk(&self) -> &Arc<FfClock> {
        &self.vidclk
    }
    /// Reloj “maestro” — audio si hay, video si no.
    pub fn master(&self) -> &Arc<FfClock> {
        self.audclk.as_ref().unwrap_or(&self.vidclk)
    }
}

impl Clock for MasterClock {
    fn now(&self) -> f64 {
        self.master().now()
    }
    fn pause(&self) {
        self.paused.store(1, Ordering::Release);
        self.vidclk.pause();
        if let Some(a) = &self.audclk {
            a.pause();
        }
    }
    fn resume(&self) {
        self.paused.store(0, Ordering::Release);
        self.vidclk.resume();
        if let Some(a) = &self.audclk {
            a.resume();
        }
    }
    fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire) != 0
    }
    fn set(&self, t: f64) {
        // Un `set` global (por el player) bumpea AMBOS seriales y
        // fija AMBOS relojes al `target`. Los productores (audio
        // callback / video loop) verán serial nuevo y descartarán
        // sus writes en vuelo con serial viejo.
        self.vidclk.set(t);
        if let Some(a) = &self.audclk {
            a.set(t);
        }
    }
}

// -------------------- compute_target_delay --------------------

/// Reimplementa `compute_target_delay` de ffplay.c: dado el delay
/// natural entre frames (por PTS), lo ajusta al drift respecto al
/// master. Devuelve segundos a dormir antes de mostrar el próximo frame.
///
/// * Si el vídeo va TARDE respecto al master (diff <= -threshold):
///   `delay = max(0, delay + diff)` — muestra ya, o incluso dropea.
/// * Si va MUY ADELANTADO (diff >= threshold && delay > FRAMEDUP):
///   `delay = delay + diff` — espera lo justo.
/// * Si va ADELANTADO poco (diff >= threshold):
///   `delay = 2 * delay` — dobla el delay para dejarlo alcanzar.
pub fn compute_target_delay(natural_delay: f64, video_pts: f64, master_now: f64) -> f64 {
    let diff = video_pts - master_now;
    let sync_threshold = natural_delay
        .max(AV_SYNC_THRESHOLD_MIN)
        .min(AV_SYNC_THRESHOLD_MAX);

    if diff.is_finite() && diff.abs() < AV_NOSYNC_THRESHOLD {
        if diff <= -sync_threshold {
            // Vídeo tarde → mostrar YA.
            return (natural_delay + diff).max(0.0);
        } else if diff >= sync_threshold && natural_delay > AV_SYNC_FRAMEDUP_THRESHOLD {
            return natural_delay + diff;
        } else if diff >= sync_threshold {
            return 2.0 * natural_delay;
        }
    }
    natural_delay
}

/// Duración natural entre frames por PTS. Si es inválida (NaN, ≤0,
/// >max_frame_duration), cae al `fallback` (ej. 1/fps).
pub fn vp_duration(cur_pts: f64, next_pts: f64, fallback: f64, max: f64) -> f64 {
    let d = next_pts - cur_pts;
    if !d.is_finite() || d <= 0.0 || d > max {
        fallback
    } else {
        d
    }
}

#[allow(dead_code)]
pub fn sleep_until(clock: &dyn Clock, target_secs: f64) {
    let now = clock.now();
    if target_secs > now {
        let delta = (target_secs - now).min(0.5);
        std::thread::sleep(Duration::from_secs_f64(delta));
    }
}

// -------------------- Tests --------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn ffclock_now_advances_with_wall() {
        let c = FfClock::new();
        c.set_pts(10.0, 0);
        sleep(Duration::from_millis(100));
        let t = {
            let g = c.inner.lock();
            g.pts + g.last_updated.elapsed().as_secs_f64()
        };
        assert!(t >= 10.09 && t < 10.20, "esperado ~10.10, got {t}");
    }

    #[test]
    fn ffclock_set_bumps_serial_and_ignores_old_writes() {
        let c = FfClock::new();
        let old_serial = c.current_serial();
        c.set(42.0);
        assert_eq!(c.current_serial(), old_serial + 1);
        // Un writer con serial viejo NO debe modificar el pts.
        c.set_pts(999.0, old_serial);
        let g = c.inner.lock();
        assert!(g.pts >= 41.9 && g.pts <= 42.1, "el set_pts residuo mutó pts: {}", g.pts);
    }

    #[test]
    fn compute_target_delay_matches_ffplay_ranges() {
        // Vídeo justo en sync → delay natural intacto.
        assert!((compute_target_delay(0.040, 5.0, 5.0) - 0.040).abs() < 1e-9);
        // Vídeo TARDE 200ms → delay = max(0, 0.04-0.2) = 0.
        assert_eq!(compute_target_delay(0.040, 4.8, 5.0), 0.0);
        // Vídeo ADELANTADO 200ms (delay < FRAMEDUP) → doble delay.
        let d = compute_target_delay(0.040, 5.2, 5.0);
        assert!((d - 0.080).abs() < 1e-9, "esperado 0.080, got {d}");
        // Diff > NOSYNC (10s) → devuelve delay natural sin ajuste.
        assert_eq!(compute_target_delay(0.040, 100.0, 5.0), 0.040);
    }

    #[test]
    fn pause_resume_no_jump() {
        let c = FfClock::new();
        c.set_pts(20.0, 0);
        sleep(Duration::from_millis(50));
        c.pause();
        let t_paused = c.now();
        sleep(Duration::from_millis(200));
        // Durante pausa el reloj NO avanza.
        assert!((c.now() - t_paused).abs() < 0.001, "el reloj avanzó durante pausa");
        c.resume();
        // Justo tras resume, `now()` == valor congelado (sin salto).
        assert!((c.now() - t_paused).abs() < 0.005);
    }
}
