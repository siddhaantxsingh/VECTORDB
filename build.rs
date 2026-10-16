// Compiles proto/vectordb.proto with protox (pure Rust; no protoc needed).
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/vectordb.proto");
    let fds = protox::compile(["proto/vectordb.proto"], ["proto"])?;
    tonic_build::configure().build_client(true).build_server(true).compile_fds(fds)?;
    Ok(())
}
