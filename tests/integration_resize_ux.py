#!/usr/bin/env python3
"""Test de UX de RESIZE para rtv — latencia, basura visual y parpadeo.

Cubre exactamente los síntomas reportados:

  A. LATENCIA: el redibujo tras un resize debe ser (casi) instantáneo.
     Medimos el tiempo entre enviar SIGWINCH y ver el clear (`ESC[2J`)
     del redibujo. Umbral: p95 < 250 ms (antes: hasta ~500 ms por el
     thread::sleep no interrumpible + cola de frames con dims viejas).

  B. BASURA VISUAL en terminal pequeña: tras encoger a un tamaño
     diminuto, NINGUNA secuencia de posicionamiento de cursor debe
     apuntar fuera de los límites (fila > rows o columna > cols) y no
     debe haber saltos de línea sueltos (scroll). Además el stream
     debe llevar autowrap desactivado (DECAWM off, `ESC[?7l`).

  C. PARPADEO del HUD: en reproducción estable el HUD solo debe
     reescribirse cuando cambia su contenido (~1-2 veces/s por el
     reloj), no a fps completos. Contamos escrituras a la fila del HUD
     por segundo. Umbral: <= 4/s. Y en terminal minúscula (< 16 cols o
     < 5 filas) el HUD debe estar OCULTO (0 escrituras).

  D. Render coherente en terminal pequeña (pyte): emulamos la terminal
     y comprobamos que tras el resize pequeño la pantalla contiene
     contenido del vídeo dentro de los límites, sin restos de HUD.

Uso: python3 tests/integration_resize_ux.py <video>
"""
import os, pty, re, sys, time, subprocess, select, signal
import fcntl, termios, struct, threading

import pyte

VIDEO = sys.argv[1]
BIN = os.path.join(os.path.dirname(__file__), "..", "target", "release", "rtv")

env = dict(os.environ)
env["TERM"] = "xterm-256color"

master, slave = pty.openpty()

def set_winsize(rows, cols):
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

COLS0, ROWS0 = 100, 30
set_winsize(ROWS0, COLS0)

proc = subprocess.Popen(
    [BIN, VIDEO, "--backend", "ascii"],
    stdin=slave, stdout=slave, stderr=subprocess.DEVNULL, env=env,
)

# ---- Lector continuo con timestamps por chunk ----
chunks = []           # [(t, bytes)]
chunks_lock = threading.Lock()
_stop = threading.Event()

def _reader():
    while not _stop.is_set():
        r, _, _ = select.select([master], [], [], 0.02)
        if r:
            try:
                data = os.read(master, 1 << 20)
            except OSError:
                return
            if data:
                with chunks_lock:
                    chunks.append((time.monotonic(), data))

reader_t = threading.Thread(target=_reader, daemon=True)
reader_t.start()

def resize(rows, cols):
    set_winsize(rows, cols)
    proc.send_signal(signal.SIGWINCH)

def wait_alive(secs):
    t0 = time.monotonic()
    while time.monotonic() - t0 < secs:
        if proc.poll() is not None:
            print(f"FAIL: rtv murió (exit={proc.returncode})")
            sys.exit(1)
        time.sleep(0.05)

def bytes_since(t):
    with chunks_lock:
        return b"".join(d for (ts, d) in chunks if ts >= t)

