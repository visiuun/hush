// Procedural Hush logo: swept fan blades on an OLED-black squircle.
// Dependency-free so build.rs can `include!` it to produce the .ico.

#[allow(dead_code)]
pub fn logo_rgba(size: u32) -> Vec<u8> {
    let s = size as f32;
    let mut out = vec![0u8; (size * size * 4) as usize];
    const SS: u32 = 4;
    let blades = 5;
    for y in 0..size {
        for x in 0..size {
            let (mut bg, mut fg, mut cov_bg) = (0.0f32, 0.0f32, 0.0f32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = (x as f32 + (sx as f32 + 0.5) / SS as f32) / s * 2.0 - 1.0;
                    let py = (y as f32 + (sy as f32 + 0.5) / SS as f32) / s * 2.0 - 1.0;
                    // Squircle background.
                    let sq = px.abs().powi(5) + py.abs().powi(5);
                    if sq <= 0.96 {
                        cov_bg += 1.0;
                        if sq > 0.80 {
                            bg += 1.0; // rim
                        }
                    }
                    let r = (px * px + py * py).sqrt();
                    let a = py.atan2(px);
                    let hub = r < 0.17 && r > 0.10;
                    let mut blade = false;
                    if (0.2..0.74).contains(&r) {
                        let t = (r - 0.2) / 0.54;
                        let sweep = a - t * 1.25;
                        let k = sweep.rem_euclid(std::f32::consts::TAU / blades as f32) / (std::f32::consts::TAU / blades as f32);
                        let width = 0.5 - 0.22 * t;
                        blade = k < width;
                    }
                    if hub || blade {
                        fg += 1.0;
                    }
                }
            }
            let n = (SS * SS) as f32;
            let (cb, rim, f) = (cov_bg / n, bg / n, fg / n);
            // Teal blades, faint rim, black body.
            let teal = [94.0, 234.0, 212.0];
            let rim_c = [34.0, 34.0, 40.0];
            let mut c = [0.0f32; 3];
            for i in 0..3 {
                c[i] = rim_c[i] * rim + teal[i] * f;
            }
            let alpha = cb.max(f);
            let idx = ((y * size + x) * 4) as usize;
            let un = |v: f32| if alpha > 0.0 { (v / alpha).min(255.0) as u8 } else { 0 };
            out[idx] = un(c[0]);
            out[idx + 1] = un(c[1]);
            out[idx + 2] = un(c[2]);
            out[idx + 3] = (alpha * 255.0).round() as u8;
        }
    }
    out
}
