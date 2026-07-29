//! Decode por hardware (VAAPI / CUDA / QSV / D3D11VA / DXVA2 /
//! VideoToolbox / Vulkan / DRM) con fallback transparente a software.
//!
//! Diseño (Fase 1 del plan de todo.md, Tarea 4):
//!
//!   * ffmpeg-the-third 5.0 NO expone wrapper seguro del API de
//!     hwaccel — todo va por `ffmpeg::sys` (bindgen crudo), aislado
//!     en ESTE módulo para que el resto del código no toque unsafe.
//!
//!   * Flujo estándar de FFmpeg (doc/examples/hw_decode.c):
//!       1. `avcodec_get_hw_config(codec, i)` — enumerar los hwaccels
//!          que el DECODER soporta con method HW_DEVICE_CTX.
//!       2. `av_hwdevice_ctx_create` — abrir el dispositivo (p.ej.
//!          /dev/dri/renderD128 para VAAPI). Si falla (sin GPU, sin
//!          permisos, headless) → se prueba el siguiente → software.
//!       3. `get_format` callback — cuando el decoder negocia el
//!          pix_fmt, elegimos el formato HW si está en la lista.
//!       4. Por frame: si `frame.format()` es el formato HW, se copia
//!          a RAM con `av_hwframe_transfer_data` (→ NV12 típicamente)
//!          y sigue el pipeline normal (sws NV12→RGB24).
//!
//!   * ¿Por qué copy-back a RAM y no zero-copy? El sink es un
//!     TERMINAL: las celdas se generan en CPU sí o sí. El ahorro está
//!     en el decode (la parte cara de AV1/HEVC 4K), no en el escalado.
//!
//!   * `get_format` es un callback C sin userdata directo. rtv abre
//!     UN solo decoder de vídeo por proceso, así que el formato HW
//!     esperado se publica en una static atómica (`EXPECTED_HW_FMT`).
//!     Si algún día hay N decoders, esto pasa a `(*ctx).opaque`.

use ffmpeg_the_third as ffmpeg;
use ffmpeg::sys as ff;
use std::ffi::CStr;
use std::sync::atomic::{AtomicI32, Ordering};

/// Preferencia del usuario (CLI `--hwdec`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwPref {
    /// Probar hwaccels en orden de preferencia de la plataforma;
    /// si ninguno funciona → software. (default)
    Auto,
    /// Solo software.
    None,
    /// Forzar un tipo concreto (si falla → software, con aviso).
    Only(ff::AVHWDeviceType),
}

impl HwPref {
    pub fn parse(s: &str) -> Result<Self, String> {
        Ok(match s {
            "auto" => HwPref::Auto,
            "none" | "no" | "off" => HwPref::None,
            "vaapi" => HwPref::Only(ff::AVHWDeviceType::VAAPI),
            "cuda" | "nvdec" => HwPref::Only(ff::AVHWDeviceType::CUDA),
            "qsv" => HwPref::Only(ff::AVHWDeviceType::QSV),
            "d3d11va" => HwPref::Only(ff::AVHWDeviceType::D3D11VA),
            "dxva2" => HwPref::Only(ff::AVHWDeviceType::DXVA2),
            "videotoolbox" | "vt" => HwPref::Only(ff::AVHWDeviceType::VIDEOTOOLBOX),
            "vulkan" => HwPref::Only(ff::AVHWDeviceType::VULKAN),
            "drm" => HwPref::Only(ff::AVHWDeviceType::DRM),
            "vdpau" => HwPref::Only(ff::AVHWDeviceType::VDPAU),
            other => {
                return Err(format!(
                    "--hwdec '{other}' no reconocido (auto|none|vaapi|cuda|qsv|d3d11va|dxva2|videotoolbox|vulkan|drm|vdpau)"
                ))
            }
        })
    }
}

/// Orden de preferencia por plataforma para `--hwdec auto`.
/// Solo se intentan los que el decoder anuncie vía
/// `avcodec_get_hw_config` — la lista es el desempate.
fn platform_preference() -> &'static [ff::AVHWDeviceType] {
    #[cfg(target_os = "linux")]
    {
        &[
            ff::AVHWDeviceType::VAAPI,
            ff::AVHWDeviceType::CUDA,
            ff::AVHWDeviceType::QSV,
            ff::AVHWDeviceType::VDPAU,
            ff::AVHWDeviceType::VULKAN,
            ff::AVHWDeviceType::DRM,
        ]
    }
    #[cfg(target_os = "windows")]
    {
        &[
            ff::AVHWDeviceType::D3D11VA,
            ff::AVHWDeviceType::DXVA2,
            ff::AVHWDeviceType::CUDA,
            ff::AVHWDeviceType::QSV,
            ff::AVHWDeviceType::VULKAN,
        ]
    }
    #[cfg(target_os = "macos")]
    {
        &[ff::AVHWDeviceType::VIDEOTOOLBOX]
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        &[]
    }
}

