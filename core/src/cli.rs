use qrcode::{QrCode, render::unicode};

pub fn print_qr_to_terminal(info: &str) {
    let code = QrCode::new(info).unwrap();
    let image = code
        .render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Dark)
        .light_color(unicode::Dense1x2::Light)
        .build();
    println!("{}", image);
}
