use qrcode::{Color, QrCode};
use slint::{Rgb8Pixel, SharedPixelBuffer};

/// Pixels por módulo do QR. A imagem é exibida com `image-rendering: pixelated`.
const SCALE: usize = 4;
/// Zona de silêncio em módulos (mínimo da norma).
const QUIET: usize = 4;

/// QR do texto como imagem RGB (preto no branco). `None` se não couber.
pub fn pixels(text: &str) -> Option<SharedPixelBuffer<Rgb8Pixel>> {
    let code = QrCode::new(text.as_bytes()).ok()?;
    let w = code.width();
    let size = (w + 2 * QUIET) * SCALE;
    let mut buf = SharedPixelBuffer::<Rgb8Pixel>::new(size as u32, size as u32);
    let px = buf.make_mut_slice();
    px.fill(Rgb8Pixel { r: 255, g: 255, b: 255 });
    for (i, color) in code.to_colors().into_iter().enumerate() {
        if color != Color::Dark {
            continue;
        }
        let (mx, my) = (i % w + QUIET, i / w + QUIET);
        for y in my * SCALE..(my + 1) * SCALE {
            px[y * size + mx * SCALE..y * size + (mx + 1) * SCALE].fill(Rgb8Pixel { r: 0, g: 0, b: 0 });
        }
    }
    Some(buf)
}

#[cfg(test)]
mod tests {
    #[test]
    fn invite_sized_link_fits() {
        let link = format!("lerrichat://join?{}", "a".repeat(400));
        let buf = super::pixels(&link).unwrap();
        assert_eq!(buf.width(), buf.height());
        // Canto superior esquerdo é zona de silêncio (branco); o padrão de
        // posição começa logo depois (preto).
        let px = buf.as_slice();
        assert_eq!(px[0].r, 255);
        let start = super::QUIET * super::SCALE;
        assert_eq!(px[start * buf.width() as usize + start].r, 0);
    }
}
