# rtv

Reproductor de vídeo para terminal, escrito en Rust. Rápido de verdad:
arranca en decenas de milisegundos, sincroniza audio y vídeo como un
reproductor serio (reloj de audio maestro, estilo ffplay) y aprovecha el
protocolo gráfico de tu terminal para sacar la máxima resolución posible.

```
rtv pelicula.mkv
```

Eso es todo. Detecta el terminal, elige el mejor backend de render, saca el
audio por el dispositivo por defecto y reproduce.

## Por qué existe

`mpv --vo=tct` funciona, pero arrastra el peso de mpv entero para pintar
celdas de colores, y `--vo=kitty` deja la sincronización fina en manos de un
camino de render que no fue pensado para terminales. rtv ataca el problema
desde el otro lado: un binario de ~1 MB que solo sabe hacer una cosa —
decodificar con FFmpeg y pintar en un terminal — y la hace con la menor
latencia y los menos bytes por frame que hemos conseguido.

| | `mpv --vo=tct` | rtv |
|---|---|---|
| Arranque | ~150–300 ms | ~20–40 ms |
| SGR por celda (half-blocks) | fg+bg siempre | delta-encoded (~30–50 % menos bytes) |
| Kitty graphics | con ACKs (`m=1`) | `q=2`, cero round-trips |
| Binario (stripped) | ~40 MB | ~1.1 MB (FFmpeg dinámico) |
| Reloj maestro | audio (libmpv) | audio (cpal), o monotónico sin audio |
| Resolución de render | fija por vo | adaptativa al tamaño real de celda |

Medido con los binarios de este repo; los números exactos dependen de la
máquina y el terminal, pero los órdenes de magnitud se mantienen.

## Características

- **Cualquier formato que trague FFmpeg** (vía `ffmpeg-the-third`, probado
  contra FFmpeg 7.1): H.264, HEVC, AV1, VP9… El decode de vídeo usa frame
  threading con todos los cores.
- **Audio real** con `cpal`: WASAPI en Windows, ALSA/PulseAudio en Linux,
  CoreAudio en macOS. Cualquier layout/formato de origen se convierte a f32
  estéreo al sample rate nativo del dispositivo con `libswresample`.
- **Sincronización A/V estilo ffplay**: el reloj de audio (posición real del
  sink, con la latencia de salida compensada y suavizada) pilota el vídeo.
  `compute_target_delay` con los mismos umbrales que ffplay. En la práctica:
  avdiff mediano de 0–2 ms en régimen estable, incluso con AV1 4K por
  software en 2 cores.
- **Seeks instantáneos estilo mpv**: `←`/`→` aterrizan en el keyframe ≤
  target y el audio salta exactamente al PTS real de aterrizaje del vídeo.
  Sin decodificar GOPs enteros en silencio, sin desincronización tras
  ráfagas de seeks.
- **Escalado adaptativo por celda real**: al arrancar se sondea el terminal
  (CSI `16t`/`14t` en kitty, WezTerm, Ghostty, iTerm2, foot, Konsole, xterm)
  para conocer el tamaño en píxeles de cada celda y escalar el vídeo a la
  resolución máxima que la ventana puede mostrar. Más ventana, más nitidez.
- **Resize en caliente, instantáneo**: el redibujo tras redimensionar tarda
  ~1 ms (espera inter-frame interrumpible por eventos + reescalado inmediato
  del frame en pantalla), y el decoder reajusta `sws_scale` sin drenar su
  colchón de pre-decode ni perder el sync.
- **Cinco backends de render** con auto-detección: Kitty graphics protocol
  (píxeles reales; Kitty/Ghostty/WezTerm), **iTerm2 inline images** (OSC 1337,
  BMP en memoria — también vía ssh con `LC_TERMINAL`), **Sixel** real (paleta
  fija 6×7×6 + dithering Bayer ordenado + RLE; mlterm/foot/contour/xterm
  `-ti vt340`), half-blocks truecolor (`▀`, 2 px por celda) y ASCII.
- **Subtítulos softsub (opt-in)**: por defecto no se muestra ningún
  subtítulo. Con `--sub` (sin valor) se usa la pista de texto embebida del
  contenedor (MKV/MP4), y con `--sub fichero.srt` (o `.ass`) se carga un
  fichero externo. El texto se pinta centrado, en negrita y blanco
  brillante, pegado justo debajo de la imagen (si hay letterbox) o en las
  2 filas reservadas encima del HUD, sin tocar el pipeline de vídeo: la
  pista embebida se carga en un
  hilo aparte con demux solo-subtítulos (`AVDISCARD_ALL` en el resto de
  streams) y el lookup por tiempo es una búsqueda binaria por frame. Tags
  ASS `{\...}` y HTML de SRT fuera.
