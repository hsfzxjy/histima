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
    assert!(stdout(&initialized).contains("schema_version = 1"));

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

fn text(path: &Path) -> &str {
    path.to_str().unwrap()
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
