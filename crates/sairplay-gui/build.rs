use image::GenericImageView;
use std::env;
use std::path::{Path, PathBuf};

const ICONS: [&str; 17] = [
    "homepod_mini_white",
    "homepod_mini_black",
    "homepod_white",
    "homepod_black",
    "homepod_mini_pair_white",
    "homepod_mini_pair_black",
    "homepod_mini_pair_mixed",
    "homepod_pair_white",
    "homepod_pair_black",
    "homepod_pair_mixed",
    "macbook",
    "mac_mini",
    "music_server",
    "airport_express",
    "tv",
    "apple_tv",
    "airplay_speakers",
];

fn main() {
    let source = Path::new("assets/device_icons_sprite.png");
    println!("cargo:rerun-if-changed={}", source.display());

    let sheet = image::open(source).expect("device icon source PNG must decode during build");
    assert_eq!(
        sheet.dimensions(),
        (800, 400),
        "device icon source must be the approved 5x4 800x400 production sheet"
    );

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));

    for (index, name) in ICONS.iter().enumerate() {
        let col = (index % 5) as u32;
        let row = (index / 5) as u32;
        let icon = sheet.crop_imm(col * 160, row * 100, 160, 100);
        let path = out_dir.join(format!("{name}.png"));
        icon.save_with_format(&path, image::ImageFormat::Png)
            .unwrap_or_else(|err| panic!("failed to write {}: {err}", path.display()));
    }
}
