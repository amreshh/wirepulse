//! Generates the Hubble Observer gRPC client from the vendored Cilium protos.
//! Uses protox (pure Rust), so no `protoc` binary is needed to build.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto");
    let fds = protox::compile(["observer/observer.proto"], ["proto"])?;
    tonic_prost_build::configure()
        // The server half is only used by the fake relay in tests.
        .build_server(true)
        .compile_fds(fds)?;
    Ok(())
}
