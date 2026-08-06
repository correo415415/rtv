// rotation.rs — auto-rotación de vídeos según metadatos del contenedor.
//
// Los vídeos grabados con el móvil en vertical se almacenan APAISADOS
// (el sensor no gira) + un "Display Matrix" en el stream que dice
// "róta(me) 90° al mostrar". Los reproductores serios (mpv, ffplay,
// VLC) aplican esa rotación automáticamente; sin ella el vídeo se ve
// tumbado. Este módulo:
//
//   1. Lee el Display Matrix del stream (coded_side_data del
//      codecpar — la ubicación moderna donde el demuxer MP4/MOV lo
//      deja en FFmpeg ≥ 6) y, como fallback, el tag de metadata
//      "rotate" (ficheros antiguos / remuxes de FFmpeg viejo).
//   2. Normaliza el ángulo a una de las 4 rotaciones cardinales
//      (0/90/180/270) — igual que hace ffplay: ángulos arbitrarios
//      no existen en la práctica (los escribe el móvil) y soportarlos
//      requeriría resampling con interpolación.
//   3. Rota el buffer RGB24 YA ESCALADO (post-sws, en el hilo del
//      decoder). Rotar después de escalar es lo barato: se rota el
//      frame pequeño de destino (p.ej. 640×360) y no el fuente 4K.
//
// Convención de signos (la de ffplay/mpv): av_display_rotation_get
// devuelve el ángulo en grados ANTIHORARIOS que la matriz aplica;
// para corregir en pantalla hay que rotar el frame `-θ` (es decir,
// θ grados HORARIOS). `Transform::Rot90` = rotar el frame 90° en
// sentido horario al presentar.

use ffmpeg_the_third as ffmpeg;

use crate::decoder::RgbFrame;

/// Rotación de presentación a aplicar a cada frame decodificado.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transform {
    None,
    /// 90° horario (vídeo de móvil "vertical" típico).
    Rot90,
    /// 180° (boca abajo).
    Rot180,
    /// 270° horario (= 90° antihorario).
    Rot270,
}

impl Transform {
    /// ¿Intercambia anchura y altura?
    pub fn swaps_dims(self) -> bool {
        matches!(self, Transform::Rot90 | Transform::Rot270)
    }

    /// Dimensiones a las que hay que ESCALAR el frame fuente (sin
    /// rotar) para que tras la rotación quede exactamente en
    /// `(dst_w, dst_h)`: con 90/270 el sws escala a las transpuestas.
    pub fn pre_rotate_dims(self, dst_w: u32, dst_h: u32) -> (u32, u32) {
        if self.swaps_dims() {
            (dst_h, dst_w)
        } else {
            (dst_w, dst_h)
        }
    }

    /// Tamaño fuente tal y como se PRESENTA (para el cálculo de
    /// layout/aspect del player).
    pub fn display_size(self, w: u32, h: u32) -> (u32, u32) {
        if self.swaps_dims() {
            (h, w)
        } else {
            (w, h)
        }
    }

    /// Etiqueta humana para `--info` / HUD (`None` si no hay rotación).
    pub fn label(self) -> Option<&'static str> {
        match self {
            Transform::None => None,
            Transform::Rot90 => Some("rotado 90°"),
            Transform::Rot180 => Some("rotado 180°"),
            Transform::Rot270 => Some("rotado 270°"),
        }
    }
}

/// Rota el frame RGB24 in-place (reescribe buffer y dims).
///
/// Coste: una pasada O(w·h) sobre el frame YA escalado al tamaño de
/// la terminal (cientos de KB) — despreciable frente al decode+sws.
/// Solo se llama cuando hay rotación real (`Transform::None` es no-op
/// sin copia).
pub fn rotate_frame(frame: &mut RgbFrame, t: Transform) {
    if t == Transform::None {
        return;
    }
    let w = frame.width as usize;
    let h = frame.height as usize;
    if w == 0 || h == 0 || frame.data.len() < w * h * 3 {
        return;
    }
    let src = &frame.data;
    let mut dst = vec![0u8; w * h * 3];
    match t {
        Transform::None => unreachable!(),
        // 90° horario: dst tiene h columnas × w filas.
        // dst(x, y) = src(col = y, fila = h-1-x)
        Transform::Rot90 => {
            let (dw, dh) = (h, w);
            for y in 0..dh {
                let drow = y * dw * 3;
                for x in 0..dw {
                    let s = ((h - 1 - x) * w + y) * 3;
                    let d = drow + x * 3;
                    dst[d..d + 3].copy_from_slice(&src[s..s + 3]);
                }
            }
            frame.width = dw as u32;
            frame.height = dh as u32;
        }
        // 180°: mismas dims; dst(x, y) = src(w-1-x, h-1-y).
        Transform::Rot180 => {
            for y in 0..h {
                let drow = y * w * 3;
                let srow = (h - 1 - y) * w * 3;
                for x in 0..w {
                    let s = srow + (w - 1 - x) * 3;
                    let d = drow + x * 3;
                    dst[d..d + 3].copy_from_slice(&src[s..s + 3]);
                }
            }
        }
        // 270° horario (90° antihorario): dst h×w.
        // dst(x, y) = src(col = w-1-y, fila = x)
        Transform::Rot270 => {
            let (dw, dh) = (h, w);
            for y in 0..dh {
                let drow = y * dw * 3;
                for x in 0..dw {
                    let s = (x * w + (w - 1 - y)) * 3;
                    let d = drow + x * 3;
                    dst[d..d + 3].copy_from_slice(&src[s..s + 3]);
                }
            }
            frame.width = dw as u32;
            frame.height = dh as u32;
        }
    }
    frame.data = dst;
}

