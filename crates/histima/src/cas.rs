use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use tima::identity::{ContentIdentity, byte_content_identity, content_identity};
use tima::runtime::{OuterValue, ValueData};

use crate::error::{Error, Result};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoredContent {
    pub identity: ContentIdentity,
    pub byte_len: u64,
    pub relative_path: String,
    pub kind: ContentKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContentKind {
    Raw,
    Bytes,
}

impl ContentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Bytes => "bytes",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "raw" => Ok(Self::Raw),
            "bytes" => Ok(Self::Bytes),
            _ => Err(Error::catalog(format!("unknown content kind {value:?}"))),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ContentStore {
    workspace_root: PathBuf,
    temporary_root: PathBuf,
}

impl ContentStore {
    pub fn open(workspace_root: &Path) -> Result<Self> {
        let objects_root = workspace_root.join("objects");
        let temporary_root = workspace_root.join("tmp");
        create_directory(&objects_root)?;
        create_directory(&temporary_root)?;
        Ok(Self {
            workspace_root: workspace_root.to_owned(),
            temporary_root,
        })
    }

    pub fn put(&self, bytes: &[u8]) -> Result<StoredContent> {
        let identity = byte_content_identity(bytes);
        self.put_known(identity, ContentKind::Raw, bytes)
    }

    pub fn put_bytes_value(&self, bytes: &[u8]) -> Result<StoredContent> {
        let value = OuterValue::plain(ValueData::Bytes(Arc::new(bytes.to_vec())));
        let identity = content_identity(&value)
            .map_err(|error| Error::catalog(format!("cannot identify byte value: {error}")))?;
        self.put_known(identity, ContentKind::Bytes, bytes)
    }

    fn put_known(
        &self,
        identity: ContentIdentity,
        kind: ContentKind,
        bytes: &[u8],
    ) -> Result<StoredContent> {
        let relative_path = object_relative_path(identity);
        let destination = self.workspace_root.join(&relative_path);
        let parent = destination
            .parent()
            .expect("content object paths always have a parent");
        create_directory(parent)?;

        if destination.exists() {
            self.validate_file(&destination, identity, kind)?;
            return Ok(stored(identity, bytes.len(), relative_path, kind));
        }

        let mut temporary = self.create_temporary_file()?;
        let temporary_path = temporary.path().to_owned();
        write_all_and_sync(temporary.file(), &temporary_path, bytes)?;
        temporary.close();
        match fs::rename(temporary.path(), &destination) {
            Ok(()) => {}
            Err(_error) if destination.exists() => {
                self.validate_file(&destination, identity, kind)?;
            }
            Err(error) => return Err(Error::io("publish content object", &destination, error)),
        }
        self.validate_file(&destination, identity, kind)?;
        Ok(stored(identity, bytes.len(), relative_path, kind))
    }

    pub fn read_recorded(
        &self,
        identity: &str,
        relative_path: &str,
        kind: ContentKind,
    ) -> Result<Vec<u8>> {
        let expected_relative = object_relative_path_text(identity)?;
        if relative_path != expected_relative {
            return Err(Error::catalog(format!(
                "content {identity} points to {relative_path:?}, expected {expected_relative:?}"
            )));
        }
        let path = self.workspace_root.join(relative_path);
        let bytes =
            fs::read(&path).map_err(|error| Error::io("read content object", &path, error))?;
        validate_bytes_text(&path, identity, kind, &bytes)?;
        Ok(bytes)
    }

    pub fn object_path(&self, identity: ContentIdentity) -> PathBuf {
        self.workspace_root.join(object_relative_path(identity))
    }

    fn validate_file(
        &self,
        path: &Path,
        expected: ContentIdentity,
        kind: ContentKind,
    ) -> Result<()> {
        let bytes =
            fs::read(path).map_err(|error| Error::io("read content object", path, error))?;
        validate_bytes(path, expected, kind, &bytes)
    }

    fn create_temporary_file(&self) -> Result<PendingFile> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for _ in 0..128 {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = self.temporary_root.join(format!(
                "import-{}-{timestamp}-{sequence}.tmp",
                std::process::id()
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(PendingFile {
                        path,
                        file: Some(file),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(Error::io("create temporary content object", &path, error));
                }
            }
        }
        Err(Error::catalog(
            "could not allocate a unique temporary content filename",
        ))
    }
}

fn stored(
    identity: ContentIdentity,
    byte_len: usize,
    relative_path: String,
    kind: ContentKind,
) -> StoredContent {
    StoredContent {
        identity,
        byte_len: byte_len as u64,
        relative_path,
        kind,
    }
}

fn object_relative_path(identity: ContentIdentity) -> String {
    let identity = identity.to_string();
    object_relative_path_text(&identity).expect("formatted Content IDs are valid")
}

fn object_relative_path_text(identity: &str) -> Result<String> {
    if identity.len() != 64 || !identity.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::catalog(format!(
            "invalid recorded Content ID {identity:?}"
        )));
    }
    Ok(format!("objects/{}/{}", &identity[..2], &identity[2..]))
}

fn create_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|error| Error::io("create directory", path, error))
}

fn write_all_and_sync(file: &mut File, path: &Path, bytes: &[u8]) -> Result<()> {
    file.write_all(bytes)
        .map_err(|error| Error::io("write temporary content object", path, error))?;
    file.sync_all()
        .map_err(|error| Error::io("sync temporary content object", path, error))
}

fn validate_bytes(
    path: &Path,
    expected: ContentIdentity,
    kind: ContentKind,
    bytes: &[u8],
) -> Result<()> {
    let observed = identity_for(kind, bytes)?;
    if observed == expected {
        return Ok(());
    }
    Err(Error::Integrity {
        path: path.to_owned(),
        expected: expected.to_string(),
        observed: observed.to_string(),
    })
}

fn validate_bytes_text(path: &Path, expected: &str, kind: ContentKind, bytes: &[u8]) -> Result<()> {
    let observed = identity_for(kind, bytes)?.to_string();
    if observed == expected {
        return Ok(());
    }
    Err(Error::Integrity {
        path: path.to_owned(),
        expected: expected.to_owned(),
        observed,
    })
}

fn identity_for(kind: ContentKind, bytes: &[u8]) -> Result<ContentIdentity> {
    match kind {
        ContentKind::Raw => Ok(byte_content_identity(bytes)),
        ContentKind::Bytes => content_identity(&OuterValue::plain(ValueData::Bytes(Arc::new(
            bytes.to_vec(),
        ))))
        .map_err(|error| Error::catalog(format!("cannot identify stored byte value: {error}"))),
    }
}

struct PendingFile {
    path: PathBuf,
    file: Option<File>,
}

impl PendingFile {
    fn path(&self) -> &Path {
        &self.path
    }

    fn file(&mut self) -> &mut File {
        self.file
            .as_mut()
            .expect("temporary content file is still open")
    }

    fn close(&mut self) {
        drop(self.file.take());
    }
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
    }
}
