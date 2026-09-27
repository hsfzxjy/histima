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
