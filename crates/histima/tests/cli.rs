use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

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
    assert!(stdout(&initialized).contains("schema_version = 6"));

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
fn cli_batch_imports_explicit_files_and_directories_in_stable_order() {
    let test = TestDirectory::new();
    let workspace = test.path().join("workspace");
    let first = test.path().join("first.bin");
    let directory = test.path().join("assets");
    let nested = directory.join("nested");
    fs::create_dir_all(&nested).unwrap();
    fs::write(&first, b"first").unwrap();
    fs::write(directory.join("z.bin"), b"z").unwrap();
    fs::write(directory.join("a.bin"), b"a").unwrap();
    fs::write(nested.join("m.bin"), b"m").unwrap();

    assert_success(&histima(["init", text(&workspace)]));

    let directory_rejected = histima(["import", text(&workspace), text(&directory)]);
    assert!(!directory_rejected.status.success());
    assert!(stderr(&directory_rejected).contains("pass --recursive"));
    let empty = histima(["stats", text(&workspace), "--json"]);
    assert_success(&empty);
    assert_eq!(json_output(&empty)["source_heads"], 0);

    let imported = histima([
        "import",
        "--workspace",
        text(&workspace),
        text(&first),
        text(&directory),
        "--recursive",
        "--json",
    ]);
    assert_success(&imported);
    let imported = json_output(&imported);
    assert_eq!(imported["count"], 4);
    let locators = imported["assets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|asset| asset["locator"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        locators,
        vec![
            text(&first),
            text(&directory.join("a.bin")),
            text(&nested.join("m.bin")),
            text(&directory.join("z.bin")),
        ]
    );

    let nested_working_directory = workspace.join("project/subdirectory");
    fs::create_dir_all(&nested_working_directory).unwrap();
    let second = test.path().join("second.bin");
    let third = test.path().join("third.bin");
    fs::write(&second, b"second").unwrap();
    fs::write(&third, b"third").unwrap();
    let nearest = histima_in(
        &nested_working_directory,
        ["import", text(&second), text(&third), "--json"],
    );
    assert_success(&nearest);
    let nearest = json_output(&nearest);
    assert_eq!(nearest["count"], 2);
    assert_eq!(nearest["assets"][0]["locator"], text(&second));
    assert_eq!(nearest["assets"][1]["locator"], text(&third));

    let stats = histima(["stats", text(&workspace), "--json"]);
    assert_success(&stats);
    assert_eq!(json_output(&stats)["source_heads"], 6);
}

#[test]
fn cli_verifies_sqlite_and_cataloged_content_without_repairing() {
    let test = TestDirectory::new();
    let workspace = test.path().join("workspace");
    let source = test.path().join("source.bin");
    fs::write(&source, b"verified content").unwrap();

    assert_success(&histima(["init", text(&workspace)]));
    let imported = histima(["import", text(&workspace), text(&source), "--json"]);
    assert_success(&imported);
    let imported = json_output(&imported);
    let content_id = imported["content_id"].as_str().unwrap();

    let verified = histima(["verify", text(&workspace), "--json"]);
    assert_success(&verified);
    let verified = json_output(&verified);
    assert_eq!(verified["valid"], true);
    assert_eq!(verified["sqlite_valid"], true);
    assert_eq!(verified["objects_checked"], 1);
    assert_eq!(verified["objects_valid"], 1);
    assert_eq!(verified["issue_count"], 0);

    let object = workspace
        .join("objects")
        .join(&content_id[..2])
        .join(&content_id[2..]);
    fs::write(&object, b"corrupt").unwrap();
    let rejected = histima(["verify", text(&workspace), "--json"]);
    assert!(!rejected.status.success());
    let rejected_json = json_output(&rejected);
    assert_eq!(rejected_json["valid"], false);
    assert_eq!(rejected_json["sqlite_valid"], true);
    assert_eq!(rejected_json["objects_checked"], 1);
    assert_eq!(rejected_json["objects_valid"], 0);
    assert_eq!(rejected_json["issue_count"], 1);
    assert_eq!(rejected_json["issues"][0]["kind"], "content");
    assert_eq!(rejected_json["issues"][0]["subject"], content_id);
    assert!(
        rejected_json["issues"][0]["message"]
            .as_str()
            .unwrap()
            .contains("content integrity failure")
    );
    assert!(stderr(&rejected).contains("workspace verification found 1 issue"));
    assert_eq!(fs::read(object).unwrap(), b"corrupt");
}

