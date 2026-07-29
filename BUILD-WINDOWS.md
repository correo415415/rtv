# Compilar rtv en Windows

## ⚠️ IMPORTANTE: versión de FFmpeg

Este proyecto usa `ffmpeg-the-third 5.0`, que **soporta FFmpeg 5.1–8.1** en
teoría, pero en la práctica **requiere FFmpeg 7.1.x en Windows** por dos
motivos:

1. Las variantes `V410` / `V308` / `V408` de `AVCodecID` (que la crate usa
   sin gate `#[cfg]`) sólo existen desde FFmpeg 7.1.
2. Los campos legacy del `AVCodec` (`supported_samplerates`, `sample_fmts`,
   `pix_fmts`, `ch_layouts`) que la crate lee directamente **fueron eliminados
   en FFmpeg 8.0** (sustituidos por `avcodec_get_supported_config`).

→ La única versión que satisface ambas condiciones es **FFmpeg 7.1.x**.

## Ruta rápida y probada (5 minutos, sin vcpkg)

### 1) Descargar FFmpeg **7.1 shared** (con .dll + .lib + include)

Ve a los builds oficiales de BtbN:

  <https://github.com/BtbN/FFmpeg-Builds/releases>

Descarga uno de estos ficheros:

- `ffmpeg-n7.1-latest-win64-lgpl-shared.zip` ← **recomendado** (LGPL, licencia laxa)
- `ffmpeg-n7.1.1-latest-win64-lgpl-shared.zip`, etc. — cualquier `n7.1.x`

⚠️ **Tiene que llevar `shared` en el nombre y ser rama `n7.1`**. Los
`static` no traen los `.lib` que necesita el linker. `master` (que es 8.x)
NO compila con esta versión de la crate.

### 2) Descomprimir en una ruta sin espacios

Descomprime en algún sitio como:

```
C:\ffmpeg
```

Tras descomprimir tienes que ver:

```
C:\ffmpeg\
   ├── bin\      (ffmpeg.exe, avcodec-*.dll, avformat-*.dll, ...)
   ├── include\  (libavcodec\, libavformat\, ...)
   └── lib\      (avcodec.lib, avformat.lib, ...)
```

Si el zip te deja una carpeta extra tipo `ffmpeg-n7.1-latest-win64-lgpl-shared\`,
mueve su **contenido** directamente a `C:\ffmpeg\` (queremos `C:\ffmpeg\include`,
no `C:\ffmpeg\ffmpeg-n7.1-...\include`).

### 3) Fijar las variables de entorno

Abre PowerShell **como el usuario que usas normalmente** (no admin) y ejecuta:

```powershell
# Para la sesión actual:
$env:FFMPEG_DIR = "C:\ffmpeg"
$env:PATH = "C:\ffmpeg\bin;" + $env:PATH

