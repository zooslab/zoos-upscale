use std::fs;
use std::path::{Path, PathBuf};

use schemars::{JsonSchema, schema_for};
use zoos_runner_protocol::{
    FakeJobRequest, ImageUpscaleJobRequest, RunnerCapabilities, RunnerEvent,
};

fn main() {
    if let Err(error) = run(std::env::args().skip(1)) {
        eprintln!("xtask failed: {error}");
        std::process::exit(1);
    }
}

fn run(arguments: impl IntoIterator<Item = String>) -> Result<(), Box<dyn std::error::Error>> {
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    let check = match arguments.as_slice() {
        [command] if command == "schema" => false,
        [command, flag] if command == "schema" && flag == "--check" => true,
        _ => return Err("usage: cargo xtask schema [--check]".into()),
    };

    let schema_directory = workspace_root().join("schemas/runner-protocol-v1");
    let schemas = [
        generated_schema::<RunnerCapabilities>("capabilities.schema.json")?,
        generated_schema::<RunnerEvent>("event.schema.json")?,
        generated_schema::<FakeJobRequest>("fake-job.schema.json")?,
        generated_schema::<ImageUpscaleJobRequest>("image-upscale-job.schema.json")?,
    ];

    for (name, contents) in schemas {
        let path = schema_directory.join(name);
        if check {
            let committed = fs::read(&path).map_err(|error| {
                format!(
                    "could not read generated schema {}: {error}",
                    path.display()
                )
            })?;
            if committed != contents {
                return Err(format!(
                    "schema drift detected in {}; run `cargo xtask schema`",
                    path.display()
                )
                .into());
            }
        } else {
            write_if_changed(&path, &contents)?;
        }
    }
    Ok(())
}

fn generated_schema<T: JsonSchema>(
    name: &'static str,
) -> Result<(&'static str, Vec<u8>), serde_json::Error> {
    let mut contents = serde_json::to_vec_pretty(&schema_for!(T))?;
    contents.push(b'\n');
    Ok((name, contents))
}

fn write_if_changed(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if fs::read(path).is_ok_and(|existing| existing == contents) {
        return Ok(());
    }
    fs::write(path, contents)
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask must live under <workspace>/crates/xtask")
        .to_path_buf()
}
