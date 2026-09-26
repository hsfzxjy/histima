use std::error::Error;
use std::ffi::{CStr, CString, c_char, c_void};
use std::fmt;
use std::fs;
use std::mem;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::abi::TIMA_ABI_VERSION;
use crate::backend::NativeArtifact;
use crate::identity::{
    ArtifactBundleIdentity, ArtifactConfiguration, ArtifactIdentity, TransformIdentity,
    artifact_bundle_identity, artifact_identity, byte_content_identity,
};
use crate::ir::{TransformId, TypedModule};

const DEFAULT_CLANG: &str = "C:/Program Files/LLVM/bin/clang.exe";
static NEXT_BUILD: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct ClangCompiler {
    executable: PathBuf,
}

impl Default for ClangCompiler {
    fn default() -> Self {
        Self::new(DEFAULT_CLANG)
    }
}

impl ClangCompiler {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    pub fn compile(
        &self,
        generated: &NativeArtifact,
        build_root: impl AsRef<Path>,
    ) -> Result<CompiledNativeArtifact, NativeBuildError> {
        let fingerprint = self.fingerprint()?;
        let sequence = NEXT_BUILD.fetch_add(1, Ordering::Relaxed);
        let build_dir = build_root
            .as_ref()
            .join(format!("tima-native-{}-{sequence}", std::process::id()));
        self.compile_into(generated, build_dir, fingerprint, true)
    }

    /// Compiles or reuses one persistent native module cache entry. Individual
    /// transform Artifact IDs remain distinct; the bundle ID captures their
    /// module order because generated ABI adapters are index-addressed.
    pub fn compile_cached(
        &self,
        generated: &NativeArtifact,
        transforms: &[TransformIdentity],
        cache_root: impl AsRef<Path>,
    ) -> Result<CachedNativeArtifact, NativeBuildError> {
        let fingerprint = self.fingerprint()?;
        let configuration = ArtifactConfiguration {
            backend: generated.backend,
            backend_version: generated.backend_version,
            compiler_version: &fingerprint.compiler_version,
            target: &fingerprint.target,
            cpu_features: &[],
            optimization: "O2",
            abi_version: generated.abi_version,
        };
        let artifact_ids = transforms
            .iter()
            .map(|transform| artifact_identity(*transform, &configuration))
            .collect::<Vec<_>>();
        let bundle_id = artifact_bundle_identity(&artifact_ids);
        let native_root = cache_root.as_ref().join("native");
        let cache_dir = native_root.join(bundle_id.to_string());
        if cached_module_is_valid(&cache_dir, &generated.source) {
            return Ok(CachedNativeArtifact {
                artifact: compiled_artifact(generated, cache_dir, fingerprint, false),
                artifact_ids,
                bundle_id,
                status: NativeCacheStatus::Hit,
            });
        }

        fs::create_dir_all(&native_root).map_err(|error| {
            NativeBuildError::new(
                "create native cache directory",
                format!("{}: {error}", native_root.display()),
            )
        })?;
        let sequence = NEXT_BUILD.fetch_add(1, Ordering::Relaxed);
        let temporary_dir = native_root.join(format!(".build-{}-{sequence}", std::process::id()));
        let mut artifact =
            self.compile_into(generated, temporary_dir.clone(), fingerprint, true)?;
        write_library_checksum(&temporary_dir, &artifact.library_path)?;
        if cache_dir.exists() {
            fs::remove_dir_all(&cache_dir).map_err(|error| {
                NativeBuildError::new(
                    "replace invalid native cache entry",
                    format!("{}: {error}", cache_dir.display()),
                )
            })?;
        }
        fs::rename(&temporary_dir, &cache_dir).map_err(|error| {
            NativeBuildError::new(
                "publish native cache entry",
                format!(
                    "{} -> {}: {error}",
                    temporary_dir.display(),
                    cache_dir.display()
                ),
            )
        })?;
        artifact.source_path = cache_dir.join("module.c");
        artifact.library_path = cache_dir.join("module.dll");
        artifact.cleanup_dir = None;
        Ok(CachedNativeArtifact {
            artifact,
            artifact_ids,
            bundle_id,
            status: NativeCacheStatus::Miss,
        })
    }

    fn fingerprint(&self) -> Result<CompilerFingerprint, NativeBuildError> {
        let compiler_version = tool_output(&self.executable, "read clang version", &["--version"])?
            .lines()
            .next()
            .unwrap_or("unknown clang version")
            .to_owned();
        let target = tool_output(&self.executable, "read clang target", &["-dumpmachine"])?;
        Ok(CompilerFingerprint {
            executable: self.executable.clone(),
            compiler_version,
            target,
        })
    }

