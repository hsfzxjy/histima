mod cli_json;

use std::collections::VecDeque;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use histima::{CATALOG_LIST_LIMIT, ExecutionEngine, ReplayPolicy, RunError, Workspace};
use tima::backend::cache::ArtifactCacheStatus;
use tima::identity::{ArtifactIdentity, ContentIdentity, RecipeIdentity, content_identity};
use tima::lineage::{LineageNode, RecordedValue};
use tima::runtime::{OuterValue, ValueData};
use tima::source::SourceFile;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputMode {
    Human,
    Json,
}

impl OutputMode {
    fn emit(self, value: serde_json::Value, human: impl FnOnce()) -> Result<(), String> {
        match self {
            Self::Human => human(),
            Self::Json => println!(
                "{}",
                serde_json::to_string_pretty(&value)
                    .map_err(|error| format!("could not encode JSON output: {error}"))?
            ),
        }
        Ok(())
    }
}

fn main() -> ExitCode {
    let mut arguments = env::args().skip(1).collect::<Vec<_>>();
    let json_flags = arguments
        .iter()
        .filter(|argument| argument.as_str() == "--json")
        .count();
    let output = if json_flags == 0 {
        OutputMode::Human
    } else {
        OutputMode::Json
    };
    if json_flags > 1 {
        return report_error(output, "--json may be supplied only once".to_owned());
    }
    arguments.retain(|argument| argument != "--json");
    match run(arguments.into_iter(), output) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => report_error(output, message),
    }
}

fn report_error(output: OutputMode, message: String) -> ExitCode {
    match output {
        OutputMode::Human => eprintln!("error: {message}"),
        OutputMode::Json => eprintln!(
            "{}",
            serde_json::to_string(&serde_json::json!({"error": {"message": message}}))
                .expect("JSON error values are serializable")
        ),
    }
    ExitCode::FAILURE
}

