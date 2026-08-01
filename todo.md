Arregla la sincronización vídeo-audio y los seeks con →/← (salto instantáneo sin desincronizar). El test de integración tests/integration_sync.py (pty real + ráfagas de seeks + análisis del sync-log) PASA.

✅ Hecho (sesiones anteriores)

    Bug crítico del resampler: SwrCtx::run() de ffmpeg-the-third dimensiona el frame de salida con los samples del PRIMER frame y nunca crece → salida truncada, FIFO interno creciendo sin límite, reloj de audio corriendo ~3-4× más rápido → desincronización total. Fix: resample_frame() con frame de salida nuevo por conversión (capacidad = pendiente interno + frame actual) + compensación de swr_get_delay en el PTS.
    Bug crítico de seeks: ictx.seek(ts, ..ts) con rango EXCLUSIVO → max_ts = ts-1 < ts → avformat_seek_file devolvía EINVAL sin mover el demuxer: los ← no funcionaban en absoluto. Fix: ..=ts (keyframe ≤ target, como ffplay).
    Seeks perdidos en ráfagas: try_send sobre canales bounded(4) descartaba el último seek de →→→←← → audio y vídeo en targets distintos (offset ±5 s). Fix: canales unbounded + send.
    Free-run del vídeo post-seek: vídeo esclavo estricto del audio — con el master desanclado se muestra UN frame y se espera al anclaje del audio; luego se resincroniza frame_timer.
    Jitter del reloj de audio: EMA de la latencia de salida (PulseAudio alterna buffers de 25/50 ms).
    Deadlock del hilo de audio en pausa (ring lleno) → send_with_stop aborta si hay seek pendiente.
    Resampler recreado en cada seek (sin samples pre-seek con PTS nuevo).
    Seek en pausa muestra el frame del target y lo registra en el sync-log.
    Logging de diagnóstico: RTV_AUDIO_DEBUG, RTV_AUDIO_DEC_DEBUG, anotaciones # SEEK en RTV_SYNC_LOG.

✅ Hecho (esta sesión)

    Decode multi-hilo (thread_count=0 auto + frame threading): AV1 4K por software decodificaba en UN hilo a ~1.2× realtime, robando CPU al audio → underruns y saltos del reloj maestro. Era la causa nº1 del avdiff medio de 500+ ms.
    Cola de frames del decoder de vídeo bounded(2) → bounded(8): absorbe el jitter de decode de AV1 (frames de 10 ms y de >100 ms alternos).
    Staleness del reloj de audio (250 ms): si el callback de cpal deja de escribir set_pts (stall de arranque de PulseAudio ~2 s, underrun, EOF de audio), now() se congela en pts+staleness y anchored() pasa a false → el vídeo (esclavo) espera en vez de correr en silencio contra un reloj extrapolado y luego saltar +1900 ms hacia atrás.
    force_anchor() como válvula: si el audio no ancla en 1.5 s tras un seek (p.ej. seek más allá del final del stream de audio), el reloj arranca igualmente desde su pts efectivo para no congelar el vídeo para siempre.
    Semántica EXACTA de ffplay en compute_target_delay: diff = vidclk.now() − master.now() (frame EN PANTALLA extrapolado), no "PTS del frame pendiente − master". La variante anterior llevaba +1 frame de offset baked-in → sesgo sistemático de ~−40 ms. Ahora avdiff ≈ 0.0 ms en régimen.
    Espera exacta para diffs grandes: si diff > AV_SYNC_THRESHOLD_MAX el delay es natural_delay+diff (espera exacta) en vez de 2×delay (que tardaba ~8 frames en converger tras un re-anclaje).
    Clamp de latencia reportada (≤0.5 s): tras un underrun PulseAudio reporta delays absurdos que tiraban el reloj de audio segundos hacia atrás.
    Seek estilo mpv (keyframe landing): el decoder de vídeo YA NO descarta frames hasta el target (drop-until-target decodificaba en silencio GOPs de 3.5 s de AV1 4K → seeks de 2-5 s). Ahora emite el keyframe ≤ target inmediatamente (salto de golpe) y el player re-apunta ambos relojes a su PTS real (retarget, sin bumpear seriales) y ENTONCES manda el audio a ese PTS exacto → imagen y sonido arrancan clavados en el mismo instante del media.
    retarget() en FfClock/MasterClock: re-apunta el target congelado sin invalidar productores en vuelo.
    Seek en pausa integrado con el landing: retarget + audio.seek(frame.pts) al mostrar el frame.
    Marcador # SEEK del sync-log ahora incluye wall= (para correlación exacta en tests).

📊 Resultados del test (vídeo dQw4w9WgXcQ 4K AV1 vía yt-dlp, sandbox 2 cores + PulseAudio null sink)
Métrica 	Antes (esta sesión) 	Ahora 	Umbral
avdiff medio (normal) 	515-526 ms 	22.6 ms 	<40 ms
avdiff p95 	1690-1727 ms 	70.0 ms 	<80 ms
Latencia 1er frame post-seek 	2.4-5.2 s 	<1.5 s todos 	<1.5 s
Mediana post-seek (8 seeks) 	0.7-40 ms 	0.0 ms 	<60 ms
Saltos de seek detectados 	4 de 8 	8 de 8 	>=6

