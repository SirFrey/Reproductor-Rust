//! Renders assets/logo.svg into the app icon files (run after editing the logo):
//!
//!     cargo run --example make_icon
//!
//! Writes assets/icon.ico (16–256 px, embedded into the exe by build.rs) and
//! assets/icon-256.png (window/taskbar icon, logo for the README).

use std::path::Path;

use image::ExtendedColorType;
use image::codecs::ico::{IcoEncoder, IcoFrame};
use resvg::{tiny_skia, usvg};

const SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

fn render(tree: &usvg::Tree, size: u32) -> Vec<u8> {
    let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("pixmap");
    let scale = size as f32 / tree.size().width();
    resvg::render(tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    pixmap.encode_png().expect("png")
}

fn main() {
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
    let svg = std::fs::read(assets.join("logo.svg")).expect("assets/logo.svg");
    let tree = usvg::Tree::from_data(&svg, &usvg::Options::default()).expect("valid SVG");

    let frames: Vec<IcoFrame> = SIZES
        .iter()
        .map(|&s| IcoFrame::with_encoded(render(&tree, s), s, s, ExtendedColorType::Rgba8).expect("ico frame"))
        .collect();
    let ico = std::fs::File::create(assets.join("icon.ico")).expect("create icon.ico");
    IcoEncoder::new(ico).encode_images(&frames).expect("write icon.ico");
    std::fs::write(assets.join("icon-256.png"), render(&tree, 256)).expect("write icon-256.png");
    println!("wrote assets/icon.ico ({SIZES:?} px) and assets/icon-256.png");
}