fn run(arguments: impl Iterator<Item = String>, output: OutputMode) -> Result<(), String> {
    let mut arguments = arguments.collect::<VecDeque<_>>();
    let command = arguments.pop_front().unwrap_or_else(|| "help".to_owned());
    if matches!(command.as_str(), "help" | "--help" | "-h") {
        print_usage();
        return Ok(());
    }
    match command.as_str() {
        "init" => {
            let workspace_path = workspace_path(&mut arguments, 0)?;
            finished(&mut arguments)?;
            let workspace = Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let info = workspace
                .catalog_info()
                .map_err(|error| error.to_string())?;
            output.emit(cli_json::init(workspace.root(), &info), || {
                println!("workspace = {}", workspace.root().display());
                println!("schema_version = {}", info.schema_version);
                println!("journal_mode = {}", info.journal_mode);
            })?;
        }
        "import" => {
            let recursive = take_flag(&mut arguments, "--recursive")?;
            let explicit_workspace = take_value_option(&mut arguments, "--workspace")?;
            let workspace_path = import_workspace_path(&mut arguments, explicit_workspace)?;
            if arguments.is_empty() {
                return Err("missing source asset path".to_owned());
            }
            let source_paths = arguments.drain(..).map(PathBuf::from).collect::<Vec<_>>();
            let batch_output = recursive || source_paths.len() != 1;
            let source_paths = expand_import_sources(&source_paths, recursive)?;
            let mut workspace =
                Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let imported = source_paths
                .iter()
                .map(|path| workspace.import_file(path))
                .collect::<histima::Result<Vec<_>>>()
                .map_err(|error| error.to_string())?;
            if batch_output {
                output.emit(cli_json::imported_batch(&imported), || {
                    println!("count = {}", imported.len());
                    for (index, asset) in imported.iter().enumerate() {
                        print_imported(asset, Some(index));
                    }
                })?;
            } else {
                let imported = imported
                    .first()
                    .expect("one non-recursive source produces one import");
                output.emit(cli_json::imported(imported), || {
                    print_imported(imported, None);
                })?;
            }
        }
        "stats" => {
            let workspace_path = workspace_path(&mut arguments, 0)?;
            finished(&mut arguments)?;
            let workspace = Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let info = workspace
                .catalog_info()
                .map_err(|error| error.to_string())?;
            let stats = workspace
                .catalog_stats()
                .map_err(|error| error.to_string())?;
            output.emit(cli_json::stats(&info, &stats), || {
                println!("schema_version = {}", info.schema_version);
                println!("contents = {}", stats.contents);
                println!("source_versions = {}", stats.source_versions);
                println!("source_heads = {}", stats.source_heads);
                println!("lineage_invocations = {}", stats.lineage_invocations);
                println!("recipe_results = {}", stats.recipe_results);
                println!("artifact_bundles = {}", stats.artifact_bundles);
                println!("artifacts = {}", stats.artifacts);
            })?;
        }
        "summary" => {
            let limit = list_limit(&mut arguments)?;
            let workspace_path = workspace_path(&mut arguments, 0)?;
            finished(&mut arguments)?;
            let workspace = Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let summary = workspace
                .summary(limit)
                .map_err(|error| error.to_string())?;
            output.emit(cli_json::summary(&summary), || {
                println!("workspace = {}", summary.workspace.display());
                println!("schema_version = {}", summary.catalog.schema_version);
                println!(
                    "foreign_keys_enabled = {}",
                    summary.catalog.foreign_keys_enabled
                );
                println!("journal_mode = {}", summary.catalog.journal_mode);
                println!("contents = {}", summary.stats.contents);
                println!("source_versions = {}", summary.stats.source_versions);
                println!("source_heads = {}", summary.stats.source_heads);
                println!(
                    "lineage_invocations = {}",
                    summary.stats.lineage_invocations
                );
                println!("recipe_results = {}", summary.stats.recipe_results);
                println!("artifact_bundles = {}", summary.stats.artifact_bundles);
                println!("artifacts = {}", summary.stats.artifacts);
                println!("assets.count = {}", summary.assets.items.len());
                println!("assets.truncated = {}", summary.assets.truncated);
                if let Some(cursor) = &summary.assets.next_cursor {
                    println!("assets.next_cursor = {cursor}");
                }
                for (index, asset) in summary.assets.items.iter().enumerate() {
                    println!("asset[{index}].locator = {}", asset.locator);
                    println!("asset[{index}].source_id = {}", asset.source_id);
                    println!("asset[{index}].content_id = {}", asset.content_id);
                    println!("asset[{index}].byte_length = {}", asset.byte_len);
                }
                println!("recipes.count = {}", summary.recipes.items.len());
                println!("recipes.truncated = {}", summary.recipes.truncated);
                if let Some(cursor) = &summary.recipes.next_cursor {
                    println!("recipes.next_cursor = {cursor}");
                }
                for (index, recipe) in summary.recipes.items.iter().enumerate() {
                    println!("recipe[{index}].recipe_id = {}", recipe.recipe_id);
                    println!("recipe[{index}].transform_id = {}", recipe.transform_id);
                    println!("recipe[{index}].transform_name = {}", recipe.transform_name);
                    println!("recipe[{index}].content_id = {}", recipe.content_id);
                    println!("recipe[{index}].byte_length = {}", recipe.byte_len);
                }
                println!("transforms.count = {}", summary.transforms.len());
                println!("transforms.truncated = {}", summary.transforms_truncated);
                for (index, transform) in summary.transforms.iter().enumerate() {
                    println!("transform[{index}].name = {}", transform.name);
                    println!(
                        "transform[{index}].implementation = {}",
                        transform.implementation.as_str()
                    );
                    println!("transform[{index}].origin = {}", transform.origin.as_str());
                    println!(
                        "transform[{index}].semantic_version = {}",
                        transform
                            .semantic_version
                            .map_or_else(|| "none".to_owned(), |version| version.to_string())
                    );
                    println!(
                        "transform[{index}].transform_id = {}",
                        transform.transform_id
                    );
                    println!("transform[{index}].signature = {}", transform.signature());
                }
            })?;
        }
        "verify" => {
            let workspace_path = workspace_path(&mut arguments, 0)?;
            finished(&mut arguments)?;
            let workspace = Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let report = workspace.verify();
            let valid = report.is_valid();
            output.emit(cli_json::verification(&report), || {
                println!("valid = {valid}");
                println!("sqlite_valid = {}", report.sqlite_valid);
                println!("objects_checked = {}", report.objects_checked);
                println!("objects_valid = {}", report.objects_valid);
                println!("issue_count = {}", report.issues.len());
                for (index, issue) in report.issues.iter().enumerate() {
                    println!("issue[{index}].kind = {}", issue.kind.as_str());
                    if let Some(subject) = &issue.subject {
                        println!("issue[{index}].subject = {subject}");
                    }
                    println!("issue[{index}].message = {}", issue.message);
                }
            })?;
            if !valid {
                return Err(format!(
                    "workspace verification found {} issue(s)",
                    report.issues.len()
                ));
            }
        }
        "assets" => {
            let limit = list_limit(&mut arguments)?;
            let after = take_value_option(&mut arguments, "--after")?;
            let prefix = take_value_option(&mut arguments, "--prefix")?;
            let workspace_path = workspace_path(&mut arguments, 0)?;
            finished(&mut arguments)?;
            let workspace = Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let page = workspace
                .assets_filtered_page(limit, after.as_deref(), prefix.as_deref())
                .map_err(|error| error.to_string())?;
            output.emit(cli_json::assets(&page), || {
                println!("count = {}", page.items.len());
                println!("truncated = {}", page.truncated);
                if let Some(cursor) = &page.next_cursor {
                    println!("next_cursor = {cursor}");
                }
                for (index, asset) in page.items.iter().enumerate() {
                    println!("asset[{index}].locator = {}", asset.locator);
                    println!("asset[{index}].source_id = {}", asset.source_id);
                    println!("asset[{index}].content_id = {}", asset.content_id);
                    println!("asset[{index}].byte_length = {}", asset.byte_len);
                }
            })?;
        }
        "recipes" => {
            let limit = list_limit(&mut arguments)?;
            let after = take_value_option(&mut arguments, "--after")?
                .map(|value| {
                    value
                        .parse::<RecipeIdentity>()
                        .map_err(|error| format!("invalid Recipe cursor: {error}"))
                })
                .transpose()?;
            let transform = take_value_option(&mut arguments, "--transform")?;
            let workspace_path = workspace_path(&mut arguments, 0)?;
            finished(&mut arguments)?;
            let workspace = Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let page = workspace
                .recipes_filtered_page(limit, after, transform.as_deref())
                .map_err(|error| error.to_string())?;
            output.emit(cli_json::recipes(&page), || {
                println!("count = {}", page.items.len());
                println!("truncated = {}", page.truncated);
                if let Some(cursor) = &page.next_cursor {
                    println!("next_cursor = {cursor}");
                }
                for (index, recipe) in page.items.iter().enumerate() {
                    println!("recipe[{index}].recipe_id = {}", recipe.recipe_id);
                    println!("recipe[{index}].transform_id = {}", recipe.transform_id);
                    println!("recipe[{index}].transform_name = {}", recipe.transform_name);
                    println!("recipe[{index}].content_id = {}", recipe.content_id);
                    println!("recipe[{index}].byte_length = {}", recipe.byte_len);
                }
            })?;
        }
        "search" => {
            let limit = list_limit(&mut arguments)?;
            let workspace_path = workspace_path(&mut arguments, 1)?;
            let query = required(&mut arguments, "catalog search query")?;
            finished(&mut arguments)?;
            let workspace = Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let result = workspace
                .search_catalog(&query, limit)
                .map_err(|error| error.to_string())?;
            output.emit(cli_json::search(&result), || {
                println!("query = {}", result.query);
                println!("assets.count = {}", result.assets.items.len());
                println!("assets.truncated = {}", result.assets.truncated);
                for (index, asset) in result.assets.items.iter().enumerate() {
                    println!("asset[{index}].locator = {}", asset.locator);
                    println!("asset[{index}].source_id = {}", asset.source_id);
                    println!("asset[{index}].content_id = {}", asset.content_id);
                    println!("asset[{index}].byte_length = {}", asset.byte_len);
                }
                println!("recipes.count = {}", result.recipes.items.len());
                println!("recipes.truncated = {}", result.recipes.truncated);
                for (index, recipe) in result.recipes.items.iter().enumerate() {
                    println!("recipe[{index}].recipe_id = {}", recipe.recipe_id);
                    println!("recipe[{index}].transform_id = {}", recipe.transform_id);
                    println!("recipe[{index}].transform_name = {}", recipe.transform_name);
                    println!("recipe[{index}].content_id = {}", recipe.content_id);
                    println!("recipe[{index}].byte_length = {}", recipe.byte_len);
                }
            })?;
        }
        "plugins" => {
            let workspace_path = workspace_path(&mut arguments, 0)?;
            finished(&mut arguments)?;
            let workspace = Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let plugins = workspace.plugins();
            output.emit(cli_json::plugins(&plugins), || {
                println!("count = {}", plugins.len());
                for (index, plugin) in plugins.iter().enumerate() {
                    println!("plugin[{index}].name = {}", plugin.name);
                    println!(
                        "plugin[{index}].semantic_version = {}",
                        plugin
                            .semantic_version
                            .expect("workspace plugins are versioned")
                    );
                    println!(
                        "plugin[{index}].abi_version = {}",
                        plugin.abi_version.expect("workspace plugins have an ABI")
                    );
                    println!("plugin[{index}].transform_id = {}", plugin.transform_id);
                    println!(
                        "plugin[{index}].artifact_id = {}",
                        plugin
                            .artifact_id
                            .expect("workspace plugins have an artifact")
                    );
                    println!(
                        "plugin[{index}].module_content_id = {}",
                        plugin
                            .module_content_id
                            .expect("workspace plugins have module content")
                    );
                    println!("plugin[{index}].signature = {}", plugin.signature());
                }
            })?;
        }
        "transforms" => {
            let workspace_path = workspace_path(&mut arguments, 0)?;
            finished(&mut arguments)?;
            let workspace = Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let transforms = workspace.available_transforms();
            output.emit(cli_json::transforms(&transforms), || {
                println!("count = {}", transforms.len());
                for (index, transform) in transforms.iter().enumerate() {
                    println!("transform[{index}].name = {}", transform.name);
                    println!(
                        "transform[{index}].implementation = {}",
                        transform.implementation.as_str()
                    );
                    println!("transform[{index}].origin = {}", transform.origin.as_str());
                    println!(
                        "transform[{index}].semantic_version = {}",
                        transform
                            .semantic_version
                            .map_or_else(|| "none".to_owned(), |version| version.to_string())
                    );
                    println!("transform[{index}].signature = {}", transform.signature());
                    println!(
                        "transform[{index}].transform_id = {}",
                        transform.transform_id
                    );
                    if let Some(abi_version) = transform.abi_version {
                        println!("transform[{index}].abi_version = {abi_version}");
                    }
                    if let Some(artifact_id) = transform.artifact_id {
                        println!("transform[{index}].artifact_id = {artifact_id}");
                    }
                    if let Some(module_content_id) = transform.module_content_id {
                        println!("transform[{index}].module_content_id = {module_content_id}");
                    }
                }
            })?;
        }
        "inspect" => {
            let kind = required(
                &mut arguments,
                "inspection kind (content, recipe, or artifact)",
            )?;
            let workspace_path = workspace_path(&mut arguments, 1)?;
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
                    output.emit(cli_json::content(&inspection), || {
                        print_content_inspection(&inspection);
                    })?;
                }
                "recipe" => {
                    let identity = identity_text
                        .parse::<RecipeIdentity>()
                        .map_err(|error| format!("invalid Recipe ID: {error}"))?;
                    let inspection = workspace
                        .inspect_recipe(identity)
                        .map_err(|error| error.to_string())?;
                    let json = cli_json::recipe(&inspection)?;
                    output.emit(json, || {
                        println!("recipe_id = {}", inspection.recipe_id);
                        println!("content_id = {}", inspection.content_id);
                        let LineageNode::Invocation(invocation) = inspection.lineage.node() else {
                            unreachable!("recipe inspection was validated before output")
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
                                unreachable!("recipe inspection was validated before output")
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
                    })?;
                }
                "artifact" => {
                    let identity = identity_text
                        .parse::<ArtifactIdentity>()
                        .map_err(|error| format!("invalid Artifact or bundle ID: {error}"))?;
                    let inspection = workspace
                        .inspect_artifact(identity)
                        .map_err(|error| error.to_string())?;
                    output.emit(cli_json::artifact(&inspection), || {
                        println!("requested_id = {}", inspection.requested_id);
                        println!("matched_artifact = {}", inspection.artifact.is_some());
                        if let Some(artifact) = &inspection.artifact {
                            println!("artifact_id = {}", artifact.artifact_id);
                            println!("transform_id = {}", artifact.transform_id);
                        }
                        println!("bundle_count = {}", inspection.bundles.items.len());
                        println!("bundles_truncated = {}", inspection.bundles.truncated);
                        for (bundle_index, bundle) in inspection.bundles.items.iter().enumerate() {
                            let prefix = format!("bundle[{bundle_index}]");
                            println!("{prefix}.bundle_id = {}", bundle.bundle_id);
                            println!("{prefix}.backend = {}", bundle.backend);
                            println!("{prefix}.backend_version = {}", bundle.backend_version);
                            println!("{prefix}.compiler_version = {}", bundle.compiler_version);
                            println!("{prefix}.target = {}", bundle.target);
                            println!("{prefix}.cpu_features = {}", bundle.cpu_features);
                            println!("{prefix}.optimization = {}", bundle.optimization);
                            println!("{prefix}.abi_version = {}", bundle.abi_version);
                            println!(
                                "{prefix}.artifact_content_id = {}",
                                bundle.artifact_content_id
                            );
                            println!(
                                "{prefix}.artifact_byte_length = {}",
                                bundle.artifact_byte_len
                            );
                            println!(
                                "{prefix}.artifact_relative_path = {}",
                                bundle.artifact_relative_path
                            );
                            println!("{prefix}.identity_valid = {}", bundle.identity_valid);
                            println!("{prefix}.artifact_valid = {}", bundle.artifact_valid);
                            println!("{prefix}.valid = {}", bundle.valid);
                            println!("{prefix}.member_count = {}", bundle.members.len());
                            for member in &bundle.members {
                                println!(
                                    "{prefix}.member[{}].artifact_id = {}",
                                    member.index, member.artifact_id
                                );
                                println!(
                                    "{prefix}.member[{}].transform_id = {}",
                                    member.index, member.transform_id
                                );
                            }
                            for (error_index, error) in bundle.validation_errors.iter().enumerate()
                            {
                                println!("{prefix}.validation_error[{error_index}] = {error}");
                            }
                        }
                    })?;
                }
                _ => {
                    return Err(format!(
                        "unknown inspection kind {kind:?}; expected content, recipe, or artifact"
                    ));
                }
            }
        }
        "materialize" => {
            let workspace_path = workspace_path(&mut arguments, 2)?;
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
            let destination = PathBuf::from(destination);
            output.emit(cli_json::materialized(identity, &destination), || {
                println!("content_id = {identity}");
                println!("materialized = {}", destination.display());
            })?;
        }
        "expression" => {
            let starting_input = take_value_option(&mut arguments, "--input")?;
            let workspace_path = workspace_path(&mut arguments, 1)?;
            let recipe_text = required(&mut arguments, "Recipe ID")?;
            finished(&mut arguments)?;
            let recipe = recipe_text
                .parse::<RecipeIdentity>()
                .map_err(|error| format!("invalid Recipe ID: {error}"))?;
            let workspace = Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let expression = workspace
                .recipe_expression(recipe, starting_input.as_deref())
                .map_err(|error| error.to_string())?;
            output.emit(
                cli_json::recipe_expression(recipe, starting_input.as_deref(), &expression),
                || println!("{expression}"),
            )?;
        }
        "pipeline" => {
            let workspace_path = workspace_path(&mut arguments, 1)?;
            let expression = required(&mut arguments, "Tima pipeline expression")?;
            finished(&mut arguments)?;
            let source_name = "<command-line-pipeline>";
            let diagnostic_source = SourceFile::new(source_name, expression.clone());
            let mut workspace =
                Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let compiled =
                workspace
                    .compile_tima(source_name, expression)
                    .map_err(|diagnostics| {
                        format!(
                            "Tima pipeline expression was rejected:\n{}",
                            render_diagnostics(&diagnostic_source, &diagnostics)
                        )
                    })?;
            let result = workspace
                .evaluate_pipeline(&compiled)
                .map_err(|error| render_run_error(&compiled.source, error))?;
            let stockable =
                matches!(&result.value.data, ValueData::Bytes(_))
                    && result.value.lineage.as_ref().is_some_and(|lineage| {
                        matches!(lineage.node(), LineageNode::Invocation(_))
                    });
            let stocked = stockable
                .then(|| workspace.record_value(&result.value))
                .transpose()
                .map_err(|error| error.to_string())?;
            output.emit(cli_json::pipeline(&result, stocked.as_ref()), || {
                println!("result_cache_hits = {}", result.result_cache.hits);
                println!("result_cache_misses = {}", result.result_cache.misses);
                println!("result_cache_stores = {}", result.result_cache.stores);
                println!(
                    "result_cache_invalidations = {}",
                    result.result_cache.invalidations
                );
                println!("result = {}", display(&result.value));
                println!("stocked = {}", stocked.is_some());
                if let Some(stocked) = &stocked {
                    println!("recipe_id = {}", stocked.recipe_id);
                    println!("content_id = {}", stocked.content_id);
                    println!("byte_length = {}", stocked.byte_len);
                }
                if let Some(lineage) = &result.value.lineage {
                    println!("{}", lineage.render());
                }
            })?;
        }
        "run" => {
            let engine = take_value_option(&mut arguments, "--engine")?
                .map(|value| value.parse::<ExecutionEngine>())
                .transpose()?
                .unwrap_or_default();
            let record_bindings = take_record_options(&mut arguments)?;
            let workspace_path = workspace_path(&mut arguments, 1)?;
            let script_path = required(&mut arguments, "Tima source path")?;
            finished(&mut arguments)?;
            let text = fs::read_to_string(&script_path)
                .map_err(|error| format!("could not read {script_path}: {error}"))?;
            let diagnostic_source = SourceFile::new(script_path.clone(), text.clone());
            let mut workspace =
                Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let compiled = workspace
                .compile_tima(script_path, text)
                .map_err(|diagnostics| {
                    format!(
                        "Tima source was rejected:\n{}",
                        render_diagnostics(&diagnostic_source, &diagnostics)
                    )
                })?;
            let result = workspace
                .execute_with_engine(&compiled, engine)
                .map_err(|error| render_run_error(&compiled.source, error))?;
            let recorded = record_bindings
                .iter()
                .map(|name| {
                    let value =
                        result.execution.bindings.get(name).ok_or_else(|| {
                            format!("cannot record unknown outer binding {name:?}")
                        })?;
                    let recorded = workspace
                        .record_value(value)
                        .map_err(|error| error.to_string())?;
                    Ok((name.clone(), recorded))
                })
                .collect::<Result<Vec<_>, String>>()?;
            let json = cli_json::run(&result, &recorded);
            output.emit(json, || {
                println!("execution_engine = {}", result.engine.name());
                println!(
                    "artifact_cache = {}",
                    result.artifact_cache.map_or("none", |status| match status {
                        ArtifactCacheStatus::Hit => "hit",
                        ArtifactCacheStatus::Miss => "miss",
                    })
                );
                if let Some(artifact) = &result.artifact {
                    println!("artifact_bundle_id = {}", artifact.bundle_id);
                    println!("artifact_content_id = {}", artifact.artifact_content_id);
                }
                println!("result_cache_hits = {}", result.result_cache.hits);
                println!("result_cache_misses = {}", result.result_cache.misses);
                println!("result_cache_stores = {}", result.result_cache.stores);
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
                if recorded.len() == 1 {
                    let (name, recorded) = &recorded[0];
                    println!("recorded_binding = {name}");
                    println!("recipe_id = {}", recorded.recipe_id);
                    println!("content_id = {}", recorded.content_id);
                    println!("byte_length = {}", recorded.byte_len);
                } else if !recorded.is_empty() {
                    println!("recorded_count = {}", recorded.len());
                    for (index, (name, recorded)) in recorded.iter().enumerate() {
                        println!("record[{index}].binding = {name}");
                        println!("record[{index}].recipe_id = {}", recorded.recipe_id);
                        println!("record[{index}].content_id = {}", recorded.content_id);
                        println!("record[{index}].byte_length = {}", recorded.byte_len);
                    }
                }
            })?;
        }
        "replay" => {
            let policy = if take_flag(&mut arguments, "--snapshot")? {
                ReplayPolicy::Snapshot
            } else {
                ReplayPolicy::Strict
            };
            let workspace_path = workspace_path(&mut arguments, 2)?;
            let script_path = required(&mut arguments, "Tima source path")?;
            let recipe_text = required(&mut arguments, "Recipe ID")?;
            finished(&mut arguments)?;
            let recipe = recipe_text
                .parse::<RecipeIdentity>()
                .map_err(|error| format!("invalid Recipe ID: {error}"))?;
            let text = fs::read_to_string(&script_path)
                .map_err(|error| format!("could not read {script_path}: {error}"))?;
            let diagnostic_source = SourceFile::new(script_path.clone(), text.clone());
            let workspace = Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let compiled = workspace
                .compile_tima(script_path, text)
                .map_err(|diagnostics| {
                    format!(
                        "Tima source was rejected:\n{}",
                        render_diagnostics(&diagnostic_source, &diagnostics)
                    )
                })?;
            let replayed = workspace
                .replay_recipe_with_policy(&compiled, recipe, policy)
                .map_err(|error| render_run_error(&compiled.source, error))?;
            let content_id = content_identity(&replayed.value)
                .map_err(|error| format!("replayed value has no content identity: {error}"))?;
            output.emit(cli_json::replay(recipe, content_id, &replayed), || {
                println!("execution_engine = interpreter");
                println!("replay_policy = {}", replayed.policy.name());
                println!("artifact_cache = none");
                println!("result_cache_hits = {}", replayed.result_cache.hits);
                println!("result_cache_misses = {}", replayed.result_cache.misses);
                println!("result_cache_stores = {}", replayed.result_cache.stores);
                println!("recipe_id = {recipe}");
                println!("content_id = {content_id}");
                println!("replayed = {}", display(&replayed.value));
                if let Some(lineage) = &replayed.value.lineage {
                    println!("{}", lineage.render());
                }
            })?;
        }
        "trace" => {
            let workspace_path = workspace_path(&mut arguments, 1)?;
            let recipe_text = required(&mut arguments, "Recipe ID")?;
            finished(&mut arguments)?;
            let recipe = recipe_text
                .parse::<RecipeIdentity>()
                .map_err(|error| format!("invalid Recipe ID: {error}"))?;
            let workspace = Workspace::open(&workspace_path).map_err(|error| error.to_string())?;
            let trace = workspace
                .trace_recipe(recipe)
                .map_err(|error| error.to_string())?;
            output.emit(cli_json::trace(&trace), || {
                println!("recipe_id = {}", trace.recipe_id);
                println!("content_id = {}", trace.content_id);
                println!("{}", trace.rendered);
            })?;
        }
        _ => {
            return Err(format!(
                "unknown command `{command}`; expected init, import, stats, summary, verify, assets, recipes, search, plugins, transforms, inspect, materialize, expression, pipeline, run, replay, or trace"
            ));
        }
    }
    Ok(())
}

