use ico::{IconDir, IconDirEntry, IconImage, ResourceType};
use image::imageops::FilterType;
use std::env;
use std::fs::File;
use std::path::{Path, PathBuf};

fn write_icon(source_png: &Path, path: &Path) {
    let master = image::open(source_png)
        .expect("open approved SAirplay2 logo")
        .into_rgba8();

    let mut icon = IconDir::new(ResourceType::Icon);
    for size in [16u32, 24, 32, 48, 64, 128] {
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