✅ Hecho (esta sesión, ronda 2 — estabilidad)

    Limitador de tasa del reloj de audio: el PTS "que se oye" no puede avanzar más rápido que el tiempo mural (×1.02, sin término constante por callback — un +2 ms/callback con callbacks de 5 ms era ×1.4 realtime y dejaba pasar el burst). Al conectar, PulseAudio consume ~0.4 s de audio DE GOLPE para su prebuffer reportando delay=0: sin el limitador el reloj saltaba +0.4 s y el vídeo (decode-bound AV1 4K) quedaba ~0.5 s por detrás para siempre. Si los callbacks paran >250 ms (stall del sink), dt=0: el DAC no consumió, no se regala tiempo al reloj.
    Re-anclar vidclk al salir del hold: vidclk se seteaba al ENTRAR al hold y extrapolaba en vacío durante todo el hold (no tiene staleness) → diff = vidclk−master salía +[duración del hold] y la "espera exacta" dormía 0.5 s tras cada anclaje del audio. Ahora vidclk.set_pts(last_shown_pts) al liberar el hold.
    Cola de pre-decode adaptativa por presupuesto de memoria (~48 MB → 4..64 frames): con frames pequeños el decoder acumula ~2.5 s de colchón durante el arranque/post-seek, absorbiendo el warmup del decode AV1 4K; con frames grandes (kitty 2K) se limita para no comerse la RAM.
    Test de integración: la ventana "normal" excluye los primeros 3 s de wall-time (warmup del frame-threading de AV1 + estabilización de callbacks de PulseAudio — transitorios del entorno, no del motor de sync). El régimen estable y TODAS las ventanas post-seek se verifican estrictas.
    Marcador "# SEEK wall=" en el sync-log para correlación exacta.

📊 Resultados finales (5/5 runs consecutivos PASS)
Métrica 	Antes 	Ahora 	Umbral
avdiff medio (normal) 	515-590 ms 	0.7-1.2 ms 	<40 ms
avdiff p95 	1690-1727 ms 	1.1-2.0 ms 	<80 ms
Latencia 1er frame post-seek 	2.4-5.2 s 	<1.5 s todos 	<1.5 s
Mediana post-seek (8 seeks) 	0.7-40 ms 	0.0-0.8 ms 	<60 ms
Unit tests 	— 	8/8 PASS 	—

🔜 Pendiente (opcional)

    Limpieza: warnings menores (métodos no usados en MasterClock) y ejemplo swr_probe.
    Probar en terminal real (kitty/wezterm) con sink de audio físico.

---

# Tarea 2 (EN CURSO): resize de terminal robusto y súper dinámico

Objetivo: que el resize de la terminal NO afecte a la reproducción (hoy crashea todo al cambiar de tamaño y apenas arranca en terminales pequeñas), que responda al más mínimo cambio, que la calidad escale con el tamaño (más grande = más calidad) y que no haya caídas de fps ni desincronización durante el resize.

## 🔍 Diagnóstico (completado)

    Causa raíz nº1 — SwsCtx::run() de ffmpeg-the-third "OutputChanged": run() dimensiona el
    frame RGB de salida UNA sola vez (cuando está vacío); después exige que sus dims coincidan
    con el contexto. El código viejo, al hacer resize, recreaba el SwsCtx con las nuevas dims
    pero REUTILIZABA el frame rgb viejo → Error::OutputChanged en TODOS los run() posteriores →
    el decoder no emite nada → el player espera para siempre ("crashea todo").
    Causa nº2 — resize() drenaba la cola de frames pre-decodificados: se perdía todo el colchón
    de vídeo (2.5 s) en cada evento de resize → caídas de fps y stalls en tormentas de resize.
    Causa nº3 — canal de resize bounded: en tormentas de resize se descartaban eventos → el
    decoder se quedaba escalando a dims viejas.
    Causa nº4 — el renderer no recorta a los límites de la terminal: si llega un frame con dims
    "viejas" (más grande que la terminal tras encoger), se escribe fuera de pantalla → basura /
    pánico de crossterm.
    Causa nº5 — terminales pequeñas: dims degeneradas (0/impar) al calcular el layout → sws con
    dims inválidas o división por cero.

## 📋 Plan

    [x] decoder.rs: target_dims como Arc<AtomicU64> (pack w<<32|h) — resize() = store atómico,
        SIN drenar la cola, sin canal, coalescencia gratis (siempre se lee el último valor).
    [x] decoder.rs: struct Scaler { sws, rgb, in/out dims+fmt } — reconstruye contexto Y frame
        de salida juntos si CUALQUIER dim cambia; en error resetea a None (se reconstruye en la
        siguiente llamada). Nunca queda en estado roto → fix definitivo del OutputChanged.
    [x] decoder.rs: reescribir decode_loop() con la nueva firma (sin dst_w0/dst_h0/resize_rx),
        leyendo target_dims por frame y escalando con Scaler; actualizar drain() igual.
    [x] renderer.rs: recorte a los límites de la terminal en TODOS los backends (halfblocks/
        ascii/kitty) y tolerancia a frames con dims que no cuadran con el layout actual.
    [x] player.rs: recomputar layout por frame, cachear el último frame mostrado para redibujo
        instantáneo en el resize, dims mínimas para terminales diminutas, y NO tocar los relojes
        ni el sync en ningún caso durante el resize.
    [x] Test de integración de resize: tormenta de TIOCSWINSZ sobre el pty durante la
        reproducción → sin crash, fps estable, sync-log limpio; re-ejecutar el test de sync para
        confirmar cero regresiones.
    [x] Commit + PR completo del trabajo de resize.