fn workspace_path(
    arguments: &mut VecDeque<String>,
    required_arguments: usize,
) -> Result<PathBuf, String> {
    if arguments.len() > required_arguments {
        return Ok(PathBuf::from(
            arguments.pop_front().expect("argument length was checked"),
        ));
    }
    let current = env::current_dir()
        .map_err(|error| format!("could not determine the current directory: {error}"))?;
    Workspace::find_nearest(&current).ok_or_else(|| {
        format!(
            "no Histima workspace found at {} or any ancestor; pass an explicit workspace path",
            current.display()
        )
    })
}

fn import_workspace_path(
    arguments: &mut VecDeque<String>,
    explicit_workspace: Option<String>,
) -> Result<PathBuf, String> {
    if let Some(workspace) = explicit_workspace {
        return Ok(PathBuf::from(workspace));
    }
    if arguments.is_empty() {
        return Err("missing source asset path".to_owned());
    }

    let current = env::current_dir()
        .map_err(|error| format!("could not determine the current directory: {error}"))?;
    let nearest = Workspace::find_nearest(&current);
    if arguments.len() > 1 {
        let candidate = PathBuf::from(arguments.front().expect("arguments are not empty"));
        if is_initialized_workspace(&candidate) || nearest.is_none() {
            arguments.pop_front();
            return Ok(candidate);
        }
    }
    nearest.ok_or_else(|| {
        format!(
            "no Histima workspace found at {} or any ancestor; pass an explicit workspace path with --workspace",
            current.display()
        )
    })
}

