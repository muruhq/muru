//! Generates the tonic and prost code for the worker protocol with the vendored protoc.

use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    let include = protoc_bin_vendored::include_path()?;

    // SAFETY: a build script is single-threaded, so setting environment
    // variables cannot race with another thread reading them.

    unsafe {
        std::env::set_var("PROTOC", &protoc);
        std::env::set_var("PROTOC_INCLUDE", &include);
    }

    // The admin messages serialize as JSON with their own field names, so
    // muructl's --json output, the MCP server and the API share one shape.
    // Their one map is ordered, so the JSON comes out the same each time.

    tonic_prost_build::configure()
        .bytes(".muru.worker.v1")
        .type_attribute(".muru.admin.v1", "#[derive(serde::Serialize)]")
        .btree_map(".muru.admin.v1")
        .compile_protos(
            &[
                "../../proto/muru/worker/v1/worker.proto",
                "../../proto/muru/admin/v1/admin.proto",
                "../../proto/muru/edge/v1/edge.proto",
            ],
            &["../../proto"],
        )?;

    // The admin API is the one protocol given out to clients that are not
    // part of Muru (`muructl proto`): a descriptor set of it, with its
    // imports, is built beside the generated code.

    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    let status = std::process::Command::new(&protoc)
        .arg("--include_imports")
        .arg(format!(
            "--descriptor_set_out={}",
            out_dir.join("public.protoset").display()
        ))
        .arg("-I../../proto")
        .arg(format!("-I{}", include.display()))
        .arg("muru/admin/v1/admin.proto")
        .status()?;

    if !status.success() {
        return Err(format!("protoc could not build the admin descriptor set: {status}").into());
    }

    println!("cargo:rerun-if-changed=../../proto/muru/worker/v1/worker.proto");
    println!("cargo:rerun-if-changed=../../proto/muru/admin/v1/admin.proto");
    println!("cargo:rerun-if-changed=../../proto/muru/edge/v1/edge.proto");
    Ok(())
}