- **HUD discreto**: barra de progreso, tiempo y volumen en 1–2 líneas
  (con `--stats` añade backend, resolución, celda, fps y drops)
  que se adaptan al ancho. Solo se repinta cuando cambia (nada de parpadeo)
  y desaparece si la ventana es demasiado pequeña para ser legible.
- **Terminal siempre limpio**: alt-screen, autowrap desactivado durante la
  reproducción, y restauración completa al salir — también con `Ctrl+C` o
  con el audio sonando. Los logs de libav (`libdav1d`, `libaom`…) van
  silenciados para que ningún `error parsing obu data` ensucie el TUI.

## Instalación

Necesitas Rust (edition 2021) y las librerías de desarrollo de FFmpeg.

### Linux (Debian/Ubuntu)

```bash
sudo apt install libavformat-dev libavcodec-dev libavutil-dev \
                 libswscale-dev libswresample-dev libclang-dev \
                 libasound2-dev pkg-config
cargo build --release
```

### macOS

```bash
brew install ffmpeg pkg-config
cargo build --release
```

### Windows

Guía completa en [`BUILD-WINDOWS.md`](BUILD-WINDOWS.md). En corto:

1. Descarga `ffmpeg-n7.1-latest-win64-lgpl-shared.zip` de
   [BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds/releases).
2. Descomprime en `C:\ffmpeg\` (deben quedar `include`, `lib` y `bin`).
3. `$env:FFMPEG_DIR = "C:\ffmpeg"` y añade `C:\ffmpeg\bin` al `PATH`.
4. `cargo clean && cargo build --release`.

Windows Terminal soporta truecolor, half-blocks y WASAPI sin problemas.

## Uso

```
rtv <fichero> [opciones]
```

| Opción | Efecto |
|---|---|
| `--backend <kitty\|iterm2\|sixel\|blocks\|ascii>` | Fuerza un backend (por defecto se auto-detecta) |
| `--scale <0.1..1.0>` | Limita la resolución de render. Útil en terminales 4K donde el decode software no da abasto |
| `--loop-video` | Reinicia al llegar al final |
| `--stats` | Telemetría en el HUD: backend, resolución, tamaño de celda, FPS mostrados/decodificados y drops (sin el flag el HUD es limpio: transporte + volumen) |
| `--no-audio` | Sin audio; el vídeo usa reloj monotónico |
| `--sub [fichero.srt\|.ass]` | Activa subtítulos: sin valor usa la pista de texto embebida del contenedor; con fichero carga subtítulos externos. Sin `--sub` no se muestran subtítulos |
| `--no-subs` | Desactiva subtítulos aunque se pase `--sub` (compatibilidad) |
| `--aid <N>` / `--alang <idioma>` | Pista de audio inicial: por índice 1-based dentro de las pistas de audio (`--aid 2` = segunda) o por idioma (`--alang spa`), como mpv. Sin match → pista "best" de FFmpeg |
| `--sid <N>` / `--slang <idioma>` | Pista de subtítulos embebida inicial (por índice de pista de texto / por idioma). Implican subtítulos ON aunque no se pase `--sub` |
| `--hwdec <auto\|none\|vaapi\|cuda\|qsv\|d3d11va\|dxva2\|videotoolbox\|vulkan\|drm\|vdpau>` | Decode por hardware. `auto` (default) prueba los hwaccels de la plataforma y cae a software si ninguno funciona; `none` fuerza software |
| `--verbose` | Deja los logs de FFmpeg en stderr (debugging) y lista los hwaccels compilados |

### Decode por hardware (`--hwdec`)

Con `--hwdec auto` (el default) rtv intenta descargar el decode a la GPU
y cae a software de forma transparente si no hay hwaccel utilizable
(sin GPU, headless, codec no soportado por el driver…). El HUD muestra
el hwaccel activo junto al backend (p.ej. `kitty+vaapi`); si el hwaccel
muere a mitad de reproducción (reset de driver), rtv reabre el decoder
en software desde el punto exacto de reproducción sin cortar audio ni
sync, y la etiqueta del HUD vuelve a mostrar solo el backend.

Los frames decodificados en GPU se copian a RAM (`av_hwframe_transfer_data`
→ NV12) porque el sink es un terminal: las celdas se generan en CPU sí o
sí. El ahorro está en el decode (la parte cara de AV1/HEVC 4K), no en el
escalado.

Matriz de soporte orientativa (depende del FFmpeg enlazado y del driver):

| SO | Orden de prueba en `auto` | Notas |
|---|---|---|
| Linux | VAAPI → CUDA/NVDEC → QSV → VDPAU → Vulkan → DRM | VAAPI cubre Intel y AMD (Mesa); necesita acceso a `/dev/dri` |
| Windows | D3D11VA → DXVA2 → CUDA → QSV → Vulkan | D3D11VA funciona sin libs extra en cualquier GPU moderna |
| macOS | VideoToolbox | Apple Silicon e Intel |

Por codec: H.264/HEVC están soportados por prácticamente cualquier GPU de
la última década; **AV1** solo por GPUs recientes (Intel Xe/Arc, AMD
RDNA2+, NVIDIA RTX 30+). El fallback es por negociación, no global: si el
decoder AV1 no anuncia el hwaccel, ese vídeo va por software aunque otro
H.264 en la misma máquina vaya por GPU.

> Nota: la ganancia con GPU real (CPU%/fps con y sin `--hwdec`) está
> pendiente de medir fuera del sandbox de CI (que no tiene `/dev/dri`;
> ahí solo se valida el camino de fallback).

### Controles

| Tecla | Acción |
|---|---|
| `Espacio` | Pausa / reanudar (también el audio) |
| `←` / `→` | Seek ±5 s |
| `↑` / `↓` | Volumen ±5 (0–200 %) |
| `a` / `#` (`A` = atrás) | Cicla la pista de AUDIO en caliente, sin cortar el playback (el HUD muestra la pista con un OSD de ~2.5 s) |
| `j` (`J` = atrás) | Cicla subtítulos: off → [externa `--sub`] → pistas embebidas → off |
| `q` / `Esc` / `Ctrl+C` | Salir |