fn is_initialized_workspace(path: &std::path::Path) -> bool {
    path.join(".histima.sql3").is_file() || path.join("catalog.sqlite3").is_file()
}

fn expand_import_sources(sources: &[PathBuf], recursive: bool) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    for source in sources {
        let metadata = fs::symlink_metadata(source).map_err(|error| {
            format!(
                "could not inspect source asset {}: {error}",
                source.display()
            )
        })?;
        if metadata.file_type().is_symlink() && recursive {
            return Err(format!(
                "recursive import does not follow symbolic link {}",
                source.display()
            ));
        }
        if metadata.is_file() || metadata.file_type().is_symlink() {
            files.push(source.clone());
        } else if metadata.is_dir() {
            if !recursive {
                return Err(format!(
                    "source asset {} is a directory; pass --recursive to traverse it",
                    source.display()
                ));
            }
            collect_directory_files(source, &mut files)?;
        } else {
            return Err(format!(
                "source asset {} is not a regular file or directory",
                source.display()
            ));
        }
    }
    Ok(files)
}

fn collect_directory_files(
    directory: &std::path::Path,
    files: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let entries = fs::read_dir(directory)
        .map_err(|error| {
            format!(
                "could not read source directory {}: {error}",
                directory.display()
            )
        })?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| {
            format!(
                "could not read source directory {}: {error}",
                directory.display()
            )
        })?;
    let mut entries = entries;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| {
            format!("could not inspect source asset {}: {error}", path.display())
        })?;
        if file_type.is_symlink() {
            return Err(format!(
                "recursive import does not follow symbolic link {}",
                path.display()
            ));
        }
        if file_type.is_dir() {
            collect_directory_files(&path, files)?;
        } else if file_type.is_file() {
            files.push(path);
        } else {
            return Err(format!(
                "source asset {} is not a regular file or directory",
                path.display()
            ));
        }
    }
    Ok(())
}

