use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;
use tima::identity::byte_content_identity;

static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
             out = source | decode.ppm | darken(0.5) | encode.ppm\n\
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

    let database = Connection::open(workspace.join("catalog.sqlite3")).unwrap();
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

    let trace = histima(["trace", text(&workspace), &recipe_id]);
    assert_success(&trace);
    assert!(stdout(&trace).contains("source "));
    assert!(stdout(&trace).contains("invoke decode.ppm"));
    assert!(stdout(&trace).contains("invoke darken"));
    assert!(stdout(&trace).contains("invoke encode.ppm"));

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

    let database = Connection::open(workspace.join("catalog.sqlite3")).unwrap();
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
             out = source | decode.png | darken(0.5) | encode.webp\n\
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
        source_text.replace("| encode.webp\n", "| encode.webp(quality=85)\n"),
    )
    .unwrap();
    fs::remove_file(&output).unwrap();
    let explicit_default = histima(["run", text(&workspace), text(&script), "--record", "out"]);
    assert_success(&explicit_default);
    assert_eq!(field(&explicit_default, "recipe_id"), recipe_id);
    assert!(stdout(&explicit_default).contains("result_cache_hits = 1"));

    let trace = histima(["trace", text(&workspace), &recipe_id]);
    assert_success(&trace);
    assert!(stdout(&trace).contains("invoke decode.png"));
    assert!(stdout(&trace).contains("invoke darken"));
    assert!(stdout(&trace).contains("invoke encode.webp"));

    let replay = histima(["replay", text(&workspace), text(&script), &recipe_id]);
    assert_success(&replay);
    assert!(stdout(&replay).contains("result_cache_hits = 1"));
    assert_eq!(field(&replay, "content_id"), field(&run, "content_id"));
}

fn histima<const N: usize>(arguments: [&str; N]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_histima"))
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
