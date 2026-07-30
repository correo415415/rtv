#!/usr/bin/env python3
"""Test de integración: backends Sixel/iTerm2 y subtítulos softsub.

Valida sobre un PTY real:
  1. sixel  — DCS `ESC P 0;1;0 q` por frame, paleta 6×7×6 completa,
              solo caracteres válidos del protocolo, ST de cierre.
  2. iterm2 — OSC 1337 File= por frame, BMP base64 decodificable con
              cabecera coherente (size == len real), dims en celdas.
  3. subs externos SRT (--sub): el texto aparece en pantalla en su
     ventana temporal, tags HTML fuera.
  4. subs embebidos MKV: la pista subrip del contenedor se muestra
     sin flags.
  5. --no-subs: no aparece texto.
  6. Regresión kitty y blocks: siguen emitiendo su protocolo.

Uso:  python3 tests/integration_backends_subs.py [ruta_rtv]
"""

import base64
import fcntl
import os
import pty
import re
import select
import struct
import subprocess
import sys
import tempfile
import termios
import time

RTV = sys.argv[1] if len(sys.argv) > 1 else "./target/release/rtv"


def run(args, secs=6, cols=100, rows=30):
    """Ejecuta rtv en un PTY, captura `secs` de output y sale con 'q'
    (reintentada: con output masivo una sola pulsación puede perderse
    en el buffer del PTY)."""
    m, s = pty.openpty()
    fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 800, 480))
    p = subprocess.Popen([RTV] + args, stdin=s, stdout=s, stderr=subprocess.DEVNULL)
    os.close(s)
    buf = bytearray()
    t0 = time.time()
    while time.time() - t0 < secs:
        r, _, _ = select.select([m], [], [], 0.05)
        if r:
            try:
                buf += os.read(m, 1 << 20)
            except OSError:
                break
    t0 = time.time()
    last_q = 0.0
    while p.poll() is None and time.time() - t0 < 15:
        if time.time() - last_q > 0.5:
            try:
                os.write(m, b"q")
            except OSError:
                break
            last_q = time.time()
        r, _, _ = select.select([m], [], [], 0.02)
        if r:
            try:
                buf += os.read(m, 1 << 20)
            except OSError:
                break
    # El bucle sale con OSError/EIO cuando el hijo cierra el PTY: puede
    # que aún no esté cosechado — esperar de verdad antes de leer rc.
    try:
        rc = p.wait(timeout=5)
    except subprocess.TimeoutExpired:
        p.kill()
        rc = None
    os.close(m)
    return bytes(buf), rc


def make_assets(tmp):
    video = os.path.join(tmp, "t.mp4")
    srt = os.path.join(tmp, "t.srt")
    mkv = os.path.join(tmp, "t.mkv")
    subprocess.run(
        ["ffmpeg", "-y", "-f", "lavfi", "-i",
         "testsrc2=size=640x360:rate=25:duration=15",
         "-f", "lavfi", "-i", "sine=frequency=440:duration=15",
         "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", video],
        check=True, capture_output=True)
    with open(srt, "w") as f:
        f.write("1\n00:00:01,000 --> 00:00:04,000\nHola mundo subtitulado\n\n"
                "2\n00:00:05,000 --> 00:00:08,000\nSegunda <i>línea</i> de prueba\n")
    subprocess.run(
        ["ffmpeg", "-y", "-i", video, "-i", srt, "-c:v", "copy", "-c:a",
         "copy", "-c:s", "srt", "-metadata:s:s:0", "language=spa", mkv],
        check=True, capture_output=True)
    return video, srt, mkv


def main():
    fails = []

    def check(name, cond, detail=""):
        status = "OK " if cond else "FAIL"
        print(f"  [{status}] {name}" + (f" — {detail}" if detail else ""))
        if not cond:
            fails.append(name)

    with tempfile.TemporaryDirectory() as tmp:
        video, srt, mkv = make_assets(tmp)

        print("== 1) backend sixel ==")
        out, rc = run([video, "--backend", "sixel", "--no-audio"])
        n = out.count(b"\x1bP0;1;0q")
        check("salida limpia (rc=0)", rc == 0, f"rc={rc}")
        check("frames DCS sixel", n >= 5, f"{n} frames")
        check("ST por frame", out.count(b"\x1b\\") >= n)
        i = out.find(b"\x1bP0;1;0q")
        j = out.find(b"\x1b\\", i)
        body = out[i + 8:j] if i >= 0 and j > i else b""
        check("solo chars válidos del protocolo",
              bool(re.fullmatch(rb'["#;0-9!$\-\?-~]*', body)))
        check("paleta completa (reg 0 y 251)",
              b"#0;2;0;0;0" in body and b"#251;2;100;100;100" in body)

        print("== 2) backend iterm2 ==")
        out, rc = run([video, "--backend", "iterm2", "--no-audio"])
        n = out.count(b"\x1b]1337;File=inline=1;")
        check("salida limpia (rc=0)", rc == 0, f"rc={rc}")
        check("frames OSC 1337", n >= 5, f"{n} frames")
        i = out.find(b"\x1b]1337;File=")
        colon = out.find(b":", i)
        bel = out.find(b"\x07", colon)
        ok_bmp = False
        if 0 <= i < colon < bel:
            try:
                bmp = base64.b64decode(out[colon + 1:bel], validate=True)
                fsize = struct.unpack("<I", bmp[2:6])[0]
                ok_bmp = bmp[:2] == b"BM" and fsize == len(bmp)
            except Exception:
                pass
        check("BMP base64 válido y coherente", ok_bmp)
        hdr = out[i:colon].decode("ascii", "replace") if i >= 0 else ""
        check("dims en celdas en la cabecera", "width=" in hdr and "height=" in hdr)

        print("== 3) subs externos SRT ==")
        out, rc = run([video, "--sub", srt, "--no-audio", "--backend", "blocks"], secs=7)
        txt = out.decode("utf-8", "replace")
        check("evento 1 visible", "Hola mundo subtitulado" in txt)
        check("evento 2 visible", "Segunda línea de prueba" in txt)
        check("tags HTML eliminados", "<i>" not in txt)

        print("== 4) subs embebidos MKV ==")
        out, rc = run([mkv, "--no-audio", "--backend", "blocks"], secs=7)
        txt = out.decode("utf-8", "replace")
        check("pista embebida visible", "Hola mundo subtitulado" in txt)

        print("== 5) --no-subs ==")
        out, rc = run([mkv, "--no-subs", "--no-audio", "--backend", "blocks"], secs=6)
        txt = out.decode("utf-8", "replace")
        check("sin texto de subs", "Hola mundo subtitulado" not in txt)

        print("== 6) regresión kitty / blocks ==")
        out, rc = run([video, "--backend", "kitty", "--no-audio"], secs=4)
        check("kitty emite APC _G", rc == 0 and out.count(b"\x1b_G") >= 5)
        out, rc = run([video, "--backend", "blocks", "--no-audio"], secs=4)
        check("blocks emite SGR truecolor", rc == 0 and b"\x1b[38;2;" in out)

    print()
    if fails:
        print(f"RESULTADO: {len(fails)} fallos: {fails}")
        sys.exit(1)
    print("RESULTADO: todos los checks OK")


if __name__ == "__main__":
    main()