# Para dejarlo permanente:
[System.Environment]::SetEnvironmentVariable("FFMPEG_DIR", "C:\ffmpeg", "User")
$oldPath = [System.Environment]::GetEnvironmentVariable("PATH", "User")
[System.Environment]::SetEnvironmentVariable("PATH", "C:\ffmpeg\bin;$oldPath", "User")
```

Cierra y abre PowerShell de nuevo.

Verifica:

```powershell
$env:FFMPEG_DIR    # → C:\ffmpeg
ffmpeg -version    # → debería imprimir "ffmpeg version n7.1..."
```

### 4) Compilar

```powershell
cd "C:\Users\PC\Desktop\SANTI\terminal player\rtv"
cargo clean            # importante — para que reintente la detección
cargo build --release
```

La primera build tarda ~30-60 s (bindgen + LTO fat). Salidas:

```
target\release\rtv.exe
```

Sobre el audio: `cpal` en Windows enlaza con **WASAPI** que forma parte del
sistema — **no hay que instalar nada más** para el audio.

### 5) Ejecutar

**Importante**: el `.exe` es dinámicamente enlazado contra las DLL de FFmpeg,
así que necesita encontrarlas al arrancar. Como pusiste `C:\ffmpeg\bin` en el
PATH en el paso 3, ya funciona. Si prefieres que sea portable, copia estas
DLL de `C:\ffmpeg\bin\` al lado del `.exe`:

```
avcodec-61.dll  avformat-61.dll  avutil-59.dll
swscale-8.dll   swresample-5.dll
```

Ejemplo de uso:

```powershell
.\target\release\rtv.exe "C:\videos\prueba.mp4"                # audio + vídeo
.\target\release\rtv.exe "C:\videos\prueba.mp4" --stats        # con FPS detallados
.\target\release\rtv.exe "C:\videos\prueba.mp4" --no-audio     # sólo vídeo
.\target\release\rtv.exe "C:\videos\prueba.mp4" --loop-video   # loop
```

## Alternativa: vcpkg

Si prefieres vcpkg:

```powershell
git clone https://github.com/microsoft/vcpkg C:\vcpkg
C:\vcpkg\bootstrap-vcpkg.bat
# Importante: la rama de vcpkg debe corresponder a FFmpeg 7.1
C:\vcpkg\vcpkg install ffmpeg[avcodec,avformat,swscale,swresample]:x64-windows
$env:VCPKG_ROOT = "C:\vcpkg"
cargo clean
cargo build --release
```

`vcpkg install` compila FFmpeg desde cero → tarda **20-40 minutos**. Por eso
recomiendo BtbN.

## Decode por hardware en Windows

`--hwdec auto` prueba **D3D11VA → DXVA2 → CUDA → QSV → Vulkan**. Buenas
noticias: **D3D11VA y DXVA2 no necesitan ninguna librería extra** — van
contra las API de Windows que ya están en el sistema, y los builds de BtbN
las traen habilitadas. Con cualquier GPU moderna (Intel/AMD/NVIDIA) el
decode de H.264/HEVC debería salir por GPU sin hacer nada; AV1 solo con
GPUs recientes (Intel Arc, AMD RDNA2+, NVIDIA RTX 30+).

En Linux, en cambio, VAAPI requiere `libva-dev` (y drivers Mesa/iHD) en
la máquina de build si compilas FFmpeg tú mismo; con el FFmpeg del sistema
(`libavcodec-dev` de la distro) ya viene incluido.

## Terminal recomendado

| Terminal | Compatible | Notas |
|---|---|---|
| **Windows Terminal** | ✅ Perfecto | Truecolor + half-blocks + audio WASAPI. Recomendado. |
| **PowerShell 7 host** | ✅ | Igual que WT. |
| **cmd.exe (Win10 22H2+)** | ✅ | Funciona; puede quedar con celdas más gordas. |
| **Alacritty / WezTerm** | ✅✅ | WezTerm además responde a CSI 16t → mejor escalado adaptativo. |

## Problemas comunes

- **`avcodec-61.dll` no se encuentra al ejecutar** → o falta `C:\ffmpeg\bin`
  en el PATH, o esas DLL no están junto al `.exe`.
- **`LINK : fatal error LNK1181: cannot open input file 'avcodec.lib'`** →
  descargaste el zip `static`. Usa el `shared`.
- **`error[E0599]: no associated function or constant named V410`** →
  Descargaste FFmpeg <7.1 (típicamente 7.0.x). Usa 7.1.x.
- **`error[E0609]: no field 'supported_samplerates'`** → Descargaste
  FFmpeg 8.x. Usa 7.1.x (`master`/`8.1` NO valen para esta crate).
- **`Package 'alsa' not found`** → sólo en Linux. Instala `libasound2-dev`.
- **Los colores se ven con líneas verticales gordas** → tu fuente no es
  cuadrada. Añade `--scale 0.5` o cambia la fuente a Cascadia Mono / Consolas.
- **No suena el audio pero aparece 🔊** → puede que el device por defecto no
  soporte la sample rate del vídeo. Prueba `--verbose` para ver el error.
  `--no-audio` deshabilita el audio si te da problemas.