#### Cambio de pista en runtime

El cambio de audio reutiliza el protocolo de seek: se bumpean los
seriales de los relojes (los chunks de la pista vieja que queden en el
ring se silencian sin tocar el reloj), el hilo de audio reabre el
decoder sobre el stream nuevo — cada pista puede tener codec,
sample-rate y layout distintos; el resampler normaliza siempre al
formato fijo del sink — y aterriza en el instante actual con recorte
sample-accurate. El vídeo ni se entera: entra en el hold estándar de
master desanclado y continúa en sync al primer chunk de la pista nueva
(|avdiff| mediano medido tras el cambio: <1 ms).

Los subtítulos son más simples: cada pista embebida se decodifica en un
hilo propio de demux-solo-subs al seleccionarla, y `off` simplemente
suelta la pista (las 2 filas reservadas se devuelven al vídeo).

## Arquitectura

Pipeline productor–consumidor con un hilo por etapa, comunicados por canales
`crossbeam` acotados:

```
                 ┌──────────────┐
                 │  fichero.mp4 │
                 └──────┬───────┘
                        │
           ┌────────────┴────────────┐
           ▼                         ▼
   ┌───────────────┐         ┌──────────────┐
   │ demux vídeo   │         │ demux audio  │
   │ decode + sws  │         │ decode + swr │
   │ (hilo 1)      │         │ (hilo 2)     │
   └──────┬────────┘         └──────┬───────┘
          │ RGB24                   │ f32 estéreo
          │ cola por presupuesto    │ ring buffer
          │ de memoria (~48 MB)     │
          ▼                         ▼
   ┌───────────────┐         ┌──────────────┐
   │ loop principal│         │ callback cpal│
   │ · sync ffplay │◄────────│ · alimenta el│
   │ · render      │ AudioClk│   reloj audio│
   │ · HUD e input │ (master)└──────────────┘
   └───────────────┘
```

Piezas clave:

- **`decoder.rs`** — demux + decode + `sws_scale` de vídeo. Los seeks van
  con serial: cada seek incrementa un contador y los frames con serial viejo
  se descartan aguas abajo. El resize es un store atómico de las dims
  destino que el escalador lee antes de cada frame.
- **`audio.rs`** — demux + decode + `swr_convert` de audio, ring buffer
  lock-free hacia el callback de cpal. El callback alimenta el reloj de
  audio con el PTS de la muestra que se está oyendo (latencia de salida
  descontada, suavizada con EMA, con limitador de tasa contra los bursts de
  prebuffer de PulseAudio).
- **`clock.rs`** — relojes estilo ffplay (`FfClock`) con seriales, anclaje,
  staleness y `compute_target_delay`.
- **`player.rs`** — el loop: input, sync, decisión de drop/espera, render y
  HUD. Las esperas se hacen con `event::poll`, así que cualquier tecla o
  resize interrumpe la espera y se atiende al instante.
