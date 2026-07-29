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

# Tarea 4 (PLAN — sin empezar): decode por hardware (VAAPI / D3D11VA / VideoToolbox)

Objetivo: descargar el decode de vídeo a la GPU cuando exista un hwaccel
disponible, con fallback transparente a software. Caso motivador: AV1/HEVC 4K,
que hoy satura los cores en decode software y limita fps/resolución de render.

## Fase 0 — Investigación y decisiones de diseño

    [ ] Inventariar qué expone ffmpeg-the-third 5.0 del API de hwaccel de FFmpeg:
        av_hwdevice_ctx_create, AVCodecContext.hw_device_ctx, av_hwframe_transfer_data,
        AVPixelFormat negotiation (get_format callback). Verificar si hay bindings
        seguros o si toca unsafe con ffmpeg_the_third::sys directamente.
    [ ] Decidir la matriz de hwaccels por plataforma y orden de preferencia:
          Linux:   VAAPI (Intel/AMD) → CUDA/NVDEC (NVIDIA) → software
          Windows: D3D11VA → DXVA2 (legacy) → software
          macOS:   VideoToolbox → software
    [ ] Decidir la estrategia de descarga de frames: siempre transferir a RAM
        (av_hwframe_transfer_data → NV12) y convertir NV12→RGB24 con sws, porque
        el destino final es el terminal (CPU). Documentar el coste del copy-back
        y por qué NO interesa zero-copy (no hay GPU en el sink).
    [ ] Medir baseline de rendimiento software (fps decode, uso de CPU, W) con
        el vídeo de referencia (dQw4w9WgXcQ 4K AV1) para comparar después.
    [ ] Investigar disponibilidad de hw decode AV1 (solo GPUs recientes: Intel
        Xe/Arc, AMD RDNA2+, NVIDIA RTX 30+) y definir el fallback por codec.

## Fase 1 — Infraestructura de dispositivo hw

    [ ] Módulo nuevo src/hwdec.rs: enumeración de hwaccels disponibles en runtime
        (av_hwdevice_iterate_types), creación del hw_device_ctx con manejo de
        errores (dispositivo ocupado, sin permisos /dev/dri, headless).
    [ ] CLI: --hwdec <auto|none|vaapi|d3d11va|videotoolbox|cuda> (default: auto)
        y mostrar el hwaccel activo en el HUD junto al backend de render.
    [ ] Selección del decoder: probar hwaccel elegido con el codec del stream;
        si get_format no ofrece el pix_fmt hw o falla la creación → log claro
        (con --verbose) y fallback a software SIN abortar.

## Fase 2 — Integración en el pipeline de decode

    [ ] decoder.rs: get_format callback para elegir el AV_PIX_FMT del hwaccel;
        mantener thread_count=0 para el fallback software (los hwaccel no usan
        frame threading — ajustar según camino activo).
    [ ] decoder.rs: tras receive_frame, si frame.format() es hw → transfer a
        frame NV12 en RAM (reutilizar buffer, no alocar por frame) y pasar ese
        frame al Scaler; si es sw → camino actual sin cambios.
    [ ] Scaler: aceptar NV12 como entrada (ya soporta cambio de in_fmt en
        caliente, verificar que sws NV12→RGB24 elige el fast path SIMD).
    [ ] Robustez: si el decode hw falla A MITAD de stream (reset de driver,
        cambio de resolución mid-stream no soportado) → reabrir el decoder en
        software desde el último keyframe, sin romper seriales ni el sync.
    [ ] Verificar interacción con seeks (flush del decoder hw) y con el resize
        (las dims destino no cambian el camino de decode, solo el sws de salida).

## Fase 3 — Validación

    [ ] Test de integración: reproducir con --hwdec auto y --hwdec none y
        comparar sync-log (mismos umbrales de avdiff) + fps decode ≥ baseline.
    [ ] Test de fallback: --hwdec vaapi en un entorno sin /dev/dri debe caer a
        software con exit 0 y sin ensuciar el TUI (el sandbox de CI vale como
        entorno negativo; el camino positivo requiere GPU real).
    [ ] Test de estrés: seeks en ráfaga + tormenta de resizes con hwdec activo
        (reusar integration_resize.py e integration_sync.py parametrizados).
    [ ] Medir y documentar en el README: CPU% y fps con/sin hwdec en al menos
        una máquina con GPU real (fuera del sandbox).
    [ ] Actualizar README (matriz de soporte por SO/GPU/codec) y BUILD-WINDOWS.md
        (D3D11VA no necesita libs extra; VAAPI necesita libva-dev en build).

Riesgos conocidos:
    * ffmpeg-the-third puede no exponer get_format de forma segura → unsafe
      controlado en hwdec.rs, aislado del resto.
    * El copy-back GPU→RAM puede comerse parte de la ganancia con PCIe lento;
      por eso la Fase 0 exige medir antes de comprometerse.
    * AV1 hw decode escasea; el default `auto` debe degradar por codec, no
      globalmente (p.ej. VAAPI para HEVC pero software para AV1 en una iGPU vieja).
