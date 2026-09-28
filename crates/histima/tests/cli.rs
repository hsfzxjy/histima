use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;
use serde_json::Value;
use tima::identity::byte_content_identity;

static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[test]
fn cli_defaults_to_the_nearest_ancestor_workspace() {
    let test = TestDirectory::new();
    let outer = test.path().join("outer");
    let inner = outer.join("projects/inner");
    let nested = inner.join("assets/generated");
    let source = test.path().join("source.bin");
    fs::write(&source, b"nearest workspace").unwrap();

    let missing = histima_in(test.path(), ["stats"]);
    assert!(!missing.status.success());
    assert!(stderr(&missing).contains("no Histima workspace found"));

    assert_success(&histima(["init", text(&outer)]));
    assert_success(&histima(["init", text(&inner)]));
    fs::create_dir_all(&nested).unwrap();

    let initialized = histima_in(&nested, ["init"]);
    assert_success(&initialized);
    assert_eq!(
        fs::canonicalize(field(&initialized, "workspace")).unwrap(),
        fs::canonicalize(&inner).unwrap()
    );

    assert_success(&histima_in(&nested, ["import", text(&source)]));
    let nearest_stats = histima_in(&nested, ["stats", "--json"]);
    assert_success(&nearest_stats);
    assert_eq!(json_output(&nearest_stats)["contents"], 1);

    let outer_stats = histima(["stats", text(&outer), "--json"]);
    assert_success(&outer_stats);
    assert_eq!(json_output(&outer_stats)["contents"], 0);

    let pipeline = histima_in(&nested, ["pipeline", "1 + 2", "--json"]);
    assert_success(&pipeline);
    assert_eq!(json_output(&pipeline)["result"]["value"], 3);
}

#[test]
fn cli_imports_inspects_and_materializes_across_processes() {
    let test = TestDirectory::new();
    let workspace = test.path().join("workspace");
    let source = test.path().join("source.bin");
    let destination = test.path().join("materialized.bin");
    fs::write(&source, b"persistent bytes").unwrap();

    let initialized = histima(["init", text(&workspace)]);
    assert_success(&initialized);
    assert!(stdout(&initialized).contains("schema_version = 3"));

    let imported = histima(["import", text(&workspace), text(&source)]);
    assert_success(&imported);
    let content_id = stdout(&imported)
        .lines()
        .find_map(|line| line.strip_prefix("content_id = "))
        .expect("import prints a Content ID")
        .to_owned();
    assert_eq!(content_id.len(), 64);

    let assets = histima(["assets", text(&workspace)]);
    assert_success(&assets);
    assert!(stdout(&assets).contains("count = 1"));
    assert!(stdout(&assets).contains("truncated = false"));
    assert_eq!(field(&assets, "asset[0].locator"), text(&source));
    assert_eq!(field(&assets, "asset[0].content_id"), content_id);

    let inspected = histima(["inspect", "content", text(&workspace), &content_id]);
    assert_success(&inspected);
    assert_eq!(field(&inspected, "kind"), "raw");
    assert_eq!(field(&inspected, "byte_length"), "16");
    assert_eq!(field(&inspected, "source_references"), "1");
    assert_eq!(field(&inspected, "recipe_references"), "0");
    assert_eq!(field(&inspected, "valid"), "true");

    let stats = histima(["stats", text(&workspace)]);
    assert_success(&stats);
    assert!(stdout(&stats).contains("contents = 1"));
    assert!(stdout(&stats).contains("source_versions = 1"));
    assert!(stdout(&stats).contains("source_heads = 1"));

    let materialized = histima([
        "materialize",
        text(&workspace),
        &content_id,
        text(&destination),
    ]);
    assert_success(&materialized);
    assert_eq!(fs::read(&destination).unwrap(), b"persistent bytes");

    let replacement = histima([
        "materialize",
        text(&workspace),
        &content_id,
        text(&destination),
    ]);
    assert!(!replacement.status.success());
    assert!(stderr(&replacement).contains("refusing to replace"));
    assert_eq!(fs::read(&destination).unwrap(), b"persistent bytes");
}

