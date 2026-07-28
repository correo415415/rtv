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

🔜 Pendiente (en curso)

    Re-ejecutar el test varias veces para verificar estabilidad (flakiness) — 1 run PASS hasta ahora.
    Considerar actualizar tests/integration_sync.py para usar los marcadores "# SEEK wall=" en vez de detectar discontinuidades de PTS (más robusto con seeks que aterrizan cerca del punto actual).
    Limpieza: warnings menores (métodos no usados en MasterClock) y ejemplo swr_probe.
