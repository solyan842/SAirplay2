use ico::{IconDir, IconDirEntry, IconImage, ResourceType};
use std::env;
use std::fs::File;
use std::path::PathBuf;

fn app_icon_rgba(size: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let n = size as f32;

    for y in 0..size {
        for x in 0..size {
            let fx = (x as f32 + 0.5) / n;
            let fy = (y as f32 + 0.5) / n;

            let margin = 0.055;
            let radius = 0.215;
            let left = margin;
            let right = 1.0 - margin;
            let top = margin;
            let bottom = 1.0 - margin;

            let qx = if fx < left + radius {
                left + radius - fx
            } else if fx > right - radius {
                fx - (right - radius)
            } else {
                0.0
            };
            let qy = if fy < top + radius {
                top + radius - fy
            } else if fy > bottom - radius {
                fy - (bottom - radius)
            } else {
                0.0
            };
            let inside = if qx > 0.0 && qy > 0.0 {
                qx * qx + qy * qy <= radius * radius
            } else {
                fx >= left && fx <= right && fy >= top && fy <= bottom
            };
            if !inside {
                continue;
            }

            let t = ((fx + fy) * 0.5).clamp(0.0, 1.0);
            let blue_r = (47.0 * (1.0 - t) + 16.0 * t).round() as u8;
            let blue_g = (145.0 * (1.0 - t) + 103.0 * t).round() as u8;
            let blue_b = (255.0 * (1.0 - t) + 242.0 * t).round() as u8;

            let nx = fx - 0.5;
            let ny = fy - 0.49;
            let dist = (nx * nx + ny * ny).sqrt();
            let outer_arc = ny <= 0.15 && (dist - 0.285).abs() <= 0.018;
            let inner_arc = ny <= 0.11 && (dist - 0.185).abs() <= 0.014;
            let stem = nx.abs() <= 0.014 && fy >= 0.27 && fy <= 0.665;
            let dot_dx = fx - 0.5;
            let dot_dy = fy - 0.685;
            let dot = dot_dx * dot_dx + dot_dy * dot_dy <= 0.046 * 0.046;

            let (r, g, b) = if outer_arc || inner_arc || stem || dot {
                (255, 255, 255)
            } else {
                (blue_r, blue_g, blue_b)
            };

            let i = ((y * size + x) * 4) as usize;
            rgba[i] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = 255;
        }
    }

    rgba
}

fn write_icon(path: &PathBuf) {
    let mut icon = IconDir::new(ResourceType::Icon);
    for size in [16u32, 24, 32, 48, 64, 128, 256] {
        let image = IconImage::from_rgba_data(size, size, app_icon_rgba(size));
        icon.add_entry(IconDirEntry::encode(&image).expect("encode SAirplay2 icon"));
    }
    let file = File::create(path).expect("create SAirplay2 icon");
    icon.write(file).expect("write SAirplay2 icon");
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let icon_path = out_dir.join("sairplay2.ico");
    write_icon(&icon_path);

    let mut resource = winres::WindowsResource::new();
    resource.set_icon(
        icon_path
            .to_str()
            .expect("SAirplay2 icon path must be valid UTF-8"),
    );
    resource.compile().expect("embed SAirplay2 Windows icon");
}
