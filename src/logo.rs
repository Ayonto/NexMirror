//! The NexMirror mark, rasterised in code (anti-aliased signed-distance shapes) so the
//! window icon, taskbar icon and in-app logo need no image files.

fn sd_round_rect(px: f32, py: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let qx = (px - cx).abs() - hw + r;
    let qy = (py - cy).abs() - hh + r;
    let (ox, oy) = (qx.max(0.0), qy.max(0.0));
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

/// Returns straight-alpha colour at normalised coordinates.
fn shade(u: f32, v: f32) -> [f32; 4] {
    if sd_round_rect(u, v, 0.5, 0.5, 0.5, 0.5, 0.23) > 0.0 {
        return [0.0; 4];
    }
    let c0 = [0.20, 0.47, 1.00];
    let c1 = [0.47, 0.30, 1.00];
    let bg = mix(c0, c1, ((u + v) * 0.5).clamp(0.0, 1.0));
    let white = [1.0, 1.0, 1.0];
    let mut c = bg;
    // Back screen (the source), drawn as an outline.
    let back = sd_round_rect(u, v, 0.42, 0.40, 0.235, 0.165, 0.05);
    if back.abs() < 0.028 {
        c = mix(c, white, 0.55);
    }
    // Front screen (the mirror), solid with a tinted display area.
    let front = sd_round_rect(u, v, 0.58, 0.59, 0.235, 0.165, 0.05);
    if front < 0.0 {
        c = white;
        if sd_round_rect(u, v, 0.58, 0.59, 0.18, 0.11, 0.025) < 0.0 {
            c = mix(bg, [0.10, 0.12, 0.30], 0.35);
        }
    }
    [c[0], c[1], c[2], 1.0]
}

/// RGBA8 (straight alpha) square image of the logo.
pub fn rgba(size: u32) -> Vec<u8> {
    let n = size as usize;
    let ss = 4;
    let mut out = vec![0u8; n * n * 4];
    for y in 0..n {
        for x in 0..n {
            let mut acc = [0f32; 4];
            for sy in 0..ss {
                for sx in 0..ss {
                    let u = (x as f32 + (sx as f32 + 0.5) / ss as f32) / n as f32;
                    let v = (y as f32 + (sy as f32 + 0.5) / ss as f32) / n as f32;
                    let c = shade(u, v);
                    for i in 0..3 {
                        acc[i] += c[i] * c[3];
                    }
                    acc[3] += c[3];
                }
            }
            let a = acc[3] / (ss * ss) as f32;
            let o = (y * n + x) * 4;
            if a > 0.0 {
                for i in 0..3 {
                    out[o + i] = ((acc[i] / acc[3]) * 255.0).round().clamp(0.0, 255.0) as u8;
                }
            }
            out[o + 3] = (a * 255.0).round() as u8;
        }
    }
    out
}
