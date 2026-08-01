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
    /// Momento mural del último set (para paused clock).
    last_updated: Instant,
    /// PTS congelado durante pause.
    pts_at_pause: f64,
    /// ¿Está el reloj "anclado" a datos reales del productor?
    /// Tras un seek (`set`) pasa a false: `now()` devuelve el target
    /// CONGELADO hasta que el productor (callback de audio / frame de
    /// vídeo mostrado) haga el primer `set_pts` con el serial nuevo.
    /// Equivale al reloj NaN de ffplay tras un seek — sin esto, el
    /// reloj corría durante los ~100-300 ms que tarda el decoder en
    /// rehidratarse, el primer frame del target llegaba "tarde", se
    /// dropeaba, y el A/V arrancaba desincronizado tras cada seek.
    anchored: bool,
    /// Máxima extrapolación permitida desde el último `set_pts` real
    /// (segundos). Si el productor deja de alimentar el reloj (stall
    /// del dispositivo de audio, underrun del ring, EOF del stream de
    /// audio), `now()` se CONGELA en `pts + staleness` y `anchored()`
    /// pasa a false — el vídeo (esclavo) se detiene en vez de correr
    /// contra un reloj que ya no representa lo que se oye. Sin esto,
    /// un stall de arranque de PulseAudio de ~2 s hacía avanzar el
    /// vídeo 2 s en silencio y luego el master saltaba hacia atrás
    /// (+1900 ms de avdiff). INFINITY = sin límite (vidclk).
    staleness: f64,
}

