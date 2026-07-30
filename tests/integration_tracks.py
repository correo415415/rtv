#!/usr/bin/env python3
"""Test de integración: cambio de pista de audio/subtítulos en runtime.

Genera un MKV con:
  * vídeo test (smptebars 640x360 25fps, 30 s)
  * 2 pistas de audio con TONOS distintos (440 Hz eng / 880 Hz spa)
  * 2 pistas de subtítulos SRT (eng: "ENGLISH ...", spa: "SPANISH ...")

Checks (pty real):
  1. Tecla `j` cicla subtítulos: off -> eng -> spa; el texto mostrado
     en pantalla cambia (se busca "ENGLISH" y luego "SPANISH" en la
     salida del pty) y aparece el OSD "Subs [".
  2. Tecla `a` cicla la pista de audio SIN romper el sync: tras el
     cambio, |avdiff| mediano del sync-log < 60 ms y el player sigue
     mostrando frames (no se congela).
  3. --aid/--alang/--sid/--slang seleccionan pista al arrancar.
  4. `a` con una sola pista de audio: OSD informativo, sin romper nada.
  5. Salida limpia con `q` (exit 0) en todos los casos.
"""
import os
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import fcntl

RTV = os.path.join(os.path.dirname(__file__), "..", "target", "release", "rtv")
FAIL = 0


def check(name, ok, detail=""):
    global FAIL
    tag = "PASS" if ok else "FAIL"
    print(f"[{tag}] {name}" + (f" — {detail}" if detail else ""))
    if not ok:
        FAIL = 1


def make_media(tmp):
    """MKV con 2 audios (tonos 440/880) + 2 subs (eng/spa)."""
    mkv = os.path.join(tmp, "multi.mkv")
    srt_en = os.path.join(tmp, "en.srt")
    srt_es = os.path.join(tmp, "es.srt")
    with open(srt_en, "w") as f:
        for i in range(30):
            f.write(f"{i+1}\n00:00:{i:02d},000 --> 00:00:{i:02d},900\nENGLISH LINE {i}\n\n")
    with open(srt_es, "w") as f:
        for i in range(30):
            f.write(f"{i+1}\n00:00:{i:02d},000 --> 00:00:{i:02d},900\nSPANISH LINE {i}\n\n")
    subprocess.run(
        [
            "ffmpeg", "-y", "-v", "error",
            "-f", "lavfi", "-i", "smptebars=size=640x360:rate=25",
            "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100",
            "-f", "lavfi", "-i", "sine=frequency=880:sample_rate=44100",
            "-i", srt_en, "-i", srt_es,
            "-map", "0:v", "-map", "1:a", "-map", "2:a", "-map", "3:s", "-map", "4:s",
            "-t", "30",
            "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p",
            "-c:a", "aac", "-b:a", "96k",
            "-c:s", "srt",
            "-metadata:s:a:0", "language=eng",
            "-metadata:s:a:1", "language=spa",
            "-metadata:s:s:0", "language=eng",
            "-metadata:s:s:1", "language=spa",
            mkv,
        ],
        check=True,
    )
    return mkv


def spawn(args, cols=100, rows=30, env_extra=None):
    env = dict(os.environ)
    env["TERM"] = "xterm-256color"
    if env_extra:
        env.update(env_extra)
    m, s = pty.openpty()
    fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    p = subprocess.Popen(
        [RTV] + args, stdin=s, stdout=s, stderr=subprocess.DEVNULL,
        env=env, close_fds=True,
    )
    os.close(s)
    return p, m


def read_pty(m, buf, dur):
    """Lee del pty durante `dur` s, acumulando en buf (bytearray)."""
    end = time.time() + dur
    while time.time() < end:
        r, _, _ = select.select([m], [], [], 0.1)
        if r:
            try:
                data = os.read(m, 65536)
            except OSError:
                break
            if not data:
                break
            buf.extend(data)
    return buf


def finish(p, m, timeout=5.0):
    try:
        os.write(m, b"q")
    except OSError:
        pass
    t0 = time.time()
    while p.poll() is None and time.time() - t0 < timeout:
        # Drenar el pty para que rtv no se bloquee en write().
        r, _, _ = select.select([m], [], [], 0.05)
        if r:
            try:
                os.read(m, 65536)
            except OSError:
                break
    # OJO: si el drenaje del pty rompe con EIO (el proceso acaba de
    # salir), `p.poll()` puede devolver None un instante (zombie aún
    # no cosechado) — usar wait() con el tiempo restante evita marcar
    # -9 a procesos que salieron limpios.
    remaining = max(0.5, timeout - (time.time() - t0))
    try:
        rc = p.wait(timeout=remaining)
    except subprocess.TimeoutExpired:
        p.kill()
        p.wait()
        rc = -9
    try:
        os.close(m)
    except OSError:
        pass
    return rc


def strip_ansi(b):
    txt = b.decode("utf-8", "replace")
    txt = re.sub(r"\x1b\][^\x07\x1b]*(\x07|\x1b\\)", "", txt)  # OSC
    txt = re.sub(r"\x1b[PX^_][^\x1b]*\x1b\\", "", txt)  # DCS/PM/APC
    txt = re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "", txt)  # CSI
    return txt


