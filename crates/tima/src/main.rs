use std::env;
use std::fs;
use std::process::ExitCode;

use tima::backend::ArtifactBackend;
use tima::backend::wasm::WasmBackend;
use tima::backend::wasm_runtime::{DEFAULT_MEMORY_LIMIT, WasmArtifactCache, WasmSession};
use tima::cache::TransformResultCache;
use tima::capability::RuntimeCapabilities;
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
        eprintln!("usage: tima <check|run|emit-wasm> <file.tima>");
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
        "run" => {
            let generated = WasmBackend
                .emit(&compiled.transforms)
                .map_err(|diagnostics| {
                    for diagnostic in diagnostics {
                        eprint!("{}", diagnostic.render(&compiled.source));
                    }
                })?;
            let transform_ids = compiled.identities.iter().collect::<Vec<_>>();
            let cached_artifact = WasmArtifactCache
                .store(&generated, &transform_ids, "build/cache")
                .map_err(|error| {
                    eprintln!("error: {error}");
                })?;
            let wasm = WasmSession::instantiate(
                &cached_artifact.artifact,
                &compiled.transforms,
                DEFAULT_MEMORY_LIMIT,
            )
            .map_err(|error| {
                eprintln!("error: could not instantiate Wasm transform artifact: {error}");
            })?;
            let mut result_cache = TransformResultCache::default();
            let capabilities = CliCapabilities;
            let execution = tima::runtime::execute_wasm_cached_with_capabilities(
                &compiled,
                &wasm,
                &mut result_cache,
                &capabilities,
            )
            .map_err(|diagnostics| {
                for diagnostic in diagnostics {
                    eprint!("{}", diagnostic.render(&compiled.source));
                }
            })?;
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
        }
        "emit-wasm" => {
            let artifact = WasmBackend
                .emit(&compiled.transforms)
                .map_err(|diagnostics| {
                    for diagnostic in diagnostics {
                        eprint!("{}", diagnostic.render(&compiled.source));
                    }
                })?;
            fs::write("module.wasm", artifact.bytes).map_err(|error| {
                eprintln!("error: could not write module.wasm: {error}");
            })?;
            println!("wrote module.wasm");
        }
        _ => {
            eprintln!("error: unknown command `{command}`");
            eprintln!("usage: tima <check|run|emit-wasm> <file.tima>");
            return Err(());
        }
    }
    Ok(())
}

struct CliCapabilities;

impl RuntimeCapabilities for CliCapabilities {
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
