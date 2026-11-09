//! Regenerates osmflow's checked-in protobuf code in `src/generated` from `proto/`.
//!
//! Run it with `just update-generated`; it needs `protoc`. CI runs `just codegen-check`, which fails if the result
//! differs from the checked-in code. This is a program rather than a build script, so building osmflow, even with
//! `--all-features`, never runs `protoc`.

use std::error;
use std::path::Path;

fn main() -> Result<(), Box<dyn error::Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let proto = root.join("proto");
    let config = |files: &[&str], out_dir: &str| {
        buffa_build::Config::new()
            .files(&files.iter().map(|f| proto.join(f)).collect::<Vec<_>>())
            .includes(&[&proto])
            .out_dir(root.join(out_dir))
            .include_file("mod.rs")
            .preserve_unknown_fields(false)
    };
    // The original OSM-binary schema: the file header, and the messages of the format.
    config(
        &["fileformat.proto", "osmformat.proto"],
        "src/generated/osmpbf",
    )
    .compile()?;
    // The projection: the fields osmflow reads, as it reads them (see the comments in the .proto file).
    config(&["osmflow_raw.proto"], "src/generated/raw").compile()?;
    Ok(())
}
