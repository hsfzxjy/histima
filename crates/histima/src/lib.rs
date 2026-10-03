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
mod stored_lineage;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tima::capability::{ENVIRONMENT_CAPABILITY, FILE_READ_CAPABILITY, HTTP_GET_CAPABILITY, World};
use tima::identity::{
    ArtifactIdentity, ContentIdentity, DependencyIdentity, IdentityDomain, IdentityPrefixResolver,
    RecipeIdentity, SourceIdentity, byte_content_identity, source_identity,
};
use tima::lineage::{Lineage, LineageNode};
use tima::plugin::{PluginDefinition, PluginParameter, PluginRegistry, PluginValueType};
use tima::runtime::{OuterValue, ValueData};
use tima::transform::{TransformInfo, TransformOrigin};

use cas::{ContentKind, ContentStore};
use catalog::{Catalog, validate_artifact_identities};

const CATALOG_FILE_NAME: &str = ".histima.sql3";
const LEGACY_CATALOG_FILE_NAME: &str = "catalog.sqlite3";
const WORLD_HTTP_READ_LIMIT: u64 = 64 * 1024 * 1024;
const PLUGIN_MANIFEST_READ_LIMIT: u64 = 1024 * 1024;
const PLUGIN_MODULE_READ_LIMIT: u64 = 64 * 1024 * 1024;

