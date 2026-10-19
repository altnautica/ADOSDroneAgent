//! Compiles the vendored capture-contract `.proto` files (`contract/proto`)
//! into Rust types with prost. protox parses them in-process, so the build
//! needs no system `protoc`.

use std::path::PathBuf;

fn main() {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = dir.join("contract/proto");
    println!("cargo:rerun-if-changed={}", root.display());
    let files: Vec<PathBuf> = ["clock", "gnss", "imu", "pose", "trigger"]
        .iter()
        .map(|n| root.join(format!("ados/capture/v1/{n}.proto")))
        .collect();
    for f in &files {
        println!("cargo:rerun-if-changed={}", f.display());
    }
    let fds = protox::compile(&files, [&root]).expect("compile contract protos");
    prost_build::Config::new()
        .compile_fds(fds)
        .expect("generate contract types");
}
