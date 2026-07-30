#!/usr/bin/env python3
"""Estrés de salida bajo decode saturado (HEVC 1080p).

Reproduce el hang intermitente al pulsar 'q' cuando el decoder está
saturado (canal bounded lleno, hilo bloqueado en try_send o dentro de
llamadas FFmpeg con frame-threading).

Estrategia: PTY pequeño (blocks backend) para que el consumo de frames
sea lento y el canal del decoder se llene; se pulsa 'q' en un momento
aleatorio (temprano = máxima probabilidad de saturación) y se exige que
el proceso muera en <= EXIT_TIMEOUT s. Se drena el PTY en un hilo para
no falsear el resultado por backpressure del propio test.
"""
import os, pty, random, signal, struct, subprocess, sys, termios, fcntl, threading, time

RTV = os.path.join(os.path.dirname(__file__), "..", "target", "release", "rtv")
VIDEO = os.environ.get("STRESS_VIDEO", "/tmp/hevc.mp4")
N_RUNS = int(os.environ.get("STRESS_RUNS", "20"))
EXIT_TIMEOUT = float(os.environ.get("STRESS_EXIT_TIMEOUT", "3.0"))

def set_winsize(fd, rows, cols):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

def one_run(i, press_after, seek_storm=False):
    m, s = pty.openpty()
    set_winsize(s, 20, 60)
    env = dict(os.environ, TERM="xterm-256color")
    env.pop("KITTY_WINDOW_ID", None); env.pop("TERM_PROGRAM", None); env.pop("LC_TERMINAL", None)
    p = subprocess.Popen([RTV, VIDEO, "--backend", "blocks", "--no-subs"],
                         stdin=s, stdout=s, stderr=subprocess.DEVNULL,
                         env=env, close_fds=True)
    os.close(s)
    stop_drain = threading.Event()
    def drain():
        while not stop_drain.is_set():
            try:
                os.read(m, 65536)
            except OSError:
                break
    dt = threading.Thread(target=drain, daemon=True); dt.start()

    time.sleep(press_after)
    if seek_storm:
        # tormenta de seeks →→→←← y 'q' inmediato: el decoder queda en
        # pleno catch-up (drop-until-target / GOP re-decode) con el
        # canal en tránsito — el peor momento para pedir salida.
        for _ in range(random.randint(3, 8)):
            os.write(m, random.choice([b"\x1b[C", b"\x1b[D"]))
            time.sleep(random.uniform(0.01, 0.12))
    t0 = time.time()
    # pulsa 'q' con reintentos (una sola pulsación puede perderse bajo carga)
    hung = False
    while True:
        try:
            os.write(m, b"q")
        except OSError:
            pass
        try:
            p.wait(timeout=0.5)
            break
        except subprocess.TimeoutExpired:
            pass
        if time.time() - t0 > EXIT_TIMEOUT:
            hung = True
            break
    exit_ms = (time.time() - t0) * 1000
    if hung:
        # diagnóstico: stack del proceso colgado
        try:
            out = subprocess.run(["cat"] + [f"/proc/{p.pid}/task/{t}/stack" for t in os.listdir(f"/proc/{p.pid}/task")],
                                 capture_output=True, text=True, timeout=2).stdout
        except Exception:
            out = "(sin stack)"
        try:
            status = open(f"/proc/{p.pid}/status").read().splitlines()[:4]
        except Exception:
            status = []
        print(f"  run {i:2d}: HANG tras {exit_ms:.0f} ms (q@{press_after:.2f}s) pid={p.pid} {status}")
        p.kill()
        try: p.wait(timeout=3)
        except subprocess.TimeoutExpired: pass
    else:
        print(f"  run {i:2d}: ok  salida en {exit_ms:4.0f} ms (q@{press_after:.2f}s)")
    stop_drain.set()
    try: os.close(m)
    except OSError: pass
    return not hung

def main():
    if not os.path.exists(VIDEO):
        print(f"falta {VIDEO}"); sys.exit(2)
    random.seed(1234)
    fails = 0
    for i in range(N_RUNS):
        # momentos tempranos: canal llenándose / decoder a tope
        press_after = random.choice([0.15, 0.3, 0.5, 0.8, 1.2, 2.0, 3.5])
        storm = (i % 2 == 1)  # mitad de los runs con tormenta de seeks
        if not one_run(i, press_after, seek_storm=storm):
            fails += 1
    print(f"\nresultado: {N_RUNS - fails}/{N_RUNS} salidas limpias, {fails} hangs")
    sys.exit(1 if fails else 0)

if __name__ == "__main__":
    main()
