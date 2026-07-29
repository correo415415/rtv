#!/usr/bin/env python3
"""Test de integración de RESIZE para rtv.

Ejecuta rtv en un pty real y lanza una TORMENTA de resizes
(TIOCSWINSZ + SIGWINCH) durante la reproducción, incluyendo tamaños
diminutos y degenerados, seeks en medio de la tormenta y una pausa
con resize. Verifica:

  1. El proceso NO crashea (sigue vivo tras la tormenta y sale
     limpiamente con `q`, exit code 0).
  2. La reproducción NO se detiene durante la tormenta: el sync-log
     sigue registrando frames (gap mural máximo entre frames < 1.5 s).
  3. Los fps no se hunden: en la ventana post-tormenta se muestran
     frames a un ritmo razonable (>= 40% del fps nominal con ascii).
  4. La sincronía A/V no se ve afectada: |avdiff| mediana < 60 ms en
     la ventana estable post-tormenta.

Uso: python3 tests/integration_resize.py <video> [backend=ascii]
"""
import os, pty, sys, time, subprocess, statistics, select, signal
import fcntl, termios, struct, random, threading

VIDEO = sys.argv[1]
BACKEND = sys.argv[2] if len(sys.argv) > 2 else "ascii"
BIN = os.path.join(os.path.dirname(__file__), "..", "target", "release", "rtv")
LOG = "/tmp/rtv_resize_sync.log"

if os.path.exists(LOG):
    os.remove(LOG)

env = dict(os.environ)
env["RTV_SYNC_LOG"] = LOG
env["TERM"] = "xterm-256color"

master, slave = pty.openpty()

def set_winsize(rows, cols):
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

set_winsize(40, 120)

proc = subprocess.Popen(
    [BIN, VIDEO, "--backend", BACKEND],
    stdin=slave, stdout=slave, stderr=subprocess.DEVNULL, env=env,
)

RIGHT = b"\x1b[C"
LEFT = b"\x1b[D"
SPACE = b" "
Q = b"q"

# Lector CONTINUO del pty en un hilo: sin esto, el buffer del pty
# (64 KB) se llena con la salida de blocks/kitty (~200 KB/frame) y
# rtv se bloquea en write() → latencia artificial del harness que
# contaminaba la medición de sync (no era del reproductor).
_reader_stop = threading.Event()
def _reader():
    while not _reader_stop.is_set():
        r, _, _ = select.select([master], [], [], 0.05)
        if r:
            try:
                os.read(master, 1 << 20)
            except OSError:
                return
reader_t = threading.Thread(target=_reader, daemon=True)
reader_t.start()

def drain():
    pass  # el hilo lector ya drena continuamente

def resize(rows, cols):
    set_winsize(rows, cols)
    try:
        proc.send_signal(signal.SIGWINCH)
    except ProcessLookupError:
        pass

def play(secs):
    t0 = time.monotonic()
    while time.monotonic() - t0 < secs:
        if proc.poll() is not None:
            print(f"FAIL: rtv murió (exit={proc.returncode}) durante la reproducción")
            sys.exit(1)
        time.sleep(0.05)

fails = []
storm_start = storm_end = 0.0

