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
const BUFFER_ENCODING_MAGIC: &[u8] = b"HISTIMA-BUFFER\0\x01";

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
    Buffer,
}

impl ContentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Bytes => "bytes",
            Self::Buffer => "buffer",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "raw" => Ok(Self::Raw),
            "bytes" => Ok(Self::Bytes),
            "buffer" => Ok(Self::Buffer),
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

    pub fn put_typed_value(&self, value: &OuterValue) -> Result<StoredContent> {
        match &value.data {
            ValueData::Bytes(bytes) => self.put_bytes_value(bytes),
            ValueData::Buffer(buffer) => {
                let identity = content_identity(value).map_err(|error| {
                    Error::catalog(format!("cannot identify Buffer value: {error}"))
                })?;
                let encoded = encode_buffer(buffer)?;
                self.put_known(identity, ContentKind::Buffer, &encoded)
            }
            _ => Err(Error::ValueNotRecordable(
                "durable recording currently supports only immutable Bytes and Buffer values"
                    .to_owned(),
            )),
        }
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
        ContentKind::Buffer => {
            let value = decode_buffer(bytes)?;
            content_identity(&OuterValue::buffer(value)).map_err(|error| {
                Error::catalog(format!("cannot identify stored Buffer value: {error}"))
            })
        }
    }
}

pub(crate) fn decode_typed_value(kind: ContentKind, bytes: &[u8]) -> Result<Option<OuterValue>> {
    match kind {
        ContentKind::Raw => Ok(None),
        ContentKind::Bytes => Ok(Some(OuterValue::plain(ValueData::Bytes(Arc::new(
            bytes.to_vec(),
        ))))),
        ContentKind::Buffer => Ok(Some(OuterValue::buffer(decode_buffer(bytes)?))),
    }
}

fn encode_buffer(buffer: &tima::runtime::BufferValue) -> Result<Vec<u8>> {
    let rank = u8::try_from(buffer.shape().len())
        .map_err(|_| Error::catalog("Buffer rank does not fit storage encoding"))?;
    let mut encoded = Vec::with_capacity(
        BUFFER_ENCODING_MAGIC.len() + 1 + buffer.shape().len() * 8 + 16 + buffer.byte_len(),
    );
    encoded.extend_from_slice(BUFFER_ENCODING_MAGIC);
    encoded.push(rank);
    for dimension in buffer.shape() {
        encoded.extend_from_slice(
            &u64::try_from(*dimension)
                .map_err(|_| Error::catalog("Buffer dimension does not fit storage encoding"))?
                .to_le_bytes(),
        );
    }
    encoded.extend_from_slice(
        &u64::try_from(buffer.outer_stride())
            .map_err(|_| Error::catalog("Buffer stride does not fit storage encoding"))?
            .to_le_bytes(),
    );
    encoded.extend_from_slice(
        &u64::try_from(buffer.byte_len())
            .map_err(|_| Error::catalog("Buffer byte length does not fit storage encoding"))?
            .to_le_bytes(),
    );
    buffer.with_bytes(|bytes| encoded.extend_from_slice(bytes));
    Ok(encoded)
}

fn decode_buffer(bytes: &[u8]) -> Result<tima::runtime::BufferValue> {
    let Some(mut remaining) = bytes.strip_prefix(BUFFER_ENCODING_MAGIC) else {
        return Err(Error::catalog(
            "stored Buffer has an invalid encoding header",
        ));
    };
    let (&rank, tail) = remaining
        .split_first()
        .ok_or_else(|| Error::catalog("stored Buffer has no encoded rank"))?;
    remaining = tail;
    if !(1..=3).contains(&rank) {
        return Err(Error::catalog(format!(
            "stored Buffer has invalid rank {rank}"
        )));
    }
    let shape = (0..rank)
        .map(|_| read_usize(&mut remaining, "dimension"))
        .collect::<Result<Vec<_>>>()?;
    let outer_stride = read_usize(&mut remaining, "outer stride")?;
    let byte_len = read_usize(&mut remaining, "byte length")?;
    if remaining.len() != byte_len {
        return Err(Error::catalog(format!(
            "stored Buffer declares {byte_len} bytes but contains {}",
            remaining.len()
        )));
    }
    tima::runtime::BufferValue::new(shape, outer_stride, remaining.to_vec())
        .map_err(|error| Error::catalog(format!("stored Buffer has invalid layout: {error}")))
}

fn read_usize(bytes: &mut &[u8], field: &str) -> Result<usize> {
    if bytes.len() < 8 {
        return Err(Error::catalog(format!(
            "stored Buffer is truncated before its {field}"
        )));
    }
    let (encoded, remaining) = bytes.split_at(8);
    *bytes = remaining;
    let encoded = u64::from_le_bytes(encoded.try_into().expect("slice length was checked"));
    usize::try_from(encoded)
        .map_err(|_| Error::catalog(format!("stored Buffer {field} does not fit usize")))
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

#[cfg(test)]
mod tests {
    use super::{ContentKind, decode_typed_value, encode_buffer, identity_for};
    use tima::identity::content_identity;
    use tima::runtime::{BufferValue, OuterValue, ValueData};

    #[test]
    fn buffer_storage_encoding_round_trips_layout_content_and_identity() {
        let buffer = BufferValue::new(vec![2, 2, 4], 10, (0..20).collect()).unwrap();
        let expected = content_identity(&OuterValue::buffer(buffer.clone())).unwrap();
        let encoded = encode_buffer(&buffer).unwrap();
        assert_eq!(
            identity_for(ContentKind::Buffer, &encoded).unwrap(),
            expected
        );

        let decoded = decode_typed_value(ContentKind::Buffer, &encoded)
            .unwrap()
            .unwrap();
        let ValueData::Buffer(decoded) = decoded.data else {
            panic!("Buffer storage must decode as an outer Buffer");
        };
        assert_eq!(decoded.shape(), &[2, 2, 4]);
        assert_eq!(decoded.outer_stride(), 10);
        assert_eq!(
            decoded.with_bytes(<[u8]>::to_vec),
            (0..20).collect::<Vec<_>>()
        );
    }

    #[test]
    fn buffer_storage_encoding_rejects_truncation_and_trailing_bytes() {
        let buffer = BufferValue::new(vec![2], 2, vec![1, 2]).unwrap();
        let mut encoded = encode_buffer(&buffer).unwrap();
        encoded.pop();
        assert!(
            decode_typed_value(ContentKind::Buffer, &encoded)
                .unwrap_err()
                .to_string()
                .contains("declares 2 bytes but contains 1")
        );
        let mut encoded = encode_buffer(&buffer).unwrap();
        encoded.push(3);
        assert!(
            decode_typed_value(ContentKind::Buffer, &encoded)
                .unwrap_err()
                .to_string()
                .contains("declares 2 bytes but contains 3")
        );
    }
}