pub fn type_name(t: ff::AVHWDeviceType) -> &'static str {
    unsafe {
        let p = ff::av_hwdevice_get_type_name(t);
        if p.is_null() {
            "?"
        } else {
            CStr::from_ptr(p).to_str().unwrap_or("?")
        }
    }
}

/// Formato de píxel HW que el `get_format` callback debe elegir.
/// AV_PIX_FMT_NONE (-1) = hwaccel inactivo (elige software).
static EXPECTED_HW_FMT: AtomicI32 = AtomicI32::new(-1);

/// Callback `get_format` estilo hw_decode.c: elige el formato HW
/// publicado en EXPECTED_HW_FMT si el decoder lo ofrece; si no,
/// delega en la elección por defecto de FFmpeg (software). Nunca
/// aborta: un hwaccel que deja de ofrecerse mid-stream (cambio de
/// resolución/perfil no soportado) degrada a software solo.
unsafe extern "C" fn get_format_cb(
    _ctx: *mut ff::AVCodecContext,
    fmts: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    let want = EXPECTED_HW_FMT.load(Ordering::Acquire);
    if want >= 0 && !fmts.is_null() {
        let mut p = fmts;
        while (*p).0 as i32 != -1 {
            if (*p).0 as i32 == want {
                return *p;
            }
            p = p.add(1);
        }
    }
    // Fallback: primer formato NO-hw de la lista (elección software).
    if !fmts.is_null() {
        let mut p = fmts;
        while (*p).0 as i32 != -1 {
            let desc = ff::av_pix_fmt_desc_get(*p);
            if !desc.is_null() && ((*desc).flags & ff::AV_PIX_FMT_FLAG_HWACCEL as u64) == 0 {
                return *p;
            }
            p = p.add(1);
        }
    }
    ff::AVPixelFormat(-1)
}

/// Hwaccel ACTIVO en un decoder abierto.
pub struct ActiveHw {
    /// Tipo de dispositivo (para el HUD/logs).
    pub device_type: ff::AVHWDeviceType,
    /// Formato de píxel HW que emitirá el decoder (p.ej. AV_PIX_FMT_VAAPI).
    pub hw_pix_fmt: ff::AVPixelFormat,
    /// Referencia al device ctx (dueña; se libera en Drop).
    device_ref: *mut ff::AVBufferRef,
}

// El AVBufferRef del device es refcounted y thread-safe (av_buffer_*);
// ActiveHw se mueve al hilo del decoder.
unsafe impl Send for ActiveHw {}

impl Drop for ActiveHw {
    fn drop(&mut self) {
        unsafe {
            if !self.device_ref.is_null() {
                ff::av_buffer_unref(&mut self.device_ref);
            }
        }
    }
}

impl ActiveHw {
    /// Nombre legible del hwaccel (logs de --verbose y diagnósticos).
    #[allow(dead_code)]
    pub fn name(&self) -> &'static str {
        type_name(self.device_type)
    }
}

