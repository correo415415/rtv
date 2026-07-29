#!/usr/bin/env python3
"""Test de RECUPERACIÓN DE CALIDAD al AGRANDAR la terminal.

Síntoma reportado: al encoger la terminal el cambio de calidad es
instantáneo, pero al agrandarla tarda en volver la calidad buena.

Causa raíz: la cola de pre-decodificación guarda hasta ~2.5 s de
frames ya escalados a las dims VIEJAS (pequeñas); el player los
reescala con nearest (borroso) hasta que el decoder alcanza las dims
nuevas.

Fix: "refine-seek" — 300 ms después del último resize que AGRANDA, el
decoder vacía su cola y re-decodifica desde el punto actual de
reproducción a las dims nuevas (hr-seek exacto con drop_until), sin
tocar relojes ni audio.

Medición: RTV_SYNC_LOG ahora incluye las dims del frame mostrado
(columnas 6 y 7: `wall master pts avdiff dropped w h`). Medimos el
tiempo de pared entre el primer frame registrado tras enviar el
SIGWINCH de agrandado y el primer frame con dims ESTRICTAMENTE
mayores. Umbral: < 1.2 s (antes del fix: ~2.5 s con la cola llena).

También comprobamos que tras la recuperación el A/V sync sigue sano
(|avdiff| mediano < 120 ms) — el refine no debe desincronizar.

Uso: python3 tests/integration_grow_quality.py <video>
"""
import os, pty, sys, time, subprocess, select, signal
import fcntl, termios, struct, threading, tempfile

VIDEO = sys.argv[1]
BIN = os.path.join(os.path.dirname(__file__), "..", "target", "release", "rtv")

env = dict(os.environ)
env["TERM"] = "xterm-256color"
log_path = tempfile.mktemp(prefix="rtv_grow_", suffix=".log")
env["RTV_SYNC_LOG"] = log_path

master, slave = pty.openpty()

def set_winsize(rows, cols):
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

# Empezamos PEQUEÑO: el decoder produce frames pequeños.
set_winsize(14, 46)

proc = subprocess.Popen(
    [BIN, VIDEO, "--backend", "ascii"],
    stdin=slave, stdout=slave, stderr=subprocess.DEVNULL, env=env,
)

# Drenar el pty para que rtv no se bloquee escribiendo.
_stop = threading.Event()
def _reader():
    while not _stop.is_set():
        r, _, _ = select.select([master], [], [], 0.02)
        if r:
            try:
                if not os.read(master, 1 << 20):
                    return
            except OSError:
                return
threading.Thread(target=_reader, daemon=True).start()

def read_log():
    rows = []
    try:
        with open(log_path) as f:
            for line in f:
                p = line.split()
                if len(p) >= 7:
                    rows.append((float(p[0]), float(p[3]), int(p[5]), int(p[6])))
    except FileNotFoundError:
        pass
    return rows  # (wall, avdiff_ms, w, h)

fails = []

# 1) Reproducir ~3 s en pequeño para que la cola se llene de frames
#    pequeños (peor caso del bug).
time.sleep(3.0)
if proc.poll() is not None:
    print("FAIL: rtv murió durante la reproducción inicial")
    sys.exit(1)

pre = read_log()
if not pre:
    print("FAIL: sync-log vacío tras 3 s de reproducción")
    proc.kill(); sys.exit(1)
w_old, h_old = pre[-1][2], pre[-1][3]
n_pre = len(pre)
print(f"dims iniciales del frame: {w_old}x{h_old} ({n_pre} frames registrados)")

# 2) AGRANDAR la terminal de golpe.
set_winsize(52, 190)
proc.send_signal(signal.SIGWINCH)

# 3) Esperar hasta ver el primer frame con dims mayores.
recovery = None
wall_resize_ref = None
deadline = time.monotonic() + 6.0
while time.monotonic() < deadline:
    rows = read_log()
    post = rows[n_pre:]
    if post and wall_resize_ref is None:
        # Primer frame registrado tras el resize → referencia temporal
        # en el reloj wall del proceso.
        wall_resize_ref = post[0][0]
    for wall, _av, w, h in post:
        if w > w_old and h > h_old:
            recovery = wall - wall_resize_ref
            break
    if recovery is not None:
        break
    time.sleep(0.05)

if recovery is None:
    fails.append("nunca aparecieron frames con dims mayores tras agrandar (>6 s)")
else:
    print(f"recuperación de calidad tras agrandar: {recovery*1000:.0f} ms")
    if recovery > 1.2:
        fails.append(f"recuperación lenta: {recovery*1000:.0f} ms (> 1200 ms)")

# 4) Dejar correr 2 s más y comprobar que el sync sigue sano tras el
#    refine (mediana de |avdiff| de los frames a dims nuevas).
time.sleep(2.0)
rows = read_log()
new_dims = [abs(av) for _w0, av, w, h in rows[n_pre:] if w > w_old and h > h_old]
if len(new_dims) < 10:
    fails.append(f"muy pocos frames a dims nuevas tras 2 s ({len(new_dims)})")
else:
    new_dims.sort()
    med = new_dims[len(new_dims) // 2]
    print(f"|avdiff| mediano post-refine: {med:.1f} ms ({len(new_dims)} frames)")
    if med > 120.0:
        fails.append(f"A/V desincronizado tras el refine: mediana {med:.1f} ms")

# 5) Salida limpia.
if proc.poll() is not None:
    fails.append("rtv murió durante el test")
else:
    os.write(master, b"q")
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        fails.append("rtv no salió con 'q' en 5 s")

_stop.set()
try:
    os.unlink(log_path)
except OSError:
    pass

if fails:
    for f in fails:
        print("FAIL:", f)
    sys.exit(1)
print("OK: recuperación de calidad al agrandar es rápida y el sync se mantiene")
