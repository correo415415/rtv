#!/usr/bin/env python3
"""Test de integración: barra de progreso clicable con ratón.

Ejecuta rtv en un pty (120x40 → HUD de 2 líneas, barra de 40 celdas en
la penúltima fila) y le inyecta eventos de ratón SGR (\\x1b[<0;COL;ROWM)
como los emite un terminal real con mouse capture activo.

Checks:
  1. rtv ACTIVA la captura de ratón al arrancar (emite ?1000/?1006 h)
     y la DESACTIVA al salir (l) — sin esto el terminal del usuario se
     queda "comido" tras salir.
  2. Click en el 75% de la barra → seek hacia delante: el PTS del
     sync-log salta a ~75% de la duración (±10%).
  3. Click en el 25% de la barra → seek hacia atrás: el PTS vuelve
     a ~25% (±10%).
  4. Click FUERA de la barra (centro del vídeo) → NO hay seek (ningún
     salto de PTS >2 s adicional).
  5. Exit limpio con `q` (rc=0).
"""
import os
import pty
import re
import select
import struct
import subprocess
import sys
import termios
import time
import fcntl

VIDEO = sys.argv[1]
RTV = os.path.join(os.path.dirname(__file__), "..", "target", "release", "rtv")
LOG = "/tmp/rtv_mouse_sync.log"
COLS, ROWS = 120, 40
FAIL = 0


def check(name, ok, detail=""):
    global FAIL
    tag = "PASS" if ok else "FAIL"
    print(f"[{tag}] {name}" + (f" — {detail}" if detail else ""))
    if not ok:
        FAIL = 1


def sgr_click(col, row):
    """Click izquierdo (press+release) en coordenadas 1-based."""
    return f"\x1b[<0;{col};{row}M\x1b[<0;{col};{row}m".encode()


def drain(m, buf):
    while select.select([m], [], [], 0)[0]:
        try:
            data = os.read(m, 65536)
        except OSError:
            return False
        if not data:
            return False
        buf.extend(data)
    return True


def wait(m, buf, dur):
    end = time.time() + dur
    while time.time() < end:
        r, _, _ = select.select([m], [], [], 0.05)
        if r:
            try:
                buf.extend(os.read(m, 65536))
            except OSError:
                break


def parse_log():
    rows = []
    if not os.path.exists(LOG):
        return rows
    with open(LOG) as f:
        for ln in f:
            if ln.startswith("#"):
                continue
            p = ln.split()
            if len(p) >= 4:
                try:
                    rows.append((float(p[0]), float(p[2])))
                except ValueError:
                    pass
    return rows


def main():
    if os.path.exists(LOG):
        os.remove(LOG)
    env = dict(os.environ)
    env["TERM"] = "xterm-256color"
    env["RTV_SYNC_LOG"] = LOG

    duration = float(
        subprocess.run(
            ["ffprobe", "-v", "error", "-show_entries", "format=duration",
             "-of", "csv=p=0", VIDEO],
            capture_output=True, text=True, check=True,
        ).stdout.strip()
    )

    m, s = pty.openpty()
    fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    p = subprocess.Popen(
        [RTV, VIDEO, "--backend", "ascii"],
        stdin=s, stdout=s, stderr=subprocess.DEVNULL, env=env, close_fds=True,
    )
    os.close(s)

    raw = bytearray()
    wait(m, raw, 3.0)  # arranque + reproducción normal

    # Geometría de la barra (debe coincidir con bar_hitbox del player):
    # HUD 2 líneas → barra en fila ROWS-1, cols 5..5+40-1, bar_w=40.
    bar_row, bar_col, bar_w = ROWS - 1, 5, 40

    # --- click al 75% ---
    col75 = bar_col + round(0.75 * (bar_w - 1))
    os.write(m, sgr_click(col75, bar_row))
    wait(m, raw, 3.0)

    # --- click al 25% ---
    col25 = bar_col + round(0.25 * (bar_w - 1))
    os.write(m, sgr_click(col25, bar_row))
    wait(m, raw, 3.0)

    # --- click fuera de la barra (centro del vídeo) ---
    os.write(m, sgr_click(COLS // 2, ROWS // 2))
    wait(m, raw, 2.5)

    os.write(m, b"q")
    t0 = time.time()
    while p.poll() is None and time.time() - t0 < 5.0:
        if not drain(m, raw):
            break
        time.sleep(0.05)
    try:
        rc = p.wait(timeout=3)
    except subprocess.TimeoutExpired:
        p.kill()
        p.wait()
        rc = -9
    # Drenaje FINAL: las secuencias de teardown (DisableMouseCapture,
    # LeaveAlternateScreen…) se escriben justo antes del exit y pueden
    # quedar en el buffer del pty después de p.wait().
    end = time.time() + 1.0
    while time.time() < end:
        r, _, _ = select.select([m], [], [], 0.1)
        if not r:
            break
        try:
            data = os.read(m, 65536)
        except OSError:
            break
        if not data:
            break
        raw.extend(data)
    try:
        os.close(m)
    except OSError:
        pass

    out = bytes(raw).decode("utf-8", "replace")
    check("exit limpio con q", rc == 0, f"rc={rc}")

    # 1. Captura de ratón activada y desactivada.
    on = re.search(r"\x1b\[\?1000h", out) or re.search(r"\x1b\[\?100[26]h", out)
    off = re.search(r"\x1b\[\?1000l", out) or re.search(r"\x1b\[\?100[26]l", out)
    check("mouse capture ON al arrancar", bool(on), "no se emitió ?1000/?1006 h")
    check("mouse capture OFF al salir", bool(off), "no se emitió ?1000/?1006 l")

    rows = parse_log()
    check("sync-log con frames", len(rows) > 30, f"{len(rows)} frames")
    if not rows:
        return finish()

    # Detectar saltos de PTS > 2 s.
    jumps = []
    for i in range(1, len(rows)):
        if abs(rows[i][1] - rows[i - 1][1]) > 2.0:
            jumps.append(i)

    check("hubo exactamente 2 seeks por click", len(jumps) == 2,
          f"{len(jumps)} saltos detectados")

    if len(jumps) >= 1:
        pts75 = rows[jumps[0]][1]
        tgt = 0.75 * duration
        check("click 75% aterriza en ~75%", abs(pts75 - tgt) < duration * 0.10,
              f"pts={pts75:.1f}s, esperado ~{tgt:.1f}s")
    if len(jumps) >= 2:
        pts25 = rows[jumps[1]][1]
        tgt = 0.25 * duration
        check("click 25% aterriza en ~25%", abs(pts25 - tgt) < duration * 0.10,
              f"pts={pts25:.1f}s, esperado ~{tgt:.1f}s")

    return finish()


def finish():
    print("\n" + ("TODO OK" if FAIL == 0 else "HAY FALLOS"))
    sys.exit(FAIL)


if __name__ == "__main__":
    main()