    fn compile_into(
        &self,
        generated: &NativeArtifact,
        build_dir: PathBuf,
        fingerprint: CompilerFingerprint,
        cleanup: bool,
    ) -> Result<CompiledNativeArtifact, NativeBuildError> {
        fs::create_dir_all(&build_dir).map_err(|error| {
            NativeBuildError::new(
                "create native build directory",
                format!("{}: {error}", build_dir.display()),
            )
        })?;
        let source_path = build_dir.join("module.c");
        let library_path = build_dir.join("module.dll");
        if let Err(error) = fs::write(&source_path, &generated.source) {
            let _ = fs::remove_dir_all(&build_dir);
            return Err(NativeBuildError::new(
                "write generated C",
                format!("{}: {error}", source_path.display()),
            ));
        }

        let output = match Command::new(&self.executable)
            .args([
                "-std=c11",
                "-O2",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-shared",
                "-nostdlib",
                "-fuse-ld=lld",
                "-Wl,/noentry",
                "-o",
            ])
            .arg(&library_path)
            .arg(&source_path)
            .output()
        {
            Ok(output) => output,
            Err(error) => {
                let _ = fs::remove_dir_all(&build_dir);
                return Err(NativeBuildError::new(
                    "launch clang",
                    format!("{}: {error}", self.executable.display()),
                ));
            }
        };
        if !output.status.success() {
            let details = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let _ = fs::remove_dir_all(&build_dir);
            return Err(NativeBuildError::new(
                "compile generated C",
                if details.is_empty() {
                    format!("clang exited with {}", output.status)
                } else {
                    details
                },
            ));
        }

        Ok(CompiledNativeArtifact {
            backend: generated.backend,
            backend_version: generated.backend_version,
            abi_version: generated.abi_version,
            compiler: self.executable.clone(),
            compiler_version: fingerprint.compiler_version,
            target: fingerprint.target,
            optimization: "O2",
            source_path,
            library_path,
            cleanup_dir: cleanup.then_some(build_dir),
        })
    }
}

#[derive(Clone, Debug)]
struct CompilerFingerprint {
    executable: PathBuf,
    compiler_version: String,
    target: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeCacheStatus {
    Hit,
    Miss,
}

#[derive(Debug)]
pub struct CachedNativeArtifact {
    pub artifact: CompiledNativeArtifact,
    pub artifact_ids: Vec<ArtifactIdentity>,
    pub bundle_id: ArtifactBundleIdentity,
    pub status: NativeCacheStatus,
}

#[derive(Debug)]
pub struct CompiledNativeArtifact {
    pub backend: &'static str,
    pub backend_version: &'static str,
    pub abi_version: u32,
    pub compiler: PathBuf,
    pub compiler_version: String,
    pub target: String,
    pub optimization: &'static str,
    pub source_path: PathBuf,
    pub library_path: PathBuf,
    cleanup_dir: Option<PathBuf>,
}

impl CompiledNativeArtifact {
    pub fn identity(&self, transform: TransformIdentity) -> ArtifactIdentity {
        artifact_identity(
            transform,
            &ArtifactConfiguration {
                backend: self.backend,
                backend_version: self.backend_version,
                compiler_version: &self.compiler_version,
                target: &self.target,
                cpu_features: &[],
                optimization: self.optimization,
                abi_version: self.abi_version,
            },
        )
    }
}

impl Drop for CompiledNativeArtifact {
    fn drop(&mut self) {
        if let Some(directory) = &self.cleanup_dir {
            let _ = fs::remove_dir_all(directory);
        }
    }
}

#[derive(Debug)]
pub struct NativeBuildError {
    stage: &'static str,
    details: String,
}

impl NativeBuildError {
    fn new(stage: &'static str, details: String) -> Self {
        Self { stage, details }
    }
}

impl fmt::Display for NativeBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "failed to {}: {}", self.stage, self.details)
    }
}

impl Error for NativeBuildError {}

