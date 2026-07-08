//! Build script for integration-test-only `ext_proc` protocol stubs.

use std::path::{Path, PathBuf};

/// Generate the Envoy `ext_proc` service definitions used by the mock processor.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let proto_dir = manifest_dir.join("../../filter/ext-proc/proto");
    let proto_files = ["envoy_common.proto", "ext_proc.proto"]
        .iter()
        .map(|name| proto_dir.join(name))
        .collect::<Vec<_>>();
    let include_dirs = [proto_dir.clone()];

    let mut config = prost_build::Config::new();
    config.extern_path(".google.protobuf.Value", "::prost_wkt_types::Value");
    config.extern_path(".google.protobuf.Struct", "::prost_wkt_types::Struct");

    let fds = protox::compile(&proto_files, &include_dirs)?;
    tonic_prost_build::configure()
        .build_client(false)
        .build_server(true)
        .compile_fds_with_config(fds, config)?;

    println!("cargo:rerun-if-changed=build.rs");
    for entry in proto_files_under(&proto_dir)? {
        if let Some(path) = entry.to_str() {
            println!("cargo:rerun-if-changed={path}");
        }
    }

    Ok(())
}

/// Recursively collect all `.proto` files under `dir`.
fn proto_files_under(dir: &Path) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut protos = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            protos.extend(proto_files_under(&path)?);
        } else if path.extension().is_some_and(|ext| ext == "proto") {
            protos.push(path);
        }
    }
    Ok(protos)
}
