use std::{env, fs, path::{Path, PathBuf}};

fn require_marker(path: &Path, marker: &str) {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("MiTV patch gate: cannot read {}: {e}", path.display()));
    if !text.contains(marker) {
        panic!(
            "MiTV patch gate: required marker missing in {}: {:?}",
            path.display(),
            marker
        );
    }
    println!("cargo:warning=MiTV patch gate PASS: {} contains {:?}", path.display(), marker);
}

fn main() {
    println!("cargo:rerun-if-changed=src/windows_raop_worker.rs");
    println!("cargo:rerun-if-changed=src/windows_raop_session.rs");
    println!("cargo:rerun-if-changed=../../native/raop-static/raop_bridge.c");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest = PathBuf::from(
        env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is required"),
    );
    let worker = manifest.join("src/windows_raop_worker.rs");
    let session = manifest.join("src/windows_raop_session.rs");
    let bridge = manifest.join("../../native/raop-static/raop_bridge.c");

    // These markers exist only after the isolated MiTV workflow patch has run.
    // The gate changes no runtime behavior; it prevents CI from packaging a
    // stale/unpatched binary while reporting a successful build.
    require_marker(&worker, "MSA RAOP COMPAT active feedback:");
    require_marker(&session, "feedback_active={}");
    require_marker(&session, "HANDSHAKE user_agent=");
    require_marker(&bridge, "raopcl_refresh_record(handle->client, stream_start)");
}
