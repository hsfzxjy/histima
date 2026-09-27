use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

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
    assert!(stdout(&initialized).contains("schema_version = 2"));

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
    assert_eq!(recipe_id.len(), 64);
    assert_eq!(content_id.len(), 64);

    let trace = histima(["trace", text(&workspace), &recipe_id]);
    assert_success(&trace);
    assert!(stdout(&trace).contains("source "));
    assert!(stdout(&trace).contains("invoke decode.ppm"));
    assert!(stdout(&trace).contains("invoke darken"));
    assert!(stdout(&trace).contains("invoke encode.ppm"));

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

    fs::write(&source, b"P3\n1 1\n255\n100 80 60\n").unwrap();
    assert_success(&histima(["import", text(&workspace), &source_locator]));
    fs::remove_file(&output).unwrap();
    let changed = histima(["run", text(&workspace), text(&script), "--record", "out"]);
    assert_success(&changed);
    assert!(stdout(&changed).contains("result_cache_hits = 0"));
    assert_ne!(field(&changed, "recipe_id"), recipe_id);
    assert_ne!(field(&changed, "content_id"), content_id);
    assert_eq!(fs::read(&output).unwrap(), b"P3\n1 1\n255\n50 40 30\n");
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