#[test]
fn cli_paginates_assets_and_recipes_with_stable_cursors() {
    let test = TestDirectory::new();
    let workspace = test.path().join("workspace");
    assert_success(&histima(["init", text(&workspace)]));

    let mut locators = Vec::new();
    let mut recipe_ids = Vec::new();
    let mut ppm_recipe_ids = Vec::new();
    for index in 0..3 {
        let file_name = if index < 2 {
            format!("group-{index}.bin")
        } else {
            "other.bin".to_owned()
        };
        let source = test.path().join(file_name);
        fs::write(&source, format!("P3\n1 1\n255\n{index} 0 0\n")).unwrap();
        let locator = portable(&source);
        assert_success(&histima(["import", text(&workspace), &locator]));
        let encoder = if index < 2 {
            "ppm.encode"
        } else {
            "webp.encode(quality=80)"
        };
        let expression = format!("asset({locator:?}) | read | ppm.decode | {encoder}");
        let pipeline = histima(["pipeline", text(&workspace), &expression, "--json"]);
        assert_success(&pipeline);
        let recipe_id = json_output(&pipeline)["stocked"]["recipe_id"]
            .as_str()
            .unwrap()
            .to_owned();
        if index < 2 {
            ppm_recipe_ids.push(recipe_id.clone());
        }
        recipe_ids.push(recipe_id);
        locators.push(locator);
    }
    locators.sort();
    recipe_ids.sort();
    ppm_recipe_ids.sort();

    let assets = histima(["assets", text(&workspace), "--limit", "2", "--json"]);
    assert_success(&assets);
    let assets = json_output(&assets);
    assert_eq!(assets["count"], 2);
    assert_eq!(assets["truncated"], true);
    assert_eq!(assets["assets"][0]["locator"], locators[0]);
    assert_eq!(assets["assets"][1]["locator"], locators[1]);
    assert_eq!(assets["next_cursor"], locators[1]);

    let assets_after = histima([
        "assets",
        "--after",
        &locators[1],
        "--limit",
        "2",
        text(&workspace),
        "--json",
    ]);
    assert_success(&assets_after);
    let assets_after = json_output(&assets_after);
    assert_eq!(assets_after["count"], 1);
    assert_eq!(assets_after["truncated"], false);
    assert_eq!(assets_after["next_cursor"], Value::Null);
    assert_eq!(assets_after["assets"][0]["locator"], locators[2]);

    let group_prefix = portable(&test.path().join("group-"));
    let filtered_assets = histima([
        "assets",
        text(&workspace),
        "--prefix",
        &group_prefix,
        "--limit",
        "1",
        "--json",
    ]);
    assert_success(&filtered_assets);
    let filtered_assets = json_output(&filtered_assets);
    assert_eq!(filtered_assets["count"], 1);
    assert_eq!(filtered_assets["truncated"], true);
    assert_eq!(filtered_assets["assets"][0]["locator"], locators[0]);
    assert_eq!(filtered_assets["next_cursor"], locators[0]);

    let filtered_assets_after = histima([
        "assets",
        text(&workspace),
        "--prefix",
        &group_prefix,
        "--after",
        &locators[0],
        "--limit",
        "1",
        "--json",
    ]);
    assert_success(&filtered_assets_after);
    let filtered_assets_after = json_output(&filtered_assets_after);
    assert_eq!(filtered_assets_after["count"], 1);
    assert_eq!(filtered_assets_after["truncated"], false);
    assert_eq!(filtered_assets_after["assets"][0]["locator"], locators[1]);

    let recipes = histima(["recipes", "--limit", "2", text(&workspace), "--json"]);
    assert_success(&recipes);
    let recipes = json_output(&recipes);
    assert_eq!(recipes["count"], 2);
    assert_eq!(recipes["truncated"], true);
    assert_eq!(recipes["recipes"][0]["recipe_id"], recipe_ids[0]);
    assert_eq!(recipes["recipes"][1]["recipe_id"], recipe_ids[1]);
    assert_eq!(recipes["next_cursor"], recipe_ids[1]);

    let recipes_after = histima([
        "recipes",
        text(&workspace),
        "--after",
        &recipe_ids[1],
        "--limit",
        "2",
        "--json",
    ]);
    assert_success(&recipes_after);
    let recipes_after = json_output(&recipes_after);
    assert_eq!(recipes_after["count"], 1);
    assert_eq!(recipes_after["truncated"], false);
    assert_eq!(recipes_after["next_cursor"], Value::Null);
    assert_eq!(recipes_after["recipes"][0]["recipe_id"], recipe_ids[2]);

    let filtered_recipes = histima([
        "recipes",
        text(&workspace),
        "--transform",
        "ppm.encode",
        "--limit",
        "1",
        "--json",
    ]);
    assert_success(&filtered_recipes);
    let filtered_recipes = json_output(&filtered_recipes);
    assert_eq!(filtered_recipes["count"], 1);
    assert_eq!(filtered_recipes["truncated"], true);
    assert_eq!(
        filtered_recipes["recipes"][0]["recipe_id"],
        ppm_recipe_ids[0]
    );
    assert_eq!(filtered_recipes["next_cursor"], ppm_recipe_ids[0]);

    let filtered_recipes_after = histima([
        "recipes",
        "--transform",
        "ppm.encode",
        "--after",
        &ppm_recipe_ids[0],
        "--limit",
        "1",
        text(&workspace),
        "--json",
    ]);
    assert_success(&filtered_recipes_after);
    let filtered_recipes_after = json_output(&filtered_recipes_after);
    assert_eq!(filtered_recipes_after["count"], 1);
    assert_eq!(filtered_recipes_after["truncated"], false);
    assert_eq!(
        filtered_recipes_after["recipes"][0]["recipe_id"],
        ppm_recipe_ids[1]
    );
    assert_eq!(
        filtered_recipes_after["recipes"][0]["transform_name"],
        "ppm.encode"
    );

    let invalid_limit = histima(["assets", text(&workspace), "--limit", "0"]);
    assert!(!invalid_limit.status.success());
    assert!(stderr(&invalid_limit).contains("list limit must be between 1 and 100"));

    let invalid_cursor = histima(["recipes", text(&workspace), "--after", "not-an-id"]);
    assert!(!invalid_cursor.status.success());
    assert!(stderr(&invalid_cursor).contains("invalid Recipe cursor"));
}

