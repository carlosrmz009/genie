// Turns icon.png into the exe's icon (a multi-size .ico, compiled into the exe as a Windows resource)
// and the window/taskbar icon (raw RGBA at the three sizes miniquad takes, included by main.rs).
use image::codecs::ico::{IcoEncoder, IcoFrame};
use image::imageops::{self, FilterType};
use image::{ColorType, RgbaImage};
use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=icon.png");
    let src = image::open("icon.png").expect("icon.png in the project root").to_rgba8();
    // Pad to a square with transparency so the icon is centred, not stretched.
    let side = src.width().max(src.height());
    let mut square = RgbaImage::new(side, side);
    imageops::overlay(&mut square, &src, ((side - src.width()) / 2) as i64, ((side - src.height()) / 2) as i64);
    let sized = |s: u32| imageops::resize(&square, s, s, FilterType::Lanczos3);

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let images: Vec<RgbaImage> = [16, 24, 32, 48, 64, 128, 256].into_iter().map(sized).collect();
    let frames: Vec<IcoFrame> = images.iter()
        .map(|img| IcoFrame::as_png(img.as_raw(), img.width(), img.height(), ColorType::Rgba8).unwrap())
        .collect();
    let ico = out.join("icon.ico");
    IcoEncoder::new(fs::File::create(&ico).unwrap()).encode_images(&frames).unwrap();
    for s in [16, 32, 64] {
        fs::write(out.join(format!("icon{s}.rgba")), sized(s).as_raw()).unwrap();
    }

    if env::var("CARGO_CFG_WINDOWS").is_ok() {
        let rc = out.join("icon.rc");
        fs::write(&rc, format!("1 ICON \"{}\"\n", ico.display().to_string().replace('\\', "/"))).unwrap();
        embed_resource::compile(&rc, embed_resource::NONE).manifest_optional().unwrap();
    }
}
