use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::backend::BackendArtifact;
use crate::backend::cranelift::host_object_file_name;
use crate::identity::{
    ArtifactBundleIdentity, ArtifactConfiguration, ArtifactIdentity, TransformIdentity,
    artifact_bundle_identity, artifact_identity, byte_content_identity,
};

static NEXT_BUILD: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactCacheStatus {
    Hit,
    Miss,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledArtifact {
    pub backend: String,
    pub backend_version: String,
    pub compiler_version: String,
    pub target: String,
    pub cpu_features: Vec<String>,
    pub optimization: String,
    pub abi_version: u32,
    pub artifact_path: PathBuf,
    pub static_size: u64,
}

impl CompiledArtifact {
    pub fn identity(&self, transform: TransformIdentity) -> ArtifactIdentity {
        let cpu_features = self
            .cpu_features
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        artifact_identity(
            transform,
            &ArtifactConfiguration {
                backend: &self.backend,
                backend_version: &self.backend_version,
                compiler_version: &self.compiler_version,
                target: &self.target,
                cpu_features: &cpu_features,
                optimization: &self.optimization,
                abi_version: self.abi_version,
            },
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedArtifact {
    pub artifact: CompiledArtifact,
    pub artifact_ids: Vec<ArtifactIdentity>,
    pub bundle_id: ArtifactBundleIdentity,
    pub status: ArtifactCacheStatus,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NativeArtifactCache;

impl NativeArtifactCache {
    pub fn store(
        &self,
        generated: &BackendArtifact,
        transforms: &[TransformIdentity],
        cache_root: impl AsRef<Path>,
    ) -> Result<CachedArtifact, ArtifactCacheError> {
        let cpu_features = generated
            .cpu_features
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let configuration = ArtifactConfiguration {
            backend: generated.backend,
            backend_version: generated.backend_version,
            compiler_version: generated.compiler_version,
            target: &generated.target,
            cpu_features: &cpu_features,
            optimization: generated.optimization,
            abi_version: generated.abi_version,
        };
        let artifact_ids = transforms
            .iter()
            .map(|transform| artifact_identity(*transform, &configuration))
            .collect::<Vec<_>>();
        let bundle_id = artifact_bundle_identity(&artifact_ids);
        let root = cache_root
            .as_ref()
            .join("artifacts")
            .join(generated.backend);
        let directory = root.join(bundle_id.to_string());
        let artifact_name = host_object_file_name();
        let artifact_path = directory.join(artifact_name);
        let checksum_path = directory.join(format!("{artifact_name}.sha256"));
        let expected_checksum = byte_content_identity(&generated.bytes).to_string();
        if fs::read(&artifact_path).is_ok_and(|bytes| bytes == generated.bytes)
            && fs::read_to_string(&checksum_path)
                .is_ok_and(|checksum| checksum == expected_checksum)
        {
            return Ok(cached(
                generated,
                artifact_path,
                artifact_ids,
                bundle_id,
                ArtifactCacheStatus::Hit,
            ));
        }

        fs::create_dir_all(&root)
            .map_err(|error| io_error("create artifact cache", &root, error))?;
        let sequence = NEXT_BUILD.fetch_add(1, Ordering::Relaxed);
        let temporary = root.join(format!(".build-{}-{sequence}", std::process::id()));
        if temporary.exists() {
            fs::remove_dir_all(&temporary)
                .map_err(|error| io_error("remove stale artifact build", &temporary, error))?;
        }
        fs::create_dir_all(&temporary)
            .map_err(|error| io_error("create artifact build", &temporary, error))?;
        fs::write(temporary.join(artifact_name), &generated.bytes).map_err(|error| {
            io_error(
                "write native artifact",
                &temporary.join(artifact_name),
                error,
            )
        })?;
        fs::write(
            temporary.join(format!("{artifact_name}.sha256")),
            &expected_checksum,
        )
        .map_err(|error| {
            io_error(
                "write native artifact checksum",
                &temporary.join(format!("{artifact_name}.sha256")),
                error,
            )
        })?;
        if directory.exists() {
            fs::remove_dir_all(&directory)
                .map_err(|error| io_error("replace native artifact", &directory, error))?;
        }
        fs::rename(&temporary, &directory)
            .map_err(|error| io_error("publish native artifact", &directory, error))?;
        Ok(cached(
            generated,
            directory.join(artifact_name),
            artifact_ids,
            bundle_id,
            ArtifactCacheStatus::Miss,
        ))
    }
}

fn cached(
    generated: &BackendArtifact,
    artifact_path: PathBuf,
    artifact_ids: Vec<ArtifactIdentity>,
    bundle_id: ArtifactBundleIdentity,
    status: ArtifactCacheStatus,
) -> CachedArtifact {
    CachedArtifact {
        artifact: CompiledArtifact {
            backend: generated.backend.to_owned(),
            backend_version: generated.backend_version.to_owned(),
            compiler_version: generated.compiler_version.to_owned(),
            target: generated.target.clone(),
            cpu_features: generated.cpu_features.clone(),
            optimization: generated.optimization.to_owned(),
            abi_version: generated.abi_version,
            artifact_path,
            static_size: generated.static_size,
        },
        artifact_ids,
        bundle_id,
        status,
    }
}

fn io_error(action: &str, path: &Path, error: std::io::Error) -> ArtifactCacheError {
    ArtifactCacheError {
        message: format!("failed to {action} {}: {error}", path.display()),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactCacheError {
    message: String,
}

impl fmt::Display for ArtifactCacheError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ArtifactCacheError {}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::backend::ArtifactBackend;
    use crate::backend::cranelift::CraneliftBackend;

    use super::{ArtifactCacheStatus, NativeArtifactCache};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn native_artifact_cache_hits_and_repairs_corruption() {
        let compiled = crate::compile(
            "scalar.tima",
            "transform scale(value: f32, factor: f32) -> f32 { return value * factor }\n",
        )
        .unwrap();
        let generated = CraneliftBackend.emit(&compiled.transforms).unwrap();
        let identities = compiled.identities.iter().collect::<Vec<_>>();
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("build")
            .join(format!(
                "native-artifact-cache-{}-{}",
                std::process::id(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed)
            ));
        let cache = NativeArtifactCache;

        let first = cache.store(&generated, &identities, &root).unwrap();
        assert_eq!(first.status, ArtifactCacheStatus::Miss);
        assert_eq!(
            first.artifact_ids[0],
            first.artifact.identity(identities[0])
        );
        let second = cache.store(&generated, &identities, &root).unwrap();
        assert_eq!(second.status, ArtifactCacheStatus::Hit);
        assert_eq!(second.bundle_id, first.bundle_id);

        fs::write(&second.artifact.artifact_path, b"corrupt").unwrap();
        let repaired = cache.store(&generated, &identities, &root).unwrap();
        assert_eq!(repaired.status, ArtifactCacheStatus::Miss);
        assert_eq!(
            fs::read(repaired.artifact.artifact_path).unwrap(),
            generated.bytes
        );
    }
}