#[test]
fn cli_searches_asset_locators_and_recorded_transform_names() {
    let test = TestDirectory::new();
    let workspace = test.path().join("workspace");
    assert_success(&histima(["init", text(&workspace)]));

    for name in ["search-ppm-one.ppm", "search-ppm-two.ppm"] {
        let source = test.path().join(name);
        fs::write(&source, b"P3\n1 1\n255\n10 20 30\n").unwrap();
        let locator = portable(&source);
        assert_success(&histima(["import", text(&workspace), &locator]));
        let expression = format!("asset({locator:?}) | read | ppm.decode | ppm.encode");
        assert_success(&histima([
            "pipeline",
            text(&workspace),
            &expression,
            "--json",
        ]));
    }

    let search = histima(["search", text(&workspace), "ppm", "--limit", "1", "--json"]);
    assert_success(&search);
    let search = json_output(&search);
    assert_eq!(search["query"], "ppm");
    assert_eq!(search["assets"]["count"], 1);
    assert_eq!(search["assets"]["truncated"], true);
    assert!(
        search["assets"]["matches"][0]["locator"]
            .as_str()
            .unwrap()
            .contains("search-ppm-one.ppm")
    );
    assert_eq!(search["recipes"]["count"], 1);
    assert_eq!(search["recipes"]["truncated"], true);
    assert_eq!(
        search["recipes"]["matches"][0]["transform_name"],
        "ppm.encode"
    );

    let case_sensitive = histima(["search", text(&workspace), "PPM", "--json"]);
    assert_success(&case_sensitive);
    let case_sensitive = json_output(&case_sensitive);
    assert_eq!(case_sensitive["assets"]["count"], 0);
    assert_eq!(case_sensitive["recipes"]["count"], 0);

    let empty = histima(["search", text(&workspace), ""]);
    assert!(!empty.status.success());
    assert!(stderr(&empty).contains("catalog search query must not be empty"));
}

