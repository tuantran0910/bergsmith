//! Compiles the protobuf contracts with `protox`, so building needs no
//! system `protoc`, and generates prost messages and tonic services.

use std::error::Error;
use std::path::PathBuf;
use std::{env, fs};

use prost::Message;

const PROTOS: [&str; 4] = [
    "proto/bergsmith/common/v1/common.proto",
    "proto/bergsmith/artifact/v1/artifact.proto",
    "proto/bergsmith/cluster/v1/cluster.proto",
    "proto/bergsmith/worker/local/v1/local.proto",
];

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=proto");
    let descriptors = protox::compile(PROTOS, ["proto"])?;
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    // Kept for tests that check the hashing rules against the schema.
    fs::write(
        out_dir.join("bergsmith_descriptors.bin"),
        descriptors.encode_to_vec(),
    )?;
    tonic_prost_build::configure()
        .build_transport(false)
        // Ordered maps keep encoding deterministic.
        .btree_map(".")
        // Credentials get a redacted `Debug` in `src/redact.rs`.
        .skip_debug(["bergsmith.cluster.v1.StorageCredentials"])
        .compile_fds(descriptors)?;
    Ok(())
}
