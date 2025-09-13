fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/hub.proto");
    tonic_build::configure()
        .out_dir("src/pb")
        .compile_protos(&["proto/hub.proto"], &["proto"])?;
    Ok(())
}
