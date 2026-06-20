//! Build script: compiles the gRPC service definitions (Phase 2b).
//!
//! tonic 0.14 split prost codegen into `tonic-prost-build`. We generate the
//! server stubs only (the platform serves the API; external systems generate
//! their own clients). Requires `protoc` on PATH at build time.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(false)
        .compile_protos(&["proto/sre.proto"], &["proto"])?;
    println!("cargo:rerun-if-changed=proto/sre.proto");
    Ok(())
}