#[test]
fn cli_runs_a_native_tima_pipeline_against_imported_assets() {
    let test = TestDirectory::new();
    let workspace = test.path().join("workspace");
    let source = test.path().join("source.ppm");
    let script = test.path().join("pipeline.tima");
    let changed_script = test.path().join("changed-transform.tima");
    let output = test.path().join("darkened.ppm");
    let restored = test.path().join("restored.ppm");
    fs::write(&source, b"P3\n1 1\n255\n200 100 50\n").unwrap();
    let source_locator = portable(&source);
    let output_locator = portable(&output);
    fs::write(
        &script,
        format!(
            "source = asset({source_locator:?})\n\
             transform darken(img: Image, factor: f32) -> Image {{\n\
                 for p in img.pixels {{\n\
                     p.r *= factor\n\
                     p.g *= factor\n\
                     p.b *= factor\n\
                 }}\n\
                 return img\n\
             }}\n\
             out = source | read | ppm.decode | darken(0.5) | ppm.encode\n\
             saved = out | save({output_locator:?})\n\
             trace(out)\n"
        ),
    )
    .unwrap();

    assert_success(&histima(["init", text(&workspace)]));
    assert_success(&histima(["import", text(&workspace), &source_locator]));

    let first = histima(["run", text(&workspace), text(&script), "--record", "out"]);
    assert_success(&first);
    assert!(stdout(&first).contains("native_cache = miss"));
    assert!(stdout(&first).contains("result_cache_hits = 0"));
    assert!(stdout(&first).contains("invoke darken"));
    assert_eq!(fs::read(&output).unwrap(), b"P3\n1 1\n255\n100 50 25\n");
    let recipe_id = field(&first, "recipe_id");
    let content_id = field(&first, "content_id");
    let native_bundle_id = field(&first, "native_bundle_id");
    let native_artifact_id = field(&first, "native_artifact_ids");
    let native_library_content_id = field(&first, "native_library_content_id");
    assert_eq!(recipe_id.len(), 64);
    assert_eq!(content_id.len(), 64);
    assert_eq!(native_bundle_id.len(), 64);
    assert_eq!(native_artifact_id.len(), 64);
    assert_eq!(native_library_content_id.len(), 64);

    let database = Connection::open(workspace.join(".histima.sql3")).unwrap();
    let metadata = database
        .query_row(
            "SELECT backend, backend_version, compiler_version, target,
                    cpu_features, optimization, abi_version, library_content_id,
                    library_relative_path
             FROM native_artifact_bundles WHERE bundle_id = ?1",
            [&native_bundle_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(metadata.0, "c");
    assert_eq!(metadata.1, "11");
    assert!(metadata.2.contains("clang"));
    assert!(!metadata.3.is_empty());
    assert_eq!(metadata.4, "");
    assert_eq!(metadata.5, "O2-fno-builtin");
    assert_eq!(metadata.6, 6);
    assert_eq!(metadata.7, native_library_content_id);
    assert_eq!(
        metadata.8,
        format!("cache/native/{native_bundle_id}/module.dll")
    );
    let recorded_member = database
        .query_row(
            "SELECT member.artifact_id, artifact.transform_id
             FROM native_artifact_bundle_members AS member
             JOIN native_artifacts AS artifact USING (artifact_id)
             WHERE member.bundle_id = ?1 AND member.artifact_index = 0",
            [&native_bundle_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .unwrap();
    assert_eq!(recorded_member.0, native_artifact_id);
    assert_eq!(recorded_member.1.len(), 64);
    drop(database);

    let inspected_bundle = histima(["inspect", "artifact", text(&workspace), &native_bundle_id]);
    assert_success(&inspected_bundle);
    assert_eq!(field(&inspected_bundle, "requested_id"), native_bundle_id);
    assert_eq!(field(&inspected_bundle, "matched_artifact"), "false");
    assert_eq!(field(&inspected_bundle, "bundle_count"), "1");
    assert_eq!(field(&inspected_bundle, "bundles_truncated"), "false");
    assert_eq!(field(&inspected_bundle, "bundle[0].backend"), "c");
    assert_eq!(field(&inspected_bundle, "bundle[0].backend_version"), "11");
    assert!(field(&inspected_bundle, "bundle[0].compiler_version").contains("clang"));
    assert!(!field(&inspected_bundle, "bundle[0].target").is_empty());
    assert_eq!(
        field(&inspected_bundle, "bundle[0].optimization"),
        "O2-fno-builtin"
    );
    assert_eq!(field(&inspected_bundle, "bundle[0].abi_version"), "6");
    assert_eq!(
        field(&inspected_bundle, "bundle[0].library_content_id"),
        native_library_content_id
    );
    assert_eq!(field(&inspected_bundle, "bundle[0].identity_valid"), "true");
    assert_eq!(field(&inspected_bundle, "bundle[0].library_valid"), "true");
    assert_eq!(field(&inspected_bundle, "bundle[0].valid"), "true");
    assert_eq!(field(&inspected_bundle, "bundle[0].member_count"), "1");
    assert_eq!(
        field(&inspected_bundle, "bundle[0].member[0].artifact_id"),
        native_artifact_id
    );

    let inspected_artifact =
        histima(["inspect", "artifact", text(&workspace), &native_artifact_id]);
    assert_success(&inspected_artifact);
    assert_eq!(field(&inspected_artifact, "matched_artifact"), "true");
    assert_eq!(
        field(&inspected_artifact, "artifact_id"),
        native_artifact_id
    );
    assert_eq!(
        field(&inspected_artifact, "transform_id"),
        recorded_member.1
    );
    assert_eq!(
        field(&inspected_artifact, "bundle[0].bundle_id"),
        native_bundle_id
    );

    let trace = histima(["trace", text(&workspace), &recipe_id]);
    assert_success(&trace);
    assert!(stdout(&trace).contains("source "));
    assert!(stdout(&trace).contains("invoke ppm.decode"));
    assert!(stdout(&trace).contains("invoke darken"));
    assert!(stdout(&trace).contains("invoke ppm.encode"));

    let recipes = histima(["recipes", text(&workspace)]);
    assert_success(&recipes);
    assert_eq!(field(&recipes, "count"), "1");
    assert_eq!(field(&recipes, "truncated"), "false");
    assert_eq!(field(&recipes, "recipe[0].recipe_id"), recipe_id);
    assert_eq!(field(&recipes, "recipe[0].transform_name"), "ppm.encode");
    assert_eq!(field(&recipes, "recipe[0].content_id"), content_id);

    let inspected_recipe = histima(["inspect", "recipe", text(&workspace), &recipe_id]);
    assert_success(&inspected_recipe);
    assert_eq!(field(&inspected_recipe, "transform_name"), "ppm.encode");
    assert_eq!(field(&inspected_recipe, "content_valid"), "true");
    assert_eq!(field(&inspected_recipe, "argument_count"), "1");
    assert!(field(&inspected_recipe, "argument[0].semantic_identity").starts_with("recipe:"));
    assert_eq!(field(&inspected_recipe, "observation_count"), "0");
    assert!(stdout(&inspected_recipe).contains("invoke darken"));

    let inspected_content = histima(["inspect", "content", text(&workspace), &content_id]);
    assert_success(&inspected_content);
    assert_eq!(field(&inspected_content, "kind"), "bytes");
    assert_eq!(field(&inspected_content, "recipe_references"), "1");
    assert_eq!(field(&inspected_content, "valid"), "true");

    let replayed = histima(["replay", text(&workspace), text(&script), &recipe_id]);
    assert_success(&replayed);
    assert!(stdout(&replayed).contains("result_cache_hits = 1"));
    assert_eq!(field(&replayed, "recipe_id"), recipe_id);
    assert_eq!(field(&replayed, "content_id"), content_id);
    assert_eq!(field(&replayed, "native_bundle_id"), native_bundle_id);
    assert_eq!(
        field(&replayed, "native_library_content_id"),
        native_library_content_id
    );
    assert!(stdout(&replayed).contains("invoke darken"));

    let materialized = histima([
        "materialize",
        text(&workspace),
        &content_id,
        text(&restored),
    ]);
    assert_success(&materialized);
    assert_eq!(fs::read(&restored).unwrap(), b"P3\n1 1\n255\n100 50 25\n");

    let stats = histima(["stats", text(&workspace)]);
    assert_success(&stats);
    assert!(stdout(&stats).contains("contents = 2"));
    assert!(stdout(&stats).contains("lineage_invocations = 3"));
    assert!(stdout(&stats).contains("recipe_results = 1"));
    assert!(stdout(&stats).contains("native_artifact_bundles = 1"));
    assert!(stdout(&stats).contains("native_artifacts = 1"));

    let renamed = fs::read_to_string(&script)
        .unwrap()
        .replace("transform darken", "transform shade")
        .replace(" | darken(", " | shade(");
    fs::write(&script, renamed).unwrap();
    fs::remove_file(&output).unwrap();
    let second = histima(["run", text(&workspace), text(&script), "--record", "out"]);
    assert_success(&second);
    assert!(stdout(&second).contains("native_cache = hit"));
    assert!(stdout(&second).contains("result_cache_hits = 1"));
    assert!(stdout(&second).contains("invoke shade"));
    assert_eq!(field(&second, "recipe_id"), recipe_id);
    assert_eq!(field(&second, "content_id"), content_id);
    assert_eq!(fs::read(&output).unwrap(), b"P3\n1 1\n255\n100 50 25\n");

    let renamed_replay = histima(["replay", text(&workspace), text(&script), &recipe_id]);
    assert_success(&renamed_replay);
    assert_eq!(field(&renamed_replay, "content_id"), content_id);
    assert!(stdout(&renamed_replay).contains("invoke darken"));

    let semantically_changed = fs::read_to_string(&script)
        .unwrap()
        .replace("p.r *= factor", "p.r *= 0.25");
    fs::write(&changed_script, semantically_changed).unwrap();
    let changed_transform = histima([
        "replay",
        text(&workspace),
        text(&changed_script),
        &recipe_id,
    ]);
    assert!(!changed_transform.status.success());
    assert!(stderr(&changed_transform).contains("unavailable or has changed"));

    fs::write(&source, b"P3\n1 1\n255\n100 80 60\n").unwrap();
    assert_success(&histima(["import", text(&workspace), &source_locator]));

    let changed_source = histima(["replay", text(&workspace), text(&script), &recipe_id]);
    assert!(!changed_source.status.success());
    assert!(stderr(&changed_source).contains("replay expected source"));

    fs::remove_file(&output).unwrap();
    let changed = histima(["run", text(&workspace), text(&script), "--record", "out"]);
    assert_success(&changed);
    assert!(stdout(&changed).contains("result_cache_hits = 0"));
    assert_ne!(field(&changed, "recipe_id"), recipe_id);
    assert_ne!(field(&changed, "content_id"), content_id);
    assert_eq!(fs::read(&output).unwrap(), b"P3\n1 1\n255\n50 40 30\n");

    let database = Connection::open(workspace.join(".histima.sql3")).unwrap();
    let library_relative_path = database
        .query_row(
            "SELECT library_relative_path FROM native_artifact_bundles WHERE bundle_id = ?1",
            [&native_bundle_id],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    drop(database);
    let library = workspace.join(library_relative_path);
    let corrupt = b"corrupt native module";
    fs::write(&library, corrupt).unwrap();
    fs::write(
        library.with_file_name("module.sha256"),
        byte_content_identity(corrupt).to_string(),
    )
    .unwrap();
    let corrupt_artifact = histima(["run", text(&workspace), text(&script)]);
    assert!(!corrupt_artifact.status.success());
    assert!(
        stderr(&corrupt_artifact)
            .contains("catalog metadata that does not match the on-disk cache")
    );

    let corrupt_inspection = histima(["inspect", "artifact", text(&workspace), &native_bundle_id]);
    assert_success(&corrupt_inspection);
    assert_eq!(
        field(&corrupt_inspection, "bundle[0].identity_valid"),
        "true"
    );
    assert_eq!(
        field(&corrupt_inspection, "bundle[0].library_valid"),
        "false"
    );
    assert_eq!(field(&corrupt_inspection, "bundle[0].valid"), "false");
    assert!(stdout(&corrupt_inspection).contains("library Content ID is"));

    let database = Connection::open(workspace.join(".histima.sql3")).unwrap();
    database
        .execute(
            "UPDATE native_artifact_bundles SET optimization = 'O0' WHERE bundle_id = ?1",
            [&native_bundle_id],
        )
        .unwrap();
    drop(database);
    let invalid_identity = histima(["inspect", "artifact", text(&workspace), &native_bundle_id]);
    assert_success(&invalid_identity);
    assert_eq!(
        field(&invalid_identity, "bundle[0].identity_valid"),
        "false"
    );
    assert!(stdout(&invalid_identity).contains("Artifact ID is"));
}

#[test]
fn cli_runs_records_and_replays_a_png_to_webp_pipeline() {
    let test = TestDirectory::new();
    let workspace = test.path().join("workspace");
    let source = test.path().join("source.png");
    let script = test.path().join("pipeline.tima");
    let output = test.path().join("darkened.webp");
    fs::write(
        &source,
        encode_test_png(2, 1, &[100, 50, 20, 255, 200, 100, 50, 128]),
    )
    .unwrap();
    let source_locator = portable(&source);
    let output_locator = portable(&output);
    fs::write(
        &script,
        format!(
            "source = asset({source_locator:?})\n\
             transform darken(img: Image, factor: f32) -> Image {{\n\
                 for p in img.pixels {{\n\
                     p.r *= factor\n\
                     p.g *= factor\n\
                     p.b *= factor\n\
                 }}\n\
                 return img\n\
             }}\n\
             out = source | read | png.decode | darken(0.5) | webp.encode\n\
             saved = out | save({output_locator:?})\n"
        ),
    )
    .unwrap();

    assert_success(&histima(["init", text(&workspace)]));
    assert_success(&histima(["import", text(&workspace), &source_locator]));
    let run = histima(["run", text(&workspace), text(&script), "--record", "out"]);
    assert_success(&run);
    assert!(stdout(&run).contains("native_cache = miss"));
    let encoded = fs::read(&output).unwrap();
    assert_eq!(&encoded[..4], b"RIFF");
    assert_eq!(&encoded[8..12], b"WEBP");
    let recipe_id = field(&run, "recipe_id");

    let source_text = fs::read_to_string(&script).unwrap();
    fs::write(
        &script,
        source_text.replace("| webp.encode\n", "| webp.encode(quality=85)\n"),
    )
    .unwrap();
    fs::remove_file(&output).unwrap();
    let explicit_default = histima(["run", text(&workspace), text(&script), "--record", "out"]);
    assert_success(&explicit_default);
    assert_eq!(field(&explicit_default, "recipe_id"), recipe_id);
    assert!(stdout(&explicit_default).contains("result_cache_hits = 1"));

    let trace = histima(["trace", text(&workspace), &recipe_id]);
    assert_success(&trace);
    assert!(stdout(&trace).contains("invoke png.decode"));
    assert!(stdout(&trace).contains("invoke darken"));
    assert!(stdout(&trace).contains("invoke webp.encode"));

    let replay = histima(["replay", text(&workspace), text(&script), &recipe_id]);
    assert_success(&replay);
    assert!(stdout(&replay).contains("result_cache_hits = 1"));
    assert_eq!(field(&replay, "content_id"), field(&run, "content_id"));
}

#[test]
fn cli_json_covers_the_workspace_lifecycle() {
    let test = TestDirectory::new();
    let workspace = test.path().join("workspace");
    let source = test.path().join("source.ppm");
    let script = test.path().join("pipeline.tima");
    let materialized = test.path().join("result.ppm");
    fs::write(&source, b"P3\n1 1\n255\n80 40 20\n").unwrap();
    let source_locator = portable(&source);
    fs::write(
        &script,
        format!(
            "source = asset({source_locator:?})\n\
             transform darken(img: Image, factor: f32) -> Image {{\n\
                 for p in img.pixels {{\n\
                     p.r *= factor\n\
                     p.g *= factor\n\
                     p.b *= factor\n\
                 }}\n\
                 return img\n\
             }}\n\
             out = source | read | ppm.decode | darken(0.5) | ppm.encode\n"
        ),
    )
    .unwrap();

    let initialized = histima(["--json", "init", text(&workspace)]);
    assert_success(&initialized);
    let initialized = json_output(&initialized);
    assert_eq!(initialized["schema_version"], 3);
    assert_eq!(initialized["journal_mode"], "wal");

    let imported = histima(["import", text(&workspace), &source_locator, "--json"]);
    assert_success(&imported);
    let imported = json_output(&imported);
    let source_content_id = json_string(&imported, "content_id");
    let source_id = json_string(&imported, "source_id");
    assert_canonical_identity(source_content_id);
    assert_canonical_identity(source_id);

    let assets = histima(["assets", "--json", text(&workspace)]);
    assert_success(&assets);
    let assets = json_output(&assets);
    assert_eq!(assets["count"], 1);
    assert_eq!(assets["truncated"], false);
    assert_eq!(assets["assets"][0]["locator"], source_locator);
    assert_eq!(assets["assets"][0]["source_id"], source_id);

    let run = histima([
        "run",
        text(&workspace),
        text(&script),
        "--record",
        "out",
        "--json",
    ]);
    assert_success(&run);
    let run = json_output(&run);
    assert_eq!(run["native_cache"], "miss");
    assert_eq!(run["result_cache"]["hits"], 0);
    assert_eq!(run["bindings"]["out"]["type"], "bytes");
    assert_eq!(run["recorded"]["binding"], "out");
    let recipe_id = json_string(&run["recorded"], "recipe_id").to_owned();
    let content_id = json_string(&run["recorded"], "content_id").to_owned();
    let bundle_id = json_string(&run["native_artifact"], "bundle_id").to_owned();
    let artifact_id = run["native_artifact"]["artifact_ids"][0]
        .as_str()
        .unwrap()
        .to_owned();
    for identity in [&recipe_id, &content_id, &bundle_id, &artifact_id] {
        assert_canonical_identity(identity);
    }

    let recipes = histima(["--json", "recipes", text(&workspace)]);
    assert_success(&recipes);
    let recipes = json_output(&recipes);
    assert_eq!(recipes["count"], 1);
    assert_eq!(recipes["recipes"][0]["recipe_id"], recipe_id);
    assert_eq!(recipes["recipes"][0]["transform_name"], "ppm.encode");

    let content = histima([
        "inspect",
        "content",
        text(&workspace),
        &content_id,
        "--json",
    ]);
    assert_success(&content);
    let content = json_output(&content);
    assert_eq!(content["kind"], "bytes");
    assert_eq!(content["valid"], true);
    assert_eq!(content["validation_error"], Value::Null);

    let recipe = histima(["--json", "inspect", "recipe", text(&workspace), &recipe_id]);
    assert_success(&recipe);
    let recipe = json_output(&recipe);
    assert_eq!(recipe["transform_name"], "ppm.encode");
    assert_eq!(
        recipe["arguments"][0]["semantic_identity"]["kind"],
        "recipe"
    );
    assert_eq!(recipe["observations"].as_array().unwrap().len(), 0);
    assert!(recipe["trace"].as_str().unwrap().contains("invoke darken"));

    let artifact = histima([
        "inspect",
        "artifact",
        text(&workspace),
        &bundle_id,
        "--json",
    ]);
    assert_success(&artifact);
    let artifact = json_output(&artifact);
    assert_eq!(artifact["matched_artifact"], Value::Null);
    assert_eq!(artifact["bundles"][0]["valid"], true);
    assert_eq!(
        artifact["bundles"][0]["members"][0]["artifact_id"],
        artifact_id
    );

    let trace = histima(["trace", text(&workspace), &recipe_id, "--json"]);
    assert_success(&trace);
    let trace = json_output(&trace);
    assert_eq!(trace["recipe_id"], recipe_id);
    assert!(
        trace["trace"]
            .as_str()
            .unwrap()
            .contains("invoke ppm.encode")
    );

    let replay = histima([
        "--json",
        "replay",
        text(&workspace),
        text(&script),
        &recipe_id,
    ]);
    assert_success(&replay);
    let replay = json_output(&replay);
    assert_eq!(replay["content_id"], content_id);
    assert_eq!(replay["result_cache"]["hits"], 1);
    assert_eq!(replay["replayed"]["type"], "bytes");

    let materialize = histima([
        "materialize",
        text(&workspace),
        &content_id,
        text(&materialized),
        "--json",
    ]);
    assert_success(&materialize);
    assert_eq!(json_output(&materialize)["content_id"], content_id);

    let stats = histima(["stats", text(&workspace), "--json"]);
    assert_success(&stats);
    let stats = json_output(&stats);
    assert_eq!(stats["recipe_results"], 1);
    assert_eq!(stats["native_artifact_bundles"], 1);

    let invalid = histima([
        "--json",
        "inspect",
        "content",
        text(&workspace),
        "not-an-identity",
    ]);
    assert!(!invalid.status.success());
    assert!(stdout(&invalid).is_empty());
    let error: Value = serde_json::from_str(stderr(&invalid).trim()).unwrap();
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid Content ID")
    );
}

#[test]
fn cli_pipeline_evaluates_one_expression_and_stocks_byte_results() {
    let test = TestDirectory::new();
    let workspace = test.path().join("workspace");
    let source = test.path().join("source.ppm");
    fs::write(&source, b"P3\n1 1\n255\n24 48 96\n").unwrap();
    let source_locator = portable(&source);
    let expression = format!("asset({source_locator:?}) | read | ppm.decode | ppm.encode");

    assert_success(&histima(["init", text(&workspace)]));
    assert_success(&histima(["import", text(&workspace), &source_locator]));

    let first = histima(["pipeline", text(&workspace), &expression]);
    assert_success(&first);
    assert_eq!(field(&first, "result_cache_hits"), "0");
    assert_eq!(field(&first, "stocked"), "true");
    let recipe_id = field(&first, "recipe_id");
    let content_id = field(&first, "content_id");
    assert_canonical_identity(&recipe_id);
    assert_canonical_identity(&content_id);
    assert!(stdout(&first).contains("invoke ppm.decode"));
    assert!(stdout(&first).contains("invoke ppm.encode"));

    let stats = histima(["stats", text(&workspace), "--json"]);
    assert_success(&stats);
    let stats = json_output(&stats);
    assert_eq!(stats["recipe_results"], 1);
    assert_eq!(stats["native_artifact_bundles"], 0);
    assert_eq!(stats["native_artifacts"], 0);

    let second = histima(["--json", "pipeline", text(&workspace), &expression]);
    assert_success(&second);
    let second = json_output(&second);
    assert_eq!(second["result"]["type"], "bytes");
    assert_eq!(second["result_cache"]["hits"], 1);
    assert_eq!(second["stocked"]["recipe_id"], recipe_id);
    assert_eq!(second["stocked"]["content_id"], content_id);

    let recipes = histima(["recipes", text(&workspace), "--json"]);
    assert_success(&recipes);
    let recipes = json_output(&recipes);
    assert_eq!(recipes["count"], 1);
    assert_eq!(recipes["recipes"][0]["recipe_id"], recipe_id);

    let scalar = histima(["pipeline", text(&workspace), "1 + 2", "--json"]);
    assert_success(&scalar);
    let scalar = json_output(&scalar);
    assert_eq!(scalar["result"]["type"], "i64");
    assert_eq!(scalar["result"]["value"], 3);
    assert_eq!(scalar["stocked"], Value::Null);

    let binding = histima(["pipeline", text(&workspace), "value = 1"]);
    assert!(!binding.status.success());
    assert!(stderr(&binding).contains("accepts exactly one expression"));

    let malformed = histima(["pipeline", text(&workspace), "asset("]);
    assert!(!malformed.status.success());
    assert!(stderr(&malformed).contains("<command-line-pipeline>:1"));
}

fn histima<const N: usize>(arguments: [&str; N]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_histima"))
        .args(arguments)
        .output()
        .unwrap()
}

fn histima_in<const N: usize>(directory: &Path, arguments: [&str; N]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_histima"))
        .current_dir(directory)
        .args(arguments)
        .output()
        .unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed:\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json_output(output: &Output) -> Value {
    serde_json::from_str(stdout(output).trim())
        .unwrap_or_else(|error| panic!("invalid JSON output: {error}\n{}", stdout(output)))
}

fn json_string<'a>(value: &'a Value, field: &str) -> &'a str {
    value[field]
        .as_str()
        .unwrap_or_else(|| panic!("JSON field {field:?} is not a string: {value}"))
}

fn assert_canonical_identity(identity: &str) {
    assert_eq!(identity.len(), 64);
    assert!(
        identity
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    );
}

fn field(output: &Output, name: &str) -> String {
    stdout(output)
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name} = ")))
        .unwrap_or_else(|| panic!("command did not print {name}:\n{}", stdout(output)))
        .to_owned()
}

fn text(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn portable(path: &Path) -> String {
    text(path).replace('\\', "/")
}

fn encode_test_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(rgba).unwrap();
        writer.finish().unwrap();
    }
    encoded
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("build")
            .join("histima-cli-tests")
            .join(format!("{}-{sequence}", std::process::id()));
        if path.exists() {
            fs::remove_dir_all(&path).unwrap();
        }
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