fn tool_output(
    executable: &Path,
    stage: &'static str,
    arguments: &[&str],
) -> Result<String, NativeBuildError> {
    let output = Command::new(executable)
        .args(arguments)
        .output()
        .map_err(|error| {
            NativeBuildError::new(stage, format!("{}: {error}", executable.display()))
        })?;
    if !output.status.success() {
        return Err(NativeBuildError::new(
            stage,
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn cached_module_is_valid(directory: &Path, expected_source: &str) -> bool {
    let source = directory.join("module.c");
    let library = directory.join("module.dll");
    let checksum = directory.join("module.sha256");
    fs::read_to_string(source).is_ok_and(|source| source == expected_source)
        && fs::read(&library).is_ok_and(|bytes| {
            fs::read_to_string(checksum)
                .is_ok_and(|expected| expected == byte_content_identity(&bytes).to_string())
        })
}

fn write_library_checksum(directory: &Path, library: &Path) -> Result<(), NativeBuildError> {
    let bytes = fs::read(library).map_err(|error| {
        NativeBuildError::new(
            "read compiled native artifact",
            format!("{}: {error}", library.display()),
        )
    })?;
    let checksum = directory.join("module.sha256");
    fs::write(&checksum, byte_content_identity(&bytes).to_string()).map_err(|error| {
        NativeBuildError::new(
            "write native artifact checksum",
            format!("{}: {error}", checksum.display()),
        )
    })
}

fn compiled_artifact(
    generated: &NativeArtifact,
    directory: PathBuf,
    fingerprint: CompilerFingerprint,
    cleanup: bool,
) -> CompiledNativeArtifact {
    CompiledNativeArtifact {
        backend: generated.backend,
        backend_version: generated.backend_version,
        abi_version: generated.abi_version,
        compiler: fingerprint.executable,
        compiler_version: fingerprint.compiler_version,
        target: fingerprint.target,
        optimization: "O2",
        source_path: directory.join("module.c"),
        library_path: directory.join("module.dll"),
        cleanup_dir: cleanup.then_some(directory),
    }
}

/// Owned mutable image descriptor used by the generated C ABI.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct AbiImage {
    pub(crate) data: *mut u8,
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) stride: usize,
}

/// Read-only image descriptor used by the generated C ABI.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct AbiImageView {
    pub(crate) data: *const u8,
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) stride: usize,
}

/// Native-safe payload used only by generated ABI adapters.
///
/// Native transform functions retain their precise C signatures. This union
/// gives the Rust runtime one stable entry point per transform without making
/// the typed IR or language semantics depend on C calling conventions.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) union AbiValue {
    pub(crate) boolean: u8,
    pub(crate) i64_value: i64,
    pub(crate) f32_value: f32,
    pub(crate) image: AbiImage,
    pub(crate) image_view: AbiImageView,
}

type AbiVersionFn = unsafe extern "C" fn() -> u32;
type InvokeFn = unsafe extern "C" fn(*const AbiValue, *mut AbiValue) -> i32;

#[derive(Debug)]
pub struct NativeModule {
    handle: *mut c_void,
    invocations: Vec<InvokeFn>,
}

impl NativeModule {
    pub fn load(
        artifact: &CompiledNativeArtifact,
        module: &TypedModule,
    ) -> Result<Self, NativeLoadError> {
        if artifact.abi_version != TIMA_ABI_VERSION {
            return Err(NativeLoadError::new(format!(
                "artifact ABI version {} does not match runtime ABI version {}",
                artifact.abi_version, TIMA_ABI_VERSION
            )));
        }
        let handle = load_library(&artifact.library_path)?;
        let result = (|| {
            let abi_version: AbiVersionFn = load_symbol(handle, c"tima_abi_version")?;
            // SAFETY: the generated symbol has the exact `AbiVersionFn` signature.
            let actual_version = unsafe { abi_version() };
            if actual_version != TIMA_ABI_VERSION {
                return Err(NativeLoadError::new(format!(
                    "loaded module ABI version {actual_version} does not match runtime ABI version {TIMA_ABI_VERSION}"
                )));
            }
            let mut invocations = Vec::with_capacity(module.transforms.len());
            for index in 0..module.transforms.len() {
                let name = CString::new(format!("tima_invoke_{index}")).unwrap();
                invocations.push(load_symbol(handle, &name)?);
            }
            Ok(invocations)
        })();
        match result {
            Ok(invocations) => Ok(Self {
                handle,
                invocations,
            }),
            Err(error) => {
                free_library(handle);
                Err(error)
            }
        }
    }

    pub(crate) fn invoke(
        &self,
        transform: TransformId,
        arguments: &[AbiValue],
    ) -> Result<AbiValue, NativeLoadError> {
        let Some(function) = self.invocations.get(transform.0 as usize) else {
            return Err(NativeLoadError::new(format!(
                "native transform index {} is unavailable",
                transform.0
            )));
        };
        let mut result = AbiValue { i64_value: 0 };
        // SAFETY: the adapter symbol was loaded with `InvokeFn`; `arguments`
        // and `result` remain valid for the duration of the synchronous call.
        let status = unsafe { function(arguments.as_ptr(), &mut result) };
        match status {
            0 => Ok(result),
            other => Err(NativeLoadError::new(format!(
                "native transform adapter returned status {other}"
            ))),
        }
    }
}

impl Drop for NativeModule {
    fn drop(&mut self) {
        free_library(self.handle);
    }
}

#[derive(Debug)]
pub struct NativeLoadError {
    details: String,
}