/// Rotación de presentación del stream de vídeo: Display Matrix del
/// codecpar (moderno) o tag `rotate` de la metadata (legado).
pub fn from_stream(stream: &ffmpeg::format::stream::Stream) -> Transform {
    if let Some(theta) = display_matrix_theta(stream) {
        return transform_from_theta(theta);
    }
    // Fallback legado: tag "rotate" (MOV antiguos, remuxes viejos).
    // Convención del tag: grados HORARIOS a aplicar al presentar
    // (la contraria a la matriz — por eso aquí no se niega).
    if let Some(r) = stream.metadata().get("rotate") {
        if let Ok(deg) = r.trim().parse::<f64>() {
            return transform_from_theta(deg);
        }
    }
    Transform::None
}

/// θ de presentación (grados horarios) desde el Display Matrix del
/// stream, o `None` si el stream no trae matriz.
fn display_matrix_theta(stream: &ffmpeg::format::stream::Stream) -> Option<f64> {
    use ffmpeg::ffi;
    unsafe {
        let par = (*stream.as_ptr()).codecpar;
        if par.is_null() {
            return None;
        }
        let sd = ffi::av_packet_side_data_get(
            (*par).coded_side_data,
            (*par).nb_coded_side_data,
            ffi::AVPacketSideDataType::DISPLAYMATRIX,
        );
        if sd.is_null() || (*sd).data.is_null() || (*sd).size < 9 * 4 {
            return None;
        }
        // La matriz son 9 int32 en fixed-point 16.16 —
        // av_display_rotation_get hace la trigonometría. Devuelve
        // grados ANTIHORARIOS aplicados por la matriz; la corrección
        // de presentación es el negado (ffplay:
        // `theta = -av_display_rotation_get(m)`).
        let m = (*sd).data as *const i32;
        let ccw = ffi::av_display_rotation_get(m);
        if ccw.is_nan() {
            return None;
        }
        Some(-ccw)
    }
}

