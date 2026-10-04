use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=SAIRPLAY_RAOP_STATIC_DIR");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace = manifest.parent().and_then(|p| p.parent()).unwrap();
    let link_dir = std::env::var_os("SAIRPLAY_RAOP_STATIC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("target/raop-static-link"));

    for file in [
        "sairplay_raop_bridge.lib",
        "sairplay_pthread.lib",
        "sairplay_crypto.lib",
        "sairplay_ssl.lib",
    ] {
        let path = link_dir.join(file);
        if !path.is_file() {
            panic!(
                "MSA Core RAOP static dependency missing: {}. Run scripts/build-raop-static-x64.ps1 before building on Windows.",
                path.display()
            );
        }
    }

    println!("cargo:rustc-link-search=native={}", link_dir.display());
    // Keep the RAOP protocol implementation at the exact MSA libraop pin, but
    // link it into the Rust process instead of spawning cliraop.exe.
    println!("cargo:rustc-link-lib=static=sairplay_raop_bridge");
    println!("cargo:rustc-link-lib=static=sairplay_pthread");
    println!("cargo:rustc-link-lib=static=sairplay_ssl");
    println!("cargo:rustc-link-lib=static=sairplay_crypto");

    for system in ["ws2_32", "crypt32", "bcrypt", "advapi32", "user32", "secur32"] {
        println!("cargo:rustc-link-lib={system}");
    }

    println!("cargo:rerun-if-changed=../../native/raop-static/raop_bridge.cpp");
    println!("cargo:rerun-if-changed=../../native/raop-static/CMakeLists.txt");
}
