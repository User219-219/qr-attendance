use qrcode::{render::svg, EcLevel, QrCode};

/// Render `data` as an SVG QR code (error correction level M, with quiet zone).
pub fn svg(data: &str) -> String {
    match QrCode::with_error_correction_level(data.as_bytes(), EcLevel::M) {
        Ok(code) => code.render::<svg::Color>().min_dimensions(320, 320).quiet_zone(true).build(),
        Err(_) => String::new(),
    }
}