fn take_record_options(arguments: &mut VecDeque<String>) -> Result<Vec<String>, String> {
    let mut bindings = Vec::new();
    while let Some(position) = arguments.iter().position(|argument| argument == "--record") {
        arguments.remove(position);
        let binding = arguments
            .get(position)
            .filter(|binding| !binding.starts_with("--"))
            .ok_or_else(|| "--record requires a binding name".to_owned())?
            .clone();
        arguments.remove(position);
        if bindings.contains(&binding) {
            return Err(format!("--record names binding {binding:?} more than once"));
        }
        bindings.push(binding);
    }
    Ok(bindings)
}

fn take_flag(arguments: &mut VecDeque<String>, flag: &str) -> Result<bool, String> {
    let count = arguments
        .iter()
        .filter(|argument| argument.as_str() == flag)
        .count();
    if count > 1 {
        return Err(format!("{flag} may be supplied only once"));
    }
    arguments.retain(|argument| argument != flag);
    Ok(count == 1)
}

fn take_value_option(
    arguments: &mut VecDeque<String>,
    option: &str,
) -> Result<Option<String>, String> {
    let positions = arguments
        .iter()
        .enumerate()
        .filter_map(|(index, argument)| (argument == option).then_some(index))
        .collect::<Vec<_>>();
    if positions.len() > 1 {
        return Err(format!("{option} may be supplied only once"));
    }
    let Some(position) = positions.first().copied() else {
        return Ok(None);
    };
    if position + 1 >= arguments.len() {
        return Err(format!("{option} requires a value"));
    }
    arguments.remove(position);
    Ok(arguments.remove(position))
}

