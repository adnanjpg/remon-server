use std::{env, error::Error, path::PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);

    tonic_build::configure()
        .file_descriptor_set_path(out_dir.join("remonproto_descriptor.bin"))
        .compile(&[r".\proto\notification.proto"], &["proto"])?;

    tonic_build::compile_protos(r".\proto\notification.proto")?;

    Ok(())
}
