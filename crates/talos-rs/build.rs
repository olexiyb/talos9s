//! Build script for talos-rs protobuf code generation
//!
//! Normal builds use the client code committed in `src/generated`, so they
//! need neither `protoc` nor a writable source tree (crates.io, `cargo
//! install`). After changing `proto/`, regenerate it with
//! `cargo build -p talos-rs --features regenerate` (needs `protoc`) and
//! commit the result; CI checks the two match.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(feature = "regenerate")]
    regenerate()?;
    Ok(())
}

#[cfg(feature = "regenerate")]
fn regenerate() -> Result<(), Box<dyn std::error::Error>> {
    // Create generated directory if it doesn't exist
    std::fs::create_dir_all("src/generated")?;

    // Configure tonic-prost-build for Talos Machine API
    tonic_prost_build::configure()
        .build_server(false) // We only need the client
        .build_client(true)
        .out_dir("src/generated")
        .compile_protos(
            &[
                "proto/google/rpc/status.proto",
                "proto/common/common.proto",
                "proto/machine/machine.proto",
                "proto/storage/storage.proto",
                "proto/time/time.proto",
                "proto/inspect/inspect.proto",
            ],
            &["proto"],
        )?;

    // Tell Cargo to re-run if proto files change
    println!("cargo:rerun-if-changed=proto/");

    Ok(())
}
