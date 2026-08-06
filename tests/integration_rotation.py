#!/usr/bin/env python3
"""Test de integración: auto-rotación por Display Matrix.

Fixture: vídeo APAISADO 320x180 cuya mitad SUPERIOR es roja y la
INFERIOR azul, remuxeado con -display_rotation -90 (la matriz que
escribe un iPhone en vertical). Presentado correctamente (90° horario)
queda VERTICAL 180x320 con la mitad IZQUIERDA azul y la DERECHA roja.

Se reproduce con --backend blocks en un pty y se parsean los SGR de
color 24-bit (38;2;r;g;b / 48;2;r;g;b) de la salida: el test exige
rojo dominante en la mitad derecha y azul en la izquierda —
verificación de píxeles real de la cadena Display Matrix → sws
transpuesto → rotate_frame → render, no solo "no crashea".

Uso: integration_rotation.py <rtv>
"""

import os
import pty
import re
import select
import signal
import subprocess
import sys
import tempfile
import time

COLS, ROWS = 80, 40


def run(cmd, **kw):
    subprocess.run(cmd, check=True, **kw)


def make_fixture(tmp):
    plain = os.path.join(tmp, "redblue.mp4")
    rot = os.path.join(tmp, "redblue_rot90.mp4")
    run([
        "ffmpeg", "-y", "-loglevel", "error",
        "-f", "lavfi", "-i", "color=red:size=320x90:rate=30",
        "-f", "lavfi", "-i", "color=blue:size=320x90:rate=30",
        "-filter_complex", "[0][1]vstack", "-t", "2",
        "-c:v", "libx264", "-pix_fmt", "yuv420p", plain,
    ])
    run([
        "ffmpeg", "-y", "-loglevel", "error",
        "-display_rotation", "-90", "-i", plain, "-c", "copy", rot,
    ])
    return rot


def capture_pty(rtv, video, seconds=4.0):
    """Reproduce `video` en un pty y devuelve la salida cruda."""
    import fcntl
    import struct
    import termios
    mfd, sfd = pty.openpty()
    fcntl.ioctl(sfd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    env = dict(os.environ, TERM="xterm-256color")
    p = subprocess.Popen(
        [rtv, video, "--backend", "blocks", "--audio-backend", "none"],
        stdin=sfd, stdout=sfd, stderr=sfd, env=env, close_fds=True,
    )
    os.close(sfd)
    out = bytearray()
    deadline = time.time() + seconds
    while time.time() < deadline and p.poll() is None:
        r, _, _ = select.select([mfd], [], [], 0.2)
        if r:
            try:
                out += os.read(mfd, 65536)
            except OSError:
                break
    if p.poll() is None:
        try:
            os.write(mfd, b"q")
        except OSError:
            pass
        try:
            p.wait(timeout=5)
        except subprocess.TimeoutExpired:
            p.send_signal(signal.SIGKILL)
            p.wait()
    # Drenaje final.
    while True:
        r, _, _ = select.select([mfd], [], [], 0.1)
        if not r:
            break
        try:
            chunk = os.read(mfd, 65536)
        except OSError:
            break
        if not chunk:
            break
        out += chunk
    os.close(mfd)
    return bytes(out)


# El backend blocks emite FG y BG en secuencias SGR SEPARADAS
# (\x1b[38;2;r;g;bm y \x1b[48;2;r;g;bm) y pinta '▀' por celda.
# Estrategia: mantener el color vigente y, en cada '▀', atribuirlo a
# la mitad izquierda/derecha según la columna (los CUP fijan columna;
# cada glifo avanza una). CUALQUIER otra secuencia CSI se salta
# ENTERA (saltar solo el ESC corrompería el conteo de columnas).
FG = re.compile(rb"\x1b\[38;2;(\d+);(\d+);(\d+)m")
BG = re.compile(rb"\x1b\[48;2;(\d+);(\d+);(\d+)m")
CUP = re.compile(rb"\x1b\[(\d+);(\d+)H")
CSI = re.compile(rb"\x1b\[[0-9;?<=>]*[a-zA-Z@`~]|\x1b.")
HALFBLOCK = b"\xe2\x96\x80"  # '▀' UTF-8


def classify(r, g, b):
    if r > 128 and b < 100:
        return "red"
    if b > 128 and r < 100:
        return "blue"
    return None


def analyze(raw):
    """Recorre la salida siguiendo los CUP: cuenta rojo/azul por mitades."""
    left = {"red": 0, "blue": 0}
    right = {"red": 0, "blue": 0}
    col = 1
    cur = []  # clasificación fg/bg vigente
    i = 0
    while i < len(raw):
        if raw[i] == 0x1B:
            m = CUP.match(raw, i)
            if m:
                col = int(m.group(2))
                i = m.end()
                continue
            m = FG.match(raw, i) or BG.match(raw, i)
            if m:
                k = classify(*(int(x) for x in m.groups()))
                is_fg = raw[i + 2:i + 4] == b"38"
                # fg y bg del blocks van en par: reset al ver el fg.
                if is_fg:
                    cur = [k] if k else []
                elif k:
                    cur.append(k)
                i = m.end()
                continue
            m = CSI.match(raw, i)
            i = m.end() if m else i + 1
            continue
        if raw[i:i + 3] == HALFBLOCK:
            for k in cur:
                (left if col <= COLS // 2 else right)[k] += 1
            col += 1
            i += 3
            continue
        ch = raw[i]
        if ch >= 0x20 and (ch & 0xC0) != 0x80:
            col += 1
        i += 1
    return left, right


def main():
    rtv = sys.argv[1]
    with tempfile.TemporaryDirectory() as tmp:
        video = make_fixture(tmp)
        # --info: dims presentadas transpuestas + etiqueta de rotación.
        info = subprocess.run(
            [rtv, "--info", video], capture_output=True, text=True, timeout=60
        ).stdout
        assert "180x320" in info, f"--info sin dims transpuestas:\n{info}"
        assert "rotado 90" in info, f"--info sin etiqueta de rotación:\n{info}"
        print("[ok] --info: 180x320 + 'rotado 90°'")

        raw = capture_pty(rtv, video)
        assert len(raw) > 1000, "salida pty vacía — ¿crasheó rtv?"
        left, right = analyze(raw)
        print(f"[dbg] left={left} right={right}")
        # Mitad izquierda: azul dominante; derecha: rojo dominante.
        assert left["blue"] > left["red"] * 3 and left["blue"] > 50, \
            f"izquierda no es azul: {left}"
        assert right["red"] > right["blue"] * 3 and right["red"] > 50, \
            f"derecha no es roja: {right}"
        print("[ok] píxeles: izquierda azul / derecha roja — rotación correcta")
    print("PASS integration_rotation")
    return 0


if __name__ == "__main__":
    sys.exit(main())