impl NativeLoadError {
    fn new(details: impl Into<String>) -> Self {
        Self {
            details: details.into(),
        }
    }
}

impl fmt::Display for NativeLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.details)
    }
}

impl Error for NativeLoadError {}

#[cfg(windows)]
fn load_symbol<T: Copy>(handle: *mut c_void, name: &CStr) -> Result<T, NativeLoadError> {
    let address = unsafe { GetProcAddress(handle, name.as_ptr()) };
    if address.is_null() {
        return Err(last_loader_error(&format!(
            "load native symbol `{}`",
            name.to_string_lossy()
        )));
    }
    debug_assert_eq!(mem::size_of::<T>(), mem::size_of::<*mut c_void>());
    // SAFETY: callers request a function pointer matching a symbol generated
    // by this crate. Function and data pointers have equal size on Windows.
    Ok(unsafe { mem::transmute_copy(&address) })
}

#[cfg(not(windows))]
fn load_symbol<T: Copy>(_handle: *mut c_void, _name: &CStr) -> Result<T, NativeLoadError> {
    Err(NativeLoadError::new(
        "dynamic loading is currently implemented only for Windows",
    ))
}

#[cfg(windows)]
fn load_library(path: &Path) -> Result<*mut c_void, NativeLoadError> {
    use std::os::windows::ffi::OsStrExt;

    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let handle = unsafe { LoadLibraryW(wide.as_ptr()) };
    if handle.is_null() {
        Err(last_loader_error(&format!(
            "load native artifact `{}`",
            path.display()
        )))
    } else {
        Ok(handle)
    }
}

#[cfg(not(windows))]
fn load_library(_path: &Path) -> Result<*mut c_void, NativeLoadError> {
    Err(NativeLoadError::new(
        "dynamic loading is currently implemented only for Windows",
    ))
}

#[cfg(windows)]
fn free_library(handle: *mut c_void) {
    if !handle.is_null() {
        unsafe {
            FreeLibrary(handle);
        }
    }
}

#[cfg(not(windows))]
fn free_library(_handle: *mut c_void) {}

#[cfg(windows)]
fn last_loader_error(action: &str) -> NativeLoadError {
    let code = unsafe { GetLastError() };
    NativeLoadError::new(format!("failed to {action} (Windows error {code})"))
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryW(path: *const u16) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
    fn FreeLibrary(module: *mut c_void) -> i32;
    fn GetLastError() -> u32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::NativeBackend;
    use crate::backend::c::CBackend;

    #[test]
    fn persistent_cache_reuses_and_validates_native_modules() {
        let compiled = crate::compile(
            "test.tima",
            "transform scale(x: f32) -> f32 { return x * 2.0 }\n",
        )
        .unwrap();
        let generated = CBackend.emit(&compiled.transforms).unwrap();
        let sequence = NEXT_BUILD.fetch_add(1, Ordering::Relaxed);
        let cache_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("build")
            .join(format!(
                "native-cache-test-{}-{sequence}",
                std::process::id()
            ));
        let identities = compiled.identities.iter().collect::<Vec<_>>();
        let compiler = ClangCompiler::default();

        let first = compiler
            .compile_cached(&generated, &identities, &cache_root)
            .unwrap();
        assert_eq!(first.status, NativeCacheStatus::Miss);
        assert_eq!(first.artifact_ids.len(), 1);
        assert_eq!(
            first.artifact_ids[0],
            first.artifact.identity(identities[0])
        );
        assert!(first.artifact.library_path.is_file());
        let source_path = first.artifact.source_path.clone();
        drop(first);

        let second = compiler
            .compile_cached(&generated, &identities, &cache_root)
            .unwrap();
        assert_eq!(second.status, NativeCacheStatus::Hit);
        let native = NativeModule::load(&second.artifact, &compiled.transforms).unwrap();
        drop(native);
        drop(second);

        fs::write(&source_path, "corrupt cache marker").unwrap();
        let repaired = compiler
            .compile_cached(&generated, &identities, &cache_root)
            .unwrap();
        assert_eq!(repaired.status, NativeCacheStatus::Miss);
        assert_eq!(fs::read_to_string(&source_path).unwrap(), generated.source);
        let library_path = repaired.artifact.library_path.clone();
        drop(repaired);

        fs::write(&library_path, b"corrupt library marker").unwrap();
        let repaired_library = compiler
            .compile_cached(&generated, &identities, &cache_root)
            .unwrap();
        assert_eq!(repaired_library.status, NativeCacheStatus::Miss);
        let native = NativeModule::load(&repaired_library.artifact, &compiled.transforms).unwrap();
        drop(native);
        drop(repaired_library);
        fs::remove_dir_all(cache_root).unwrap();
    }
}