fn list_limit(arguments: &mut VecDeque<String>) -> Result<usize, String> {
    let Some(value) = take_value_option(arguments, "--limit")? else {
        return Ok(CATALOG_LIST_LIMIT);
    };
    let limit = value
        .parse::<usize>()
        .map_err(|_| format!("invalid list limit {value:?}; expected an integer"))?;
    if !(1..=CATALOG_LIST_LIMIT).contains(&limit) {
        return Err(format!(
            "list limit must be between 1 and {CATALOG_LIST_LIMIT}"
        ));
    }
    Ok(limit)
}

fn required(arguments: &mut VecDeque<String>, name: &str) -> Result<String, String> {
    arguments
        .pop_front()
        .ok_or_else(|| format!("missing {name}"))
}

fn finished(arguments: &mut VecDeque<String>) -> Result<(), String> {
    if let Some(argument) = arguments.pop_front() {
        return Err(format!("unexpected additional argument {argument:?}"));
    }
    Ok(())
}

fn print_usage() {
    eprintln!("usage:");
    eprintln!("  histima [--json] <command> ...");
    eprintln!("  histima init [workspace]");
    eprintln!(
        "  histima import [workspace] <source-path>... [--recursive] [--workspace <workspace>]"
    );
    eprintln!("  histima stats [workspace]");
    eprintln!("  histima summary [workspace] [--limit <1-100>]");
    eprintln!("  histima verify [workspace]");
    eprintln!(
        "  histima assets [workspace] [--prefix <locator-prefix>] [--limit <1-100>] [--after <locator>]"
    );
    eprintln!(
        "  histima recipes [workspace] [--transform <name>] [--limit <1-100>] [--after <recipe-id>]"
    );
    eprintln!("  histima search [workspace] <query> [--limit <1-100>]");
    eprintln!("  histima plugins [workspace]");
    eprintln!("  histima transforms [workspace]");
    eprintln!("  histima inspect content [workspace] <content-id>");
    eprintln!("  histima inspect recipe [workspace] <recipe-id>");
    eprintln!("  histima inspect artifact [workspace] <artifact-or-bundle-id>");
    eprintln!("  histima materialize [workspace] <content-id> <destination>");
    eprintln!("  histima expression [workspace] <recipe-id> [--input <tima-expression>]");
    eprintln!("  histima pipeline [workspace] <tima-expression>");
    eprintln!(
        "  histima run [workspace] <file.tima> [--engine <interpreter|hybrid-aot>] [--record <binding>]..."
    );
    eprintln!("  histima replay [workspace] <file.tima> <recipe-id> [--snapshot]");
    eprintln!("  histima trace [workspace] <recipe-id>");
    eprintln!(
        "  omitted workspaces resolve to the nearest initialized workspace at or above the current directory"
    );
    eprintln!("  --json may appear anywhere in the command");
}

fn print_imported(imported: &histima::ImportedAsset, index: Option<usize>) {
    let prefix = index.map_or_else(String::new, |index| format!("asset[{index}]."));
    println!("{prefix}locator = {}", imported.locator);
    println!("{prefix}content_id = {}", imported.content_id);
    println!("{prefix}source_id = {}", imported.source_id);
    println!("{prefix}byte_length = {}", imported.byte_len);
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
        RecordedValue::Fraction(value) => value.to_string(),
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
        ValueData::Buffer(buffer) => format!(
            "buffer(shape={:?}, outer_stride={}, bytes={})",
            buffer.shape(),
            buffer.outer_stride(),
            buffer.byte_len()
        ),
        ValueData::Transform(id) => format!("<transform {}>", id.0),
        ValueData::Lineage(lineage) => lineage.render(),
    }
}
