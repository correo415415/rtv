Arregla la sincronización vídeo-audio y los seeks con →/← (salto instantáneo sin desincronizar). El test de integración tests/integration_sync.py (pty real + ráfagas de seeks + análisis del sync-log) PASA.
✅ Hecho

    Bug crítico del resampler: SwrCtx::run() de ffmpeg-the-third dimensiona el frame de salida con los samples del PRIMER frame y nunca crece → salida truncada, FIFO interno creciendo sin límite, reloj de audio corriendo ~3-4× más rápido → desincronización total. Fix: resample_frame() con frame de salida nuevo por conversión (capacidad = pendiente interno + frame actual) + compensación de swr_get_delay en el PTS.
    Bug crítico de seeks: ictx.seek(ts, ..ts) con rango EXCLUSIVO → max_ts = ts-1 < ts → avformat_seek_file devolvía EINVAL sin mover el demuxer: los ← no funcionaban en absoluto. Fix: ..=ts (keyframe ≤ target, como ffplay).
    Seeks perdidos en ráfagas: try_send sobre canales bounded(4) descartaba el último seek de →→→←← → audio y vídeo en targets distintos (offset ±5 s). Fix: canales unbounded + send.
    Free-run del vídeo post-seek: vídeo esclavo estricto del audio — con el master desanclado se muestra UN frame (el del target → salto de golpe) y se espera al anclaje del audio; luego se resincroniza frame_timer.
    Jitter del reloj de audio: EMA de la latencia de salida (PulseAudio alterna buffers de 25/50 ms; usar el tamaño del callback actual metía diente de sierra ±25 ms).
    Offset sistemático ±40 ms: corrección proporcional suave dentro del umbral de ffplay en compute_target_delay.
    Deadlock del hilo de audio en pausa (ring lleno) → send_with_stop aborta si hay seek pendiente.
    Resampler recreado en cada seek (sin samples pre-seek con PTS nuevo).
    Seek en pausa muestra el frame del target y lo registra en el sync-log.
    Logging de diagnóstico: RTV_AUDIO_DEBUG, RTV_AUDIO_DEC_DEBUG, anotaciones # SEEK en RTV_SYNC_LOG.

📊 Resultados del test (vídeo dQw4w9WgXcQ vía yt-dlp)
Métrica 	Antes 	Ahora 	Umbral
avdiff medio (normal) 	149.7 ms 	39.8 ms 	<40 ms
avdiff p95 	600 ms 	62.7 ms 	<80 ms
Latencia 1er frame post-seek 	>2 s / perdidos 	<1.5 s todos 	<1.5 s
Mediana post-seek (7 seeks) 	77-90 ms 	39-40 ms 	<60 ms
🔜 Pendiente (en curso)

    Reducir el sesgo residual de ~40 ms: pasar compute_target_delay a la semántica exacta de ffplay (diff = vidclk.now() - master, no PTS del frame pendiente).
    Re-ejecutar el test varias veces para verificar estabilidad (flakiness) y con el vídeo 4K original.
    Limpieza: warnings menores y ejemplo swr_probe.