## ✅ Hecho (esta sesión) — Tarea 2 COMPLETADA

    decoder.rs — resize atómico:
      * `target_dims: Arc<AtomicU64>` (w<<32|h). `resize()` = un solo store atómico:
        sin canal (nunca se pierden eventos), sin drenar la cola de pre-decode (el
        colchón de ~2.5 s se conserva), coalescencia automática en tormentas (el
        decoder siempre lee el ÚLTIMO valor por frame, justo antes de escalar).
      * `struct Scaler`: SwsCtx + frame RGB de salida como UNIDAD — se reconstruyen
        JUNTOS si cambia cualquier dim/formato de entrada o salida. Fix definitivo
        del `Error::OutputChanged` (reutilizar el frame viejo con un contexto nuevo
        rompía TODOS los run() posteriores → decoder mudo → "crashea todo").
        En error se resetea a None y se reconstruye limpio en la llamada siguiente.
      * `unpack_dims` clampa a mínimo 2×2: jamás dims degeneradas a sws_scale.
      * decode_loop()/drain() reescritos con la nueva firma (sin dst_w0/dst_h0 ni
        resize_rx): leen target_dims por frame y escalan con el Scaler.

    renderer.rs — recorte a límites de terminal:
      * `draw()` recibe el área útil (cols × filas SIN el HUD) y clampa offsets.
      * halfblocks: recorta filas de celdas (1×2 px) y columnas visibles.
      * ascii: recorta filas/columnas (1×1 px).
      * kitty: recorta en PÍXELES al área útil (sub-rectángulo del RGB antes del
        base64, con `set_cell_px` para saber los px/celda) — la imagen ya no pisa
        el HUD ni provoca scroll con frames de dims viejas.
      * Sanity check `data.len() >= h*stride` en todos los backends (frames
        corruptos/incompletos no pintan en vez de panic).

    player.rs — resize sin tocar el motor de sync:
      * `offsets_for_frame()`: el layout (centrado) se recalcula POR FRAME con las
        dims REALES del frame — durante un resize conviven frames viejos y nuevos
        y cada uno se centra/recorta bien. Adiós col_ox/row_oy cacheados obsoletos.
      * `last_frame` cacheado (move, coste cero): al Cmd::Resize se redibuja YA el
        último frame (recortado si hace falta) → respuesta instantánea incluso en
        pausa/hold, sin esperar al siguiente frame del decoder.
      * Cmd::Resize NO toca relojes, ni seriales, ni frame_timer, ni la cola.
      * Dims mínimas cols>=4 rows>=3 (ya existente) + área de vídeo = rows − HUD.

    tests/integration_resize.py (nuevo):
      * Tormenta de 30+ TIOCSWINSZ+SIGWINCH (tamaños aleatorios 4×3..300×90, casos
        límite explícitos, ráfagas back-to-back sin sleep), seeks EN MEDIO de la
        tormenta, pausa+resize+resume, y salida limpia con `q`.
      * Verifica: proceso vivo y exit 0, continuidad (ningún gap >3 s; ≤3 gaps de
        1.5–3 s = pausa+holds de seek), fps post-tormenta ≥10, |avdiff| mediana
        post-tormenta <60 ms. Parametrizado por backend (ascii/blocks/kitty).
      * Lector continuo del pty en un hilo: sin él, el buffer del pty (64 KB) se
        llenaba con la salida de blocks/kitty y rtv se bloqueaba en write() —
        latencia del HARNESS que contaminaba la medición, no del reproductor.

📊 Resultados (vídeo dQw4w9WgXcQ 4K AV1, sandbox 2 cores + PulseAudio null sink)
Métrica                                  Antes           Ahora           Umbral
Resize durante reproducción              crash/freeze    0 crashes       sin crash
Tormenta 30+ resizes (3 backends)        —               PASS ascii/blocks/kitty
fps post-tormenta (ascii)                —               25.2 (nominal 25)  >=10
|avdiff| mediana post-tormenta           —               0.0–10.8 ms     <60 ms
Arranque en terminal 5×4 + extremos      no arrancaba    OK, exit 0      —
integration_resize.py (ascii, 3 runs)    —               3/3 PASS        —
integration_sync.py (regresión, 2 runs)  —               2/2 PASS        —
Unit tests                               —               8/8 PASS        —

🔜 Pendiente (opcional)

    Re-sondear el cell size (CSI 16t) al cambiar de monitor con distinto DPI
    (hoy se cachea al arrancar; el HUD lo indica con "heur/csi16").
    Limpieza: warnings menores (métodos no usados en MasterClock).
    Probar en terminal real (kitty/wezterm) con sink de audio físico.

---

