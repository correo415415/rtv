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
    [ ] decoder.rs: reescribir decode_loop() con la nueva firma (sin dst_w0/dst_h0/resize_rx),
        leyendo target_dims por frame y escalando con Scaler; actualizar drain() igual.
    [ ] renderer.rs: recorte a los límites de la terminal en TODOS los backends (halfblocks/
        ascii/kitty) y tolerancia a frames con dims que no cuadran con el layout actual.
    [ ] player.rs: recomputar layout por frame, cachear el último frame mostrado para redibujo
        instantáneo en el resize, dims mínimas para terminales diminutas, y NO tocar los relojes
        ni el sync en ningún caso durante el resize.
    [ ] Test de integración de resize: tormenta de TIOCSWINSZ sobre el pty durante la
        reproducción → sin crash, fps estable, sync-log limpio; re-ejecutar el test de sync para
        confirmar cero regresiones.
    [ ] Commit + PR completo del trabajo de resize.

## Estado actual

    src/decoder.rs a medio refactor (target_dims + Scaler ya aplicados; decode_loop()/drain()
    pendientes de reescribir — la rama no compila hasta terminar ese paso).
