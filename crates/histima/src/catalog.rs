use std::fs;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use tima::backend::wasm_runtime::CachedWasmArtifact;
use tima::identity::{
    ArtifactBundleIdentity, ArtifactConfiguration, ArtifactIdentity, ContentIdentity,
    RecipeIdentity, SemanticValueIdentity, SourceIdentity, TransformIdentity,
    artifact_bundle_identity, artifact_identity, byte_content_identity,
};
use tima::lineage::{Lineage, LineageArgument, LineageNode, RecordedValue};

use crate::cas::{ContentKind, StoredContent};
use crate::error::{Error, Result};
use crate::stored_lineage::StoredRecipe;

const LATEST_SCHEMA_VERSION: i64 = 4;
pub const CATALOG_LIST_LIMIT: usize = 100;

const MIGRATION_1: &str = r#"
CREATE TABLE contents (
    content_id   TEXT PRIMARY KEY CHECK (length(content_id) = 64),
    byte_length  INTEGER NOT NULL CHECK (byte_length >= 0),
    relative_path TEXT NOT NULL UNIQUE
) STRICT;

CREATE TABLE source_assets (
    source_id  TEXT PRIMARY KEY CHECK (length(source_id) = 64),
    locator    TEXT NOT NULL,
    content_id TEXT NOT NULL REFERENCES contents(content_id) ON DELETE RESTRICT,
    UNIQUE (locator, content_id),
    UNIQUE (locator, source_id)
) STRICT;

CREATE INDEX source_assets_locator_idx ON source_assets(locator);
CREATE INDEX source_assets_content_idx ON source_assets(content_id);

CREATE TABLE source_heads (
    locator   TEXT PRIMARY KEY,
    source_id TEXT NOT NULL,
    FOREIGN KEY (locator, source_id)
        REFERENCES source_assets(locator, source_id)
        ON DELETE RESTRICT
) STRICT;
"#;

const MIGRATION_2: &str = r#"
ALTER TABLE contents
    ADD COLUMN kind TEXT NOT NULL DEFAULT 'raw'
    CHECK (kind IN ('raw', 'bytes'));

CREATE TABLE lineage_invocations (
    recipe_id     TEXT PRIMARY KEY CHECK (length(recipe_id) = 64),
    transform_id  TEXT NOT NULL CHECK (length(transform_id) = 64),
    transform_name TEXT NOT NULL
) STRICT;

CREATE TABLE lineage_arguments (
    recipe_id            TEXT NOT NULL REFERENCES lineage_invocations(recipe_id) ON DELETE CASCADE,
    argument_index       INTEGER NOT NULL CHECK (argument_index >= 0),
    argument_name        TEXT NOT NULL,
    semantic_kind        TEXT NOT NULL CHECK (semantic_kind IN ('content', 'source', 'recipe')),
    semantic_id          TEXT NOT NULL CHECK (length(semantic_id) = 64),
    recorded_kind        TEXT NOT NULL,
    recorded_text        TEXT,
    recorded_content_id  TEXT,
    parent_kind          TEXT CHECK (parent_kind IN ('source', 'recipe')),
    parent_id            TEXT,
    PRIMARY KEY (recipe_id, argument_index),
    CHECK ((parent_kind IS NULL) = (parent_id IS NULL)),
    CHECK (recorded_content_id IS NULL OR length(recorded_content_id) = 64),
    CHECK (parent_id IS NULL OR length(parent_id) = 64)
) STRICT;

CREATE TABLE external_observations (
    dependency_id      TEXT PRIMARY KEY CHECK (length(dependency_id) = 64),
    capability         TEXT NOT NULL,
    observation_key    BLOB NOT NULL,
    observed_content_id TEXT NOT NULL CHECK (length(observed_content_id) = 64)
) STRICT;

CREATE TABLE invocation_observations (
    recipe_id       TEXT NOT NULL REFERENCES lineage_invocations(recipe_id) ON DELETE CASCADE,
    observation_index INTEGER NOT NULL CHECK (observation_index >= 0),
    dependency_id   TEXT NOT NULL REFERENCES external_observations(dependency_id) ON DELETE RESTRICT,
    PRIMARY KEY (recipe_id, observation_index),
    UNIQUE (recipe_id, dependency_id)
) STRICT;

CREATE TABLE recipe_results (
    recipe_id      TEXT PRIMARY KEY REFERENCES lineage_invocations(recipe_id) ON DELETE RESTRICT,
    content_id     TEXT NOT NULL REFERENCES contents(content_id) ON DELETE RESTRICT,
    rendered_trace TEXT NOT NULL
) STRICT;

CREATE INDEX recipe_results_content_idx ON recipe_results(content_id);
"#;

const MIGRATION_3: &str = r#"
CREATE TABLE native_artifact_bundles (
    bundle_id             TEXT PRIMARY KEY CHECK (length(bundle_id) = 64),
    backend               TEXT NOT NULL,
    backend_version       TEXT NOT NULL,
    compiler_version      TEXT NOT NULL,
    target                TEXT NOT NULL,
    cpu_features          TEXT NOT NULL,
    optimization          TEXT NOT NULL,
    abi_version           INTEGER NOT NULL CHECK (abi_version >= 0),
    library_content_id    TEXT NOT NULL CHECK (length(library_content_id) = 64),
    library_byte_length   INTEGER NOT NULL CHECK (library_byte_length >= 0),
    library_relative_path TEXT NOT NULL UNIQUE
) STRICT;

CREATE TABLE native_artifacts (
    artifact_id  TEXT PRIMARY KEY CHECK (length(artifact_id) = 64),
    transform_id TEXT NOT NULL CHECK (length(transform_id) = 64)
) STRICT;

