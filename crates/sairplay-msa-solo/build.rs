use std::{env, fs, path::{Path, PathBuf}};

fn marker_present(path: &Path, marker: &str) -> Result<(), String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if text.contains(marker) {
        println!("cargo:warning=MiTV patch gate PASS: {} contains {:?}", path.display(), marker);
        Ok(())
    } else {
        Err(format!("{} missing {:?}", path.display(), marker))
    }
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
    // Collect every failure in one pass so one CI run identifies whether the
    // patch loss is isolated to one source file or affects the whole repo patch.
    let checks = [
        (&worker, "MSA RAOP COMPAT active feedback:"),
        (&session, "feedback_active={}"),
        (&session, "HANDSHAKE user_agent="),
        (&bridge, "raopcl_refresh_record(handle->client, stream_start)"),
    ];

    let mut missing = Vec::new();
    for (path, marker) in checks {
        if let Err(error) = marker_present(path, marker) {
            missing.push(error);
        }
    }

    if !missing.is_empty() {
        panic!(
            "MiTV patch gate: {} required marker(s) missing after workflow patch:\n{}",
            missing.len(),
            missing.join("\n")
        );
    }
}
