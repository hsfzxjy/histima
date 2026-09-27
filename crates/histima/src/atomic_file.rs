use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{Error, Result};

static OUTPUT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let Some(file_name) = path.file_name() else {
        return Err(Error::InvalidMaterializationPath(path.to_owned()));
    };
    if path.exists() {
        return Err(Error::MaterializationExists(path.to_owned()));
    }

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = (0..128)
        .find_map(|_| {
            let sequence = OUTPUT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let temporary_path = parent.join(format!(
                ".{}.histima-{}-{timestamp}-{sequence}.tmp",
                file_name.to_string_lossy(),
                std::process::id()
            ));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary_path)
            {
                Ok(file) => Some(Ok(PendingOutput {
                    path: temporary_path,
                    file: Some(file),
                })),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(Error::io(
                    "create temporary materialization",
                    &temporary_path,
                    error,
                ))),
            }
        })
        .transpose()?
        .ok_or_else(|| Error::catalog("could not allocate a temporary materialization file"))?;
    publish(temporary, path, bytes)
}

fn publish(mut temporary: PendingOutput, destination: &Path, bytes: &[u8]) -> Result<()> {
    temporary
        .file
        .as_mut()
        .expect("temporary materialization is still open")
        .write_all(bytes)
        .map_err(|error| Error::io("write temporary materialization", &temporary.path, error))?;
    temporary
        .file
        .as_mut()
        .expect("temporary materialization is still open")
        .sync_all()
        .map_err(|error| Error::io("sync temporary materialization", &temporary.path, error))?;
    drop(temporary.file.take());
    match fs::hard_link(&temporary.path, destination) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(Error::MaterializationExists(destination.to_owned()))
        }
        Err(error) => Err(Error::io("publish materialization", destination, error)),
    }
}

struct PendingOutput {
    path: PathBuf,
    file: Option<File>,
}

impl Drop for PendingOutput {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
    }
}