#[test]
fn cli_summarizes_bounded_actionable_workspace_state() {
    let test = TestDirectory::new();
    let workspace = test.path().join("workspace");
    assert_success(&histima(["init", text(&workspace)]));

    for name in ["summary-one.ppm", "summary-two.ppm"] {
        let source = test.path().join(name);
        fs::write(&source, b"P3\n1 1\n255\n40 50 60\n").unwrap();
        let locator = portable(&source);
        assert_success(&histima(["import", text(&workspace), &locator]));
        let expression = format!("asset({locator:?}) | read | ppm.decode | ppm.encode");
        assert_success(&histima([
            "pipeline",
            text(&workspace),
            &expression,
            "--json",
        ]));
    }

    let summary = histima(["summary", text(&workspace), "--limit", "1", "--json"]);
    assert_success(&summary);
    let summary = json_output(&summary);
    assert_eq!(summary["workspace"], text(&workspace));
    assert_eq!(summary["catalog"]["schema_version"], 6);
    assert_eq!(summary["catalog"]["foreign_keys_enabled"], true);
    assert_eq!(summary["counts"]["source_heads"], 2);
    assert_eq!(summary["counts"]["recipe_results"], 2);
    assert_eq!(summary["assets"]["count"], 1);
    assert_eq!(summary["assets"]["truncated"], true);
    let asset_cursor = summary["assets"]["next_cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(summary["recipes"]["count"], 1);
    assert_eq!(summary["recipes"]["truncated"], true);
    assert!(summary["recipes"]["next_cursor"].is_string());
    assert_eq!(summary["transforms"]["count"], 1);
    assert_eq!(summary["transforms"]["truncated"], true);
    assert_eq!(summary["transforms"]["transforms"][0]["name"], "png.decode");

    let remaining_assets = histima([
        "assets",
        text(&workspace),
        "--after",
        &asset_cursor,
        "--json",
    ]);
    assert_success(&remaining_assets);
    let remaining_assets = json_output(&remaining_assets);
    assert_eq!(remaining_assets["count"], 1);
    assert_eq!(remaining_assets["truncated"], false);
}

#[test]
fn cli_executes_and_strictly_replays_workspace_world_file_reads() {
    let test = TestDirectory::new();
    let workspace = test.path().join("workspace");
    let script = test.path().join("world-read.tima");
    assert_success(&histima(["init", text(&workspace)]));
    fs::write(workspace.join("config.bin"), b"first config").unwrap();
    fs::write(
        &script,
        "transform load() -> Bytes uses file.read {\n\
             return file.read(\"config.bin\")\n\
         }\n\
         out = load()\n",
    )
    .unwrap();

    let run = histima(["run", text(&workspace), text(&script), "--record", "out"]);
    assert_success(&run);
    assert!(stdout(&run).contains("execution_engine = interpreter"));
    let recipe = field(&run, "recipe_id");

    let trace = histima(["trace", text(&workspace), &recipe]);
    assert_success(&trace);
    assert!(stdout(&trace).contains("observe file.read"));
    assert!(stdout(&trace).contains("config.bin"));

    let replay = histima(["replay", text(&workspace), text(&script), &recipe]);
    assert_success(&replay);
    assert_eq!(field(&replay, "recipe_id"), recipe);

    fs::write(workspace.join("config.bin"), b"changed config").unwrap();
    let changed = histima(["replay", text(&workspace), text(&script), &recipe]);
    assert!(!changed.status.success());
    assert!(stderr(&changed).contains("replay expected external `file.read` dependency"));

    fs::remove_file(workspace.join("config.bin")).unwrap();
    let snapshot = histima([
        "replay",
        text(&workspace),
        text(&script),
        &recipe,
        "--snapshot",
    ]);
    assert_success(&snapshot);
    assert!(stdout(&snapshot).contains("replay_policy = snapshot"));
    assert_eq!(field(&snapshot, "recipe_id"), recipe);
}

#[test]
fn cli_runs_an_interpreted_tima_pipeline_against_imported_assets() {
    let test = TestDirectory::new();
    let workspace = test.path().join("workspace");
    let source = test.path().join("source.ppm");
    let script = test.path().join("pipeline.tima");
    let changed_script = test.path().join("changed-transform.tima");
    let output = test.path().join("copied.ppm");
    let restored = test.path().join("restored.ppm");
    fs::write(&source, b"P3\n1 1\n255\n200 100 50\n").unwrap();
    let source_locator = portable(&source);
    let output_locator = portable(&output);
    fs::write(
        &script,
        format!(
            "source = asset({source_locator:?})\n\
             transform copy(img: Buffer) -> Buffer {{\n\
                 return img\n\
             }}\n\
             out = source | read | ppm.decode | copy | ppm.encode\n\
             saved = out | save({output_locator:?})\n\
             trace(out)\n"
        ),
    )
    .unwrap();

    assert_success(&histima(["init", text(&workspace)]));
    assert_success(&histima(["import", text(&workspace), &source_locator]));

    let first = histima(["run", text(&workspace), text(&script), "--record", "out"]);
    assert_success(&first);
    assert!(stdout(&first).contains("execution_engine = interpreter"));
    assert!(stdout(&first).contains("artifact_cache = none"));
    assert!(stdout(&first).contains("result_cache_hits = 0"));
    assert!(stdout(&first).contains("invoke copy"));
    assert_eq!(fs::read(&output).unwrap(), b"P3\n1 1\n255\n200 100 50\n");
    let recipe_id = field(&first, "recipe_id");
    let content_id = field(&first, "content_id");
    assert_eq!(recipe_id.len(), 64);
    assert_eq!(content_id.len(), 64);

    let trace = histima(["trace", text(&workspace), &recipe_id]);
    assert_success(&trace);
    assert!(stdout(&trace).contains("source "));
    assert!(stdout(&trace).contains("invoke ppm.decode"));
    assert!(stdout(&trace).contains("invoke copy"));
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
    assert!(stdout(&inspected_recipe).contains("invoke copy"));

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
    assert!(stdout(&replayed).contains("execution_engine = interpreter"));
    assert!(stdout(&replayed).contains("invoke copy"));

    let materialized = histima([
        "materialize",
        text(&workspace),
        &content_id,
        text(&restored),
    ]);
    assert_success(&materialized);
    assert_eq!(fs::read(&restored).unwrap(), b"P3\n1 1\n255\n200 100 50\n");

    let stats = histima(["stats", text(&workspace)]);
    assert_success(&stats);
    assert!(stdout(&stats).contains("contents = 2"));
    assert!(stdout(&stats).contains("lineage_invocations = 3"));
    assert!(stdout(&stats).contains("recipe_results = 1"));
    assert!(stdout(&stats).contains("artifact_bundles = 0"));
    assert!(stdout(&stats).contains("artifacts = 0"));

    let renamed = fs::read_to_string(&script)
        .unwrap()
        .replace("transform copy", "transform shade")
        .replace(" | copy", " | shade");
    fs::write(&script, renamed).unwrap();
    fs::remove_file(&output).unwrap();
    let second = histima(["run", text(&workspace), text(&script), "--record", "out"]);
    assert_success(&second);
    assert!(stdout(&second).contains("artifact_cache = none"));
    assert!(stdout(&second).contains("result_cache_hits = 1"));
    assert!(stdout(&second).contains("invoke shade"));
    assert_eq!(field(&second, "recipe_id"), recipe_id);
    assert_eq!(field(&second, "content_id"), content_id);
    assert_eq!(fs::read(&output).unwrap(), b"P3\n1 1\n255\n200 100 50\n");

    let renamed_replay = histima(["replay", text(&workspace), text(&script), &recipe_id]);
    assert_success(&renamed_replay);
    assert_eq!(field(&renamed_replay, "content_id"), content_id);
    assert!(stdout(&renamed_replay).contains("invoke copy"));

    let semantically_changed = fs::read_to_string(&script)
        .unwrap()
        .replace("return img", "return buffer_zero(img)");
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
    assert_eq!(fs::read(&output).unwrap(), b"P3\n1 1\n255\n100 80 60\n");
}

#[test]
fn cli_runs_records_and_replays_a_user_darkened_png_to_webp_pipeline() {
    let test = TestDirectory::new();
    let workspace = test.path().join("workspace");
    let source = test.path().join("source.png");
    let script = test.path().join("pipeline.tima");
    let output = test.path().join("darkened.webp");
    let proof = test.path().join("darkened.png");
    fs::write(
        &source,
        encode_test_png(2, 1, &[100, 50, 20, 255, 200, 100, 50, 128]),
    )
    .unwrap();
    let source_locator = portable(&source);
    let output_locator = portable(&output);
    let proof_locator = portable(&proof);
    fs::write(
        &script,
        format!(
            "source = asset({source_locator:?})\n\
             transform darken_byte(value: u8, offset: i64, factor: f32) -> u8 {{\n\
                 pixel = offset / 4\n\
                 alpha = pixel * 4 + 3\n\
                 if offset == alpha {{ return value }} else {{ return u8.scale(value, factor) }}\n\
             }}\n\
             transform darken(buffer: Buffer, factor: f32) -> Buffer {{\n\
                 for byte, offset in buffer.bytes {{\n\
                     byte = darken_byte(byte, offset, factor)\n\
                 }}\n\
                 return buffer\n\
             }}\n\
             darkened = source | read | png.decode | darken(0.5)\n\
             out = darkened | webp.encode\n\
             proof = darkened | png.encode\n\
             saved = out | save({output_locator:?})\n\
             proof_saved = proof | save({proof_locator:?})\n"
        ),
    )
    .unwrap();

    assert_success(&histima(["init", text(&workspace)]));
    assert_success(&histima(["import", text(&workspace), &source_locator]));
    let run = histima(["run", text(&workspace), text(&script), "--record", "out"]);
    assert_success(&run);
    assert!(stdout(&run).contains("execution_engine = interpreter"));
    let encoded = fs::read(&output).unwrap();
    assert_eq!(&encoded[..4], b"RIFF");
    assert_eq!(&encoded[8..12], b"WEBP");
    assert_eq!(
        decode_test_png_rgba(&fs::read(&proof).unwrap()),
        (2, 1, vec![50, 25, 10, 255, 100, 50, 25, 128])
    );
    let recipe_id = field(&run, "recipe_id");

    let source_text = fs::read_to_string(&script).unwrap();
    fs::write(
        &script,
        source_text.replace(
            "out = darkened | webp.encode\n",
            "out = darkened | webp.encode(quality=85)\n",
        ),
    )
    .unwrap();
    fs::remove_file(&output).unwrap();
    fs::remove_file(&proof).unwrap();
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
             transform copy(img: Buffer) -> Buffer {{\n\
                 return img\n\
             }}\n\
             out = source | read | ppm.decode | copy | ppm.encode\n"
        ),
    )
    .unwrap();

    let initialized = histima(["--json", "init", text(&workspace)]);
    assert_success(&initialized);
    let initialized = json_output(&initialized);
    assert_eq!(initialized["schema_version"], 6);
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
    assert_eq!(run["execution_engine"], "interpreter");
    assert_eq!(run["artifact_cache"], Value::Null);
    assert_eq!(run["artifact"], Value::Null);
    assert_eq!(run["result_cache"]["hits"], 0);
    assert_eq!(run["bindings"]["out"]["type"], "bytes");
    assert_eq!(run["recorded"]["binding"], "out");
    let recipe_id = json_string(&run["recorded"], "recipe_id").to_owned();
    let content_id = json_string(&run["recorded"], "content_id").to_owned();
    for identity in [&recipe_id, &content_id] {
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
    assert!(recipe["trace"].as_str().unwrap().contains("invoke copy"));

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
    assert_eq!(stats["artifact_bundles"], 0);

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
    let alternate = test.path().join("alternate.ppm");
    fs::write(&source, b"P3\n1 1\n255\n24 48 96\n").unwrap();
    fs::write(&alternate, b"P3\n1 1\n255\n96 48 24\n").unwrap();
    let source_locator = portable(&source);
    let alternate_locator = portable(&alternate);
    let expression = format!("asset({source_locator:?}) | read | ppm.decode | ppm.encode");

    assert_success(&histima(["init", text(&workspace)]));
    assert_success(&histima(["import", text(&workspace), &source_locator]));
    assert_success(&histima(["import", text(&workspace), &alternate_locator]));

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
    assert_eq!(stats["artifact_bundles"], 0);
    assert_eq!(stats["artifacts"], 0);

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

    let generated = histima(["expression", text(&workspace), &recipe_id]);
    assert_success(&generated);
    let generated_expression = stdout(&generated).trim().to_owned();
    assert_eq!(generated_expression.lines().count(), 1);
    assert!(generated_expression.contains("read(asset("));
    assert!(generated_expression.contains("ppm.decode#"));
    assert!(generated_expression.contains("ppm.encode#"));
    assert!(generated_expression.ends_with(&format!("#{recipe_id}")));
    let regenerated = histima([
        "pipeline",
        text(&workspace),
        &generated_expression,
        "--json",
    ]);
    assert_success(&regenerated);
    assert_eq!(json_output(&regenerated)["stocked"]["recipe_id"], recipe_id);

    let starting_input = format!("asset({alternate_locator:?}) | read");
    let substituted = histima([
        "expression",
        text(&workspace),
        &recipe_id,
        "--input",
        &starting_input,
        "--json",
    ]);
    assert_success(&substituted);
    let substituted = json_output(&substituted);
    assert_eq!(substituted["recipe_id"], recipe_id);
    assert_eq!(substituted["starting_input"], starting_input);
    let substituted_expression = json_string(&substituted, "expression");
    assert!(!substituted_expression.contains(&recipe_id));
    assert!(substituted_expression.contains(&alternate_locator));
    let evaluated = histima([
        "pipeline",
        text(&workspace),
        substituted_expression,
        "--json",
    ]);
    assert_success(&evaluated);
    assert_ne!(json_output(&evaluated)["stocked"]["recipe_id"], recipe_id);

    let invalid_input = histima([
        "expression",
        text(&workspace),
        &recipe_id,
        "--input",
        "value = 1",
    ]);
    assert!(!invalid_input.status.success());
    assert!(stderr(&invalid_input).contains("exactly one Tima outer expression"));

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

#[test]
fn cli_loads_hashes_caches_and_replays_a_workspace_wasm_plugin() {
    let test = TestDirectory::new();
    let workspace = test.path().join("workspace");
    let source = test.path().join("source.ppm");
    let script = test.path().join("plugin-pipeline.tima");
    fs::write(&source, b"P3\n1 1\n255\n24 48 96\n").unwrap();
    let source_locator = portable(&source);

    assert_success(&histima(["init", text(&workspace)]));
    let plugin_directory = workspace.join("plugins");
    fs::create_dir_all(&plugin_directory).unwrap();
    let checked_in_module = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("plugins/ppm-encode/ppm_encode.wasm");
    let module = fs::read(checked_in_module).unwrap();
    let module_path = plugin_directory.join("fixture.wasm");
    fs::write(&module_path, &module).unwrap();
    let module_content = tima::identity::byte_content_identity(&module);
    let transform_id = tima::identity::registered_wasm_transform_identity(
        "fixture.encode",
        1,
        4,
        &[("buffer", 2)],
        1,
    );
    fs::write(
        plugin_directory.join("fixture.toml"),
        format!(
            "name = \"fixture.encode\"\n\
             semantic_version = 1\n\
             abi_version = 4\n\
             module = \"fixture.wasm\"\n\
             module_content = \"{module_content}\"\n\
             result = \"bytes\"\n\
             [[parameters]]\n\
             name = \"buffer\"\n\
             type = \"buffer\"\n"
        ),
    )
    .unwrap();
    fs::write(
        workspace.join(".histima.toml"),
        "[plugins]\nmanifests = [\"plugins/fixture.toml\"]\n",
    )
    .unwrap();
    let artifact_id = tima::identity::registered_wasm_artifact_identity(transform_id, &module, 4);
    let plugins = histima(["plugins", text(&workspace), "--json"]);
    assert_success(&plugins);
    let plugins = json_output(&plugins);
    assert_eq!(plugins["count"], 1);
    let plugin = &plugins["plugins"][0];
    assert_eq!(plugin["name"], "fixture.encode");
    assert_eq!(plugin["semantic_version"], 1);
    assert_eq!(plugin["abi_version"], 4);
    assert_eq!(plugin["transform_id"], transform_id.to_string());
    assert_eq!(plugin["artifact_id"], artifact_id.to_string());
    assert_eq!(plugin["module_content_id"], module_content.to_string());
    assert_eq!(plugin["origin"], "workspace");
    assert_eq!(plugin["implementation"], "registered-wasm");
    assert_eq!(
        plugin["signature"],
        "fixture.encode(buffer: BufferView) -> Bytes"
    );
    assert_eq!(plugin["parameters"][0]["name"], "buffer");
    assert_eq!(plugin["parameters"][0]["type"], "BufferView");
    assert_eq!(plugin["result"], "Bytes");

    let transforms = histima(["transforms", text(&workspace), "--json"]);
    assert_success(&transforms);
    let transforms = json_output(&transforms);
    assert_eq!(transforms["count"], 6);
    let transforms = transforms["transforms"].as_array().unwrap();
    let builtin = transforms
        .iter()
        .find(|transform| transform["name"] == "ppm.decode")
        .unwrap();
    assert_eq!(builtin["origin"], "standard");
    assert_eq!(builtin["implementation"], "registered-wasm");
    assert_eq!(builtin["semantic_version"], 3);
    assert_eq!(
        builtin["signature"],
        "ppm.decode(bytes: BytesView) -> Buffer"
    );
    assert_eq!(
        builtin["transform_id"],
        tima::identity::registered_transform_identity("ppm.decode", 3).to_string()
    );
    assert_eq!(builtin["artifact_id"], Value::Null);
    let external = transforms
        .iter()
        .find(|transform| transform["name"] == "fixture.encode")
        .unwrap();
    assert_eq!(external["origin"], "workspace");
    assert_eq!(external["implementation"], "registered-wasm");
    assert_eq!(external["signature"], plugin["signature"]);
    assert_eq!(external["transform_id"], transform_id.to_string());
    assert_eq!(external["artifact_id"], artifact_id.to_string());
    assert_eq!(external["module_content_id"], module_content.to_string());

    let human_plugins = histima(["plugins", text(&workspace)]);
    assert_success(&human_plugins);
    assert_eq!(field(&human_plugins, "count"), "1");
    assert_eq!(
        field(&human_plugins, "plugin[0].signature"),
        "fixture.encode(buffer: BufferView) -> Bytes"
    );
    fs::write(
        &script,
        format!(
            "source = asset({source_locator:?})\n\
             out = source | read | ppm.decode | fixture.encode#{}\n",
            &transform_id.to_string()[..16]
        ),
    )
    .unwrap();
    assert_success(&histima(["import", text(&workspace), &source_locator]));

    let first = histima(["run", text(&workspace), text(&script), "--record", "out"]);
    assert_success(&first);
    assert_eq!(field(&first, "result_cache_hits"), "0");
    let recipe = field(&first, "recipe_id");
    let content = field(&first, "content_id");
    let trace = histima(["trace", text(&workspace), &recipe]);
    assert_success(&trace);
    assert!(stdout(&trace).contains("invoke fixture.encode"));

    let second = histima(["run", text(&workspace), text(&script), "--record", "out"]);
    assert_success(&second);
    assert_eq!(field(&second, "result_cache_hits"), "1");
    assert_eq!(field(&second, "recipe_id"), recipe);
    assert_eq!(field(&second, "content_id"), content);

    let replay = histima(["replay", text(&workspace), text(&script), &recipe]);
    assert_success(&replay);
    assert_eq!(field(&replay, "content_id"), content);
    assert!(stdout(&replay).contains("invoke fixture.encode"));

    let mut tampered = module.clone();
    tampered[0] ^= 1;
    fs::write(&module_path, tampered).unwrap();
    let rejected = histima(["stats", text(&workspace)]);
    assert!(!rejected.status.success());
    assert!(stderr(&rejected).contains("expected module content"));

    fs::write(&module_path, module).unwrap();
    assert_success(&histima([
        "replay",
        text(&workspace),
        text(&script),
        &recipe,
    ]));
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

fn decode_test_png_rgba(encoded: &[u8]) -> (u32, u32, Vec<u8>) {
    let decoder = png::Decoder::new(std::io::Cursor::new(encoded));
    let mut reader = decoder.read_info().unwrap();
    assert_eq!(reader.info().color_type, png::ColorType::Rgba);
    assert_eq!(reader.info().bit_depth, png::BitDepth::Eight);
    let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
    let frame = reader.next_frame(&mut pixels).unwrap();
    pixels.truncate(frame.buffer_size());
    (frame.width, frame.height, pixels)
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
