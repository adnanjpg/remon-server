use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    // Build timestamp — exposed via env!("BUILD_TIME") at compile time.
    // Touched on every build because the time itself changes; pinning
    // `rerun-if-changed` to nothing forces cargo to re-evaluate.
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=BUILD_TIME={}", secs);
    println!("cargo:rerun-if-changed=build.rs");
}
