include!("src/app/icon_draw.rs");

/// Multi-size .ico with 32-bit BMP entries.
fn build_ico() -> Vec<u8> {
    let sizes = [16u32, 20, 24, 32, 40, 48, 64, 128, 256];
    let entries: Vec<Vec<u8>> = sizes
        .iter()
        .map(|&s| {
            let rgba = logo_rgba(s);
            let mut b = Vec::new();
            // BITMAPINFOHEADER (height doubled for the AND mask).
            for v in [40u32, s, s * 2] {
                b.extend_from_slice(&v.to_le_bytes());
            }
            b.extend_from_slice(&1u16.to_le_bytes());
            b.extend_from_slice(&32u16.to_le_bytes());
            for _ in 0..6 {
                b.extend_from_slice(&0u32.to_le_bytes());
            }
            for y in (0..s).rev() {
                for x in 0..s {
                    let i = ((y * s + x) * 4) as usize;
                    b.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
                }
            }
            let mask_row = s.div_ceil(32) * 4;
            b.extend(std::iter::repeat_n(0u8, (mask_row * s) as usize));
            b
        })
        .collect();
    let mut out = vec![0, 0, 1, 0];
    out.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (s, e) in sizes.iter().zip(&entries) {
        let d = if *s >= 256 { 0 } else { *s as u8 };
        out.extend_from_slice(&[d, d, 0, 0, 1, 0, 32, 0]);
        out.extend_from_slice(&(e.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += e.len() as u32;
    }
    for e in entries {
        out.extend(e);
    }
    out
}

fn main() {
    println!("cargo:rerun-if-changed=src/app/icon_draw.rs");
    println!("cargo:rerun-if-changed=build.rs");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("hush.ico");
    std::fs::write(&out, build_ico()).unwrap();
    let mut res = winresource::WindowsResource::new();
    res.set_icon(out.to_str().unwrap());
    res.set("FileDescription", "Hush fan control");
    res.set("ProductName", "Hush");
    res.set("CompanyName", "visiuun");
    res.set("LegalCopyright", "© 2026 visiuun · CC BY-NC 4.0");
    res.compile().unwrap();
}
