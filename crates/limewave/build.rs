//! Gives the Windows executable its icon, made from the app's 256 px PNG, and the version
//! and copyright details shown in its Properties. The copyright line is also handed to the
//! code as `LIMEWAVE_COPYRIGHT` for the About window.

use std::{env, fs, path::PathBuf};

const ICON_PNG: &str = "../../assets/icon/icon-256.png";

/// The year in the copyright notice, as in the licence files.
const COPYRIGHT_YEAR: u32 = 2026;

fn main() {
    println!("cargo:rerun-if-changed={ICON_PNG}");
    // The version and authors live in the workspace manifest.
    println!("cargo:rerun-if-changed=../../Cargo.toml");
    let copyright = format!(
        "Copyright (c) {COPYRIGHT_YEAR} {}",
        env::var("CARGO_PKG_AUTHORS")
            .unwrap_or_default()
            .replace(':', ", ")
    );
    println!("cargo:rustc-env=LIMEWAVE_COPYRIGHT={copyright}");
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
    fs::write(out.join("limewave.rc"), resource_script(&copyright)).expect("write the .rc");
    // Does nothing off Windows, and builds without a resource compiler
    // still succeed, only without the icon and version details.
    embed_resource::compile(out.join("limewave.rc"), embed_resource::NONE)
        .manifest_optional()
        .expect("compile the resources");
}

/// The icon and a version block carrying `copyright`. Numbers stand in for the `winver.h`
/// names, so the script needs no includes: 0x40004 is a Win32 program and 0x809, 1200 is
/// British English in UTF-16.
fn resource_script(copyright: &str) -> String {
    let var = |name: &str| env::var(name).unwrap_or_default();
    let version = var("CARGO_PKG_VERSION");
    let numbers = ["MAJOR", "MINOR", "PATCH"]
        .map(|part| var(&format!("CARGO_PKG_VERSION_{part}")))
        .join(",");
    format!(
        r#"1 ICON "limewave.ico"

1 VERSIONINFO
FILEVERSION {numbers},0
PRODUCTVERSION {numbers},0
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "080904B0"
    BEGIN
      VALUE "FileDescription", "Limewave"
      VALUE "ProductName", "Limewave"
      VALUE "FileVersion", "{version}"
      VALUE "ProductVersion", "{version}"
      VALUE "LegalCopyright", "{copyright}"
      VALUE "OriginalFilename", "limewave.exe"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x809, 1200
  END
END
"#
    )
}