def find_after(t_mark, needle, timeout=2.0):
    """Devuelve el timestamp del primer chunk >= t_mark que contiene
    `needle`, o None."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        with chunks_lock:
            for ts, d in chunks:
                if ts >= t_mark and needle in d:
                    return ts
        time.sleep(0.005)
    return None

fails = []

# ================= Warmup =================
wait_alive(3.0)

raw_start = bytes_since(0)
# B(parte): autowrap desactivado al arrancar.
if b"\x1b[?7l" not in raw_start:
    fails.append("no se desactiva el autowrap (ESC[?7l) al arrancar")

# ================= A. Latencia de resize =================
latencies = []
sizes = [(20, 60), (35, 110), (15, 45), (40, 130), (25, 80),
         (12, 36), (45, 150), (30, 100)]
for (r, c) in sizes:
    time.sleep(0.35)  # dejar estabilizar (sin 2J pendientes)
    t_mark = time.monotonic()
    resize(r, c)
    t_seen = find_after(t_mark, b"\x1b[2J", timeout=2.0)
    if t_seen is None:
        fails.append(f"resize a {c}x{r}: sin redibujo (2J) en 2 s")
    else:
        latencies.append(t_seen - t_mark)
if latencies:
    latencies.sort()
    p95 = latencies[max(0, int(len(latencies) * 0.95) - 1)]
    print(f"[latencia] n={len(latencies)} min={latencies[0]*1000:.0f}ms "
          f"mediana={latencies[len(latencies)//2]*1000:.0f}ms p95={p95*1000:.0f}ms")
    if p95 > 0.25:
        fails.append(f"latencia p95 de resize {p95*1000:.0f}ms > 250ms")

# ================= C1. Parpadeo del HUD en estable =================
resize(30, 100)
time.sleep(0.5)
t0 = time.monotonic()
wait_alive(3.0)
raw = bytes_since(t0)
# Escrituras a la fila 30 (HUD 1 línea a 100x30) — patrón ESC[30;1H.
hud_writes = raw.count(b"\x1b[30;1H")
rate = hud_writes / 3.0
print(f"[hud] escrituras/s en estable = {rate:.1f}")
if rate > 4.0:
    fails.append(f"HUD se reescribe {rate:.1f} veces/s (>4) — parpadeo")

# ================= B + C2 + D. Terminal minúscula =================
TR, TC = 4, 12   # 12 cols × 4 filas: por debajo del umbral del HUD
resize(TR, TC)
time.sleep(0.4)  # margen para drenar frames con dims viejas
t0 = time.monotonic()
wait_alive(2.5)
raw = bytes_since(t0)

# B: ninguna posición de cursor fuera de límites.
cup = re.compile(rb"\x1b\[(\d+);(\d+)H")
out_of_bounds = []
for m in cup.finditer(raw):
    rr, cc = int(m.group(1)), int(m.group(2))
    if rr > TR or cc > TC:
        out_of_bounds.append((rr, cc))
print(f"[bounds] posiciones fuera de {TC}x{TR}: {len(out_of_bounds)}"
      + (f" ej={out_of_bounds[:5]}" if out_of_bounds else ""))
if out_of_bounds:
    fails.append(f"{len(out_of_bounds)} escrituras fuera de límites en "
                 f"terminal {TC}x{TR} (basura visual)")

# B: sin newlines sueltos que provoquen scroll.
if b"\n" in raw.replace(b"\r\n", b""):
    fails.append("newlines en el stream de render (scroll fantasma)")

# C2: HUD oculto en terminal minúscula (ninguna escritura a la fila TR
# que sea de texto HUD — el vídeo sí puede pintar la fila TR).
# Verificación textual con pyte más abajo.
hud_row_writes = raw.count(f"\x1b[{TR};1H".encode())

# D: emulación con pyte — la pantalla no debe contener texto de HUD
# ("q=", "vol", "fps", "▶") y sí contenido de vídeo (chars del degradado).
screen = pyte.Screen(TC, TR)
stream = pyte.ByteStream(screen)
screen.resize(TR, TC)
try:
    stream.feed(raw)
except Exception as e:
    fails.append(f"pyte no pudo parsear el stream: {e}")
disp = "\n".join(screen.display)
for token in ("q=", "vol", "fps", "▶", "⏸"):
    if token in disp:
        fails.append(f"HUD visible en terminal minúscula ({token!r} en pantalla)")
        break
video_chars = sum(disp.count(ch) for ch in ".:-=+*#%@")
print(f"[tiny] pantalla {TC}x{TR}: hud_row_writes={hud_row_writes} "
      f"video_chars={video_chars}")
if video_chars == 0:
    fails.append("sin contenido de vídeo en terminal minúscula")

# ================= Vuelta a grande: recuperación =================
resize(35, 120)
t_mark = time.monotonic()
t_seen = find_after(t_mark, b"\x1b[2J", timeout=2.0)
if t_seen is None:
    fails.append("sin redibujo al volver a agrandar")
else:
    print(f"[recuperación] redibujo al agrandar en {(t_seen-t_mark)*1000:.0f}ms")
wait_alive(2.0)

# ================= Salida limpia =================
os.write(master, b"q")
t0 = time.monotonic()
while proc.poll() is None and time.monotonic() - t0 < 10:
    time.sleep(0.02)
if proc.poll() is None:
    proc.kill()
    fails.append("rtv no salió con q")
elif proc.returncode != 0:
    fails.append(f"exit code {proc.returncode} != 0")

_stop.set(); reader_t.join(timeout=2)
os.close(master); os.close(slave)

if fails:
    print("\nFAIL:")
    for f in fails:
        print(" -", f)
    sys.exit(1)
print("\nOK: resize instantáneo, sin basura visual ni parpadeo en terminal pequeña")