/// Intenta habilitar decode HW sobre un `AVCodecContext` YA configurado
/// (parámetros del stream copiados, threading seteado) pero AÚN NO
/// abierto con avcodec_open2. `codec` es el decoder que se va a usar
/// (el campo `(*ctx).codec` aún es null antes de open). Devuelve
/// `Some(ActiveHw)` si un hwaccel quedó enganchado (hw_device_ctx +
/// get_format seteados) o `None` si se queda en software (sin tocar
/// el contexto).
///
/// # Safety
/// `ctx` debe ser un AVCodecContext válido no abierto y `codec` el
/// AVCodec con el que se abrirá.
pub unsafe fn try_enable(
    ctx: *mut ff::AVCodecContext,
    codec: *const ff::AVCodec,
    pref: HwPref,
) -> Option<ActiveHw> {
    if matches!(pref, HwPref::None) {
        EXPECTED_HW_FMT.store(-1, Ordering::Release);
        return None;
    }
    if codec.is_null() {
        return None;
    }

    // Candidatos = hwaccels que ESTE decoder soporta con HW_DEVICE_CTX.
    let mut candidates: Vec<(ff::AVHWDeviceType, ff::AVPixelFormat)> = Vec::new();
    let mut i = 0;
    loop {
        let cfg = ff::avcodec_get_hw_config(codec, i);
        if cfg.is_null() {
            break;
        }
        let method_ok =
            ((*cfg).methods & ff::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX.0 as i32) != 0;
        if method_ok {
            candidates.push(((*cfg).device_type, (*cfg).pix_fmt));
        }
        i += 1;
    }
    if candidates.is_empty() {
        return None;
    }

    // Orden de prueba según preferencia.
    let try_order: Vec<(ff::AVHWDeviceType, ff::AVPixelFormat)> = match pref {
        HwPref::Only(t) => candidates.iter().copied().filter(|(dt, _)| *dt == t).collect(),
        HwPref::Auto => {
            let prefs = platform_preference();
            let mut v: Vec<_> = Vec::new();
            for want in prefs {
                if let Some(c) = candidates.iter().find(|(dt, _)| dt == want) {
                    v.push(*c);
                }
            }
            // Cualquier otro que el codec anuncie y no esté en la lista
            // de la plataforma va al final (mejor intentarlo que sw).
            for c in &candidates {
                if !v.contains(c) {
                    v.push(*c);
                }
            }
            v
        }
        HwPref::None => unreachable!(),
    };

    for (dev_type, hw_fmt) in try_order {
        let mut device_ref: *mut ff::AVBufferRef = std::ptr::null_mut();
        let ret = ff::av_hwdevice_ctx_create(
            &mut device_ref,
            dev_type,
            std::ptr::null(),
            std::ptr::null_mut(),
            0,
        );
        if ret < 0 || device_ref.is_null() {
            continue; // sin dispositivo (headless, sin permisos…) → siguiente
        }
        // Enganchar al contexto: el codec toma su PROPIA ref.
        (*ctx).hw_device_ctx = ff::av_buffer_ref(device_ref);
        if (*ctx).hw_device_ctx.is_null() {
            ff::av_buffer_unref(&mut device_ref);
            continue;
        }
        (*ctx).get_format = Some(get_format_cb);
        EXPECTED_HW_FMT.store(hw_fmt.0 as i32, Ordering::Release);
        return Some(ActiveHw {
            device_type: dev_type,
            hw_pix_fmt: hw_fmt,
            device_ref,
        });
    }
    EXPECTED_HW_FMT.store(-1, Ordering::Release);
    None
}

/// Desengancha el hwaccel de un contexto (usado en el fallback
/// mid-stream: se limpia la static para que get_format elija sw).
pub fn disable_expected_fmt() {
    EXPECTED_HW_FMT.store(-1, Ordering::Release);
}

/// Copia un frame HW (superficie GPU) a RAM. `dst` se resetea y FFmpeg
/// lo rellena con el formato de transferencia nativo (NV12 casi
/// siempre). Copia también los props (pts, etc.). Devuelve false si la
/// transferencia falló (driver caído, formato no mapeable).
pub fn transfer_to_ram(
    src: &ffmpeg::util::frame::video::Video,
    dst: &mut ffmpeg::util::frame::video::Video,
) -> bool {
    unsafe {
        ff::av_frame_unref(dst.as_mut_ptr());
        if ff::av_hwframe_transfer_data(dst.as_mut_ptr(), src.as_ptr(), 0) < 0 {
            return false;
        }
        ff::av_frame_copy_props(dst.as_mut_ptr(), src.as_ptr());
    }
    true
}

/// ¿Es `fmt` el formato HW activo? (comparación por valor crudo).
pub fn is_hw_frame(frame: &ffmpeg::util::frame::video::Video, hw: &ActiveHw) -> bool {
    unsafe { (*frame.as_ptr()).format == hw.hw_pix_fmt.0 as i32 }
}

/// Nombre legible de un device type guardado como i32 crudo (para el
/// HUD: el player lee `DecoderHandle::hw_state` atómico; -1 = software).
pub fn name_of_raw(v: i32) -> Option<&'static str> {
    if v <= 0 {
        return None;
    }
    Some(type_name(ff::AVHWDeviceType(v as _)))
}

/// Lista los hwaccels compilados en el FFmpeg enlazado (para --verbose
/// y diagnósticos).
pub fn available_types() -> Vec<&'static str> {
    let mut v = Vec::new();
    let mut t = ff::AVHWDeviceType::NONE;
    unsafe {
        loop {
            t = ff::av_hwdevice_iterate_types(t);
            if t == ff::AVHWDeviceType::NONE {
                break;
            }
            v.push(type_name(t));
        }
    }
    v
}