- **`renderer.rs`** — los cinco backends (kitty, iTerm2, Sixel,
  halfblocks, ascii). Todos recortan a los límites reales del área de
  vídeo, de modo que un frame con dimensiones desfasadas (resize en
  vuelo) nunca desborda la pantalla.
- **`subs.rs`** — subtítulos softsub: parsers SRT/ASS puros en Rust para
  archivos externos (`--sub`) y un hilo demuxer/decoder propio para la
  pista embebida del contenedor (con `AVDISCARD_ALL` en el resto de
  streams para que el demux de subs sea casi gratis). El player consulta
  los eventos activos por PTS con búsqueda binaria en cada refresco.
- **`terminfo.rs`** — sondeo del tamaño de celda (CSI `16t`/`14t`) con
  timeout de 20 ms y lista blanca de terminales que responden; heurística
  8×16 para el resto. En Windows nunca se sondea.

## Estructura del repo

```
rtv/
├── Cargo.toml
├── README.md
├── BUILD-WINDOWS.md         # guía de build para Windows
├── todo.md                  # notas de trabajo y plan de las tareas
├── src/
│   ├── main.rs              # CLI, init de FFmpeg, silenciado de logs
│   ├── player.rs            # loop principal y sync
│   ├── decoder.rs           # hilo de vídeo
│   ├── hwdec.rs             # decode por hardware (unsafe FFmpeg aislado)
│   ├── audio.rs             # hilo de audio + sink cpal
│   ├── clock.rs             # relojes ffplay-style
│   ├── renderer.rs          # backends de render + HUD
│   ├── subs.rs              # subtítulos softsub SRT/ASS (externos y embebidos)
│   ├── tracks.rs            # inventario de pistas + selección --aid/--alang/--sid/--slang
│   ├── terminfo.rs          # detección del tamaño de celda
│   └── input.rs             # eventos de teclado/resize
└── tests/
    ├── integration_sync.py       # sync A/V + seeks, en pty real
    ├── integration_resize.py     # tormenta de resizes + seeks + pausa
    ├── integration_resize_ux.py  # latencia de resize, parpadeo, límites
    ├── integration_grow_quality.py # recuperación de calidad al agrandar
    ├── integration_hwdec.py      # --hwdec: fallback transparente y CLI
    ├── integration_backends_subs.py # Sixel/iTerm2 reales + subs SRT/ASS/embebidos
    ├── integration_tracks.py     # cambio de pista audio/subs en runtime + CLI
    └── stress_exit_hang.py       # salida limpia bajo decode saturado (HEVC)
```

## Tests

Los tests de integración ejecutan el binario release dentro de un pty real,
le inyectan teclas, resizes (`TIOCSWINSZ` + `SIGWINCH`) y analizan tanto el
log de sincronía (`RTV_SYNC_LOG`) como el propio stream de escape sequences
(con [pyte](https://github.com/selectel/pyte) como emulador de terminal):

```bash
cargo build --release
python3 tests/integration_sync.py       video.mp4
python3 tests/integration_resize.py     video.mp4 [ascii|blocks]
python3 tests/integration_resize_ux.py  video.mp4
python3 tests/integration_grow_quality.py video.mp4
python3 tests/integration_hwdec.py      video.mp4
```

Verifican, entre otras cosas: |avdiff| en régimen y tras cada seek, latencia
del primer frame post-seek, supervivencia a tormentas de 60+ resizes con
tamaños degenerados (4×3), latencia de redibujo tras resize, que ninguna
secuencia de cursor escriba fuera de los límites del terminal y que el HUD
no se repinte más de lo necesario.

## Estado y hoja de ruta

Hecho:

- [x] Audio con cpal + swresample, reloj de audio maestro
- [x] Motor de sync estilo ffplay (drop/duplicado con umbrales de ffplay)
- [x] Seeks instantáneos con aterrizaje en keyframe y audio alineado al PTS real
- [x] Escalado adaptativo por tamaño real de celda
- [x] Resize en caliente instantáneo, sin perder sync ni colchón de decode
- [x] HUD adaptativo sin parpadeo; oculto en ventanas minúsculas
- [x] Decode por hardware (`--hwdec`): VAAPI/CUDA/QSV (Linux),
      D3D11VA/DXVA2 (Windows), VideoToolbox (macOS), con fallback
      transparente a software incluso a mitad de stream

Pendiente:

- [ ] Medir la ganancia de `--hwdec` en una máquina con GPU real
      (el sandbox de CI no tiene `/dev/dri`)
- [ ] Barra de progreso clicable con ratón

## Licencia

MIT.
