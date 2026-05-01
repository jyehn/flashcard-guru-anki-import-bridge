// Generates a C header for the FFI surface. Run by cargo before compiling
// the staticlib. The header lands at $OUT_DIR/include/rust_import_core.h
// and the build script (scripts/build-rust.sh) copies it into the xcframework.

use std::env;
use std::path::PathBuf;

fn main() {
    let crate_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR must be set");
    let header_path: PathBuf = [&out_dir, "include", "rust_import_core.h"].iter().collect();

    if let Some(parent) = header_path.parent() {
        std::fs::create_dir_all(parent).expect("create include dir");
    }

    let config = cbindgen::Config::from_file(format!("{crate_dir}/cbindgen.toml"))
        .expect("read cbindgen.toml");

    cbindgen::Builder::new()
        .with_crate(&crate_dir)
        .with_config(config)
        .generate()
        .expect("generate C header")
        .write_to_file(&header_path);

    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=cbindgen.toml");
    println!("cargo:include={}", header_path.parent().unwrap().display());
}
