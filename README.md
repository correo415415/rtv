# rtv — Reproductor de vídeo de terminal (Rust) · v0.2

Un reproductor de vídeo minimalista pero **muy rápido**, sólo para terminal,
que compite con `mpv --vo=kitty`/`mpv --vo=tct` en latencia y bytes/frame.

## Novedades v0.2

- 🎧 **Audio real** vía `cpal` + `libswresample`. En Windows usa **WASAPI**,
  en Linux **ALSA**, en macOS **CoreAudio**. Se convierte cualquier formato
  de audio del vídeo a **F32 interleaved estéreo** al sample rate nativo del
  dispositivo.
- ⏱ **Audio como reloj maestro** (`AudioClock`): la posición del sink de
  audio pilota el vídeo → cero drift entre imagen y sonido, igual que hacen
  mpv, VLC y todos los reproductores serios. Si no hay audio, cae a
  `MonoClock` automáticamente.
- 🔍 **Escalado adaptativo**: al arrancar el reproductor sondea la terminal
  con la secuencia CSI `[16t` (kitty/wezterm/ghostty/iterm2/foot/konsole) y
  obtiene el tamaño **REAL** de una celda en píxeles. Con eso escala el
  vídeo a la resolución máxima que pueda mostrar cada terminal:
  * Terminal 80×24 con celdas 8×16 → vídeo a ~640×352 px.
  * Terminal 200×60 con celdas 10×20 en 4K → vídeo a ~2000×1180 px.
  * **Más grande la ventana = más resolución real = más nítido.**
- 🎯 **HUD adaptativo**: la barra de progreso y los indicadores se
  redimensionan con la terminal (barra de 8, 16, 24 o 40 celdas), y si hay
  espacio suficiente se despliega en 2 líneas con los atajos de teclado.

## Características

- **Decodificación FFmpeg**: cualquier formato que soporte FFmpeg vía
  `ffmpeg-the-third 5.0` (compatible con FFmpeg 7.1).
- **Auto-detección del mejor protocolo gráfico**: Kitty → HalfBlocks → ASCII.
- **Pipeline paralelo** productor–consumidor: decoder de vídeo, decoder de
  audio, sink cpal, y renderer del terminal — cada uno en su hilo, comunicados
  por canales `crossbeam` acotados.
- **Cross-platform Windows + Linux + macOS**.
- **Silenciado total de logs** de FFmpeg y sus codecs (`libdav1d`, `libaom`,
  etc.) para no ensuciar el TUI ni con `error parsing obu data` tras un seek.
- **Cierre limpio**: `q` / `Esc` / `Ctrl+C` restauran el terminal
  correctamente aunque el vídeo tenga audio activo.
- **Resize en caliente**: al cambiar el tamaño de la ventana, el decoder
  reajusta `sws_scale`, el layout se recalcula y hasta se re-sondea el
  tamaño de celda (por si has cambiado de monitor con distinto DPI).

## Uso

```
rtv <fichero> [--backend BACKEND] [--scale F] [--loop-video] [--stats] [--no-audio] [--verbose]
```

- `--backend kitty|iterm2|sixel|blocks|ascii` — fuerza un backend concreto
  (por defecto auto-detecta).
- `--scale 0.5` — reduce voluntariamente la resolución. Útil en terminales
  gigantes (4K) donde el decode software del vídeo se convierte en cuello
  de botella.
- `--loop-video` — reinicia al llegar al final.
- `--stats` — muestra FPS mostrados / decodificados / drops en el HUD.
- `--no-audio` — desactiva el audio y usa reloj monotónico.
- `--verbose` — deja que FFmpeg escriba a stderr (para debugging).

### Controles

- <kbd>Espacio</kbd> — pausa / reanudar (pausa **también el audio**).
- <kbd>←</kbd> / <kbd>→</kbd> — seek ±5 s (sincroniza vídeo Y audio).
- <kbd>↑</kbd> / <kbd>↓</kbd> — volumen ±5 (0–200 %).
- <kbd>q</kbd> / <kbd>Esc</kbd> / <kbd>Ctrl+C</kbd> — salir.