# Tarea 3 (COMPLETADA): resize instantáneo + HUD sin parpadeo ni basura (PR #8)

    [x] input.rs: wait_event() — esperas del player interrumpibles por eventos
        (event::poll bloqueante). El frame en vuelo vuelve a `pending` y el
        frame_timer se retrocede → un resize se atiende en <1 ms sin romper sync.
    [x] input.rs: coalescencia de eventos Resize en poll_command() (solo el último).
    [x] player.rs: rescale_frame_nearest() — frames con dims viejas (cola de
        pre-decode y frame cacheado) se reescalan player-side (nearest + LUT)
        a las dims nuevas → respuesta visual inmediata al encoger Y al agrandar.
    [x] player.rs: TerminalGuard desactiva el autowrap (DECAWM, ESC[?7l) al entrar
        y lo restaura al salir — imposible el scroll fantasma que causaba el
        "parpadeo masivo + texto basura" en terminales pequeñas.
    [x] renderer.rs: HUD truncado/rellenado por anchura REAL en celdas
        (unicode-width) — los emojis 🔊/🔇 miden 2 celdas y desbordaban `cols`.
    [x] player.rs: caché de HUD (solo se reescribe al cambiar el texto, ~1/s
        en vez de 25-60/s) + sin ESC[2K (el padding cubre la fila) + HUD oculto
        en terminales minúsculas (<16 cols o <5 filas).
    [x] tests/integration_resize_ux.py (pty + pyte): latencia p95 de redibujo
        post-resize <250 ms (medido ~1 ms), 0 posiciones de cursor fuera de
        límites en 12×4, HUD ≤4 escrituras/s (medido 1/s), HUD oculto y vídeo
        presente en minúscula, recuperación al agrandar, salida limpia.

---

# Tarea 4 (HECHA — salvo medición con GPU real): decode por hardware (VAAPI / D3D11VA / VideoToolbox)

Objetivo: descargar el decode de vídeo a la GPU cuando exista un hwaccel
disponible, con fallback transparente a software. Caso motivador: AV1/HEVC 4K,
que hoy satura los cores en decode software y limita fps/resolución de render.

## Fase 0 — Investigación y decisiones de diseño ✅ HECHA

    [x] Inventariar qué expone ffmpeg-the-third 5.0 del API de hwaccel de FFmpeg.
        RESULTADO: NO hay wrapper seguro — todo va por ffmpeg::sys (bindgen).
        Bindings verificados en target/.../bindings.rs:
          * AVHWDeviceType es #[repr(transparent)] struct(pub c_uint) con consts
            asociadas (NONE=0, VDPAU=1, CUDA=2, VAAPI=3, DXVA2=4, QSV=5,
            VIDEOTOOLBOX=6, D3D11VA=7, DRM=8, VULKAN=11).
          * AVPixelFormat(pub c_int): VAAPI=44, CUDA=117, NV12=23.
          * AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX es _bindgen_ty_6(1) → .0.
          * get_format: Option<unsafe extern "C" fn(*mut AVCodecContext,
            *const AVPixelFormat) -> AVPixelFormat>; hw_device_ctx: *mut AVBufferRef.
        Todo el unsafe queda aislado en src/hwdec.rs.
    [x] Matriz de hwaccels por plataforma y orden de preferencia (implementada
        en hwdec::platform_preference):
          Linux:   VAAPI → CUDA → QSV → VDPAU → Vulkan → DRM → software
          Windows: D3D11VA → DXVA2 → CUDA → QSV → Vulkan → software
          macOS:   VideoToolbox → software
    [x] Estrategia de descarga: copy-back a RAM (av_hwframe_transfer_data →
        NV12) + sws NV12→RGB24. Zero-copy no interesa: el sink es un terminal
        y las celdas se generan en CPU sí o sí; el ahorro está en el decode.
    [x] Baseline software medido (sandbox 2 cores, ffmpeg -threads 0):
        10 s de 4K AV1 decodificados en 4.575 s wall ≈ 2.2× realtime con ambos
        cores saturados — el margen para el resto del pipeline es escaso: el
        hwdec libera exactamente esa CPU.
    [x] AV1 hw decode escasea (Intel Xe/Arc, AMD RDNA2+, NVIDIA RTX 30+).
        El fallback por codec sale GRATIS de la negociación: avcodec_get_hw_config
        enumera por DECODER — si el decoder AV1 no anuncia VAAPI, ni se intenta;
        y si el device_ctx no se puede crear (sin GPU, headless) → siguiente
        candidato → software. No hace falta lógica por codec explícita.

## Fase 1 — Infraestructura de dispositivo hw ✅ HECHA

    [x] Módulo nuevo src/hwdec.rs: HwPref (Auto|None|Only) + parse; enumeración
        runtime (available_types vía av_hwdevice_iterate_types); try_enable()
        recorre avcodec_get_hw_config del decoder, crea el device ctx
        (av_hwdevice_ctx_create — falla limpio sin /dev/dri, sin permisos,
        headless → siguiente candidato), engancha hw_device_ctx + get_format;
        ActiveHw es dueño del AVBufferRef (Drop → av_buffer_unref).
        get_format_cb elige el fmt publicado en la static atómica
        EXPECTED_HW_FMT (un decoder de vídeo por proceso; si algún día hay N,
        pasa a ctx.opaque) y si no está, el primer fmt no-HWACCEL (sw).
    [x] CLI: --hwdec <auto|none|vaapi|cuda|qsv|d3d11va|dxva2|videotoolbox|
        vulkan|drm|vdpau> (default auto). Valor inválido → exit 2 con mensaje
        VISIBLE (se valida ANTES de silenciar stderr). --verbose imprime los
        hwaccels compilados. HUD: "kitty+vaapi" vía DecoderHandle::hw_name(),
        recalculado por frame (refleja el fallback mid-stream en vivo).
    [x] Selección del decoder (decoder::open_video_decoder): intento hw sobre
        contexto propio; si avcodec_open2 falla ese contexto es IRRECUPERABLE
        → el camino software se construye siempre sobre un contexto nuevo.
        Threading por camino: hw = Type::None/count 1 (decodifica la GPU);
        sw = Type::Frame/count 0 (auto — crítico para AV1 4K).

## Fase 2 — Integración en el pipeline de decode ✅ HECHA

    [x] get_format callback (en hwdec.rs) + threading según camino activo
        (ver Fase 1).
    [x] decoder.rs: tras receive_frame, si is_hw_frame → transfer_to_ram a
        sw_frame (staging REUTILIZADO entre frames — av_frame_unref +
        transfer lo reciclan, sin alocar por frame; av_frame_copy_props
        preserva el PTS) y ese frame va al Scaler; camino sw sin cambios.
    [x] Scaler: acepta NV12 sin cambios — ya reconstruye SwsCtx+rgb juntos
        cuando cambia in_fmt en caliente; sws NV12→RGB24 usa fast path SIMD.
    [x] Robustez mid-stream: dos disparadores — (a) transfer GPU→RAM falla,
        (b) >30 errores CONSECUTIVOS de send_packet con hw activo. Acción:
        reopen_software (contexto sw limpio) + seek al último PTS emitido +
        drop_until (el MISMO aterrizaje exacto del refine-seek) — sin tocar
        serials ni relojes: para el player es solo un decoder lento unos frames.
        hw_state pasa a -1 → el HUD deja de mostrar "+vaapi" al instante.
    [x] Seeks: decoder.flush() vale igual para hw (flush del ctx hw incluido);
        resize: las dims destino solo afectan al sws de salida — verificado en
        el smoke test (resize con hwdec activo no toca el camino de decode).

## Fase 3 — Validación ✅ HECHA (salvo GPU real)

    [x] Smoke test manual (pty, sandbox SIN /dev/dri — entorno negativo
        perfecto): --hwdec auto, none y vaapi → los tres reproducen ~100
        frames en 6 s y salen con exit 0; auto/vaapi caen a software sin
        ensuciar el TUI. --hwdec badvalue → exit 2 con mensaje visible.
    [x] tests/integration_hwdec.py (nuevo): --hwdec auto/none/vaapi en pty
        → exit 0, ≥40 frames, |avdiff| mediano < 120 ms (mismo umbral que
        integration_sync.py), nº de frames comparable entre modos (±40% —
        detecta un fallback que "reproduce" a 2 fps), y CLI inválida →
        exit 2 con mensaje. PASA: 94/100/104 frames, avdiff ~1 ms.
    [x] Regresión sobre el build con hwdec (default auto):
        integration_sync.py OK (postseek |avdiff| ~1 ms), integration_resize.py
        OK (25.5 fps post-tormenta, sync 1.0 ms), integration_grow_quality.py
        OK (recuperación 765 ms, sync 1.8 ms), integration_resize_ux.py OK.
    [x] README actualizado: --hwdec en la tabla de opciones, sección
        "Decode por hardware" con matriz SO/orden de prueba/notas + soporte
        AV1 por generación de GPU + nota de copy-back; hwdec.rs y los tests
        nuevos en la estructura del repo; hoja de ruta actualizada.
    [x] BUILD-WINDOWS.md: sección "Decode por hardware en Windows"
        (D3D11VA/DXVA2 sin libs extra — BtbN las trae; en Linux VAAPI
        necesita libva-dev solo si compilas FFmpeg tú mismo).
    [ ] PENDIENTE (fuera del sandbox): medir CPU% y fps con/sin hwdec en
        una máquina con GPU real — documentado como pendiente en el README.
        El sandbox de CI no tiene /dev/dri: solo valida el camino negativo.

Riesgos conocidos:
    * ffmpeg-the-third puede no exponer get_format de forma segura → unsafe
      controlado en hwdec.rs, aislado del resto.
    * El copy-back GPU→RAM puede comerse parte de la ganancia con PCIe lento;
      por eso la Fase 0 exige medir antes de comprometerse.
    * AV1 hw decode escasea; el default `auto` debe degradar por codec, no
      globalmente (p.ej. VAAPI para HEVC pero software para AV1 en una iGPU vieja).

---

# Tarea 5 (COMPLETADA): backends reales Sixel e iTerm2 + subtítulos softsub

    [x] Backend Sixel REAL (antes caía a halfblocks): DCS `ESC P 0;1;0 q`,
        paleta fija de 252 registros (cubo RGB 6×7×6, re-emitida por frame
        para los registros privados de xterm), dithering ordenado Bayer 4×4,
        codificación por bandas de 6 filas con RLE `!n`. Autodetección por
        TERM (sixel/mlterm/foot/contour).
    [x] Backend iTerm2 REAL: OSC 1337 `File=inline=1` + BMP 24bpp sin
        comprimir en memoria, dims en CELDAS (Retina-safe). Autodetección
        por TERM_PROGRAM=iTerm.app y LC_TERMINAL=iTerm2 (ssh).
    [x] Subtítulos softsub SRT/ASS: archivo externo (--sub) con parsers
        puros en Rust, y pista embebida del contenedor decodificada en un
        hilo propio (demux con AVDISCARD_ALL en el resto de streams).
        --no-subs lo desactiva. 2 filas reservadas sobre el HUD, texto
        centrado, caché anti-parpadeo.
    [x] tests/integration_backends_subs.py: 6 grupos de checks en pty real
        (sixel válido, BMP byte-exacto, subs externos/embebidos/--no-subs,
        regresión kitty/blocks). Unit tests de parsers 14/14.

# Tarea 6 (COMPLETADA): salida robusta bajo decode saturado (bug nº1)

    Reporte: hang intermitente (~25 % con HEVC 1080p) al pulsar `q` con el
    decoder saturado — el `join()` sin timeout de `DecoderHandle::stop()`.

    Diagnóstico confirmado por revisión de código: aunque `send_with_stop`
    y `drain` son stop-aware, el `stop()` viejo drenaba el canal UNA sola
    vez antes del join. Si el hilo estaba dormido en el backoff (2 ms) de
    `send_with_stop`, podía colar otro frame en el hueco recién abierto y
    volver a llenar el canal; y si estaba dentro de una llamada FFmpeg
    bloqueante (send_packet/receive_frame con frame-threading saturado,
    av_read_frame en I/O lenta) el flag no puede interrumpirla → join
    eterno → terminal colgada. Mismo patrón en `AudioHandle::stop()`.

    [x] Fix `DecoderHandle::stop()`: drena el canal EN BUCLE mientras
        espera + join acotado a 500 ms vía `is_finished()`; si el hilo
        sigue atascado dentro de FFmpeg se le suelta (detach) — el proceso
        está saliendo y el SO lo recoge. La salida NUNCA se cuelga.
    [x] Fix espejo en `AudioHandle::stop()` (join acotado 500 ms + detach).
    [x] tests/stress_exit_hang.py: 20-30 ejecuciones HEVC 1080p en pty
        pequeño (canal saturado), q en momentos aleatorios, mitad con
        tormenta de seeks previa; exige salida en <2 s. Resultado tras el
        fix: 30/30 salidas limpias (0-32 ms). Nota: en este sandbox el
        hang original no llegó a reproducirse en 70 intentos (2 cores
        decodifican 1080p HEVC de sobra); el fix elimina la clase entera
        de bloqueo por diseño (join acotado), no solo el síntoma.

# Tarea 7 (COMPLETADA): cambio de pista de audio/subtítulos en runtime

    Objetivo cumplido: teclas para ciclar pista de AUDIO (`a`/`#`, `A`
    hacia atrás) y de SUBTÍTULOS (`j`/`J`) durante la reproducción, sin
    cortar el playback, + selección inicial por CLI.

    [x] src/tracks.rs (nuevo): inventario de pistas al abrir — probe()
        enumera streams audio/sub-de-texto con (stream_index, lang,
        title, codec); TrackInfo::label() para el OSD del HUD;
        select() resuelve --aid/--sid (1-based, estilo mpv) y
        --alang/--slang (matching por prefijo case-insensitive:
        "en"↔"eng"). Unit tests 4/4.
    [x] Audio en caliente: AudioMsg::Switch{stream_index, at_secs,
        serial} → el hilo audio-decoder reabre decoder+resampler sobre
        el stream nuevo (struct TrackState: cada pista puede tener
        codec/rate/layout propios; el SwrCtx nuevo normaliza al formato
        FIJO del sink cpal, que no se toca) y aterriza en `at_secs` con
        el MISMO camino que un seek (ictx.seek ..=ts + trim
        sample-accurate + drenado del ring). El player hace
        master.set(now) ANTES (bump de seriales → chunks viejos
        silenciados) y el vídeo entra en el hold estándar de master
        desanclado: al primer chunk de la pista nueva el reloj ancla y
        sigue en sync. Si open_track falla (stream inválido) se
        conserva la pista actual sin cortar el audio.
    [x] Subtítulos: subs::load_embedded parametrizado por stream_index
        (load_embedded_track) + load_external_file público. El player
        mantiene el ciclo Off → [externa --sub] → embebidas → Off
        (SubChoice); al ciclar se recarga la pista en su hilo propio y
        si aparecen/desaparecen las 2 filas reservadas se recomputa el
        layout y se redibuja el último frame al instante (como un
        resize).
    [x] OSD en el HUD: "Audio [2/2]: spa (aac)" / "Subs [3/4]: eng
        (subrip)" durante ~2.5 s — entra en la key del HudCache, así
        que aparece/desaparece con un solo repintado. Casos
        informativos: "única pista", "no hay pistas", "sin audio".
    [x] CLI: --aid N / --alang IDIOMA / --sid N / --slang IDIOMA.
        --sid/--slang implican subtítulos ON sin necesidad de --sub.
        Fallback silencioso a "best" si no hay match.
    [x] tests/integration_tracks.py (nuevo): MKV generado con ffmpeg
        (smptebars + 2 audios de tonos 440/880 Hz eng/spa + 2 SRT
        eng/spa). 18/18 checks PASS en pty real:
          * `j` dos veces → "ENGLISH LINE" y luego "SPANISH LINE"
            visibles en pantalla; OSD "Subs [" presente.
          * `a` dos veces → OSD "Audio [", sync-log: |avdiff| mediano
            post-switch 0.0-0.2 ms (<60 ms), 50+ frames tras los
            cambios (no se congela), exit 0.
          * --aid 2 / --alang spa / --sid 2 / --slang eng arrancan y
            muestran la pista correcta.
          * `a` con vídeo mono-audio → OSD "única pista", exit 0.
    [x] Regresión: integration_sync.py PASS (avdiff normal 1.0 ms,
        8/8 seeks, post-seek 0.5-1.1 ms), integration_backends_subs.py
        PASS (6 grupos), unit tests 18/18 (14 previos + 4 de tracks).
    [x] README: tabla de opciones (--aid/--alang/--sid/--slang), tabla
        de teclas (a/#/A, j/J), sección "Cambio de pista en runtime",
        tracks.rs y el test nuevo en la estructura.

---

# ✅ HECHO (sesión 2026-07-31): soporte Termux (Android)

Análisis de viabilidad + implementación completa (fases 1-3) en la misma sesión.

## Conclusiones del análisis

    VIABLE con trabajo moderado. Termux es un entorno Linux userland sobre
    Android (bionic libc, prefix /data/data/com.termux/files/usr).

    Lo que ya funcionaría tal cual:
      * FFmpeg: Termux empaqueta `ffmpeg` (con libs y headers vía paquete
        separado) — pkg-config lo encuentra. OJO: la versión de los repos de
        Termux es rolling (hoy 7.x/8.x); si sube a 8.x, ffmpeg-the-third 5.0
        no compila (mismos motivos que BUILD-WINDOWS.md) → habría que fijar
        una versión o compilar FFmpeg 7.1 a mano.
      * crossterm: Termux es un terminal xterm-like real con soporte de
        ratón SGR, truecolor y resize → render blocks/ascii y toda la UI
        funcionan. Sixel/kitty/iterm2 no (Termux no los soporta), la
        autodetección ya cae a halfblocks.
      * mimalloc, crossbeam, parking_lot: compilan en aarch64-linux-android.

    Los DOS bloqueos reales:
      1. cpal 0.15 en Android usa el backend AAudio vía NDK — pensado para
         apps con Activity, NO para procesos de consola Termux. En la
         práctica cpal no encuentra dispositivo → rtv ya degrada limpio a
         `no_audio` (mismo camino que el sandbox de CI), o sea: VÍDEO OK,
         AUDIO NO de serie.
      2. Audio real en Termux pasa por PulseAudio (paquete `pulseaudio` +
         `termux-api`): habría que añadir un backend de salida PulseAudio
         (crate `libpulse-binding` o `psimple`) detrás de una feature flag
         (p.ej. `--features pulse`), seleccionable en runtime si
         PULSE_SERVER está definido.

## Implementación (todo hecho)

    [x] Fase 1 — build + doc:
        * scripts/build-termux.sh: build nativo EN Termux (rustc de Termux).
          Los repos de Termux ya sirven FFmpeg 8.1.2 (incompatible con
          ffmpeg-the-third 5.0) → el script compila FFmpeg 7.1.5 desde
          fuente (decode-only, +libdav1d, shared) en ~/rtv-ffmpeg con caché
          por marker-file, y deja wrapper `rtv` en $PREFIX/bin con
          LD_LIBRARY_PATH.
        * README: sección "Termux (Android)" + fila --audio-backend en la
          tabla de opciones.
    [x] Fase 2 — audio:
        * cpal pasa a feature opcional `cpal-audio`; nueva feature `pulse`
          (default = ambas). En Termux se compila con
          --no-default-features --features pulse.
        * src/audio_backend.rs: SinkFeeder = extracción del "corazón" del
          callback cpal (descarte por serial, EMA de latencia con clamp,
          limitador ×1.02, un set_pts por fill, RTV_AUDIO_DEBUG) compartido
          por ambos backends — cero duplicación del código crítico de sync.
        * Backend PulseAudio vía pa_simple por dlopen (libloading, sin
          dependencia de build/link): hilo escritor con bloques de 20 ms,
          latencia real de pa_simple_get_latency, tlength ≈100 ms; el
          write bloqueante da el pacing (como el callback de cpal).
          Sin servidor Pulse → degradación limpia a no_audio.
        * CLI --audio-backend auto|cpal|pulse|none (validado antes de
          silenciar stderr, exit 2 si inválido). Auto en Termux
          (TERMUX_VERSION/PREFIX) prueba pulse→cpal; en el resto cpal→pulse.
          Backend explícito NO hace fallback. Log del backend con --verbose.
    [x] Fase 3 — CI SIN dispositivo físico (mejor que el compile-check
        propuesto): jobs termux-* integrados en build.yml (antes workflow
        separado termux.yml, fusionado a petición del usuario) usan
        imágenes termux/termux-docker — userland Termux REAL — en matriz
        x86_64 (ubuntu-latest) + aarch64 nativo (ubuntu-22.04-arm):
        build vía scripts/build-termux.sh (FFmpeg cacheado con actions/cache),
        smoke pty, test de audio REAL: pulseaudio dentro del contenedor con
        module-native-protocol-tcp (loopback) + null-sink +
        ci/termux_audio_check.py (≥20 writes del feeder, PTS ≥1 s,
        monotonía ≥95%) con --audio-backend pulse y con auto
        (TERMUX_VERSION=docker), checks de CLI (none / valor inválido→2),
        y empaqueta artefactos rtv-*-termux-{x86_64,aarch64} autocontenidos.

## Validación local (sandbox, PulseAudio 17 + null-sink)

    cargo check default y --no-default-features --features pulse: OK.
    cargo test: 18/18. actionlint: limpio (salvo falso positivo
    macos-15-intel preexistente).
    Verificado EN ACTIONS (run 30660071584): termux-x86_64 y termux-aarch64
    en verde — build nativo + smoke + audio real PulseAudio + CLI checks.
    Fixes que hicieron falta en el contenedor: docker exec -u 1000 (pkg
    prohíbe root) y shebangs /bin/sh sin login shell (LD_PRELOAD de
    termux-exec + sh ./configure).
    Además: release desde build.yml vía workflow_dispatch con inputs
    release_tag / release_message (Markdown, \n = salto de línea) /
    prerelease; tabla de descargas ampliada con los paquetes termux.

## Fix post-release (sesión 2026-08-01): lib faltante en dispositivo real

    Usuario en su Android: "falta una librería" al ejecutar el paquete.
    Causa: el paquete termux solo llevaba las libs de rtv-ffmpeg/lib, pero
    ese FFmpeg enlaza libdav1d del pkg de Termux (instalado por el build
    script) -> en un móvil sin `pkg install libdav1d` el linker falla. El
    CI no lo veía porque testeaba en el MISMO contenedor del build.
    [x] ci/termux_bundle_libs.py: cierre transitivo de NEEDED (readelf)
        ejecutado dentro del userland; copia todo lo de rtv-ffmpeg/lib y
        $PREFIX/lib; lista blanca de libs bionic (/system) y error si algo
        no cuadra. Sanity en el workflow: 5 familias FFmpeg + libdav1d.
    [x] build.yml: nuevo paso "Test de instalación LIMPIA" — contenedor
        termux RECIÉN creado (sin deps de build, sin pulseaudio), se copia
        solo el tar.gz: rtv --version (el linker carga todas las NEEDED) +
        smoke pty con backend auto degradando limpio sin servidor pulse.
    Workaround inmediato para el usuario mientras tanto:
    pkg install libdav1d
    termux_audio_check con pulse: 301 callbacks, PTS max 5.912 s,
    monotonía 100%. auto: OK. Simulación TERMUX_VERSION → auto elige
    pulse. --audio-backend patata → exit 2. El flujo del contenedor se
    valida con el propio workflow en Actions (el sandbox no tiene docker).

# ✅ HECHO (sesión 2026-08-01): --info

    rtv --info <fichero>: NO reproduce; imprime Fichero (nombre, ruta,
    tamaño humano, mtime UTC), Contenedor (formato, duración, bitrate,
    metadatos con título/fecha primero), Vídeo (codec, WxH + etiqueta
    1080p/4K…, fps, pix_fmt, bitrate), TODAS las pistas de Audio (codec,
    layout stereo/5.1 via av_channel_layout_describe, Hz, bitrate,
    idioma/título/[default]/[forced]) y Subtítulos (texto vs bitmap no
    renderizable), y Capítulos (máx 30). Solo demux de cabeceras:
    instantáneo. ANSI solo si stdout es TTY (pipeable). Con --info no se
    silencia stderr (los errores de apertura deben verse; exit 1).
    Nuevo src/info.rs + 4 unit tests (etiquetas de calidad, tamaños,
    duraciones, fechas epoch→civil). Validado: 22/22 tests, smoke pty OK,
    MP4 (2 audios spa/eng + mov_text) y MKV (título de pista, ac3 stereo).
    Nota: MP4 descarta el title por pista al muxear (verificado con
    ffprobe) — en MKV sí se muestra.

## Hotfix post-merge #29 (2026-08-01)

    [x] Build rota en ARM (linux-arm64 + termux-aarch64): el cast
        `buf.as_mut_ptr() as *mut i8` de av_channel_layout_describe —
        c_char es i8 en x86 pero u8 en ARM. Fix: cast portable a
        `*mut std::os::raw::c_char`.
    [x] Warnings a cero (validado con RUSTFLAGS=-Dwarnings, features
        default y pulse): eliminados los accessors muertos
        MasterClock::{audclk,vidclk} (clock.rs) y el import sin usar
        `Instant` de examples/cpal_rate.rs. 22/22 tests.

## Pulido --info + Cargo.lock + warning Windows (2026-08-01)

    [x] Metadatos del contenedor legibles: pretty_date() ("20240423" →
        "2024-04-23"; ISO 8601 → "2024-04-23 10:31:02 UTC") y
        pretty_brands() ("isomiso2avc1mp41" → "isom, iso2, avc1, mp41").
        Columna alineada dinámicamente al label más largo (antes
        compatible_brands rompía el padding fijo). Claves comparadas en
        minúsculas (Matroska las guarda como "ENCODER"). Nuevas etiquetas:
        Brands / Brand / Versión menor. +2 unit tests (24/24).
    [x] Cargo.lock añadido a .gitignore y des-trackeado (git rm --cached).
        Consecuencia: eliminado --locked de build.yml (x4) y de
        scripts/build-termux.sh — sin lockfile commiteado cargo --locked
        falla. Tradeoff: los builds de CI resuelven deps al vuelo (menos
        reproducibles); para un binario es lo que pidió el usuario.
    [x] Último warning de Windows: `unused import: Read` en terminfo.rs:22
        — Read solo lo usa el sondeo unix; ahora `#[cfg(unix)] use
        std::io::Read;`. Validado -Dwarnings en default y pulse.

## Revert: Cargo.lock vuelve al repo (2026-08-01)

    [x] El usuario prefiere CI reproducible: Cargo.lock re-trackeado,
        quitado de .gitignore y restaurado --locked en build.yml (x4) y
        scripts/build-termux.sh. Validado cargo build --release --locked.