/// Normaliza un ángulo horario arbitrario a la rotación cardinal más
/// cercana (redondeo como ffplay: solo se soportan múltiplos de 90;
/// 89.98° del sensor del móvil ⇒ 90°).
fn transform_from_theta(theta_cw: f64) -> Transform {
    // A [0, 360): rem_euclid de f64.
    let t = theta_cw.rem_euclid(360.0);
    // Cardinal más cercano (355° → 0).
    match ((t / 90.0).round() as i64).rem_euclid(4) {
        1 => Transform::Rot90,
        2 => Transform::Rot180,
        3 => Transform::Rot270,
        _ => Transform::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_2x3(pix: &[[u8; 3]]) -> RgbFrame {
        // 2 columnas × 3 filas, orden fila a fila.
        assert_eq!(pix.len(), 6);
        RgbFrame {
            width: 2,
            height: 3,
            pts: 0.0,
            serial: 0,
            data: pix.iter().flatten().copied().collect(),
        }
    }

    fn px(f: &RgbFrame, x: u32, y: u32) -> [u8; 3] {
        let i = ((y * f.width + x) * 3) as usize;
        [f.data[i], f.data[i + 1], f.data[i + 2]]
    }

    // Píxeles nombrados: a b / c d / e f  (2 ancho × 3 alto)
    const A: [u8; 3] = [1, 0, 0];
    const B: [u8; 3] = [2, 0, 0];
    const C: [u8; 3] = [3, 0, 0];
    const D: [u8; 3] = [4, 0, 0];
    const E: [u8; 3] = [5, 0, 0];
    const F: [u8; 3] = [6, 0, 0];

    #[test]
    fn rot90_horario() {
        let mut f = frame_2x3(&[A, B, C, D, E, F]);
        rotate_frame(&mut f, Transform::Rot90);
        // 90° CW de (a b / c d / e f) = (e c a / f d b), 3×2.
        assert_eq!((f.width, f.height), (3, 2));
        assert_eq!(px(&f, 0, 0), E);
        assert_eq!(px(&f, 1, 0), C);
        assert_eq!(px(&f, 2, 0), A);
        assert_eq!(px(&f, 0, 1), F);
        assert_eq!(px(&f, 1, 1), D);
        assert_eq!(px(&f, 2, 1), B);
    }

    #[test]
    fn rot180() {
        let mut f = frame_2x3(&[A, B, C, D, E, F]);
        rotate_frame(&mut f, Transform::Rot180);
        // 180° = (f e / d c / b a), 2×3.
        assert_eq!((f.width, f.height), (2, 3));
        assert_eq!(px(&f, 0, 0), F);
        assert_eq!(px(&f, 1, 0), E);
        assert_eq!(px(&f, 0, 2), B);
        assert_eq!(px(&f, 1, 2), A);
    }

    #[test]
    fn rot270_horario() {
        let mut f = frame_2x3(&[A, B, C, D, E, F]);
        rotate_frame(&mut f, Transform::Rot270);
        // 270° CW (= 90° CCW) de (a b / c d / e f) = (b d f / a c e).
        assert_eq!((f.width, f.height), (3, 2));
        assert_eq!(px(&f, 0, 0), B);
        assert_eq!(px(&f, 1, 0), D);
        assert_eq!(px(&f, 2, 0), F);
        assert_eq!(px(&f, 0, 1), A);
        assert_eq!(px(&f, 1, 1), C);
        assert_eq!(px(&f, 2, 1), E);
    }

    #[test]
    fn rot90_y_rot270_se_anulan() {
        let orig = frame_2x3(&[A, B, C, D, E, F]);
        let mut f = frame_2x3(&[A, B, C, D, E, F]);
        rotate_frame(&mut f, Transform::Rot90);
        rotate_frame(&mut f, Transform::Rot270);
        assert_eq!(f.data, orig.data);
        assert_eq!((f.width, f.height), (2, 3));
    }

    #[test]
    fn theta_normalizacion() {
        assert_eq!(transform_from_theta(0.0), Transform::None);
        assert_eq!(transform_from_theta(90.0), Transform::Rot90);
        assert_eq!(transform_from_theta(180.0), Transform::Rot180);
        assert_eq!(transform_from_theta(270.0), Transform::Rot270);
        // Negativos y >360 (rem_euclid).
        assert_eq!(transform_from_theta(-90.0), Transform::Rot270);
        assert_eq!(transform_from_theta(-270.0), Transform::Rot90);
        assert_eq!(transform_from_theta(450.0), Transform::Rot90);
        // Redondeo al cardinal más cercano (sensor del móvil: 89.98°).
        assert_eq!(transform_from_theta(89.98), Transform::Rot90);
        assert_eq!(transform_from_theta(180.02), Transform::Rot180);
        assert_eq!(transform_from_theta(-90.01), Transform::Rot270);
        assert_eq!(transform_from_theta(359.9), Transform::None);
        // Ángulos raros → cardinal más cercano (como ffplay redondea).
        assert_eq!(transform_from_theta(44.0), Transform::None);
        assert_eq!(transform_from_theta(46.0), Transform::Rot90);
    }

    #[test]
    fn pre_rotate_y_display_dims() {
        assert_eq!(Transform::Rot90.pre_rotate_dims(640, 360), (360, 640));
        assert_eq!(Transform::Rot180.pre_rotate_dims(640, 360), (640, 360));
        assert_eq!(Transform::None.pre_rotate_dims(640, 360), (640, 360));
        assert_eq!(Transform::Rot90.display_size(1920, 1080), (1080, 1920));
        assert_eq!(Transform::Rot270.display_size(1920, 1080), (1080, 1920));
        assert_eq!(Transform::Rot180.display_size(1920, 1080), (1920, 1080));
    }

    #[test]
    fn display_matrix_via_ffi() {
        // av_display_rotation_get(m) = -atan2(m[1], m[0]) en grados
        // (fixed-point 16.16). La matriz típica de iPhone "vertical"
        // tiene m[1]=+1 ⇒ get devuelve -90 (CCW) ⇒ presentación
        // θ = -get = +90 horario ⇒ Rot90. Verificamos la cadena
        // completa signo-a-signo contra el FFmpeg real enlazado.
        let f = |v: f64| (v * 65536.0) as i32;
        let m_iphone: [i32; 9] = [f(0.0), f(1.0), 0, f(-1.0), f(0.0), 0, 0, 0, 1 << 30];
        let ccw = unsafe { ffmpeg::ffi::av_display_rotation_get(m_iphone.as_ptr()) };
        assert!((ccw + 90.0).abs() < 0.01, "ccw={ccw}");
        assert_eq!(transform_from_theta(-ccw), Transform::Rot90);

        let m180: [i32; 9] = [f(-1.0), f(0.0), 0, f(0.0), f(-1.0), 0, 0, 0, 1 << 30];
        let ccw = unsafe { ffmpeg::ffi::av_display_rotation_get(m180.as_ptr()) };
        assert!((ccw.abs() - 180.0).abs() < 0.01, "ccw={ccw}");
        assert_eq!(transform_from_theta(-ccw), Transform::Rot180);
    }
}