pub use catalog::{
    ArtifactBundleMember, ArtifactInfo, ArtifactSummary, AssetSummary, CATALOG_LIST_LIMIT,
    CatalogInfo, CatalogPage, CatalogSearch, CatalogStats, RecipeSummary,
};
pub use error::{Error, Result};
pub use runner::{PipelineExecution, ProgramExecution, RecipeReplay, ReplayPolicy, RunError};

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentInspection {
    pub content_id: ContentIdentity,
    pub kind: String,
    pub byte_len: u64,
    pub relative_path: String,
    pub source_references: u64,
    pub recipe_references: u64,
    pub valid: bool,
    pub validation_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecipeInspection {
    pub recipe_id: RecipeIdentity,
    pub content_id: ContentIdentity,
    pub lineage: Lineage,
    pub rendered: String,
    pub content: ContentInspection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactBundleInspection {
    pub bundle_id: tima::identity::ArtifactBundleIdentity,
    pub backend: String,
    pub backend_version: String,
    pub compiler_version: String,
    pub target: String,
    pub cpu_features: String,
    pub optimization: String,
    pub abi_version: u32,
    pub artifact_content_id: ContentIdentity,
    pub artifact_byte_len: u64,
    pub artifact_relative_path: String,
    pub members: Vec<ArtifactBundleMember>,
    pub identity_valid: bool,
    pub artifact_valid: bool,
    pub valid: bool,
    pub validation_errors: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactInspection {
    pub requested_id: String,
    pub artifact: Option<ArtifactSummary>,
    pub bundles: CatalogPage<ArtifactBundleInspection>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerificationIssueKind {
    Sqlite,
    Catalog,
    Content,
}

impl VerificationIssueKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::Catalog => "catalog",
            Self::Content => "content",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationIssue {
    pub kind: VerificationIssueKind,
    pub subject: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceVerification {
    pub sqlite_valid: bool,
    pub objects_checked: u64,
    pub objects_valid: u64,
    pub issues: Vec<VerificationIssue>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceSummary {
    pub workspace: PathBuf,
    pub catalog: CatalogInfo,
    pub stats: CatalogStats,
    pub assets: CatalogPage<AssetSummary>,
    pub recipes: CatalogPage<RecipeSummary>,
    pub transforms: Vec<TransformInfo>,
    pub transforms_truncated: bool,
}

impl WorkspaceVerification {
    pub fn is_valid(&self) -> bool {
        self.sqlite_valid && self.issues.is_empty()
    }
}

pub struct Workspace {
    root: PathBuf,
    content: ContentStore,
    catalog: Catalog,
    world: WorldPolicy,
    plugins: Arc<PluginRegistry>,
}

#[derive(Debug)]
struct WorldPolicy {
    environment: BTreeSet<String>,
    retain_environment: BTreeSet<String>,
    file_roots: Vec<PathBuf>,
    http_prefixes: Vec<String>,
}

impl WorldPolicy {
    fn load(root: &Path) -> Result<Self> {
        let workspace_root = fs::canonicalize(root)
            .map_err(|error| Error::io("resolve Histima workspace", root, error))?;
        let mut policy = Self {
            environment: BTreeSet::new(),
            retain_environment: BTreeSet::new(),
            file_roots: vec![workspace_root],
            http_prefixes: Vec::new(),
        };
        let config_path = root.join(".histima.toml");
        if !config_path.is_file() {
            return Ok(policy);
        }
        let text = fs::read_to_string(&config_path)
            .map_err(|error| Error::io("read workspace configuration", &config_path, error))?;
        let config = toml::from_str::<toml::Value>(&text).map_err(|error| {
            Error::catalog(format!("invalid {}: {error}", config_path.display()))
        })?;
        let Some(world) = config.get("world") else {
            return Ok(policy);
        };
        let world = world.as_table().ok_or_else(|| {
            Error::catalog(format!(
                "[world] in {} must be a table",
                config_path.display()
            ))
        })?;

        policy.environment.extend(config_strings(
            world.get("environment"),
            "[world].environment",
        )?);
        policy.retain_environment.extend(config_strings(
            world.get("retain_environment"),
            "[world].retain_environment",
        )?);
        if let Some(name) = policy
            .retain_environment
            .iter()
            .find(|name| !policy.environment.contains(*name))
        {
            return Err(Error::catalog(format!(
                "[world].retain_environment entry {name:?} must also appear in [world].environment"
            )));
        }
        policy.http_prefixes = config_strings(world.get("http_prefixes"), "[world].http_prefixes")?;
        for prefix in &policy.http_prefixes {
            if !(prefix.starts_with("http://") || prefix.starts_with("https://")) {
                return Err(Error::catalog(format!(
                    "[world].http_prefixes entry {prefix:?} must begin with http:// or https://"
                )));
            }
        }
        for configured in config_strings(world.get("file_roots"), "[world].file_roots")? {
            let configured = PathBuf::from(configured);
            let configured = if configured.is_absolute() {
                configured
            } else {
                root.join(configured)
            };
            let resolved = fs::canonicalize(&configured).map_err(|error| {
                Error::io("resolve configured World file root", &configured, error)
            })?;
            if !resolved.is_dir() {
                return Err(Error::catalog(format!(
                    "configured World file root {} is not a directory",
                    resolved.display()
                )));
            }
            if !policy.file_roots.contains(&resolved) {
                policy.file_roots.push(resolved);
            }
        }
        Ok(policy)
    }

    fn resolve_file(
        &self,
        workspace: &Path,
        requested: &str,
    ) -> std::result::Result<PathBuf, String> {
        let requested_path = Path::new(requested);
        let candidate = if requested_path.is_absolute() {
            requested_path.to_owned()
        } else {
            workspace.join(requested_path)
        };
        let resolved = fs::canonicalize(&candidate)
            .map_err(|error| format!("could not resolve {}: {error}", candidate.display()))?;
        if self
            .file_roots
            .iter()
            .any(|root| resolved.starts_with(root))
        {
            Ok(resolved)
        } else {
            Err(format!(
                "{} is outside the workspace and configured file roots",
                resolved.display()
            ))
        }
    }
}

fn config_strings(value: Option<&toml::Value>, key: &str) -> Result<Vec<String>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .ok_or_else(|| Error::catalog(format!("{key} must be an array of strings")))?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| Error::catalog(format!("{key} must contain only strings")))
        })
        .collect()
}

fn load_plugins(root: &Path) -> Result<Arc<PluginRegistry>> {
    let config_path = root.join(".histima.toml");
    if !config_path.is_file() {
        return Ok(Arc::default());
    }
    let text = fs::read_to_string(&config_path)
        .map_err(|error| Error::io("read workspace configuration", &config_path, error))?;
    let config = toml::from_str::<toml::Value>(&text)
        .map_err(|error| Error::catalog(format!("invalid {}: {error}", config_path.display())))?;
    let Some(plugins) = config.get("plugins") else {
        return Ok(Arc::default());
    };
    let plugins = plugins.as_table().ok_or_else(|| {
        Error::catalog(format!(
            "[plugins] in {} must be a table",
            config_path.display()
        ))
    })?;
    reject_unknown_fields(plugins, &["manifests"], "[plugins]", &config_path)?;
    let manifests = config_strings(plugins.get("manifests"), "[plugins].manifests")?;
    let workspace_root = fs::canonicalize(root)
        .map_err(|error| Error::io("resolve Histima workspace", root, error))?;
    let mut definitions = Vec::with_capacity(manifests.len());
    let mut loaded_manifests = BTreeSet::new();
    for configured in manifests {
        let path =
            confined_plugin_path(&workspace_root, &root.join(&configured), "plugin manifest")?;
        if !loaded_manifests.insert(path.clone()) {
            return Err(Error::catalog(format!(
                "plugin manifest {} is configured more than once",
                path.display()
            )));
        }
        definitions.push(load_plugin_manifest(&workspace_root, &path)?);
    }
    PluginRegistry::new(definitions)
        .map(Arc::new)
        .map_err(|error| Error::catalog(error.to_string()))
}

fn load_plugin_manifest(workspace_root: &Path, path: &Path) -> Result<PluginDefinition> {
    let manifest_size = fs::metadata(path)
        .map_err(|error| Error::io("inspect plugin manifest", path, error))?
        .len();
    if manifest_size > PLUGIN_MANIFEST_READ_LIMIT {
        return Err(Error::catalog(format!(
            "plugin manifest {} is {manifest_size} bytes; the limit is {PLUGIN_MANIFEST_READ_LIMIT}",
            path.display()
        )));
    }
    let text =
        fs::read_to_string(path).map_err(|error| Error::io("read plugin manifest", path, error))?;
    let manifest = toml::from_str::<toml::Value>(&text)
        .map_err(|error| Error::catalog(format!("invalid {}: {error}", path.display())))?;
    let table = manifest.as_table().ok_or_else(|| {
        Error::catalog(format!(
            "plugin manifest {} must be a table",
            path.display()
        ))
    })?;
    const FIELDS: &[&str] = &[
        "name",
        "semantic_version",
        "abi_version",
        "module",
        "module_content",
        "result",
        "parameters",
    ];
    reject_unknown_fields(table, FIELDS, "plugin manifest", path)?;

    let name = manifest_string(table, "name", path)?.to_owned();
    let semantic_version = manifest_u32(table, "semantic_version", path)?;
    let abi_version = manifest_u32(table, "abi_version", path)?;
    let module_name = manifest_string(table, "module", path)?;
    let module_content = manifest_string(table, "module_content", path)?
        .parse::<ContentIdentity>()
        .map_err(|error| {
            Error::catalog(format!(
                "plugin manifest {} has invalid module_content: {error}",
                path.display()
            ))
        })?;
    let result = manifest_value_type(manifest_string(table, "result", path)?, path)?;
    let parameters = table
        .get("parameters")
        .ok_or_else(|| manifest_missing("parameters", path))?
        .as_array()
        .ok_or_else(|| {
            Error::catalog(format!(
                "plugin manifest {} field `parameters` must be an array of tables",
                path.display()
            ))
        })?
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let parameter = value.as_table().ok_or_else(|| {
                Error::catalog(format!(
                    "plugin manifest {} parameter {index} must be a table",
                    path.display()
                ))
            })?;
            reject_unknown_fields(parameter, &["name", "type"], "plugin parameter", path)?;
            Ok(PluginParameter {
                name: manifest_string(parameter, "name", path)?.to_owned(),
                value_type: manifest_value_type(manifest_string(parameter, "type", path)?, path)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let module_candidate = path
        .parent()
        .expect("a canonical manifest path has a parent")
        .join(module_name);
    let module_path = confined_plugin_path(workspace_root, &module_candidate, "plugin module")?;
    let module_size = fs::metadata(&module_path)
        .map_err(|error| Error::io("inspect plugin module", &module_path, error))?
        .len();
    if module_size > PLUGIN_MODULE_READ_LIMIT {
        return Err(Error::catalog(format!(
            "plugin module {} is {module_size} bytes; the limit is {PLUGIN_MODULE_READ_LIMIT}",
            module_path.display()
        )));
    }
    let module_bytes = fs::read(&module_path)
        .map_err(|error| Error::io("read plugin module", &module_path, error))?;
    Ok(PluginDefinition {
        name,
        semantic_version,
        abi_version,
        parameters,
        result,
        expected_module_content: module_content,
        module_bytes,
    })
}

fn confined_plugin_path(workspace_root: &Path, candidate: &Path, kind: &str) -> Result<PathBuf> {
    let resolved = fs::canonicalize(candidate)
        .map_err(|error| Error::io("resolve configured plugin path", candidate, error))?;
    if !resolved.starts_with(workspace_root) {
        return Err(Error::catalog(format!(
            "{kind} {} is outside workspace {}",
            resolved.display(),
            workspace_root.display()
        )));
    }
    if !resolved.is_file() {
        return Err(Error::catalog(format!(
            "{kind} {} is not a file",
            resolved.display()
        )));
    }
    Ok(resolved)
}

fn reject_unknown_fields(
    table: &toml::map::Map<String, toml::Value>,
    allowed: &[&str],
    kind: &str,
    path: &Path,
) -> Result<()> {
    if let Some(field) = table
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err(Error::catalog(format!(
            "{kind} {} has unknown field `{field}`",
            path.display()
        )));
    }
    Ok(())
}

fn manifest_string<'a>(
    table: &'a toml::map::Map<String, toml::Value>,
    field: &str,
    path: &Path,
) -> Result<&'a str> {
    table
        .get(field)
        .ok_or_else(|| manifest_missing(field, path))?
        .as_str()
        .ok_or_else(|| {
            Error::catalog(format!(
                "plugin manifest {} field `{field}` must be a string",
                path.display()
            ))
        })
}

fn manifest_u32(
    table: &toml::map::Map<String, toml::Value>,
    field: &str,
    path: &Path,
) -> Result<u32> {
    let value = table
        .get(field)
        .ok_or_else(|| manifest_missing(field, path))?
        .as_integer()
        .ok_or_else(|| {
            Error::catalog(format!(
                "plugin manifest {} field `{field}` must be a non-negative 32-bit integer",
                path.display()
            ))
        })?;
    u32::try_from(value).map_err(|_| {
        Error::catalog(format!(
            "plugin manifest {} field `{field}` must be a non-negative 32-bit integer",
            path.display()
        ))
    })
}

fn manifest_missing(field: &str, path: &Path) -> Error {
    Error::catalog(format!(
        "plugin manifest {} is missing field `{field}`",
        path.display()
    ))
}

fn manifest_value_type(value: &str, path: &Path) -> Result<PluginValueType> {
    match value {
        "bytes" => Ok(PluginValueType::Bytes),
        "buffer" => Ok(PluginValueType::Buffer),
        "i64" => Ok(PluginValueType::I64),
        _ => Err(Error::catalog(format!(
            "plugin manifest {} has unsupported value type {value:?}; expected bytes, buffer, or i64",
            path.display()
        ))),
    }
}

impl Workspace {
    /// Finds the nearest initialized Histima workspace at or above `start`.
    ///
    /// Discovery is read-only. A directory is an initialized workspace when
    /// it contains the catalog created by [`Workspace::open`].
    pub fn find_nearest(start: impl AsRef<Path>) -> Option<PathBuf> {
        start
            .as_ref()
            .ancestors()
            .find(|directory| {
                directory.join(CATALOG_FILE_NAME).is_file()
                    || directory.join(LEGACY_CATALOG_FILE_NAME).is_file()
            })
            .map(Path::to_owned)
    }

    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_owned();
        fs::create_dir_all(&root)
            .map_err(|error| Error::io("create Histima workspace", &root, error))?;
        migrate_legacy_catalog(&root)?;
        let content = ContentStore::open(&root)?;
        let catalog = Catalog::open(&root.join(CATALOG_FILE_NAME))?;
        let world = WorldPolicy::load(&root)?;
        let plugins = load_plugins(&root)?;
        Ok(Self {
            root,
            content,
            catalog,
            world,
            plugins,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the workspace's configured plugin transforms in stable name order.
    pub fn plugins(&self) -> Vec<TransformInfo> {
        self.plugins
            .transform_infos()
            .filter(|transform| transform.origin == TransformOrigin::Workspace)
            .collect()
    }

    /// Returns all process-wide and workspace-configured callable transforms.
    pub fn available_transforms(&self) -> Vec<TransformInfo> {
        tima::registered_transform_infos(&self.plugins)
    }

    /// Verifies durable catalog structure and every cataloged CAS object.
    ///
    /// Verification is read-only and deliberately does not repair or remove
    /// invalid data.
    pub fn verify(&self) -> WorkspaceVerification {
        let mut report = WorkspaceVerification {
            sqlite_valid: true,
            objects_checked: 0,
            objects_valid: 0,
            issues: Vec::new(),
        };
        match self.catalog.sqlite_integrity_issues() {
            Ok(issues) => {
                report.sqlite_valid = issues.is_empty();
                report
                    .issues
                    .extend(issues.into_iter().map(|message| VerificationIssue {
                        kind: VerificationIssueKind::Sqlite,
                        subject: None,
                        message,
                    }));
            }
            Err(error) => {
                report.sqlite_valid = false;
                report.issues.push(VerificationIssue {
                    kind: VerificationIssueKind::Sqlite,
                    subject: None,
                    message: error.to_string(),
                });
            }
        }

        let objects = match self.catalog.all_contents() {
            Ok(objects) => objects,
            Err(error) => {
                report.issues.push(VerificationIssue {
                    kind: VerificationIssueKind::Catalog,
                    subject: None,
                    message: error.to_string(),
                });
                return report;
            }
        };
        for object in objects {
            report.objects_checked += 1;
            match self
                .content
                .read_recorded(&object.content_id, &object.relative_path, object.kind)
            {
                Ok(bytes) if bytes.len() as u64 == object.byte_len => {
                    report.objects_valid += 1;
                }
                Ok(bytes) => report.issues.push(VerificationIssue {
                    kind: VerificationIssueKind::Content,
                    subject: Some(object.content_id.clone()),
                    message: format!(
                        "catalog length is {}, but stored length is {}",
                        object.byte_len,
                        bytes.len()
                    ),
                }),
                Err(error) => report.issues.push(VerificationIssue {
                    kind: VerificationIssueKind::Content,
                    subject: Some(object.content_id),
                    message: error.to_string(),
                }),
            }
        }
        report
    }

    pub fn compile_tima(
        &self,
        name: impl Into<String>,
        text: impl Into<String>,
    ) -> std::result::Result<tima::CompiledProgram, Vec<tima::diagnostic::Diagnostic>> {
        tima::compile_with_plugins(name, text, Arc::clone(&self.plugins))
    }

    fn retain_world_snapshot(
        &self,
        capability: &str,
        key: &[u8],
        bytes: &[u8],
    ) -> std::result::Result<(), String> {
        let content = self.content.put(bytes).map_err(|error| error.to_string())?;
        self.catalog
            .record_world_snapshot(capability, key, &content)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn world_snapshot(
        &self,
        dependency: DependencyIdentity,
    ) -> std::result::Result<Option<Vec<u8>>, String> {
        let Some(object) = self
            .catalog
            .world_snapshot(dependency)
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        let bytes = self
            .content
            .read_recorded(&object.content_id, &object.relative_path, object.kind)
            .map_err(|error| error.to_string())?;
        if bytes.len() as u64 != object.byte_len {
            return Err(format!(
                "snapshot content {} has catalog length {} but stored length {}",
                object.content_id,
                object.byte_len,
                bytes.len()
            ));
        }
        Ok(Some(bytes))
    }

    /// Root for compiled artifacts and future non-semantic execution caches.
    pub fn artifact_cache_root(&self) -> PathBuf {
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

    pub fn assets(&self) -> Result<CatalogPage<AssetSummary>> {
        self.assets_page(CATALOG_LIST_LIMIT, None)
    }

    pub fn assets_page(
        &self,
        limit: usize,
        after: Option<&str>,
    ) -> Result<CatalogPage<AssetSummary>> {
        self.assets_filtered_page(limit, after, None)
    }

    pub fn assets_filtered_page(
        &self,
        limit: usize,
        after: Option<&str>,
        locator_prefix: Option<&str>,
    ) -> Result<CatalogPage<AssetSummary>> {
        self.catalog.assets(limit, after, locator_prefix)
    }

    pub fn recipes(&self) -> Result<CatalogPage<RecipeSummary>> {
        self.recipes_page(CATALOG_LIST_LIMIT, None)
    }

    pub fn recipes_page(
        &self,
        limit: usize,
        after: Option<RecipeIdentity>,
    ) -> Result<CatalogPage<RecipeSummary>> {
        self.recipes_filtered_page(limit, after, None)
    }

    pub fn recipes_filtered_page(
        &self,
        limit: usize,
        after: Option<RecipeIdentity>,
        transform_name: Option<&str>,
    ) -> Result<CatalogPage<RecipeSummary>> {
        self.catalog.recipes(limit, after, transform_name)
    }

    /// Searches current asset locators and recorded transform names.
    ///
    /// Matching is case-sensitive substring matching. `limit` is applied
    /// independently to assets and recipes.
    pub fn search_catalog(&self, query: &str, limit: usize) -> Result<CatalogSearch> {
        self.catalog.search(query, limit)
    }

    /// Returns a bounded, deterministic overview of durable workspace state.
    ///
    /// Assets and recipes retain their normal cursors so callers can continue
    /// through the full catalog with the dedicated listing APIs.
    pub fn summary(&self, limit: usize) -> Result<WorkspaceSummary> {
        let assets = self.assets_page(limit, None)?;
        let recipes = self.recipes_page(limit, None)?;
        let mut transforms = self.available_transforms();
        let transforms_truncated = transforms.len() > limit;
        transforms.truncate(limit);
        Ok(WorkspaceSummary {
            workspace: self.root.clone(),
            catalog: self.catalog_info()?,
            stats: self.catalog_stats()?,
            assets,
            recipes,
            transforms,
            transforms_truncated,
        })
    }

    pub fn inspect_content(&self, identity: ContentIdentity) -> Result<ContentInspection> {
        let object = self
            .catalog
            .content(identity)?
            .ok_or_else(|| Error::ContentNotFound(identity.to_string()))?;
        let (source_references, recipe_references) =
            self.catalog.content_reference_counts(identity)?;
        let validation_error =
            match self
                .content
                .read_recorded(&object.content_id, &object.relative_path, object.kind)
            {
                Ok(bytes) if bytes.len() as u64 == object.byte_len => None,
                Ok(bytes) => Some(format!(
                    "catalog length is {}, but stored length is {}",
                    object.byte_len,
                    bytes.len()
                )),
                Err(error) => Some(error.to_string()),
            };
        Ok(ContentInspection {
            content_id: identity,
            kind: object.kind.as_str().to_owned(),
            byte_len: object.byte_len,
            relative_path: object.relative_path,
            source_references,
            recipe_references,
            valid: validation_error.is_none(),
            validation_error,
        })
    }

    pub fn inspect_recipe(&self, recipe: RecipeIdentity) -> Result<RecipeInspection> {
        let stored = self
            .catalog
            .replay_record(recipe)?
            .ok_or_else(|| Error::RecipeNotFound(recipe.to_string()))?;
        let trace = self
            .catalog
            .trace(recipe)?
            .ok_or_else(|| Error::RecipeNotFound(recipe.to_string()))?;
        if stored.content_id != trace.content_id {
            return Err(Error::catalog(format!(
                "recipe {recipe} has inconsistent result content identities"
            )));
        }
        let content = self.inspect_content(stored.content_id)?;
        Ok(RecipeInspection {
            recipe_id: recipe,
            content_id: stored.content_id,
            lineage: stored.lineage,
            rendered: trace.rendered,
            content,
        })
    }

    /// Renders a durable recipe as one outer-Tima expression. A supplied
    /// starting input replaces the primary first-argument lineage root and
    /// therefore describes a new derivation rather than replaying the recipe.
    pub fn recipe_expression(
        &self,
        recipe: RecipeIdentity,
        starting_input: Option<&str>,
    ) -> Result<String> {
        let stored = self
            .catalog
            .replay_record(recipe)?
            .ok_or_else(|| Error::RecipeNotFound(recipe.to_string()))?;
        stored
            .lineage
            .outer_expression(starting_input)
            .map_err(|error| Error::RecipeNotExpressible {
                identity: recipe.to_string(),
                reason: error.to_string(),
            })
    }

    /// Searches the supplied digest in both the Artifact-ID and bundle-ID
    /// namespaces. Both are canonical 256-bit identities, so the CLI can
    /// inspect either without a separate discriminator.
    pub fn inspect_artifact(&self, identity: ArtifactIdentity) -> Result<ArtifactInspection> {
        let requested_id = identity.to_string();
        let catalog = self.catalog.inspect_artifact(&requested_id)?;
        if catalog.artifact.is_none() && catalog.bundles.items.is_empty() {
            return Err(Error::ArtifactNotFound(requested_id));
        }
        let bundles = catalog
            .bundles
            .items
            .into_iter()
            .map(|bundle| {
                let mut identity_errors = validate_artifact_identities(&bundle);
                let expected_relative_path = match bundle.backend.as_str() {
                    "wasm" => format!("cache/artifacts/wasm/{}/module.wasm", bundle.bundle_id),
                    "c" => format!("cache/native/{}/module.dll", bundle.bundle_id),
                    backend => format!("cache/artifacts/{backend}/{}/module", bundle.bundle_id),
                };
                let mut artifact_errors = Vec::new();
                if bundle.artifact_relative_path != expected_relative_path {
                    artifact_errors.push(format!(
                        "artifact path is {:?}, expected {expected_relative_path:?}",
                        bundle.artifact_relative_path
                    ));
                } else {
                    let artifact_path = self.root.join(Path::new(&bundle.artifact_relative_path));
                    match fs::read(&artifact_path) {
                        Ok(bytes) => {
                            let observed_content = byte_content_identity(&bytes);
                            if observed_content != bundle.artifact_content_id {
                                artifact_errors.push(format!(
                                    "artifact Content ID is {observed_content}, expected {}",
                                    bundle.artifact_content_id
                                ));
                            }
                            if bytes.len() as u64 != bundle.artifact_byte_len {
                                artifact_errors.push(format!(
                                    "artifact byte length is {}, expected {}",
                                    bytes.len(),
                                    bundle.artifact_byte_len
                                ));
                            }
                        }
                        Err(error) => artifact_errors.push(format!(
                            "could not read {}: {error}",
                            artifact_path.display()
                        )),
                    }
                }
                let identity_valid = identity_errors.is_empty();
                let artifact_valid = artifact_errors.is_empty();
                identity_errors.append(&mut artifact_errors);
                Ok(ArtifactBundleInspection {
                    bundle_id: bundle.bundle_id,
                    backend: bundle.backend,
                    backend_version: bundle.backend_version,
                    compiler_version: bundle.compiler_version,
                    target: bundle.target,
                    cpu_features: bundle.cpu_features,
                    optimization: bundle.optimization,
                    abi_version: bundle.abi_version,
                    artifact_content_id: bundle.artifact_content_id,
                    artifact_byte_len: bundle.artifact_byte_len,
                    artifact_relative_path: bundle.artifact_relative_path,
                    members: bundle.members,
                    identity_valid,
                    artifact_valid,
                    valid: identity_valid && artifact_valid,
                    validation_errors: identity_errors,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(ArtifactInspection {
            requested_id,
            artifact: catalog.artifact,
            bundles: CatalogPage {
                items: bundles,
                truncated: catalog.bundles.truncated,
                next_cursor: catalog.bundles.next_cursor,
            },
        })
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

    fn replay_target(&self, recipe: RecipeIdentity) -> Result<OuterValue> {
        let stored = self
            .catalog
            .replay_record(recipe)?
            .ok_or_else(|| Error::RecipeNotFound(recipe.to_string()))?;
        let mut value = self.typed_value(stored.content_id)?.ok_or_else(|| {
            Error::catalog(format!(
                "recipe {recipe} points to unsupported content {}",
                stored.content_id
            ))
        })?;
        value.lineage = Some(stored.lineage);
        Ok(value)
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
        Ok(Some(OuterValue::plain(ValueData::Bytes(Arc::new(bytes)))))
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

fn migrate_legacy_catalog(root: &Path) -> Result<()> {
    let catalog = root.join(CATALOG_FILE_NAME);
    let legacy = root.join(LEGACY_CATALOG_FILE_NAME);
    if catalog.exists() || !legacy.is_file() {
        return Ok(());
    }

    // Fold any committed WAL pages into the main file before renaming it. A
    // successful close also releases/removes SQLite's transient sidecars.
    let connection = rusqlite::Connection::open(&legacy)?;
    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    drop(connection);
    fs::rename(&legacy, &catalog)
        .map_err(|error| Error::io("rename legacy Histima catalog", &legacy, error))
}

impl World for Workspace {
    fn environment(&self, name: &str) -> std::result::Result<Vec<u8>, String> {
        if !self.world.environment.contains(name) {
            return Err(format!(
                "environment value `{name}` is not granted by [world].environment"
            ));
        }
        let bytes = std::env::var(name)
            .map(String::into_bytes)
            .map_err(|error| format!("environment value `{name}` is unavailable: {error}"))?;
        if self.world.retain_environment.contains(name) {
            self.retain_world_snapshot(ENVIRONMENT_CAPABILITY, name.as_bytes(), &bytes)?;
        }
        Ok(bytes)
    }

    fn read_file(&self, path: &str) -> std::result::Result<Vec<u8>, String> {
        let resolved = self.world.resolve_file(&self.root, path)?;
        let bytes = fs::read(&resolved)
            .map_err(|error| format!("could not read {}: {error}", resolved.display()))?;
        self.retain_world_snapshot(FILE_READ_CAPABILITY, path.as_bytes(), &bytes)?;
        Ok(bytes)
    }

    fn http_get(&self, url: &str) -> std::result::Result<Vec<u8>, String> {
        if !self
            .world
            .http_prefixes
            .iter()
            .any(|prefix| url.starts_with(prefix))
        {
            return Err(format!(
                "URL `{url}` is not granted by [world].http_prefixes"
            ));
        }
        let config = ureq::Agent::config_builder()
            .max_redirects(0)
            .timeout_global(Some(std::time::Duration::from_secs(30)))
            .build();
        let agent = ureq::Agent::new_with_config(config);
        let mut response = agent
            .get(url)
            .call()
            .map_err(|error| format!("HTTP GET failed: {error}"))?;
        if response.status().is_redirection() {
            return Err(format!(
                "HTTP redirects are disabled because the redirected URL has not been granted: {}",
                response.status()
            ));
        }
        let bytes = response
            .body_mut()
            .with_config()
            .limit(WORLD_HTTP_READ_LIMIT)
            .read_to_vec()
            .map_err(|error| format!("could not read HTTP response body: {error}"))?;
        self.retain_world_snapshot(HTTP_GET_CAPABILITY, url.as_bytes(), &bytes)?;
        Ok(bytes)
    }

    fn read_asset(&self, locator: &str) -> std::result::Result<Vec<u8>, String> {
        Workspace::read_asset(self, locator).map_err(|error| error.to_string())
    }

    fn write_asset(&self, locator: &str, bytes: &[u8]) -> std::result::Result<(), String> {
        atomic_file::write_new(Path::new(locator), bytes).map_err(|error| error.to_string())
    }
}

impl IdentityPrefixResolver for Workspace {
    fn matching_identities(
        &self,
        domain: IdentityDomain,
        prefix: &str,
    ) -> std::result::Result<Vec<String>, String> {
        self.catalog
            .identity_prefix_matches(domain, prefix)
            .map_err(|error| error.to_string())
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
                schema_version: 6,
                foreign_keys_enabled: true,
                journal_mode: "wal".to_owned(),
            }
        );
        assert_eq!(workspace.catalog_stats().unwrap(), CatalogStats::default());
    }

    #[test]
    fn workspace_identity_prefix_lookup_reports_local_collisions() {
        let test = TestDirectory::new("identity-prefixes");
        let mut workspace = Workspace::open(test.path().join("workspace")).unwrap();
        let mut first_by_prefix = std::collections::BTreeMap::new();
        let (_, prefix) = (0_u8..17)
            .find_map(|value| {
                let imported = workspace
                    .import_bytes(&format!("{value}.bin"), &[value])
                    .unwrap();
                let identity = imported.content_id.to_string();
                let prefix = identity[..1].to_owned();
                first_by_prefix
                    .insert(prefix.clone(), identity)
                    .map(|first| (first, prefix))
            })
            .expect("17 distinct Content IDs collide in one hexadecimal digit");

        let matches = IdentityPrefixResolver::matching_identities(
            &workspace,
            IdentityDomain::Content,
            &prefix,
        )
        .unwrap();
        assert_eq!(matches.len(), 2);
        assert!(matches.iter().all(|identity| identity.starts_with(&prefix)));
    }

    #[test]
    fn workspace_world_confines_files_and_loads_explicit_grants() {
        let test = TestDirectory::new("world-policy");
        let root = test.path().join("workspace");
        let external = test.path().join("shared");
        let denied = test.path().join("denied");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&external).unwrap();
        fs::create_dir_all(&denied).unwrap();
        fs::write(root.join("local.bin"), b"local").unwrap();
        fs::write(external.join("shared.bin"), b"shared").unwrap();
        fs::write(denied.join("secret.bin"), b"secret").unwrap();
        fs::write(
            root.join(".histima.toml"),
            "[world]\n\
             environment = [\"HISTIMA_TEST_MODE\"]\n\
             retain_environment = [\"HISTIMA_TEST_MODE\"]\n\
             file_roots = [\"../shared\"]\n\
             http_prefixes = [\"https://example.test/assets/\"]\n",
        )
        .unwrap();

        let workspace = Workspace::open(&root).unwrap();
        assert!(workspace.world.environment.contains("HISTIMA_TEST_MODE"));
        assert!(
            workspace
                .world
                .retain_environment
                .contains("HISTIMA_TEST_MODE")
        );
        assert_eq!(
            workspace.world.http_prefixes,
            ["https://example.test/assets/"]
        );
        assert_eq!(World::read_file(&workspace, "local.bin").unwrap(), b"local");
        assert_eq!(
            World::read_file(&workspace, external.join("shared.bin").to_str().unwrap()).unwrap(),
            b"shared"
        );
        let error =
            World::read_file(&workspace, denied.join("secret.bin").to_str().unwrap()).unwrap_err();
        assert!(error.contains("outside the workspace and configured file roots"));
        assert!(
            World::environment(&workspace, "UNDECLARED")
                .unwrap_err()
                .contains("not granted")
        );
        assert!(
            World::http_get(&workspace, "https://other.test/assets/a")
                .unwrap_err()
                .contains("not granted")
        );
    }

    #[test]
    fn finds_the_nearest_initialized_workspace_without_creating_one() {
        let test = TestDirectory::new("workspace-discovery");
        let outer = test.path().join("outer");
        let inner = outer.join("projects/inner");
        let nested = inner.join("assets/generated");
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(Workspace::find_nearest(&nested), None);
        drop(Workspace::open(&outer).unwrap());
        assert!(outer.join(CATALOG_FILE_NAME).is_file());
        assert!(!outer.join(LEGACY_CATALOG_FILE_NAME).exists());
        assert_eq!(Workspace::find_nearest(&nested), Some(outer.clone()));
        drop(Workspace::open(&inner).unwrap());
        assert_eq!(Workspace::find_nearest(&nested), Some(inner));
    }

    #[test]
    fn opening_a_legacy_workspace_renames_its_catalog_without_losing_data() {
        let test = TestDirectory::new("legacy-catalog-name");
        let root = test.path().join("workspace");
        let nested = root.join("assets");
        let mut workspace = Workspace::open(&root).unwrap();
        workspace.import_bytes("old.bin", b"preserved").unwrap();
        drop(workspace);

        fs::rename(
            root.join(CATALOG_FILE_NAME),
            root.join(LEGACY_CATALOG_FILE_NAME),
        )
        .unwrap();
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(Workspace::find_nearest(&nested), Some(root.clone()));

        let reopened = Workspace::open(&root).unwrap();
        assert_eq!(reopened.catalog_stats().unwrap().contents, 1);
        assert!(root.join(CATALOG_FILE_NAME).is_file());
        assert!(!root.join(LEGACY_CATALOG_FILE_NAME).exists());
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
                artifact_bundles: 0,
                artifacts: 0,
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
                artifact_bundles: 0,
                artifacts: 0,
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
    fn catalog_asset_listing_is_ordered_and_bounded() {
        let test = TestDirectory::new("asset-list");
        let mut workspace = Workspace::open(test.path().join("workspace")).unwrap();
        for index in (0..=CATALOG_LIST_LIMIT).rev() {
            workspace
                .import_bytes(&format!("asset-{index:03}"), b"shared")
                .unwrap();
        }

        let page = workspace.assets().unwrap();

        assert_eq!(page.items.len(), CATALOG_LIST_LIMIT);
        assert!(page.truncated);
        assert_eq!(page.next_cursor.as_deref(), Some("asset-099"));
        assert_eq!(page.items.first().unwrap().locator, "asset-000");
        assert_eq!(page.items.last().unwrap().locator, "asset-099");
        let remainder = workspace
            .assets_page(10, page.next_cursor.as_deref())
            .unwrap();
        assert_eq!(remainder.items.len(), 1);
        assert!(!remainder.truncated);
        assert_eq!(remainder.next_cursor, None);
        assert_eq!(remainder.items[0].locator, "asset-100");
        let inspection = workspace.inspect_content(page.items[0].content_id).unwrap();
        assert!(inspection.valid);
        assert_eq!(inspection.kind, "raw");
        assert_eq!(inspection.source_references, 101);
        assert_eq!(inspection.recipe_references, 0);
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
        let inspection = workspace.inspect_content(imported.content_id).unwrap();
        assert!(!inspection.valid);
        assert!(
            inspection
                .validation_error
                .unwrap()
                .contains("content integrity failure")
        );
        let error = workspace.import_file(&input).unwrap_err();
        assert!(matches!(error, Error::Integrity { .. }));
    }

    #[test]
    fn workspace_verification_checks_recorded_content_lengths() {
        let test = TestDirectory::new("verify-length");
        let root = test.path().join("workspace");
        let mut workspace = Workspace::open(&root).unwrap();
        let imported = workspace.import_bytes("asset", b"stored bytes").unwrap();
        drop(workspace);

        let connection = rusqlite::Connection::open(root.join(CATALOG_FILE_NAME)).unwrap();
        connection
            .execute(
                "UPDATE contents SET byte_length = byte_length + 1 WHERE content_id = ?1",
                [imported.content_id.to_string()],
            )
            .unwrap();
        drop(connection);

        let workspace = Workspace::open(root).unwrap();
        let report = workspace.verify();
        assert!(!report.is_valid());
        assert!(report.sqlite_valid);
        assert_eq!(report.objects_checked, 1);
        assert_eq!(report.objects_valid, 0);
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].kind, VerificationIssueKind::Content);
        let content_id = imported.content_id.to_string();
        assert_eq!(
            report.issues[0].subject.as_deref(),
            Some(content_id.as_str())
        );
        assert!(report.issues[0].message.contains("catalog length"));
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

        World::write_asset(&workspace, output.to_str().unwrap(), b"first").unwrap();
        let error =
            World::write_asset(&workspace, output.to_str().unwrap(), b"replacement").unwrap_err();

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

        let first = OuterValue::plain(ValueData::Bytes(Arc::new(b"first".to_vec())))
            .with_lineage(lineage.clone());
        let conflicting =
            OuterValue::plain(ValueData::Bytes(Arc::new(b"second".to_vec()))).with_lineage(lineage);
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
