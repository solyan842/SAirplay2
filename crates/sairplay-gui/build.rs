use ico::{IconDir, IconDirEntry, IconImage, ResourceType};
use image::imageops::FilterType;
use std::env;
use std::fs::File;
use std::path::{Path, PathBuf};

fn round_alpha_mask(image: &mut image::RgbaImage, radius_fraction: f32) {
    let width = image.width();
    let height = image.height();
    let radius = width.min(height) as f32 * radius_fraction;
    let max_x = width as f32 - 1.0;
    let max_y = height as f32 - 1.0;

    for y in 0..height {
        for x in 0..width {
            let fx = x as f32;
            let fy = y as f32;
            let dx = if fx < radius {
                radius - fx
            } else if fx > max_x - radius {
                fx - (max_x - radius)
            } else {
                0.0
            };
            let dy = if fy < radius {
                radius - fy
            } else if fy > max_y - radius {
                fy - (max_y - radius)
            } else {
                0.0
            };
            if dx > 0.0 && dy > 0.0 && dx * dx + dy * dy > radius * radius {
                image.get_pixel_mut(x, y).0[3] = 0;
            }
        }
    }
}

fn write_icon(source_png: &Path, path: &Path) {
    let mut master = image::open(source_png)
        .expect("open approved SAirplay2 logo")
        .into_rgba8();

    // The supplied master is a full square canvas. Preserve every logo pixel,
    // but make the area outside the app-icon rounded rectangle transparent.
    round_alpha_mask(&mut master, 0.205);

    let mut icon = IconDir::new(ResourceType::Icon);
    for size in [16u32, 24, 32, 48, 64, 128, 256] {
        let resized = image::imageops::resize(&master, size, size, FilterType::Lanczos3);
        let image = IconImage::from_rgba_data(size, size, resized.into_raw());
        icon.add_entry(IconDirEntry::encode(&image).expect("encode approved SAirplay2 icon"));
    }

    let file = File::create(path).expect("create SAirplay2 icon");
    icon.write(file).expect("write SAirplay2 icon");
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/sairplay2-logo.png");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let source_png = manifest_dir.join("assets").join("sairplay2-logo.png");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let icon_path = out_dir.join("sairplay2.ico");

    write_icon(&source_png, &icon_path);

    let mut resource = winres::WindowsResource::new();
    resource.set_icon(
        icon_path
            .to_str()
            .expect("SAirplay2 icon path must be valid UTF-8"),
    );
    resource.compile().expect("embed approved SAirplay2 Windows icon");
}