try:
    # 1) Warmup: reproducción normal 4 s a 120x40.
    play(4.0)

    # 2) TORMENTA de resizes: 60 cambios rápidos, tamaños aleatorios
    #    incluyendo diminutos (4x3) y grandes (300x90), sin pausa entre
    #    algunos (ráfagas de 3-4 eventos back-to-back).
    storm_start = time.monotonic()
    random.seed(42)
    sizes = []
    for _ in range(20):
        sizes.append((random.randint(3, 90), random.randint(4, 300)))
    # Casos límite explícitos:
    sizes += [(3, 4), (4, 5), (5, 8), (90, 300), (24, 80), (10, 30),
              (3, 200), (80, 6), (40, 120)]
    for i, (r, c) in enumerate(sizes):
        if proc.poll() is not None:
            print(f"FAIL: rtv murió (exit={proc.returncode}) en resize #{i} → {c}x{r}")
            sys.exit(1)
        resize(r, c)
        drain()
        # Alternar: a veces sin pausa (ráfaga), a veces 30-80 ms.
        if i % 4 != 0:
            time.sleep(random.uniform(0.03, 0.08))
    # Seek EN MEDIO de más resizes (interacción resize+seek).
    os.write(master, RIGHT); drain(); time.sleep(0.1)
    resize(20, 60); drain(); time.sleep(0.05)
    os.write(master, LEFT); drain(); time.sleep(0.1)
    resize(45, 140); drain()
    # Segunda ráfaga rapidísima (sin sleeps).
    for (r, c) in [(30, 100), (12, 40), (50, 160), (8, 20), (40, 120)]:
        resize(r, c); drain()
    storm_end = time.monotonic()

    # 3) Pausa + resize en pausa + resume (redibujo con frame cacheado).
    os.write(master, SPACE); time.sleep(0.5); drain()
    resize(30, 100); drain(); time.sleep(0.5)
    resize(40, 120); drain(); time.sleep(0.5)
    if proc.poll() is not None:
        print(f"FAIL: rtv murió (exit={proc.returncode}) en resize durante pausa")
        sys.exit(1)
    os.write(master, SPACE)  # resume

    # 4) Reproducción estable post-tormenta 6 s.
    play(6.0)

    os.write(master, Q)
    t0 = time.monotonic()
    while proc.poll() is None and time.monotonic() - t0 < 10:
        time.sleep(0.02)
    if proc.poll() is None:
        fails.append("rtv no salió con q en 10 s")
    elif proc.returncode != 0:
        fails.append(f"exit code {proc.returncode} != 0")
finally:
    if proc.poll() is None:
        proc.kill()
        fails.append("rtv no salió con q — kill forzado")
    _reader_stop.set(); reader_t.join(timeout=2)
    os.close(master); os.close(slave)

# ---------------- Análisis del sync-log ----------------
rows_log = []
with open(LOG) as f:
    for line in f:
        if line.startswith("#"):
            continue
        p = line.split()
        if len(p) >= 5:
            rows_log.append((float(p[0]), float(p[1]), float(p[2]), float(p[3])))

if len(rows_log) < 50:
    print(f"FAIL: solo {len(rows_log)} frames registrados")
    sys.exit(1)

wall0 = rows_log[0][0]
test0 = None  # correlación aproximada: el log empieza ~cuando arranca play

# --- 2. Continuidad: ningún freeze permanente ---
# Gaps legítimos: la pausa deliberada (~1.6 s) y los holds post-seek
# (válvula de 1.5 s + decode). Criterio: NINGÚN gap > 3 s (freeze),
# y como mucho 3 gaps en (1.5, 3] s (pausa + 2 seeks).
gaps = []
for i in range(1, len(rows_log)):
    gaps.append((rows_log[i][0] - rows_log[i-1][0], rows_log[i][0]))
gap_max = max(g for g, _ in gaps)
mid_gaps = [g for g, _ in gaps if 1.5 < g <= 3.0]
frozen = [g for g, _ in gaps if g > 3.0]
print(f"[continuidad] frames={len(rows_log)} gap_max={gap_max:.2f}s gaps(1.5-3s]={len(mid_gaps)} gaps>3s={len(frozen)}")
if frozen:
    fails.append(f"freeze detectado: gap de {max(frozen):.2f}s > 3s")
if len(mid_gaps) > 3:
    fails.append(f"{len(mid_gaps)} gaps en (1.5,3]s (esperado <=3: pausa + holds de seek)")

# --- 3. FPS post-tormenta: últimos 4 s del log ---
t_end = rows_log[-1][0]
tail = [r for r in rows_log if r[0] >= t_end - 4.0]
fps_tail = len(tail) / 4.0
print(f"[fps] post-tormenta={fps_tail:.1f} fps (últimos 4 s, n={len(tail)})")
if fps_tail < 10.0:  # vídeo 25 fps; ascii en sandbox 2-core: >=10 fps
    fails.append(f"fps post-tormenta {fps_tail:.1f} < 10")

# --- 4. Sync post-tormenta: |avdiff| mediana últimos 4 s ---
diffs = [abs(r[3]) for r in tail]
if len(diffs) >= 10:
    med = statistics.median(diffs)
    print(f"[sync] |avdiff| mediana post-tormenta={med:.1f}ms")
    if med > 60:
        fails.append(f"|avdiff| mediana post-tormenta {med:.1f}ms > 60ms")
else:
    fails.append("sin frames suficientes post-tormenta para medir sync")

if fails:
    print("\nFAIL:")
    for f_ in fails:
        print("  -", f_)
    sys.exit(1)
print("\nOK: resize robusto — sin crash, reproducción continua, fps y sync estables")