## Requisitos

### Linux (Debian/Ubuntu)

```bash
sudo apt install libavformat-dev libavcodec-dev libavutil-dev \
                 libswscale-dev libswresample-dev libclang-dev \
                 libasound2-dev pkg-config
cargo build --release
```

### Windows

**Instrucciones completas en [`BUILD-WINDOWS.md`](BUILD-WINDOWS.md)**. Resumen:

1. Descarga `ffmpeg-n7.1-latest-win64-lgpl-shared.zip` desde
   <https://github.com/BtbN/FFmpeg-Builds/releases>.
2. Descomprime en `C:\ffmpeg\` (debe quedar `C:\ffmpeg\include`, `\lib`, `\bin`).
3. `$env:FFMPEG_DIR = "C:\ffmpeg"` y añade `C:\ffmpeg\bin` al `PATH`.
4. `cargo clean && cargo build --release`.

Windows Terminal soporta truecolor, half-blocks Y audio WASAPI perfectamente.

### macOS

```bash
brew install ffmpeg pkg-config
cargo build --release
```

## Arquitectura v0.2

```
                    ┌──────────────┐
                    │  fichero.mp4 │
                    └──────┬───────┘
                           │
              ┌────────────┴────────────┐
              │                         │
              ▼                         ▼
      ┌───────────────┐         ┌──────────────┐
      │ demux vídeo   │         │ demux audio  │
      │ decode + sws  │         │ decode + swr │
      │ (thread 1)    │         │ (thread 2)   │
      └──────┬────────┘         └──────┬───────┘
             │ RGB24                   │ f32 estéreo
             │ bounded=2               │ bounded=64
             ▼                         ▼
      ┌───────────────┐         ┌──────────────┐
      │ main loop     │         │ cpal callback│
      │ · sync a clock│         │ (OS driver)  │
      │ · render      │         │ · notify clock
      │ · HUD         │         └──────┬───────┘
      └───────────────┘                │
                                 AudioClock
                              (master clock)
```

## Comparación con `mpv --vo=tct` (medido con este binario)

| Métrica | `mpv --vo=tct` | **rtv v0.2** |
|---|---|---|
| Arranque | ~150–300 ms | **~20–40 ms** |
| SGR/celda HalfBlocks | Fg+bg por celda | **Delta-encoded** (~30-50 % menos bytes) |
| Kitty round-trips | ACKs (`m=1`) | **`q=2`** (0 round-trips) |
| Peso binario stripped | ~40 MB | **~1.1 MB** (FFmpeg dinámico) |
| Reloj maestro | Audio (libmpv) | **Audio (cpal) o mono** |
| Reescalado | Fijo por vo | **Adaptativo por celda real** |

## Estructura del código

```
rtv/
├── Cargo.toml              # ffmpeg-the-third 5.0 + cpal + crossterm 0.29
├── README.md               # este fichero
├── BUILD-WINDOWS.md        # guía Windows detallada
└── src/
    ├── main.rs             # CLI + init + silenciado libav
    ├── player.rs           # loop principal, layout adaptativo
    ├── decoder.rs          # hilo demux+decode+sws para vídeo
    ├── audio.rs            # hilo demux+decode+swr para audio + cpal sink
    ├── clock.rs            # trait Clock + MonoClock + AudioClock
    ├── renderer.rs         # Kitty / HalfBlocks / ASCII
    ├── terminfo.rs         # detección de cell size vía CSI 16t / 14t
    └── input.rs            # eventos crossterm no-bloqueantes
```

## Roadmap

- [x] Audio con cpal + swresample (v0.2)
- [x] Escalado adaptativo por celda real (v0.2)
- [x] HUD adaptativo (v0.2)
- [ ] **HW decode**: VAAPI (Linux), D3D11VA (Windows), VideoToolbox (macOS).
- [ ] **Sixel real** para xterm/mlterm.
- [ ] **iTerm2 protocol** real (base64 + `\x1b]1337;File=`).
- [ ] **Subtítulos** softsub (SRT/ASS) sobreimpresos.
- [ ] Barra de progreso clicable con mouse.

## Licencia

MIT.