def parse_sync_log(path):
    rows = []
    if not os.path.exists(path):
        return rows
    with open(path) as f:
        for ln in f:
            if ln.startswith("#"):
                continue
            parts = ln.split()
            if len(parts) >= 4:
                try:
                    rows.append((float(parts[0]), float(parts[3])))
                except ValueError:
                    pass
    return rows


def median(xs):
    if not xs:
        return float("nan")
    s = sorted(xs)
    return s[len(s) // 2]


def main():
    tmp = tempfile.mkdtemp(prefix="rtvtracks_")
    mkv = make_media(tmp)

    # ---------- 1. Ciclado de subtítulos con `j` ----------
    p, m = spawn([mkv, "--backend", "ascii"])
    buf = bytearray()
    read_pty(m, buf, 2.5)          # arranque, subs off
    pre = strip_ansi(bytes(buf))
    os.write(m, b"j")              # -> pista embebida 1 (eng)
    buf2 = bytearray()
    read_pty(m, buf2, 3.0)
    after_j1 = strip_ansi(bytes(buf2))
    os.write(m, b"j")              # -> pista embebida 2 (spa)
    buf3 = bytearray()
    read_pty(m, buf3, 3.0)
    after_j2 = strip_ansi(bytes(buf3))
    rc = finish(p, m)
    check("subs: exit 0 tras ciclar con j", rc == 0, f"rc={rc}")
    check("subs: sin texto antes de activar", "ENGLISH LINE" not in pre)
    check("subs: pista eng visible tras 1×j", "ENGLISH LINE" in after_j1,
          "no apareció ENGLISH")
    check("subs: pista spa visible tras 2×j", "SPANISH LINE" in after_j2,
          "no apareció SPANISH")
    check("subs: OSD de feedback", "Subs [" in after_j1 or "Subs [" in after_j2)

    # ---------- 2. Ciclado de audio con `a` + sync ----------
    slog = os.path.join(tmp, "sync.log")
    p, m = spawn([mkv, "--backend", "ascii"], env_extra={"RTV_SYNC_LOG": slog})
    buf = bytearray()
    read_pty(m, buf, 3.0)
    os.write(m, b"a")              # -> pista 2 (spa 880 Hz)
    buf2 = bytearray()
    read_pty(m, buf2, 4.0)
    osd_txt = strip_ansi(bytes(buf2))
    os.write(m, b"a")              # -> vuelta a pista 1
    buf3 = bytearray()
    read_pty(m, buf3, 4.0)
    rc = finish(p, m)
    check("audio: exit 0 tras ciclar con a", rc == 0, f"rc={rc}")
    check("audio: OSD de feedback", "Audio [" in osd_txt,
          "no apareció 'Audio [' en el HUD")
    rows = parse_sync_log(slog)
    check("audio: sync-log con frames", len(rows) > 50, f"{len(rows)} frames")
    if rows:
        # Frames de los últimos 3 s (tras ambos cambios): sync estable.
        t_end = rows[-1][0]
        tail = [d for (w, d) in rows if w >= t_end - 3.0]
        med = abs(median(tail))
        check("audio: |avdiff| mediano post-switch < 60 ms", med < 60.0,
              f"{med:.1f} ms sobre {len(tail)} frames")
        # El player no se congeló: hay frames DESPUÉS del 2º cambio.
        n_after = sum(1 for (w, d) in rows if w >= t_end - 2.0)
        check("audio: sigue mostrando frames tras los cambios", n_after >= 20,
              f"{n_after} frames en los últimos 2 s")

    # ---------- 3. Selección inicial por CLI ----------
    for args, name in [
        (["--aid", "2"], "--aid 2"),
        (["--alang", "spa"], "--alang spa"),
        (["--sid", "2"], "--sid 2"),
        (["--slang", "eng"], "--slang eng"),
    ]:
        p, m = spawn([mkv, "--backend", "ascii"] + args)
        buf = bytearray()
        read_pty(m, buf, 2.5)
        txt = strip_ansi(bytes(buf))
        rc = finish(p, m)
        check(f"CLI {name}: reproduce y exit 0", rc == 0, f"rc={rc}")
        if name == "--sid 2":
            check("CLI --sid 2: muestra pista spa", "SPANISH LINE" in txt)
        if name == "--slang eng":
            check("CLI --slang eng: muestra pista eng", "ENGLISH LINE" in txt)

    # ---------- 4. `a` con una sola pista (vídeo mono-audio) ----------
    mono = os.path.join(tmp, "mono.mp4")
    subprocess.run(
        ["ffmpeg", "-y", "-v", "error",
         "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=25",
         "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100",
         "-t", "8", "-c:v", "libx264", "-preset", "ultrafast",
         "-pix_fmt", "yuv420p", "-c:a", "aac", mono],
        check=True,
    )
    p, m = spawn([mono, "--backend", "ascii"])
    buf = bytearray()
    read_pty(m, buf, 2.0)
    os.write(m, b"a")
    buf2 = bytearray()
    read_pty(m, buf2, 2.0)
    txt = strip_ansi(bytes(buf2))
    rc = finish(p, m)
    check("mono: `a` no rompe nada (exit 0)", rc == 0, f"rc={rc}")
    check("mono: OSD 'única pista'", "nica pista" in txt, "sin OSD informativo")

    print("\n" + ("TODO OK" if FAIL == 0 else "HAY FALLOS"))
    sys.exit(FAIL)


if __name__ == "__main__":
    signal.alarm(300)
    main()
