use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use tima::identity::SourceIdentity;

use crate::cas::StoredContent;
use crate::error::{Error, Result};

const LATEST_SCHEMA_VERSION: i64 = 1;

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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CatalogObject {
    pub content_id: String,
    pub byte_len: u64,
    pub relative_path: String,
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
        let byte_len = i64::try_from(content.byte_len)
            .map_err(|_| Error::catalog("content byte length does not fit SQLite INTEGER"))?;
        let content_id = content.identity.to_string();
        let source_id = source_id.to_string();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT OR IGNORE INTO contents(content_id, byte_length, relative_path)
             VALUES (?1, ?2, ?3)",
            params![content_id, byte_len, content.relative_path],
        )?;
        let recorded_content = transaction.query_row(
            "SELECT byte_length, relative_path FROM contents WHERE content_id = ?1",
            [&content_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )?;
        if recorded_content != (byte_len, content.relative_path.clone()) {
            return Err(Error::catalog(format!(
                "content {content_id} already has incompatible metadata"
            )));
        }

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
                "SELECT content.content_id, content.byte_length, content.relative_path
                 FROM source_heads AS head
                 JOIN source_assets AS source
                   ON source.locator = head.locator AND source.source_id = head.source_id
                 JOIN contents AS content ON content.content_id = source.content_id
                 WHERE head.locator = ?1",
                [locator],
                |row| {
                    let byte_len = row.get::<_, i64>(1)?;
                    Ok((row.get::<_, String>(0)?, byte_len, row.get::<_, String>(2)?))
                },
            )
            .optional()?
            .map(|(content_id, byte_len, relative_path)| {
                let byte_len = u64::try_from(byte_len).map_err(|_| {
                    Error::catalog(format!(
                        "content {content_id} has a negative byte length in the catalog"
                    ))
                })?;
                Ok(CatalogObject {
                    content_id,
                    byte_len,
                    relative_path,
                })
            })
            .transpose()
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
        })
    }
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
    for (version, name, sql) in [(1_i64, "initial catalog", MIGRATION_1)] {
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
