use std::error::Error as StdError;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum Error {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Database(rusqlite::Error),
    NonUtf8Locator(PathBuf),
    AssetNotFound(String),
    ContentNotFound(String),
    RecipeNotFound(String),
    ArtifactNotFound(String),
    ValueNotRecordable(String),
    InvalidMaterializationPath(PathBuf),
    MaterializationExists(PathBuf),
    Integrity {
        path: PathBuf,
        expected: String,
        observed: String,
    },
    CatalogInvariant(String),
}

impl Error {
    pub(crate) fn io(operation: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.to_owned(),
            source,
        }
    }

    pub(crate) fn catalog(message: impl Into<String>) -> Self {
        Self::CatalogInvariant(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "could not {operation} {}: {source}",
                path.display()
            ),
            Self::Database(error) => write!(formatter, "catalog database error: {error}"),
            Self::NonUtf8Locator(path) => write!(
                formatter,
                "asset locator is not valid UTF-8: {}",
                path.display()
            ),
            Self::AssetNotFound(locator) => {
                write!(formatter, "asset locator {locator:?} is not imported")
            }
            Self::ContentNotFound(identity) => {
                write!(
                    formatter,
                    "content {identity} is not recorded in this workspace"
                )
            }
            Self::RecipeNotFound(identity) => {
                write!(
                    formatter,
                    "recipe {identity} is not recorded in this workspace"
                )
            }
            Self::ArtifactNotFound(identity) => write!(
                formatter,
                "artifact or native bundle {identity} is not recorded in this workspace"
            ),
            Self::ValueNotRecordable(message) => formatter.write_str(message),
            Self::InvalidMaterializationPath(path) => write!(
                formatter,
                "materialization path must name a file: {}",
                path.display()
            ),
            Self::MaterializationExists(path) => write!(
                formatter,
                "refusing to replace existing materialization: {}",
                path.display()
            ),
            Self::Integrity {
                path,
                expected,
                observed,
            } => write!(
                formatter,
                "content integrity failure for {}: expected {expected}, observed {observed}",
                path.display()
            ),
            Self::CatalogInvariant(message) => {
                write!(formatter, "catalog invariant failed: {message}")
            }
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Database(source) => Some(source),
            Self::NonUtf8Locator(_)
            | Self::AssetNotFound(_)
            | Self::ContentNotFound(_)
            | Self::RecipeNotFound(_)
            | Self::ArtifactNotFound(_)
            | Self::ValueNotRecordable(_)
            | Self::InvalidMaterializationPath(_)
            | Self::MaterializationExists(_)
            | Self::Integrity { .. }
            | Self::CatalogInvariant(_) => None,
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(value: rusqlite::Error) -> Self {
        Self::Database(value)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
