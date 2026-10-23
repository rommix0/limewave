//! Gives the Windows executable its icon, made from the app's 256 px PNG.

use std::{env, fs, path::PathBuf};

const ICON_PNG: &str = "../../assets/icon/icon-256.png";

fn main() {
    println!("cargo:rerun-if-changed={ICON_PNG}");
    let png = fs::read(ICON_PNG).expect("read the app icon");
    // An .ico holding one PNG-compressed image: a 6-byte header, one
    // 16-byte directory entry (0 means 256 px), then the PNG itself.
    let mut ico = Vec::with_capacity(22 + png.len());
    for word in [0u16, 1, 1] {
        ico.extend(word.to_le_bytes());
    }
    ico.extend([0, 0, 0, 0]);
    ico.extend(1u16.to_le_bytes());
    ico.extend(32u16.to_le_bytes());
    ico.extend((png.len() as u32).to_le_bytes());
    ico.extend(22u32.to_le_bytes());
    ico.extend(png);

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    fs::write(out.join("limewave.ico"), ico).expect("write the .ico");
    fs::write(out.join("limewave.rc"), "1 ICON \"limewave.ico\"\n").expect("write the .rc");
    // Does nothing off Windows, and builds without a resource compiler
    // still succeed, only without the icon.
    embed_resource::compile(out.join("limewave.rc"), embed_resource::NONE)
        .manifest_optional()
        .expect("compile the icon resource");
}
