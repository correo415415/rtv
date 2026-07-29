#!/usr/bin/env python3
"""Test de integración del decode por hardware (--hwdec).

Este sandbox NO tiene /dev/dri ni GPU: es el ENTORNO NEGATIVO perfecto
para validar el contrato principal del hwdec — el fallback transparente
a software. El camino positivo (hwaccel realmente activo) requiere GPU
real y queda documentado como pendiente en el README.

Comprobaciones:

  1. --hwdec auto, none y vaapi reproducen N segundos en un pty y salen
     con exit 0 (q). vaapi sin /dev/dri DEBE degradar a software sin
     abortar ni ensuciar el TUI.

  2. El sync-log de cada modo tiene un nº de frames comparable (todos
     acaban decodificando por software → mismo pipeline) y el A/V sync
     es sano: |avdiff| mediano < 120 ms — los mismos umbrales que
     integration_sync.py. El fallback no debe costar sync.

  3. --hwdec badvalue → exit 2 con mensaje de uso en stderr (validación
     ANTES del silenciado de stderr).

Uso: python3 tests/integration_hwdec.py <video>
"""
import os, pty, sys, time, subprocess, select, signal
import fcntl, termios, struct, threading, tempfile, statistics

VIDEO = sys.argv[1]
BIN = os.path.join(os.path.dirname(__file__), "..", "target", "release", "rtv")

PLAY_SECS = 6.0
MIN_FRAMES = 40          # ~6 s a 25 fps con margen amplio de warmup
AVDIFF_MEDIAN_MS = 120.0 # mismo umbral que integration_sync.py

fails = []


def run_mode(mode):
    """Reproduce PLAY_SECS con --hwdec <mode> en un pty. Devuelve
    (exit_code, rows) donde rows = [(wall, avdiff_ms), ...]."""
    env = dict(os.environ)
    env["TERM"] = "xterm-256color"
    log_path = tempfile.mktemp(prefix=f"rtv_hwdec_{mode}_", suffix=".log")
    env["RTV_SYNC_LOG"] = log_path

    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))

    proc = subprocess.Popen(
        [BIN, VIDEO, "--backend", "ascii", "--hwdec", mode],
        stdin=slave, stdout=slave, stderr=subprocess.DEVNULL, env=env,
    )

    # Drenar el pty: sin esto rtv se bloquea escribiendo frames.
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

    time.sleep(PLAY_SECS)
    os.write(master, b"q")
    try:
        code = proc.wait(timeout=8)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
        code = -9
    _stop.set()
    os.close(master)
    os.close(slave)

    rows = []
    try:
        with open(log_path) as f:
            for line in f:
                if line.startswith("#"):
                    continue
                p = line.split()
                if len(p) >= 4:
                    try:
                        rows.append((float(p[0]), float(p[3])))
                    except ValueError:
                        pass  # cabecera
    except FileNotFoundError:
        pass
    try:
        os.unlink(log_path)
    except OSError:
        pass
    return code, rows


# ── 1+2: auto / none / vaapi deben reproducir con sync sano ─────────
frame_counts = {}
for mode in ("auto", "none", "vaapi"):
    code, rows = run_mode(mode)
    n = len(rows)
    frame_counts[mode] = n
    print(f"--hwdec {mode}: exit={code} frames={n}")
    if code != 0:
        fails.append(f"--hwdec {mode}: exit {code} != 0")
        continue
    if n < MIN_FRAMES:
        fails.append(f"--hwdec {mode}: solo {n} frames (< {MIN_FRAMES})")
        continue
    # Sync: descartar el warmup (primer segundo) como en los otros tests.
    t0 = rows[0][0]
    settled = [abs(av) for (w, av) in rows if w - t0 > 1.0]
    if settled:
        med = statistics.median(settled)
        print(f"  |avdiff| mediano tras warmup: {med:.1f} ms")
        if med > AVDIFF_MEDIAN_MS:
            fails.append(f"--hwdec {mode}: avdiff mediano {med:.1f} ms > {AVDIFF_MEDIAN_MS}")

# Los tres modos acaban en software en este sandbox → nº de frames
# comparable (±40%). Detecta un fallback que "reproduce" a 2 fps.
if all(m in frame_counts and frame_counts[m] >= MIN_FRAMES for m in ("auto", "none", "vaapi")):
    base = frame_counts["none"]
    for mode in ("auto", "vaapi"):
        ratio = frame_counts[mode] / base
        if not (0.6 <= ratio <= 1.4):
            fails.append(
                f"--hwdec {mode}: {frame_counts[mode]} frames vs none={base} (ratio {ratio:.2f} fuera de 0.6-1.4)"
            )

# ── 3: CLI inválida → exit 2 con mensaje visible ────────────────────
p = subprocess.run([BIN, VIDEO, "--hwdec", "badvalue"],
                   capture_output=True, text=True, timeout=10)
print(f"--hwdec badvalue: exit={p.returncode}")
if p.returncode != 2:
    fails.append(f"--hwdec badvalue: exit {p.returncode} != 2")
if "no reconocido" not in p.stderr:
    fails.append("--hwdec badvalue: falta el mensaje de uso en stderr")

if fails:
    print("\nFAIL:")
    for f in fails:
        print("  *", f)
    sys.exit(1)
print("\nOK: fallback transparente, sync sano y validación CLI correctos")
