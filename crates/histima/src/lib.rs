//! Histima's durable host boundary around the Tima language runtime.
//!
//! Large immutable payloads live in a filesystem content-addressed store.
//! SQLite stores queryable identities and relationships; it is not the blob
//! store. Durable derived byte results retain separate recipe, content, and
//! normalized lineage records for cross-process inspection.

mod atomic_file;
mod cas;
mod catalog;
mod error;
mod runner;

use std::fs;
use std::path::{Path, PathBuf};

use tima::capability::RuntimeCapabilities;
use tima::identity::{ContentIdentity, RecipeIdentity, SourceIdentity, source_identity};
use tima::lineage::LineageNode;
use tima::runtime::{OuterValue, ValueData};

use cas::{ContentKind, ContentStore};
use catalog::Catalog;

pub use catalog::{CatalogInfo, CatalogStats};
pub use error::{Error, Result};
pub use runner::{ProgramExecution, RunError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedAsset {
    pub locator: String,
    pub content_id: ContentIdentity,
    pub source_id: SourceIdentity,
    pub byte_len: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedResult {
    pub recipe_id: RecipeIdentity,
    pub content_id: ContentIdentity,
    pub byte_len: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurableTrace {
    pub recipe_id: RecipeIdentity,
    pub content_id: ContentIdentity,
    pub rendered: String,
}

pub struct Workspace {
    root: PathBuf,
    content: ContentStore,
    catalog: Catalog,
}

impl Workspace {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_owned();
        fs::create_dir_all(&root)
            .map_err(|error| Error::io("create Histima workspace", &root, error))?;
        let content = ContentStore::open(&root)?;
        let catalog = Catalog::open(&root.join("catalog.sqlite3"))?;
        Ok(Self {
            root,
            content,
            catalog,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Root for compiled artifacts and future non-semantic execution caches.
    pub fn native_cache_root(&self) -> PathBuf {
        self.root.join("cache")
    }

    pub fn import_file(&mut self, path: impl AsRef<Path>) -> Result<ImportedAsset> {
        let path = path.as_ref();
        let locator = path
            .to_str()
            .ok_or_else(|| Error::NonUtf8Locator(path.to_owned()))?;
        let bytes = fs::read(path).map_err(|error| Error::io("read source asset", path, error))?;
        self.import_bytes(locator, &bytes)
    }

    pub fn read_asset(&self, locator: &str) -> Result<Vec<u8>> {
        let object = self
            .catalog
            .current_source(locator)?
            .ok_or_else(|| Error::AssetNotFound(locator.to_owned()))?;
        let bytes =
            self.content
                .read_recorded(&object.content_id, &object.relative_path, object.kind)?;
        if bytes.len() as u64 != object.byte_len {
            return Err(Error::catalog(format!(
                "content {} has catalog length {} but stored length {}",
                object.content_id,
                object.byte_len,
                bytes.len()
            )));
        }
        Ok(bytes)
    }

    pub fn read_content(&self, identity: ContentIdentity) -> Result<Vec<u8>> {
        let object = self
            .catalog
            .content(identity)?
            .ok_or_else(|| Error::ContentNotFound(identity.to_string()))?;
        let bytes =
            self.content
                .read_recorded(&object.content_id, &object.relative_path, object.kind)?;
        if bytes.len() as u64 != object.byte_len {
            return Err(Error::catalog(format!(
                "content {} has catalog length {} but stored length {}",
                object.content_id,
                object.byte_len,
                bytes.len()
            )));
        }
        Ok(bytes)
    }

    pub fn content_path(&self, identity: ContentIdentity) -> PathBuf {
        self.content.object_path(identity)
    }

    pub fn materialize_content(
        &self,
        identity: ContentIdentity,
        destination: impl AsRef<Path>,
    ) -> Result<()> {
        let bytes = self.read_content(identity)?;
        atomic_file::write_new(destination.as_ref(), &bytes)
    }

    /// Persists one invocation-derived immutable byte value and its semantic
    /// lineage. Other outer value encodings remain intentionally unsupported.
    pub fn record_value(&mut self, value: &OuterValue) -> Result<RecordedResult> {
        let ValueData::Bytes(bytes) = &value.data else {
            return Err(Error::ValueNotRecordable(
                "durable recording currently supports only immutable byte values".to_owned(),
            ));
        };
        let Some(lineage) = &value.lineage else {
            return Err(Error::ValueNotRecordable(
                "durable recording requires transform invocation lineage".to_owned(),
            ));
        };
        if !matches!(lineage.node(), LineageNode::Invocation(_)) {
            return Err(Error::ValueNotRecordable(
                "durable recording requires transform invocation lineage".to_owned(),
            ));
        }
        let content = self.content.put_bytes_value(bytes)?;
        let trace = self.catalog.record_result(lineage, &content)?;
        Ok(RecordedResult {
            recipe_id: trace.recipe_id,
            content_id: trace.content_id,
            byte_len: content.byte_len,
        })
    }

    pub fn trace_recipe(&self, recipe: RecipeIdentity) -> Result<DurableTrace> {
        let trace = self
            .catalog
            .trace(recipe)?
            .ok_or_else(|| Error::RecipeNotFound(recipe.to_string()))?;
        self.read_content(trace.content_id)?;
        Ok(DurableTrace {
            recipe_id: trace.recipe_id,
            content_id: trace.content_id,
            rendered: trace.rendered,
        })
    }

    fn cached_value(&self, recipe: RecipeIdentity) -> Result<Option<OuterValue>> {
        let Some(trace) = self.catalog.trace(recipe)? else {
            return Ok(None);
        };
        self.typed_value(trace.content_id)?.map_or_else(
            || {
                Err(Error::catalog(format!(
                    "recipe {recipe} points to unsupported content {}",
                    trace.content_id
                )))
            },
            |value| Ok(Some(value)),
        )
    }

    fn typed_value(&self, identity: ContentIdentity) -> Result<Option<OuterValue>> {
        let Some(object) = self.catalog.content(identity)? else {
            return Ok(None);
        };
        if object.kind != ContentKind::Bytes {
            return Ok(None);
        }
        let bytes =
            self.content
                .read_recorded(&object.content_id, &object.relative_path, object.kind)?;
        if bytes.len() as u64 != object.byte_len {
            return Err(Error::catalog(format!(
                "content {} has catalog length {} but stored length {}",
                object.content_id,
                object.byte_len,
                bytes.len()
            )));
        }
        Ok(Some(OuterValue::plain(ValueData::Bytes(bytes.into()))))
    }

    pub fn catalog_info(&self) -> Result<CatalogInfo> {
        self.catalog.info()
    }

    pub fn catalog_stats(&self) -> Result<CatalogStats> {
        self.catalog.stats()
    }

    fn import_bytes(&mut self, locator: &str, bytes: &[u8]) -> Result<ImportedAsset> {
        let content = self.content.put(bytes)?;
        let source_id = source_identity(locator, content.identity);
        self.catalog.record_import(locator, source_id, &content)?;
        Ok(ImportedAsset {
            locator: locator.to_owned(),
            content_id: content.identity,
            source_id,
            byte_len: content.byte_len,
        })
    }
}

impl RuntimeCapabilities for Workspace {
    fn environment(&self, name: &str) -> std::result::Result<Vec<u8>, String> {
        Err(format!(
            "environment value `{name}` is unavailable in this Histima workspace"
        ))
    }

    fn read_asset(&self, locator: &str) -> std::result::Result<Vec<u8>, String> {
        Workspace::read_asset(self, locator).map_err(|error| error.to_string())
    }

    fn write_asset(&self, locator: &str, bytes: &[u8]) -> std::result::Result<(), String> {
        atomic_file::write_new(Path::new(locator), bytes).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use tima::identity::byte_content_identity;

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn workspace_applies_catalog_migrations_and_connection_policy() {
        let test = TestDirectory::new("catalog");
        let workspace = Workspace::open(test.path().join("workspace")).unwrap();

        assert_eq!(
            workspace.catalog_info().unwrap(),
            CatalogInfo {
                schema_version: 2,
                foreign_keys_enabled: true,
                journal_mode: "wal".to_owned(),
            }
        );
        assert_eq!(workspace.catalog_stats().unwrap(), CatalogStats::default());
    }

    #[test]
    fn import_deduplicates_content_and_preserves_source_versions() {
        let test = TestDirectory::new("import");
        let workspace_path = test.path().join("workspace");
        let first_path = test.path().join("first.ppm");
        let second_path = test.path().join("second.ppm");
        write(&first_path, b"same bytes");
        write(&second_path, b"same bytes");
        let mut workspace = Workspace::open(&workspace_path).unwrap();

        let first = workspace.import_file(&first_path).unwrap();
        let duplicate = workspace.import_file(&first_path).unwrap();
        let second = workspace.import_file(&second_path).unwrap();

        assert_eq!(first, duplicate);
        assert_eq!(first.content_id, second.content_id);
        assert_ne!(first.source_id, second.source_id);
        assert_eq!(
            workspace.read_content(first.content_id).unwrap(),
            b"same bytes"
        );
        assert_eq!(workspace.read_asset(&first.locator).unwrap(), b"same bytes");
        assert_eq!(
            workspace.catalog_stats().unwrap(),
            CatalogStats {
                contents: 1,
                source_versions: 2,
                source_heads: 2,
                lineage_invocations: 0,
                recipe_results: 0,
            }
        );

        write(&first_path, b"changed bytes");
        let changed = workspace.import_file(&first_path).unwrap();
        assert_ne!(changed.content_id, first.content_id);
        assert_ne!(changed.source_id, first.source_id);
        assert_eq!(
            workspace.read_asset(&first.locator).unwrap(),
            b"changed bytes"
        );
        assert_eq!(
            workspace.catalog_stats().unwrap(),
            CatalogStats {
                contents: 2,
                source_versions: 3,
                source_heads: 2,
                lineage_invocations: 0,
                recipe_results: 0,
            }
        );
        assert!(
            workspace_path
                .join("tmp")
                .read_dir()
                .unwrap()
                .next()
                .is_none()
        );

        drop(workspace);
        let reopened = Workspace::open(&workspace_path).unwrap();
        assert_eq!(
            reopened.read_asset(&first.locator).unwrap(),
            b"changed bytes"
        );
        assert_eq!(reopened.catalog_stats().unwrap().contents, 2);
    }

    #[test]
    fn content_corruption_is_detected_before_reuse() {
        let test = TestDirectory::new("integrity");
        let input = test.path().join("source.bin");
        write(&input, b"original");
        let mut workspace = Workspace::open(test.path().join("workspace")).unwrap();
        let imported = workspace.import_file(&input).unwrap();
        write(&workspace.content_path(imported.content_id), b"corrupt");

        let error = workspace.read_content(imported.content_id).unwrap_err();
        assert!(matches!(error, Error::Integrity { .. }));
        let error = workspace.import_file(&input).unwrap_err();
        assert!(matches!(error, Error::Integrity { .. }));
    }

    #[test]
    fn materialization_is_atomic_and_does_not_replace_existing_files() {
        let test = TestDirectory::new("materialize");
        let input = test.path().join("source.bin");
        let output = test.path().join("output.bin");
        write(&input, b"stored bytes");
        let mut workspace = Workspace::open(test.path().join("workspace")).unwrap();
        let imported = workspace.import_file(input).unwrap();

        workspace
            .materialize_content(imported.content_id, &output)
            .unwrap();
        assert_eq!(fs::read(&output).unwrap(), b"stored bytes");

        let error = workspace
            .materialize_content(imported.content_id, &output)
            .unwrap_err();
        assert!(matches!(error, Error::MaterializationExists(_)));
        assert_eq!(fs::read(&output).unwrap(), b"stored bytes");
        assert!(test.path().read_dir().unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".histima-")
        }));
    }

    #[test]
    fn unknown_asset_locator_is_not_read_from_the_ambient_filesystem() {
        let test = TestDirectory::new("explicit-read");
        let workspace = Workspace::open(test.path().join("workspace")).unwrap();
        let existing = test.path().join("ambient.bin");
        write(&existing, b"ambient");

        let error = workspace
            .read_asset(existing.to_str().unwrap())
            .unwrap_err();

        assert!(matches!(error, Error::AssetNotFound(_)));
    }

    #[test]
    fn runtime_output_capability_does_not_replace_existing_files() {
        let test = TestDirectory::new("runtime-output");
        let workspace = Workspace::open(test.path().join("workspace")).unwrap();
        let output = test.path().join("output.bin");

        RuntimeCapabilities::write_asset(&workspace, output.to_str().unwrap(), b"first").unwrap();
        let error =
            RuntimeCapabilities::write_asset(&workspace, output.to_str().unwrap(), b"replacement")
                .unwrap_err();

        assert!(error.contains("refusing to replace"));
        assert_eq!(fs::read(output).unwrap(), b"first");
    }

    #[test]
    fn one_recipe_cannot_be_recorded_with_conflicting_content() {
        use tima::lineage::{Lineage, LineageArgument};

        let test = TestDirectory::new("recipe-conflict");
        let mut workspace = Workspace::open(test.path().join("workspace")).unwrap();
        let compiled = tima::compile(
            "test.tima",
            "transform keep(value: i64) -> i64 { return value }\n",
        )
        .unwrap();
        let argument = OuterValue::plain(ValueData::Integer(1));
        let lineage = Lineage::invocation(
            "keep",
            compiled.identities.get(tima::ir::TransformId(0)),
            vec![LineageArgument::record("value", &argument).unwrap()],
            vec![],
        )
        .unwrap();

        let first = OuterValue::plain(ValueData::Bytes(b"first".as_slice().into()))
            .with_lineage(lineage.clone());
        let conflicting =
            OuterValue::plain(ValueData::Bytes(b"second".as_slice().into())).with_lineage(lineage);
        workspace.record_value(&first).unwrap();

        let error = workspace.record_value(&conflicting).unwrap_err();
        assert!(error.to_string().contains("already maps to content"));
        assert_eq!(workspace.catalog_stats().unwrap().recipe_results, 1);
    }

    fn write(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join("build")
                .join("histima-tests")
                .join(format!("{name}-{}-{sequence}", std::process::id()));
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

    #[test]
    fn imported_content_uses_tima_content_identity() {
        let test = TestDirectory::new("identity");
        let input = test.path().join("source.bin");
        write(&input, b"identity bytes");
        let mut workspace = Workspace::open(test.path().join("workspace")).unwrap();

        let imported = workspace.import_file(input).unwrap();

        assert_eq!(
            imported.content_id,
            byte_content_identity(b"identity bytes")
        );
    }
}
