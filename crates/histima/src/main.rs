use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use histima::{RunError, Workspace};
use tima::backend::native::NativeCacheStatus;
use tima::identity::{ContentIdentity, RecipeIdentity};
use tima::runtime::{OuterValue, ValueData};
use tima::source::SourceFile;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut arguments = env::args().skip(1);
    let command = arguments.next().unwrap_or_else(|| "help".to_owned());
    if matches!(command.as_str(), "help" | "--help" | "-h") {
        print_usage();
        return Ok(());
    }
    match command.as_str() {
        "init" => {
            let workspace_path = required(&mut arguments, "workspace path")?;
            finished(&mut arguments)?;
            let workspace = Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let info = workspace
                .catalog_info()
                .map_err(|error| error.to_string())?;
            println!("workspace = {}", workspace.root().display());
            println!("schema_version = {}", info.schema_version);
            println!("journal_mode = {}", info.journal_mode);
        }
        "import" => {
            let workspace_path = required(&mut arguments, "workspace path")?;
            let source_path = required(&mut arguments, "source asset path")?;
            finished(&mut arguments)?;
            let mut workspace =
                Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let imported = workspace
                .import_file(&source_path)
                .map_err(|error| error.to_string())?;
            println!("locator = {}", imported.locator);
            println!("content_id = {}", imported.content_id);
            println!("source_id = {}", imported.source_id);
            println!("byte_length = {}", imported.byte_len);
        }
        "stats" => {
            let workspace_path = required(&mut arguments, "workspace path")?;
            finished(&mut arguments)?;
            let workspace = Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let info = workspace
                .catalog_info()
                .map_err(|error| error.to_string())?;
            let stats = workspace
                .catalog_stats()
                .map_err(|error| error.to_string())?;
            println!("schema_version = {}", info.schema_version);
            println!("contents = {}", stats.contents);
            println!("source_versions = {}", stats.source_versions);
            println!("source_heads = {}", stats.source_heads);
            println!("lineage_invocations = {}", stats.lineage_invocations);
            println!("recipe_results = {}", stats.recipe_results);
        }
        "materialize" => {
            let workspace_path = required(&mut arguments, "workspace path")?;
            let identity_text = required(&mut arguments, "Content ID")?;
            let destination = required(&mut arguments, "destination path")?;
            finished(&mut arguments)?;
            let identity = identity_text
                .parse::<ContentIdentity>()
                .map_err(|error| format!("invalid Content ID: {error}"))?;
            let workspace = Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            workspace
                .materialize_content(identity, &destination)
                .map_err(|error| error.to_string())?;
            println!("content_id = {identity}");
            println!("materialized = {}", PathBuf::from(destination).display());
        }
        "run" => {
            let workspace_path = required(&mut arguments, "workspace path")?;
            let script_path = required(&mut arguments, "Tima source path")?;
            let record_binding = match arguments.next() {
                None => None,
                Some(option) if option == "--record" => {
                    let binding = required(&mut arguments, "binding name after --record")?;
                    finished(&mut arguments)?;
                    Some(binding)
                }
                Some(argument) => {
                    return Err(format!(
                        "unexpected additional argument {argument:?}; expected --record <binding>"
                    ));
                }
            };
            let text = fs::read_to_string(&script_path)
                .map_err(|error| format!("could not read {script_path}: {error}"))?;
            let diagnostic_source = SourceFile::new(script_path.clone(), text.clone());
            let compiled = tima::compile(script_path, text).map_err(|diagnostics| {
                format!(
                    "Tima source was rejected:\n{}",
                    render_diagnostics(&diagnostic_source, &diagnostics)
                )
            })?;
            let mut workspace =
                Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let result = workspace
                .execute(&compiled)
                .map_err(|error| render_run_error(&compiled.source, error))?;
            let recorded = record_binding
                .as_ref()
                .map(|name| {
                    let value =
                        result.execution.bindings.get(name).ok_or_else(|| {
                            format!("cannot record unknown outer binding {name:?}")
                        })?;
                    workspace
                        .record_value(value)
                        .map_err(|error| error.to_string())
                })
                .transpose()?;
            println!(
                "native_cache = {}",
                match result.native_cache {
                    NativeCacheStatus::Hit => "hit",
                    NativeCacheStatus::Miss => "miss",
                }
            );
            let unbound_trace = result.execution.last_value.as_ref().and_then(|last| {
                let ValueData::Lineage(lineage) = &last.data else {
                    return None;
                };
                (!result
                    .execution
                    .bindings
                    .values()
                    .any(|value| value == last))
                .then_some(lineage)
            });
            for (name, value) in &result.execution.bindings {
                println!("{name} = {}", display(value));
            }
            if let Some(lineage) = unbound_trace {
                println!("{}", lineage.render());
            }
            if let (Some(name), Some(recorded)) = (record_binding, recorded) {
                println!("recorded_binding = {name}");
                println!("recipe_id = {}", recorded.recipe_id);
                println!("content_id = {}", recorded.content_id);
                println!("byte_length = {}", recorded.byte_len);
            }
        }
        "trace" => {
            let workspace_path = required(&mut arguments, "workspace path")?;
            let recipe_text = required(&mut arguments, "Recipe ID")?;
            finished(&mut arguments)?;
            let recipe = recipe_text
                .parse::<RecipeIdentity>()
                .map_err(|error| format!("invalid Recipe ID: {error}"))?;
            let workspace = Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let trace = workspace
                .trace_recipe(recipe)
                .map_err(|error| error.to_string())?;
            println!("recipe_id = {}", trace.recipe_id);
            println!("content_id = {}", trace.content_id);
            println!("{}", trace.rendered);
        }
        _ => {
            return Err(format!(
                "unknown command `{command}`; expected init, import, stats, materialize, run, or trace"
            ));
        }
    }
    Ok(())
}

fn required(arguments: &mut impl Iterator<Item = String>, name: &str) -> Result<String, String> {
    arguments.next().ok_or_else(|| format!("missing {name}"))
}

fn finished(arguments: &mut impl Iterator<Item = String>) -> Result<(), String> {
    if let Some(argument) = arguments.next() {
        return Err(format!("unexpected additional argument {argument:?}"));
    }
    Ok(())
}

fn print_usage() {
    eprintln!("usage:");
    eprintln!("  histima init <workspace>");
    eprintln!("  histima import <workspace> <source-file>");
    eprintln!("  histima stats <workspace>");
    eprintln!("  histima materialize <workspace> <content-id> <destination>");
    eprintln!("  histima run <workspace> <file.tima> [--record <binding>]");
    eprintln!("  histima trace <workspace> <recipe-id>");
}

fn render_run_error(source: &SourceFile, error: RunError) -> String {
    match error {
        RunError::CodeGeneration(diagnostics) => format!(
            "generated-C code generation failed:\n{}",
            render_diagnostics(source, &diagnostics)
        ),
        RunError::Runtime(diagnostics) => format!(
            "Tima execution failed:\n{}",
            render_diagnostics(source, &diagnostics)
        ),
        other => other.to_string(),
    }
}

fn render_diagnostics(source: &SourceFile, diagnostics: &[tima::diagnostic::Diagnostic]) -> String {
    diagnostics
        .iter()
        .map(|diagnostic| diagnostic.render(source))
        .collect()
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
            image.bytes().len()
        ),
        ValueData::Transform(id) => format!("<transform {}>", id.0),
        ValueData::Lineage(lineage) => lineage.render(),
    }
}
