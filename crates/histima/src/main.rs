use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use histima::{RunError, Workspace};
use tima::backend::native::NativeCacheStatus;
use tima::identity::{ContentIdentity, RecipeIdentity, content_identity};
use tima::lineage::{LineageNode, RecordedValue};
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
            println!(
                "native_artifact_bundles = {}",
                stats.native_artifact_bundles
            );
            println!("native_artifacts = {}", stats.native_artifacts);
        }
        "assets" => {
            let workspace_path = required(&mut arguments, "workspace path")?;
            finished(&mut arguments)?;
            let workspace = Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let page = workspace.assets().map_err(|error| error.to_string())?;
            println!("count = {}", page.items.len());
            println!("truncated = {}", page.truncated);
            for (index, asset) in page.items.iter().enumerate() {
                println!("asset[{index}].locator = {}", asset.locator);
                println!("asset[{index}].source_id = {}", asset.source_id);
                println!("asset[{index}].content_id = {}", asset.content_id);
                println!("asset[{index}].byte_length = {}", asset.byte_len);
            }
        }
        "recipes" => {
            let workspace_path = required(&mut arguments, "workspace path")?;
            finished(&mut arguments)?;
            let workspace = Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let page = workspace.recipes().map_err(|error| error.to_string())?;
            println!("count = {}", page.items.len());
            println!("truncated = {}", page.truncated);
            for (index, recipe) in page.items.iter().enumerate() {
                println!("recipe[{index}].recipe_id = {}", recipe.recipe_id);
                println!("recipe[{index}].transform_id = {}", recipe.transform_id);
                println!("recipe[{index}].transform_name = {}", recipe.transform_name);
                println!("recipe[{index}].content_id = {}", recipe.content_id);
                println!("recipe[{index}].byte_length = {}", recipe.byte_len);
            }
        }
        "inspect" => {
            let kind = required(&mut arguments, "inspection kind (content or recipe)")?;
            let workspace_path = required(&mut arguments, "workspace path")?;
            let identity_text = required(&mut arguments, "identity")?;
            finished(&mut arguments)?;
            let workspace = Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            match kind.as_str() {
                "content" => {
                    let identity = identity_text
                        .parse::<ContentIdentity>()
                        .map_err(|error| format!("invalid Content ID: {error}"))?;
                    let inspection = workspace
                        .inspect_content(identity)
                        .map_err(|error| error.to_string())?;
                    print_content_inspection(&inspection);
                }
                "recipe" => {
                    let identity = identity_text
                        .parse::<RecipeIdentity>()
                        .map_err(|error| format!("invalid Recipe ID: {error}"))?;
                    let inspection = workspace
                        .inspect_recipe(identity)
                        .map_err(|error| error.to_string())?;
                    println!("recipe_id = {}", inspection.recipe_id);
                    println!("content_id = {}", inspection.content_id);
                    let LineageNode::Invocation(invocation) = inspection.lineage.node() else {
                        return Err(format!(
                            "recorded recipe {} does not have invocation lineage",
                            inspection.recipe_id
                        ));
                    };
                    println!("transform_name = {}", invocation.transform_name);
                    println!("transform_id = {}", invocation.transform_id);
                    println!("content_valid = {}", inspection.content.valid);
                    if let Some(error) = &inspection.content.validation_error {
                        println!("content_validation_error = {error}");
                    }
                    println!("argument_count = {}", invocation.arguments.len());
                    for (index, argument) in invocation.arguments.iter().enumerate() {
                        println!("argument[{index}].name = {}", argument.name);
                        println!(
                            "argument[{index}].semantic_identity = {}",
                            argument.semantic_identity
                        );
                        println!(
                            "argument[{index}].recorded_value = {}",
                            display_recorded_value(&argument.value)
                        );
                        if let Some(parent) = &argument.lineage {
                            println!(
                                "argument[{index}].parent = {}",
                                lineage_identity(parent.node())
                            );
                        }
                    }
                    println!("observation_count = {}", invocation.observations.len());
                    for (index, observation) in invocation.observations.iter().enumerate() {
                        let LineageNode::ExternalObservation(observation) = observation.node()
                        else {
                            return Err(format!(
                                "recipe {} has a non-external observation",
                                inspection.recipe_id
                            ));
                        };
                        println!(
                            "observation[{index}].dependency_id = {}",
                            observation.dependency_id
                        );
                        println!(
                            "observation[{index}].capability = {}",
                            observation.capability
                        );
                        println!(
                            "observation[{index}].key = {}",
                            display_observation_key(&observation.key)
                        );
                        println!(
                            "observation[{index}].content_id = {}",
                            observation.observed_content
                        );
                    }
                    println!("trace:");
                    println!("{}", inspection.rendered);
                }
                _ => {
                    return Err(format!(
                        "unknown inspection kind {kind:?}; expected content or recipe"
                    ));
                }
            }
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
            println!("result_cache_hits = {}", result.result_cache.hits);
            println!("result_cache_misses = {}", result.result_cache.misses);
            println!("result_cache_stores = {}", result.result_cache.stores);
            println!("native_bundle_id = {}", result.native_artifact.bundle_id);
            println!(
                "native_artifact_ids = {}",
                result
                    .native_artifact
                    .artifact_ids
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            );
            println!(
                "native_library_content_id = {}",
                result.native_artifact.library_content_id
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
        "replay" => {
            let workspace_path = required(&mut arguments, "workspace path")?;
            let script_path = required(&mut arguments, "Tima source path")?;
            let recipe_text = required(&mut arguments, "Recipe ID")?;
            finished(&mut arguments)?;
            let recipe = recipe_text
                .parse::<RecipeIdentity>()
                .map_err(|error| format!("invalid Recipe ID: {error}"))?;
            let text = fs::read_to_string(&script_path)
                .map_err(|error| format!("could not read {script_path}: {error}"))?;
            let diagnostic_source = SourceFile::new(script_path.clone(), text.clone());
            let compiled = tima::compile(script_path, text).map_err(|diagnostics| {
                format!(
                    "Tima source was rejected:\n{}",
                    render_diagnostics(&diagnostic_source, &diagnostics)
                )
            })?;
            let workspace = Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let replayed = workspace
                .replay_recipe(&compiled, recipe)
                .map_err(|error| render_run_error(&compiled.source, error))?;
            let content_id = content_identity(&replayed.value)
                .map_err(|error| format!("replayed value has no content identity: {error}"))?;
            println!(
                "native_cache = {}",
                match replayed.native_cache {
                    NativeCacheStatus::Hit => "hit",
                    NativeCacheStatus::Miss => "miss",
                }
            );
            println!("result_cache_hits = {}", replayed.result_cache.hits);
            println!("result_cache_misses = {}", replayed.result_cache.misses);
            println!("result_cache_stores = {}", replayed.result_cache.stores);
            println!("native_bundle_id = {}", replayed.native_artifact.bundle_id);
            println!(
                "native_artifact_ids = {}",
                replayed
                    .native_artifact
                    .artifact_ids
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            );
            println!(
                "native_library_content_id = {}",
                replayed.native_artifact.library_content_id
            );
            println!("recipe_id = {recipe}");
            println!("content_id = {content_id}");
            println!("replayed = {}", display(&replayed.value));
            if let Some(lineage) = &replayed.value.lineage {
                println!("{}", lineage.render());
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
                "unknown command `{command}`; expected init, import, stats, assets, recipes, inspect, materialize, run, replay, or trace"
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
    eprintln!("  histima assets <workspace>");
    eprintln!("  histima recipes <workspace>");
    eprintln!("  histima inspect content <workspace> <content-id>");
    eprintln!("  histima inspect recipe <workspace> <recipe-id>");
    eprintln!("  histima materialize <workspace> <content-id> <destination>");
    eprintln!("  histima run <workspace> <file.tima> [--record <binding>]");
    eprintln!("  histima replay <workspace> <file.tima> <recipe-id>");
    eprintln!("  histima trace <workspace> <recipe-id>");
}

fn print_content_inspection(inspection: &histima::ContentInspection) {
    println!("content_id = {}", inspection.content_id);
    println!("kind = {}", inspection.kind);
    println!("byte_length = {}", inspection.byte_len);
    println!("relative_path = {}", inspection.relative_path);
    println!("source_references = {}", inspection.source_references);
    println!("recipe_references = {}", inspection.recipe_references);
    println!("valid = {}", inspection.valid);
    if let Some(error) = &inspection.validation_error {
        println!("validation_error = {error}");
    }
}

fn display_recorded_value(value: &RecordedValue) -> String {
    match value {
        RecordedValue::Null => "null".to_owned(),
        RecordedValue::Bool(value) => value.to_string(),
        RecordedValue::Integer(value) => value.to_string(),
        RecordedValue::Float(value) => value.to_string(),
        RecordedValue::String(value) => format!("{value:?}"),
        RecordedValue::Materialized { kind, content_id } => {
            format!("{kind}:{content_id}")
        }
        RecordedValue::Source { locator, source_id } => {
            format!("source:{source_id} locator={locator:?}")
        }
    }
}

fn lineage_identity(node: &LineageNode) -> String {
    match node {
        LineageNode::Source(source) => source.source_id.map_or_else(
            || format!("unobserved-source:{:?}", source.locator),
            |identity| format!("source:{identity}"),
        ),
        LineageNode::Invocation(invocation) => format!("recipe:{}", invocation.recipe_id),
        LineageNode::ExternalObservation(observation) => {
            format!("dependency:{}", observation.dependency_id)
        }
    }
}

fn display_observation_key(key: &[u8]) -> String {
    match std::str::from_utf8(key) {
        Ok(text) => format!("utf8:{text:?}"),
        Err(_) => format!(
            "hex:{}",
            key.iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ),
    }
}

fn render_run_error(source: &SourceFile, error: RunError) -> String {
    match error {
        RunError::Storage(error) => error.to_string(),
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