CREATE TABLE native_artifact_bundle_members (
    bundle_id     TEXT NOT NULL REFERENCES native_artifact_bundles(bundle_id) ON DELETE CASCADE,
    artifact_index INTEGER NOT NULL CHECK (artifact_index >= 0),
    artifact_id   TEXT NOT NULL REFERENCES native_artifacts(artifact_id) ON DELETE RESTRICT,
    PRIMARY KEY (bundle_id, artifact_index)
) STRICT;

CREATE INDEX native_artifact_members_artifact_idx
    ON native_artifact_bundle_members(artifact_id);
"#;

const MIGRATION_4: &str = r#"
ALTER TABLE native_artifact_bundles RENAME TO artifact_bundles;
ALTER TABLE artifact_bundles RENAME COLUMN library_content_id TO artifact_content_id;
ALTER TABLE artifact_bundles RENAME COLUMN library_byte_length TO artifact_byte_length;
ALTER TABLE artifact_bundles RENAME COLUMN library_relative_path TO artifact_relative_path;
ALTER TABLE native_artifacts RENAME TO artifacts;
ALTER TABLE native_artifact_bundle_members RENAME TO artifact_bundle_members;
DROP INDEX native_artifact_members_artifact_idx;
CREATE INDEX artifact_members_artifact_idx ON artifact_bundle_members(artifact_id);
"#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogInfo {
    pub schema_version: u32,
    pub foreign_keys_enabled: bool,
    pub journal_mode: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CatalogStats {
    pub contents: u64,
    pub source_versions: u64,
    pub source_heads: u64,
    pub lineage_invocations: u64,
    pub recipe_results: u64,
    pub artifact_bundles: u64,
    pub artifacts: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactInfo {
    pub bundle_id: ArtifactBundleIdentity,
    pub artifact_ids: Vec<ArtifactIdentity>,
    pub artifact_content_id: ContentIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogPage<T> {
    pub items: Vec<T>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetSummary {
    pub locator: String,
    pub source_id: SourceIdentity,
    pub content_id: ContentIdentity,
    pub byte_len: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecipeSummary {
    pub recipe_id: RecipeIdentity,
    pub transform_id: TransformIdentity,
    pub transform_name: String,
    pub content_id: ContentIdentity,
    pub byte_len: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactSummary {
    pub artifact_id: ArtifactIdentity,
    pub transform_id: TransformIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactBundleMember {
    pub index: u32,
    pub artifact_id: ArtifactIdentity,
    pub transform_id: TransformIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CatalogArtifactBundle {
    pub bundle_id: ArtifactBundleIdentity,
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CatalogArtifactInspection {
    pub artifact: Option<ArtifactSummary>,
    pub bundles: CatalogPage<CatalogArtifactBundle>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CatalogObject {
    pub content_id: String,
    pub byte_len: u64,
    pub relative_path: String,
    pub kind: ContentKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CatalogTrace {
    pub recipe_id: RecipeIdentity,
    pub content_id: ContentIdentity,
    pub rendered: String,
}

pub(crate) struct Catalog {
    connection: Connection,
}

impl Catalog {
    pub fn open(path: &Path) -> Result<Self> {
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        let journal_mode =
            connection.pragma_update_and_check(None, "journal_mode", "WAL", |row| {
                row.get::<_, String>(0)
            })?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            return Err(Error::catalog(format!(
                "SQLite refused WAL journal mode and selected {journal_mode:?}"
            )));
        }
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        apply_migrations(&mut connection)?;
        Ok(Self { connection })
    }

    pub fn record_import(
        &mut self,
        locator: &str,
        source_id: SourceIdentity,
        content: &StoredContent,
    ) -> Result<()> {
        if content.kind != ContentKind::Raw {
            return Err(Error::catalog(
                "source imports must use raw content identity",
            ));
        }
        let content_id = content.identity.to_string();
        let source_id = source_id.to_string();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        insert_content(&transaction, content)?;

        transaction.execute(
            "INSERT OR IGNORE INTO source_assets(source_id, locator, content_id)
             VALUES (?1, ?2, ?3)",
            params![source_id, locator, content_id],
        )?;
        let recorded_source = transaction.query_row(
            "SELECT locator, content_id FROM source_assets WHERE source_id = ?1",
            [&source_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        if recorded_source != (locator.to_owned(), content_id) {
            return Err(Error::catalog(format!(
                "source {source_id} already has incompatible metadata"
            )));
        }

        transaction.execute(
            "INSERT INTO source_heads(locator, source_id) VALUES (?1, ?2)
             ON CONFLICT(locator) DO UPDATE SET source_id = excluded.source_id",
            params![locator, source_id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn current_source(&self, locator: &str) -> Result<Option<CatalogObject>> {
        self.connection
            .query_row(
                "SELECT content.content_id, content.byte_length, content.relative_path, content.kind
                 FROM source_heads AS head
                 JOIN source_assets AS source
                   ON source.locator = head.locator AND source.source_id = head.source_id
                 JOIN contents AS content ON content.content_id = source.content_id
                 WHERE head.locator = ?1",
                [locator],
                |row| {
                    let byte_len = row.get::<_, i64>(1)?;
                    Ok((
                        row.get::<_, String>(0)?,
                        byte_len,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?
            .map(|(content_id, byte_len, relative_path, kind)| {
                let byte_len = u64::try_from(byte_len).map_err(|_| {
                    Error::catalog(format!(
                        "content {content_id} has a negative byte length in the catalog"
                    ))
                })?;
                let kind = ContentKind::parse(&kind)?;
                if kind != ContentKind::Raw {
                    return Err(Error::catalog(format!(
                        "source locator {locator:?} points to non-raw content {content_id}"
                    )));
                }
                Ok(CatalogObject {
                    content_id,
                    byte_len,
                    relative_path,
                    kind,
                })
            })
            .transpose()
    }

    pub fn content(&self, identity: ContentIdentity) -> Result<Option<CatalogObject>> {
        let identity = identity.to_string();
        self.connection
            .query_row(
                "SELECT content_id, byte_length, relative_path, kind
                 FROM contents WHERE content_id = ?1",
                [&identity],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?
            .map(|(content_id, byte_len, relative_path, kind)| {
                let byte_len = u64::try_from(byte_len).map_err(|_| {
                    Error::catalog(format!(
                        "content {content_id} has a negative byte length in the catalog"
                    ))
                })?;
                Ok(CatalogObject {
                    content_id,
                    byte_len,
                    relative_path,
                    kind: ContentKind::parse(&kind)?,
                })
            })
            .transpose()
    }

    pub fn assets(&self) -> Result<CatalogPage<AssetSummary>> {
        let mut statement = self.connection.prepare(
            "SELECT head.locator, head.source_id, source.content_id, content.byte_length
             FROM source_heads AS head
             JOIN source_assets AS source
               ON source.locator = head.locator AND source.source_id = head.source_id
             JOIN contents AS content ON content.content_id = source.content_id
             ORDER BY head.locator
             LIMIT ?1",
        )?;
        let limit = i64::try_from(CATALOG_LIST_LIMIT + 1)
            .map_err(|_| Error::catalog("catalog listing limit does not fit SQLite INTEGER"))?;
        let rows = statement
            .query_map([limit], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut items = rows
            .into_iter()
            .map(|(locator, source_id, content_id, byte_len)| {
                let source_id = source_id.parse::<SourceIdentity>().map_err(|error| {
                    Error::catalog(format!(
                        "asset locator {locator:?} has invalid Source ID {source_id:?}: {error}"
                    ))
                })?;
                let content_id = content_id.parse::<ContentIdentity>().map_err(|error| {
                    Error::catalog(format!(
                        "asset locator {locator:?} has invalid Content ID {content_id:?}: {error}"
                    ))
                })?;
                let byte_len = u64::try_from(byte_len).map_err(|_| {
                    Error::catalog(format!(
                        "content {content_id} has a negative byte length in the catalog"
                    ))
                })?;
                Ok(AssetSummary {
                    locator,
                    source_id,
                    content_id,
                    byte_len,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(bounded_page(&mut items))
    }

    pub fn recipes(&self) -> Result<CatalogPage<RecipeSummary>> {
        let mut statement = self.connection.prepare(
            "SELECT result.recipe_id, invocation.transform_id,
                    invocation.transform_name, result.content_id, content.byte_length
             FROM recipe_results AS result
             JOIN lineage_invocations AS invocation USING (recipe_id)
             JOIN contents AS content ON content.content_id = result.content_id
             ORDER BY result.recipe_id
             LIMIT ?1",
        )?;
        let limit = i64::try_from(CATALOG_LIST_LIMIT + 1)
            .map_err(|_| Error::catalog("catalog listing limit does not fit SQLite INTEGER"))?;
        let rows = statement
            .query_map([limit], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut items = rows
            .into_iter()
            .map(
                |(recipe_id, transform_id, transform_name, content_id, byte_len)| {
                    let recipe_id = recipe_id.parse::<RecipeIdentity>().map_err(|error| {
                        Error::catalog(format!(
                            "catalog has invalid Recipe ID {recipe_id:?}: {error}"
                        ))
                    })?;
                    let transform_id =
                        transform_id
                            .parse::<TransformIdentity>()
                            .map_err(|error| {
                                Error::catalog(format!(
                                    "recipe {recipe_id} has invalid Transform ID {transform_id:?}: {error}"
                                ))
                            })?;
                    let content_id = content_id.parse::<ContentIdentity>().map_err(|error| {
                        Error::catalog(format!(
                            "recipe {recipe_id} has invalid Content ID {content_id:?}: {error}"
                        ))
                    })?;
                    let byte_len = u64::try_from(byte_len).map_err(|_| {
                        Error::catalog(format!(
                            "content {content_id} has a negative byte length in the catalog"
                        ))
                    })?;
                    Ok(RecipeSummary {
                        recipe_id,
                        transform_id,
                        transform_name,
                        content_id,
                        byte_len,
                    })
                },
            )
            .collect::<Result<Vec<_>>>()?;
        Ok(bounded_page(&mut items))
    }

    pub fn content_reference_counts(&self, identity: ContentIdentity) -> Result<(u64, u64)> {
        let identity = identity.to_string();
        let sources = self.connection.query_row(
            "SELECT COUNT(*) FROM source_assets WHERE content_id = ?1",
            [&identity],
            |row| row.get::<_, i64>(0),
        )?;
        let recipes = self.connection.query_row(
            "SELECT COUNT(*) FROM recipe_results WHERE content_id = ?1",
            [&identity],
            |row| row.get::<_, i64>(0),
        )?;
        let sources = u64::try_from(sources)
            .map_err(|_| Error::catalog("source reference count is negative"))?;
        let recipes = u64::try_from(recipes)
            .map_err(|_| Error::catalog("recipe reference count is negative"))?;
        Ok((sources, recipes))
    }

    pub fn inspect_artifact(&self, identity: &str) -> Result<CatalogArtifactInspection> {
        let artifact = self
            .connection
            .query_row(
                "SELECT artifact_id, transform_id
                 FROM artifacts WHERE artifact_id = ?1",
                [identity],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .map(|(artifact_id, transform_id)| -> Result<ArtifactSummary> {
                let artifact_id = artifact_id.parse::<ArtifactIdentity>().map_err(|error| {
                    Error::catalog(format!(
                        "catalog has invalid Artifact ID {artifact_id:?}: {error}"
                    ))
                })?;
                let transform_id = transform_id.parse::<TransformIdentity>().map_err(|error| {
                    Error::catalog(format!(
                        "artifact {artifact_id} has invalid Transform ID {transform_id:?}: {error}"
                    ))
                })?;
                Ok(ArtifactSummary {
                    artifact_id,
                    transform_id,
                })
            })
            .transpose()?;

        let limit = i64::try_from(CATALOG_LIST_LIMIT + 1)
            .map_err(|_| Error::catalog("catalog listing limit does not fit SQLite INTEGER"))?;
        let mut statement = self.connection.prepare(
            "SELECT bundle_id
             FROM (
                 SELECT bundle_id FROM artifact_bundles WHERE bundle_id = ?1
                 UNION
                 SELECT bundle_id FROM artifact_bundle_members WHERE artifact_id = ?1
             )
             ORDER BY bundle_id = ?1 DESC, bundle_id
             LIMIT ?2",
        )?;
        let bundle_ids = statement
            .query_map(params![identity, limit], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut bundles = bundle_ids
            .into_iter()
            .map(|bundle_id| load_artifact_bundle(&self.connection, &bundle_id))
            .collect::<Result<Vec<_>>>()?;
        Ok(CatalogArtifactInspection {
            artifact,
            bundles: bounded_page(&mut bundles),
        })
    }

    pub fn record_result(
        &mut self,
        lineage: &Lineage,
        content: &StoredContent,
    ) -> Result<CatalogTrace> {
        let LineageNode::Invocation(root) = lineage.node() else {
            return Err(Error::catalog(
                "a durable recipe result must have invocation lineage",
            ));
        };
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        insert_content(&transaction, content)?;
        insert_lineage(&transaction, lineage)?;
        let recipe_id = root.recipe_id.to_string();
        let content_id = content.identity.to_string();
        let rendered = lineage.render();
        transaction.execute(
            "INSERT OR IGNORE INTO recipe_results(recipe_id, content_id, rendered_trace)
             VALUES (?1, ?2, ?3)",
            params![recipe_id, content_id, rendered],
        )?;
        let recorded = transaction.query_row(
            "SELECT content_id, rendered_trace FROM recipe_results WHERE recipe_id = ?1",
            [&recipe_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        if recorded.0 != content_id {
            return Err(Error::catalog(format!(
                "recipe {recipe_id} already maps to content {}, not {content_id}",
                recorded.0
            )));
        }
        transaction.commit()?;
        Ok(CatalogTrace {
            recipe_id: root.recipe_id,
            content_id: content.identity,
            rendered: recorded.1,
        })
    }

    pub fn trace(&self, recipe: RecipeIdentity) -> Result<Option<CatalogTrace>> {
        let recipe_text = recipe.to_string();
        self.connection
            .query_row(
                "SELECT content_id, rendered_trace FROM recipe_results WHERE recipe_id = ?1",
                [&recipe_text],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .map(|(content_id, rendered)| {
                let content_id = content_id.parse::<ContentIdentity>().map_err(|error| {
                    Error::catalog(format!(
                        "recipe {recipe} has invalid Content ID {content_id:?}: {error}"
                    ))
                })?;
                Ok(CatalogTrace {
                    recipe_id: recipe,
                    content_id,
                    rendered,
                })
            })
            .transpose()
    }

    pub fn replay_record(&self, recipe: RecipeIdentity) -> Result<Option<StoredRecipe>> {
        crate::stored_lineage::load(&self.connection, recipe)
    }

    pub fn record_artifact(
        &self,
        cached: &CachedWasmArtifact,
        transforms: &[TransformIdentity],
        workspace_root: &Path,
    ) -> Result<ArtifactInfo> {
        if cached.artifact_ids.len() != transforms.len() {
            return Err(Error::catalog(format!(
                "artifact bundle {} has {} Artifact IDs for {} transforms",
                cached.bundle_id,
                cached.artifact_ids.len(),
                transforms.len()
            )));
        }
        for (index, (artifact_id, transform_id)) in cached
            .artifact_ids
            .iter()
            .zip(transforms.iter())
            .enumerate()
        {
            let expected = cached.artifact.identity(*transform_id);
            if *artifact_id != expected {
                return Err(Error::catalog(format!(
                    "artifact bundle {} Artifact ID at index {index} is {artifact_id}, expected {expected}",
                    cached.bundle_id
                )));
            }
        }
        let expected_bundle = artifact_bundle_identity(&cached.artifact_ids);
        if cached.bundle_id != expected_bundle {
            return Err(Error::catalog(format!(
                "artifact bundle ID {} rebuilds as {expected_bundle}",
                cached.bundle_id
            )));
        }

        let artifact_relative_path =
            format!("cache/artifacts/wasm/{}/module.wasm", cached.bundle_id);
        let expected_artifact = workspace_root.join(Path::new(&artifact_relative_path));
        if cached.artifact.module_path != expected_artifact {
            return Err(Error::catalog(format!(
                "artifact bundle {} is outside its expected workspace cache location",
                cached.bundle_id
            )));
        }
        let artifact_bytes = fs::read(&expected_artifact)
            .map_err(|error| Error::io("read cached Wasm artifact", &expected_artifact, error))?;
        let artifact_content_id = byte_content_identity(&artifact_bytes);
        let artifact_byte_length = i64::try_from(artifact_bytes.len())
            .map_err(|_| Error::catalog("artifact byte length does not fit SQLite INTEGER"))?;
        let abi_version = i64::from(cached.artifact.abi_version);
        // The current compiler targets the baseline architecture. This field
        // stays explicit so adding selected CPU features changes metadata and
        // Artifact identity together later.
        let cpu_features = "";
        let bundle_id = cached.bundle_id.to_string();
        let expected_metadata = (
            cached.artifact.backend.to_owned(),
            cached.artifact.backend_version.to_owned(),
            cached.artifact.compiler_version.to_owned(),
            cached.artifact.target.to_owned(),
            cpu_features.to_owned(),
            cached.artifact.optimization.to_owned(),
            abi_version,
            artifact_content_id.to_string(),
            artifact_byte_length,
            artifact_relative_path.clone(),
        );

        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute(
            "INSERT OR IGNORE INTO artifact_bundles(
                 bundle_id, backend, backend_version, compiler_version, target,
                 cpu_features, optimization, abi_version, artifact_content_id,
                 artifact_byte_length, artifact_relative_path
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                bundle_id,
                expected_metadata.0,
                expected_metadata.1,
                expected_metadata.2,
                expected_metadata.3,
                expected_metadata.4,
                expected_metadata.5,
                expected_metadata.6,
                expected_metadata.7,
                expected_metadata.8,
                expected_metadata.9,
            ],
        )?;
        let recorded_metadata = transaction.query_row(
            "SELECT backend, backend_version, compiler_version, target,
                    cpu_features, optimization, abi_version, artifact_content_id,
                    artifact_byte_length, artifact_relative_path
             FROM artifact_bundles WHERE bundle_id = ?1",
            [&bundle_id],
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
                    row.get::<_, i64>(8)?,
                    row.get::<_, String>(9)?,
                ))
            },
        )?;
        if recorded_metadata != expected_metadata {
            return Err(Error::catalog(format!(
                "artifact bundle {} has catalog metadata that does not match the on-disk cache",
                cached.bundle_id
            )));
        }

        for (index, (artifact_id, transform_id)) in cached
            .artifact_ids
            .iter()
            .zip(transforms.iter())
            .enumerate()
        {
            let artifact_id = artifact_id.to_string();
            let transform_id = transform_id.to_string();
            transaction.execute(
                "INSERT OR IGNORE INTO artifacts(artifact_id, transform_id)
                 VALUES (?1, ?2)",
                params![artifact_id, transform_id],
            )?;
            let recorded_transform = transaction.query_row(
                "SELECT transform_id FROM artifacts WHERE artifact_id = ?1",
                [&artifact_id],
                |row| row.get::<_, String>(0),
            )?;
            if recorded_transform != transform_id {
                return Err(Error::catalog(format!(
                    "Artifact ID {artifact_id} is already associated with a different Transform ID"
                )));
            }
            let index = i64::try_from(index)
                .map_err(|_| Error::catalog("artifact index exceeds SQLite INTEGER"))?;
            transaction.execute(
                "INSERT OR IGNORE INTO artifact_bundle_members(
                     bundle_id, artifact_index, artifact_id
                 ) VALUES (?1, ?2, ?3)",
                params![bundle_id, index, artifact_id],
            )?;
            let recorded_artifact = transaction.query_row(
                "SELECT artifact_id FROM artifact_bundle_members
                 WHERE bundle_id = ?1 AND artifact_index = ?2",
                params![bundle_id, index],
                |row| row.get::<_, String>(0),
            )?;
            if recorded_artifact != artifact_id {
                return Err(Error::catalog(format!(
                    "artifact bundle {} has a different Artifact ID at index {index}",
                    cached.bundle_id
                )));
            }
        }
        let member_count = transaction.query_row(
            "SELECT COUNT(*) FROM artifact_bundle_members WHERE bundle_id = ?1",
            [&bundle_id],
            |row| row.get::<_, i64>(0),
        )?;
        if member_count != i64::try_from(cached.artifact_ids.len()).unwrap_or(i64::MAX) {
            return Err(Error::catalog(format!(
                "artifact bundle {} has {member_count} recorded members, expected {}",
                cached.bundle_id,
                cached.artifact_ids.len()
            )));
        }
        transaction.commit()?;
        Ok(ArtifactInfo {
            bundle_id: cached.bundle_id,
            artifact_ids: cached.artifact_ids.clone(),
            artifact_content_id,
        })
    }

    pub fn info(&self) -> Result<CatalogInfo> {
        let schema_version = current_schema_version(&self.connection)?;
        let schema_version = u32::try_from(schema_version)
            .map_err(|_| Error::catalog("schema version does not fit u32"))?;
        let foreign_keys_enabled =
            self.connection
                .pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))?;
        let journal_mode = self
            .connection
            .pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))?;
        Ok(CatalogInfo {
            schema_version,
            foreign_keys_enabled,
            journal_mode,
        })
    }

    pub fn stats(&self) -> Result<CatalogStats> {
        Ok(CatalogStats {
            contents: count(&self.connection, "contents")?,
            source_versions: count(&self.connection, "source_assets")?,
            source_heads: count(&self.connection, "source_heads")?,
            lineage_invocations: count(&self.connection, "lineage_invocations")?,
            recipe_results: count(&self.connection, "recipe_results")?,
            artifact_bundles: count(&self.connection, "artifact_bundles")?,
            artifacts: count(&self.connection, "artifacts")?,
        })
    }
}

fn bounded_page<T>(items: &mut Vec<T>) -> CatalogPage<T> {
    let truncated = items.len() > CATALOG_LIST_LIMIT;
    items.truncate(CATALOG_LIST_LIMIT);
    CatalogPage {
        items: std::mem::take(items),
        truncated,
    }
}

fn load_artifact_bundle(
    connection: &Connection,
    bundle_identity: &str,
) -> Result<CatalogArtifactBundle> {
    let metadata = connection.query_row(
        "SELECT bundle_id, backend, backend_version, compiler_version, target,
                cpu_features, optimization, abi_version, artifact_content_id,
                artifact_byte_length, artifact_relative_path
         FROM artifact_bundles WHERE bundle_id = ?1",
        [bundle_identity],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, String>(10)?,
            ))
        },
    )?;
    let bundle_id = metadata
        .0
        .parse::<ArtifactBundleIdentity>()
        .map_err(|error| {
            Error::catalog(format!(
                "catalog has invalid artifact bundle ID {:?}: {error}",
                metadata.0
            ))
        })?;
    let abi_version = u32::try_from(metadata.7).map_err(|_| {
        Error::catalog(format!(
            "artifact bundle {bundle_id} has invalid ABI version {}",
            metadata.7
        ))
    })?;
    let artifact_content_id = metadata.8.parse::<ContentIdentity>().map_err(|error| {
        Error::catalog(format!(
            "artifact bundle {bundle_id} has invalid artifact Content ID {:?}: {error}",
            metadata.8
        ))
    })?;
    let artifact_byte_len = u64::try_from(metadata.9).map_err(|_| {
        Error::catalog(format!(
            "artifact bundle {bundle_id} has a negative artifact byte length"
        ))
    })?;

    let mut statement = connection.prepare(
        "SELECT member.artifact_index, member.artifact_id, artifact.transform_id
         FROM artifact_bundle_members AS member
         JOIN artifacts AS artifact USING (artifact_id)
         WHERE member.bundle_id = ?1
         ORDER BY member.artifact_index",
    )?;
    let member_rows = statement
        .query_map([bundle_identity], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let members = member_rows
        .into_iter()
        .map(|(index, artifact_id, transform_id)| {
            let index = u32::try_from(index).map_err(|_| {
                Error::catalog(format!(
                    "artifact bundle {bundle_id} has invalid member index {index}"
                ))
            })?;
            let artifact_id = artifact_id.parse::<ArtifactIdentity>().map_err(|error| {
                Error::catalog(format!(
                    "artifact bundle {bundle_id} has invalid Artifact ID {artifact_id:?}: {error}"
                ))
            })?;
            let transform_id = transform_id.parse::<TransformIdentity>().map_err(|error| {
                Error::catalog(format!(
                    "artifact {artifact_id} has invalid Transform ID {transform_id:?}: {error}"
                ))
            })?;
            Ok(ArtifactBundleMember {
                index,
                artifact_id,
                transform_id,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    for (expected, member) in members.iter().enumerate() {
        if usize::try_from(member.index).ok() != Some(expected) {
            return Err(Error::catalog(format!(
                "artifact bundle {bundle_id} member indices are not contiguous"
            )));
        }
    }

    Ok(CatalogArtifactBundle {
        bundle_id,
        backend: metadata.1,
        backend_version: metadata.2,
        compiler_version: metadata.3,
        target: metadata.4,
        cpu_features: metadata.5,
        optimization: metadata.6,
        abi_version,
        artifact_content_id,
        artifact_byte_len,
        artifact_relative_path: metadata.10,
        members,
    })
}

pub(crate) fn validate_artifact_identities(bundle: &CatalogArtifactBundle) -> Vec<String> {
    let mut errors = Vec::new();
    if bundle.cpu_features.is_empty() {
        let configuration = ArtifactConfiguration {
            backend: &bundle.backend,
            backend_version: &bundle.backend_version,
            compiler_version: &bundle.compiler_version,
            target: &bundle.target,
            cpu_features: &[],
            optimization: &bundle.optimization,
            abi_version: bundle.abi_version,
        };
        for member in &bundle.members {
            let expected = artifact_identity(member.transform_id, &configuration);
            if member.artifact_id != expected {
                errors.push(format!(
                    "member {} Artifact ID is {}, expected {expected}",
                    member.index, member.artifact_id
                ));
            }
        }
    } else {
        errors.push(
            "cannot validate non-empty CPU features with the current catalog encoding".to_owned(),
        );
    }
    let artifact_ids = bundle
        .members
        .iter()
        .map(|member| member.artifact_id)
        .collect::<Vec<_>>();
    let expected_bundle = artifact_bundle_identity(&artifact_ids);
    if bundle.bundle_id != expected_bundle {
        errors.push(format!(
            "bundle ID is {}, expected {expected_bundle}",
            bundle.bundle_id
        ));
    }
    errors
}

fn apply_migrations(connection: &mut Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             version INTEGER PRIMARY KEY CHECK (version > 0),
             name TEXT NOT NULL UNIQUE
         ) STRICT;",
    )?;
    let current = current_schema_version(connection)?;
    if current > LATEST_SCHEMA_VERSION {
        return Err(Error::catalog(format!(
            "catalog schema version {current} is newer than supported version {LATEST_SCHEMA_VERSION}"
        )));
    }
    for (version, name, sql) in [
        (1_i64, "initial catalog", MIGRATION_1),
        (2_i64, "durable recipe results", MIGRATION_2),
        (3_i64, "native artifact metadata", MIGRATION_3),
        (4_i64, "backend-neutral artifact metadata", MIGRATION_4),
    ] {
        if version <= current {
            continue;
        }
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(sql)?;
        transaction.execute(
            "INSERT INTO schema_migrations(version, name) VALUES (?1, ?2)",
            params![version, name],
        )?;
        transaction.commit()?;
    }
    Ok(())
}

fn insert_content(transaction: &Transaction<'_>, content: &StoredContent) -> Result<()> {
    let byte_len = i64::try_from(content.byte_len)
        .map_err(|_| Error::catalog("content byte length does not fit SQLite INTEGER"))?;
    let content_id = content.identity.to_string();
    transaction.execute(
        "INSERT OR IGNORE INTO contents(content_id, byte_length, relative_path, kind)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            content_id,
            byte_len,
            content.relative_path,
            content.kind.as_str()
        ],
    )?;
    let recorded = transaction.query_row(
        "SELECT byte_length, relative_path, kind FROM contents WHERE content_id = ?1",
        [&content_id],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        },
    )?;
    let expected = (
        byte_len,
        content.relative_path.clone(),
        content.kind.as_str().to_owned(),
    );
    if recorded != expected {
        return Err(Error::catalog(format!(
            "content {content_id} already has incompatible metadata"
        )));
    }
    Ok(())
}

fn insert_lineage(transaction: &Transaction<'_>, lineage: &Lineage) -> Result<()> {
    match lineage.node() {
        LineageNode::Source(source) => {
            let (Some(source_id), Some(content_id)) = (source.source_id, source.observed_content)
            else {
                return Err(Error::catalog(format!(
                    "source {:?} was not observed before durable recording",
                    source.locator
                )));
            };
            let recorded = transaction
                .query_row(
                    "SELECT locator, content_id FROM source_assets WHERE source_id = ?1",
                    [source_id.to_string()],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            if recorded != Some((source.locator.to_string(), content_id.to_string())) {
                return Err(Error::catalog(format!(
                    "source lineage {source_id} is not present in this workspace"
                )));
            }
        }
        LineageNode::ExternalObservation(observation) => {
            let dependency_id = observation.dependency_id.to_string();
            transaction.execute(
                "INSERT OR IGNORE INTO external_observations(
                     dependency_id, capability, observation_key, observed_content_id
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    dependency_id,
                    observation.capability.as_ref(),
                    observation.key.as_ref(),
                    observation.observed_content.to_string()
                ],
            )?;
            let recorded = transaction.query_row(
                "SELECT capability, observation_key, observed_content_id
                 FROM external_observations WHERE dependency_id = ?1",
                [&dependency_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )?;
            let expected = (
                observation.capability.to_string(),
                observation.key.to_vec(),
                observation.observed_content.to_string(),
            );
            if recorded != expected {
                return Err(Error::catalog(format!(
                    "dependency {dependency_id} already has incompatible lineage"
                )));
            }
        }
        LineageNode::Invocation(invocation) => {
            for argument in invocation.arguments.iter() {
                if let Some(parent) = &argument.lineage {
                    insert_lineage(transaction, parent)?;
                }
            }
            for observation in invocation.observations.iter() {
                insert_lineage(transaction, observation)?;
            }

            let recipe_id = invocation.recipe_id.to_string();
            let transform_id = invocation.transform_id.to_string();
            transaction.execute(
                "INSERT OR IGNORE INTO lineage_invocations(
                     recipe_id, transform_id, transform_name
                 ) VALUES (?1, ?2, ?3)",
                params![recipe_id, transform_id, invocation.transform_name.as_ref()],
            )?;
            let recorded_transform = transaction.query_row(
                "SELECT transform_id FROM lineage_invocations WHERE recipe_id = ?1",
                [&recipe_id],
                |row| row.get::<_, String>(0),
            )?;
            if recorded_transform != transform_id {
                return Err(Error::catalog(format!(
                    "recipe {recipe_id} already has transform {recorded_transform}, not {transform_id}"
                )));
            }

            for (index, argument) in invocation.arguments.iter().enumerate() {
                insert_argument(transaction, &recipe_id, index, argument)?;
            }
            ensure_related_count(
                transaction,
                "lineage_arguments",
                &recipe_id,
                invocation.arguments.len(),
            )?;
            for (index, observation) in invocation.observations.iter().enumerate() {
                let LineageNode::ExternalObservation(observation) = observation.node() else {
                    return Err(Error::catalog(
                        "invocation observation is not an external dependency",
                    ));
                };
                let index = i64::try_from(index)
                    .map_err(|_| Error::catalog("observation index does not fit SQLite INTEGER"))?;
                transaction.execute(
                    "INSERT OR IGNORE INTO invocation_observations(
                         recipe_id, observation_index, dependency_id
                     ) VALUES (?1, ?2, ?3)",
                    params![recipe_id, index, observation.dependency_id.to_string()],
                )?;
                let recorded = transaction.query_row(
                    "SELECT dependency_id FROM invocation_observations
                     WHERE recipe_id = ?1 AND observation_index = ?2",
                    params![recipe_id, index],
                    |row| row.get::<_, String>(0),
                )?;
                if recorded != observation.dependency_id.to_string() {
                    return Err(Error::catalog(format!(
                        "recipe {recipe_id} observation {index} has incompatible lineage"
                    )));
                }
            }
            ensure_related_count(
                transaction,
                "invocation_observations",
                &recipe_id,
                invocation.observations.len(),
            )?;
        }
    }
    Ok(())
}

fn ensure_related_count(
    transaction: &Transaction<'_>,
    table: &str,
    recipe_id: &str,
    expected: usize,
) -> Result<()> {
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE recipe_id = ?1");
    let recorded = transaction.query_row(&sql, [recipe_id], |row| row.get::<_, i64>(0))?;
    let expected = i64::try_from(expected)
        .map_err(|_| Error::catalog(format!("{table} count does not fit SQLite INTEGER")))?;
    if recorded != expected {
        return Err(Error::catalog(format!(
            "recipe {recipe_id} has {recorded} recorded {table} rows, expected {expected}"
        )));
    }
    Ok(())
}

fn insert_argument(
    transaction: &Transaction<'_>,
    recipe_id: &str,
    index: usize,
    argument: &LineageArgument,
) -> Result<()> {
    let index = i64::try_from(index)
        .map_err(|_| Error::catalog("argument index does not fit SQLite INTEGER"))?;
    let (semantic_kind, semantic_id) = semantic_identity(argument.semantic_identity);
    let (recorded_kind, recorded_text, recorded_content_id) = recorded_value(&argument.value)?;
    let (parent_kind, parent_id) = argument
        .lineage
        .as_ref()
        .map(lineage_parent)
        .transpose()?
        .map_or((None, None), |(kind, id)| (Some(kind), Some(id)));
    transaction.execute(
        "INSERT OR IGNORE INTO lineage_arguments(
             recipe_id, argument_index, argument_name, semantic_kind, semantic_id,
             recorded_kind, recorded_text, recorded_content_id, parent_kind, parent_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            recipe_id,
            index,
            argument.name.as_ref(),
            semantic_kind,
            semantic_id,
            recorded_kind,
            recorded_text,
            recorded_content_id,
            parent_kind,
            parent_id
        ],
    )?;
    let recorded = transaction.query_row(
        "SELECT semantic_kind, semantic_id, recorded_kind, recorded_text,
                recorded_content_id, parent_kind, parent_id
         FROM lineage_arguments WHERE recipe_id = ?1 AND argument_index = ?2",
        params![recipe_id, index],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        },
    )?;
    let expected = (
        semantic_kind.to_owned(),
        semantic_id,
        recorded_kind.to_owned(),
        recorded_text,
        recorded_content_id,
        parent_kind.map(str::to_owned),
        parent_id,
    );
    if recorded != expected {
        return Err(Error::catalog(format!(
            "recipe {recipe_id} argument {index} already has incompatible lineage"
        )));
    }
    Ok(())
}

fn semantic_identity(identity: SemanticValueIdentity) -> (&'static str, String) {
    match identity {
        SemanticValueIdentity::Content(identity) => ("content", identity.to_string()),
        SemanticValueIdentity::Source(identity) => ("source", identity.to_string()),
        SemanticValueIdentity::Recipe(identity) => ("recipe", identity.to_string()),
    }
}

fn recorded_value(value: &RecordedValue) -> Result<(&'static str, Option<String>, Option<String>)> {
    Ok(match value {
        RecordedValue::Null => ("null", None, None),
        RecordedValue::Bool(value) => ("bool", Some(u8::from(*value).to_string()), None),
        RecordedValue::Integer(value) => ("i64", Some(value.to_string()), None),
        RecordedValue::Float(value) => ("f32", Some(format!("{:08x}", value.to_bits())), None),
        RecordedValue::String(value) => ("string", Some(value.to_string()), None),
        RecordedValue::Materialized { kind, content_id } => {
            if !matches!(*kind, "bytes" | "list" | "record" | "image") {
                return Err(Error::catalog(format!(
                    "unsupported recorded materialized value kind {kind:?}"
                )));
            }
            (kind, None, Some(content_id.to_string()))
        }
        RecordedValue::Source { locator, .. } => ("source", Some(locator.to_string()), None),
    })
}

fn lineage_parent(lineage: &Lineage) -> Result<(&'static str, String)> {
    match lineage.node() {
        LineageNode::Source(source) => source
            .source_id
            .map(|identity| ("source", identity.to_string()))
            .ok_or_else(|| Error::catalog("argument source lineage is not observed")),
        LineageNode::Invocation(invocation) => Ok(("recipe", invocation.recipe_id.to_string())),
        LineageNode::ExternalObservation(_) => Err(Error::catalog(
            "external observations cannot be argument lineage parents",
        )),
    }
}

fn current_schema_version(connection: &Connection) -> rusqlite::Result<i64> {
    connection.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )
}

fn count(connection: &Connection, table: &str) -> Result<u64> {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    let value = connection.query_row(&sql, [], |row| row.get::<_, i64>(0))?;
    u64::try_from(value).map_err(|_| Error::catalog(format!("negative row count for {table}")))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn existing_initial_catalog_migrates_through_current_version() {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("build")
            .join("histima-catalog-tests")
            .join(format!("{}-{sequence}", std::process::id()));
        if directory.exists() {
            fs::remove_dir_all(&directory).unwrap();
        }
        fs::create_dir_all(&directory).unwrap();
        let database = directory.join(".histima.sql3");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE schema_migrations (
                     version INTEGER PRIMARY KEY CHECK (version > 0),
                     name TEXT NOT NULL UNIQUE
                 ) STRICT;",
            )
            .unwrap();
        connection.execute_batch(MIGRATION_1).unwrap();
        connection
            .execute(
                "INSERT INTO schema_migrations(version, name) VALUES (1, 'initial catalog')",
                [],
            )
            .unwrap();
        let identity = "0".repeat(64);
        connection
            .execute(
                "INSERT INTO contents(content_id, byte_length, relative_path)
                 VALUES (?1, 0, ?2)",
                params![identity, format!("objects/00/{}", "0".repeat(62))],
            )
            .unwrap();
        drop(connection);

        let catalog = Catalog::open(&database).unwrap();

        assert_eq!(catalog.info().unwrap().schema_version, 4);
        assert_eq!(
            catalog
                .connection
                .query_row("SELECT kind FROM contents", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "raw"
        );
        assert_eq!(
            catalog
                .connection
                .query_row("SELECT COUNT(*) FROM artifact_bundles", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
        drop(catalog);
        fs::remove_dir_all(directory).unwrap();
    }
}
