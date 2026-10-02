use std::env;
use std::fs;
use std::process::ExitCode;

use tima::backend::ArtifactBackend;
use tima::backend::cranelift::{CraneliftBackend, host_object_file_name};
use tima::cache::TransformResultCache;
use tima::capability::World;
use tima::runtime::{OuterValue, ValueData};
use tima::source::SourceFile;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(()) => ExitCode::FAILURE,
    }
}

fn run() -> Result<(), ()> {
    let mut arguments = env::args().skip(1);
    let command = arguments.next().unwrap_or_else(|| "help".to_owned());
    if command == "help" || command == "--help" || command == "-h" {
        eprintln!("usage: tima <check|run|run-native|emit-object> <file.tima>");
        return Ok(());
    }
    let Some(path) = arguments.next() else {
        eprintln!("error: missing Tima source path");
        return Err(());
    };
    if arguments.next().is_some() {
        eprintln!("error: unexpected additional arguments");
        return Err(());
    }
    let text = fs::read_to_string(&path).map_err(|error| {
        eprintln!("error: could not read {path}: {error}");
    })?;
    let diagnostic_source = SourceFile::new(path.clone(), text.clone());
    let compiled = tima::compile(path, text).map_err(|diagnostics| {
        for diagnostic in diagnostics {
            eprint!("{}", diagnostic.render(&diagnostic_source));
        }
    })?;

    match command.as_str() {
        "check" => {
            println!("ok: {} transform(s)", compiled.transforms.transforms.len());
            for (index, transform) in compiled.transforms.transforms.iter().enumerate() {
                println!(
                    "{} = {}",
                    transform.name,
                    compiled.identities.get(tima::ir::TransformId(index as u32))
                );
            }
        }
        "run" | "run-native" => {
            let mut result_cache = TransformResultCache::default();
            let capabilities = CliCapabilities;
            let (execution, artifact) = if command == "run-native" {
                let execution = tima::runtime::execute_aot_cached_with_capabilities(
                    &compiled,
                    &mut result_cache,
                    &capabilities,
                    std::path::Path::new("build").join("tima-native"),
                )
                .map_err(|diagnostics| {
                    for diagnostic in diagnostics {
                        eprint!("{}", diagnostic.render(&compiled.source));
                    }
                })?;
                (execution.execution, execution.artifact)
            } else {
                let execution = tima::runtime::execute_cached_with_capabilities(
                    &compiled,
                    &mut result_cache,
                    &capabilities,
                )
                .map_err(|diagnostics| {
                    for diagnostic in diagnostics {
                        eprint!("{}", diagnostic.render(&compiled.source));
                    }
                })?;
                (execution, None)
            };
            let unbound_trace = execution.last_value.as_ref().and_then(|last| {
                let ValueData::Lineage(lineage) = &last.data else {
                    return None;
                };
                (!execution.bindings.values().any(|value| value == last)).then_some(lineage)
            });
            for (name, value) in &execution.bindings {
                println!("{name} = {}", display(value));
            }
            if let Some(lineage) = unbound_trace {
                println!("{}", lineage.render());
            }
            if command == "run-native" {
                if let Some(artifact) = artifact {
                    println!(
                        "native artifact = {} ({:?})",
                        artifact.bundle_id, artifact.status
                    );
                } else {
                    println!("native artifact = none (interpreter fallback)");
                }
            }
        }
        "emit-object" => {
            let artifact = CraneliftBackend
                .emit(&compiled.transforms)
                .map_err(|diagnostics| {
                    for diagnostic in diagnostics {
                        eprint!("{}", diagnostic.render(&compiled.source));
                    }
                })?;
            let output = host_object_file_name();
            fs::write(output, artifact.bytes).map_err(|error| {
                eprintln!("error: could not write {output}: {error}");
            })?;
            println!("wrote {output} for {}", artifact.target);
        }
        _ => {
            eprintln!("error: unknown command `{command}`");
            eprintln!("usage: tima <check|run|run-native|emit-object> <file.tima>");
            return Err(());
        }
    }
    Ok(())
}

struct CliCapabilities;

impl World for CliCapabilities {
    fn environment(&self, name: &str) -> Result<Vec<u8>, String> {
        Err(format!(
            "environment value `{name}` was not provided by the CLI"
        ))
    }

    fn read_asset(&self, locator: &str) -> Result<Vec<u8>, String> {
        fs::read(locator).map_err(|error| error.to_string())
    }

    fn write_asset(&self, locator: &str, bytes: &[u8]) -> Result<(), String> {
        fs::write(locator, bytes).map_err(|error| error.to_string())
    }
}

fn display(value: &OuterValue) -> String {
    match &value.data {
        ValueData::Null => "null".to_owned(),
        ValueData::Bool(value) => value.to_string(),
        ValueData::Integer(value) => value.to_string(),
        ValueData::Float(value) => value.to_string(),
        ValueData::Fraction(value) => value.to_string(),
        ValueData::String(value) => format!("{value:?}"),
        ValueData::Bytes(value) => format!("bytes({})", value.len()),
        ValueData::List(values) => format!(
            "[{}]",
            values.iter().map(display).collect::<Vec<_>>().join(", ")
        ),
        ValueData::Record(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(name, value)| format!("{name}: {}", display(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ValueData::Asset(asset) => format!("asset({:?})", asset.locator),
        ValueData::Image(image) => format!(
            "image(format={}, width={}, height={}, stride={}, bytes={})",
            image.format(),
            image.width(),
            image.height(),
            image.stride(),
            image.byte_len()
        ),
        ValueData::Transform(id) => format!("<transform {}>", id.0),
        ValueData::Lineage(lineage) => lineage.render(),
    }
}