impl FfClock {
    pub fn new() -> Arc<Self> {
        let now = Instant::now();
        Arc::new(Self {
            inner: Mutex::new(FfClockInner {
                pts: 0.0,
                last_updated: now,
                pts_at_pause: 0.0,
                anchored: false,
                staleness: f64::INFINITY,
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
        let mut g = self.inner.lock();
        g.pts = pts;
        g.last_updated = Instant::now();
        g.anchored = true;
    }

    /// Bump del serial. Llamar ANTES de tocar el pts.
    pub fn bump_serial(&self) -> i32 {
        let s = self.serial.fetch_add(1, Ordering::AcqRel) + 1;
        s
    }

    pub fn current_serial(&self) -> i32 {
        self.serial.load(Ordering::Acquire)
    }

    /// Configura la máxima extrapolación sin datos reales (ver campo
    /// `staleness`). El player la fija en el reloj de AUDIO (≈250 ms;
    /// los callbacks llegan cada 25-100 ms, así que 250 ms sin datos
    /// = el dispositivo NO está consumiendo).
    pub fn set_staleness(&self, secs: f64) {
        self.inner.lock().staleness = secs.max(0.0);
    }

    /// ¿Está el reloj anclado a datos reales del productor? Tras un
    /// `set()` (seek) devuelve false hasta el primer `set_pts` con el
    /// serial nuevo. También devuelve false si el último dato real es
    /// más viejo que `staleness` (stall/underrun/EOF del audio). El
    /// player lo usa para decidir si el vídeo debe ESPERAR (reloj
    /// congelado) o seguir el reloj corriendo.
    pub fn anchored(&self) -> bool {
        let g = self.inner.lock();
        if !g.anchored {
            return false;
        }
        if self.paused.load(Ordering::Acquire) != 0 {
            return true; // en pausa no hay datos nuevos y es normal
        }
        g.last_updated.elapsed().as_secs_f64() <= g.staleness
    }

    /// Re-apunta el target congelado SIN bumpear el serial. Se usa
    /// cuando el vídeo aterriza en el keyframe real (<= target del
    /// seek): el reloj pasa a estar congelado en el PTS de aterrizaje
    /// para que el audio arranque alineado con la imagen mostrada.
    pub fn retarget(&self, t: f64) {
        let mut g = self.inner.lock();
        g.pts = t.max(0.0);
        g.last_updated = Instant::now();
        g.pts_at_pause = t.max(0.0);
        g.anchored = false;
    }

    /// Válvula de seguridad: ancla el reloj en su pts actual aunque
    /// ningún productor haya escrito todavía. Se usa si el audio no
    /// llega en un tiempo razonable tras un seek (p.ej. seek más allá
    /// del final del stream de audio) — sin esto el vídeo se quedaba
    /// congelado para siempre esperando un anclaje que nunca llega.
    pub fn force_anchor(&self) {
        let mut g = self.inner.lock();
        // Congela el pts efectivo actual y re-arranca desde ahí
        // (cubre tanto "nunca anclado" como "anclado pero stale").
        let elapsed = g.last_updated.elapsed().as_secs_f64().min(g.staleness);
        g.pts += elapsed;
        g.last_updated = Instant::now();
        g.anchored = true;
    }
}

impl Clock for FfClock {
    fn now(&self) -> f64 {
        let g = self.inner.lock();
        if self.paused.load(Ordering::Acquire) != 0 {
            return g.pts_at_pause;
        }
        // Sin anclar (justo tras seek / arranque): el tiempo queda
        // congelado en el target hasta que llegue el primer dato real.
        if !g.anchored {
            return g.pts;
        }
        // now = pts + tiempo mural transcurrido desde el último set_pts.
        // Fórmula equivalente a la de ffplay `pts_drift + av_gettime()`
        // pero con `Instant` que es monotónico y sin origen fijo.
        // Acotado a `staleness`: sin datos frescos el reloj se congela.
        let elapsed = g.last_updated.elapsed().as_secs_f64().min(g.staleness);
        g.pts + elapsed
    }

    fn pause(&self) {
        if self.paused.swap(1, Ordering::AcqRel) == 0 {
            // Congelamos el pts efectivo en este instante (un solo lock,
            // sin ventana de carrera entre cálculo y escritura).
            let mut g = self.inner.lock();
            let elapsed = g.last_updated.elapsed().as_secs_f64().min(g.staleness);
            g.pts_at_pause = g.pts + elapsed;
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
        // Desanclar: `now()` devuelve `t` congelado hasta el primer
        // `set_pts` de un productor con el serial nuevo.
        g.anchored = false;
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
    /// Reloj “maestro” — audio si hay, video si no.
    pub fn master(&self) -> &Arc<FfClock> {
        self.audclk.as_ref().unwrap_or(&self.vidclk)
    }
    /// ¿Está el reloj maestro anclado (produciendo tiempo real)?
    pub fn master_anchored(&self) -> bool {
        self.master().anchored()
    }
    /// Re-apunta AMBOS relojes al PTS de aterrizaje real de un seek
    /// SIN bumpear seriales (los productores en vuelo siguen válidos).
    pub fn retarget(&self, t: f64) {
        self.vidclk.retarget(t);
        if let Some(a) = &self.audclk {
            a.retarget(t);
        }
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

/// Reimplementa `compute_target_delay` de ffplay.c con su semántica
/// EXACTA: `diff = get_clock(vidclk) - get_master_clock()` — es decir,
/// el drift entre el reloj de vídeo (PTS del frame EN PANTALLA,
/// extrapolado) y el maestro. NO es "PTS del próximo frame - master":
/// esa variante llevaba un +1 frame de offset baked-in que, combinado
/// con la corrección suave, dejaba un sesgo sistemático de ~-40 ms.
///
/// * Si el vídeo va TARDE respecto al master (diff <= -threshold):
///   `delay = max(0, delay + diff)` — muestra ya, o incluso dropea.
/// * Si va MUY ADELANTADO (diff >= threshold && delay > FRAMEDUP):
///   `delay = delay + diff` — espera lo justo.
/// * Si va ADELANTADO poco (diff >= threshold):
///   `delay = 2 * delay` — dobla el delay para dejarlo alcanzar.
pub fn compute_target_delay(natural_delay: f64, diff: f64) -> f64 {
    let sync_threshold = natural_delay
        .max(AV_SYNC_THRESHOLD_MIN)
        .min(AV_SYNC_THRESHOLD_MAX);

    if diff.is_finite() && diff.abs() < AV_NOSYNC_THRESHOLD {
        if diff <= -sync_threshold {
            // Vídeo tarde → mostrar YA.
            return (natural_delay + diff).max(0.0);
        } else if diff >= sync_threshold
            && (natural_delay > AV_SYNC_FRAMEDUP_THRESHOLD || diff > AV_SYNC_THRESHOLD_MAX)
        {
            // Muy adelantado (o salto grande del master hacia atrás,
            // p.ej. re-anclaje del audio tras un stall): esperar EXACTO.
            // Con el doblado de ffplay un salto de +300 ms tardaba ~8
            // frames en converger, todos mostrados fuera de sync.
            return natural_delay + diff;
        } else if diff >= sync_threshold {
            return 2.0 * natural_delay;
        }
        // Corrección SUAVE dentro del umbral: ffplay tolera hasta
        // ±sync_threshold sin corregir, lo que deja offsets
        // sistemáticos de ~±40 ms clavados para siempre (p.ej. el
        // establecido al anclar el audio tras un seek).
        //
        // Dos regímenes:
        //   * |diff| <= 10 ms → corrección COMPLETA en un frame
        //     (acotada a ±30% del delay natural). Desplazar la
        //     presentación <=10 ms es invisible, y elimina de golpe
        //     el residuo que la corrección proporcional dejaba
        //     muriendo geométricamente (décimas de ms de mediana en
        //     el sync-log post-seek durante segundos).
        //   * |diff| > 10 ms → corrección proporcional (50% del diff
        //     por frame, misma cota) que converge sin jitter visible.
        let correction = if diff.abs() <= 0.010 {
            diff.clamp(-natural_delay * 0.3, natural_delay * 0.3)
        } else {
            (diff * 0.5).clamp(-natural_delay * 0.3, natural_delay * 0.3)
        };
        return (natural_delay + correction).max(0.0);
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
        let t = c.now();
        assert!(t >= 10.09 && t < 10.20, "esperado ~10.10, got {t}");
    }

    #[test]
    fn ffclock_frozen_after_seek_until_anchored() {
        let c = FfClock::new();
        c.set_pts(5.0, 0);
        c.set(42.0); // seek → desanclado
        sleep(Duration::from_millis(80));
        // Congelado en el target mientras no llegue dato real.
        assert!((c.now() - 42.0).abs() < 0.001, "reloj corrió desanclado: {}", c.now());
        // Primer dato real con serial nuevo → re-ancla y corre.
        c.set_pts(42.0, c.current_serial());
        sleep(Duration::from_millis(60));
        assert!(c.now() > 42.05, "reloj no corre tras re-anclar: {}", c.now());
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
        // Vídeo justo en sync (diff=0) → delay natural intacto.
        assert!((compute_target_delay(0.040, 0.0) - 0.040).abs() < 1e-9);
        // Vídeo TARDE 200ms → delay = max(0, 0.04-0.2) = 0.
        assert_eq!(compute_target_delay(0.040, -0.2), 0.0);
        // Vídeo ADELANTADO 60ms (delay < FRAMEDUP, diff < MAX) → doble delay.
        let d = compute_target_delay(0.040, 0.06);
        assert!((d - 0.080).abs() < 1e-9, "esperado 0.080, got {d}");
        // Vídeo MUY adelantado (200ms > THRESHOLD_MAX) → espera exacta.
        let d = compute_target_delay(0.040, 0.2);
        assert!((d - 0.240).abs() < 1e-9, "esperado 0.240, got {d}");
        // Diff > NOSYNC (10s) → devuelve delay natural sin ajuste.
        assert_eq!(compute_target_delay(0.040, 95.0), 0.040);
    }

    #[test]
    fn compute_target_delay_small_diff_full_correction() {
        // |diff| <= 10 ms → corrección COMPLETA en un frame (invisible
        // al ojo, elimina el residuo post-seek en una pasada).
        let d = compute_target_delay(0.040, 0.008);
        assert!((d - 0.048).abs() < 1e-9, "esperado 0.048, got {d}");
        let d = compute_target_delay(0.040, -0.008);
        assert!((d - 0.032).abs() < 1e-9, "esperado 0.032, got {d}");
        // Pero acotada a ±30% del delay natural (natural muy corto).
        let d = compute_target_delay(0.010, 0.009);
        assert!((d - 0.013).abs() < 1e-9, "esperado 0.013 (cap 30%), got {d}");
        // |diff| > 10 ms → régimen proporcional (50%), también acotado.
        let d = compute_target_delay(0.040, 0.020);
        assert!((d - 0.050).abs() < 1e-9, "esperado 0.050, got {d}");
    }

    #[test]
    fn retarget_keeps_serial_and_unanchors() {
        let c = FfClock::new();
        c.set(30.0); // seek → serial+1, congelado en 30
        let s = c.current_serial();
        c.retarget(27.5); // aterrizaje en keyframe real
        assert_eq!(c.current_serial(), s, "retarget no debe bumpear serial");
        assert!((c.now() - 27.5).abs() < 0.001, "congelado en el landing pts");
        c.set_pts(27.5, s);
        sleep(Duration::from_millis(50));
        assert!(c.now() > 27.52, "reloj corre tras anclar en el landing");
    }

    #[test]
    fn staleness_freezes_and_unanchors() {
        let c = FfClock::new();
        c.set_staleness(0.08);
        c.set_pts(5.0, 0);
        assert!(c.anchored());
        sleep(Duration::from_millis(150));
        // Congelado en pts + staleness, y des-anclado.
        assert!((c.now() - 5.08).abs() < 0.02, "now={}", c.now());
        assert!(!c.anchored(), "debería estar stale");
        // Un dato fresco re-ancla y el reloj corre de nuevo.
        c.set_pts(5.05, 0);
        assert!(c.anchored());
        sleep(Duration::from_millis(40));
        assert!(c.now() > 5.08 && c.now() < 5.15, "now={}", c.now());
    }

    #[test]
    fn force_anchor_starts_clock() {
        let c = FfClock::new();
        c.set(10.0);
        assert!(!c.anchored());
        c.force_anchor();
        assert!(c.anchored());
        sleep(Duration::from_millis(50));
        assert!(c.now() > 10.04, "reloj corre tras force_anchor: {}", c.now());
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
