use std::ffi::c_void;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use libloading::Library;

use crate::abi::{
    ABI_CAPACITY_WORD, ABI_IMAGE_FORMAT_WORD, ABI_IMAGE_HEIGHT_WORD, ABI_IMAGE_STRIDE_WORD,
    ABI_IMAGE_WIDTH_WORD, ABI_LENGTH_WORD, ABI_POINTER_WORD, ABI_STATUS_OK, AbiValue,
};
use crate::backend::ArtifactBackend;
use crate::backend::cache::{CachedArtifact, NativeArtifactCache};
use crate::backend::cranelift::CraneliftBackend;
use crate::diagnostic::Diagnostic;
use crate::identity::TransformIdentities;
use crate::ir::{TransformId, Type, TypedModule};
use crate::source::Span;

static NEXT_LINK: AtomicU64 = AtomicU64::new(0);

type NativeEntry = unsafe extern "C" fn(*mut c_void, *const AbiValue, *mut AbiValue) -> i32;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NativeScalar {
    Bool(bool),
    U8(u8),
    I64(i64),
    F32(f32),
}

impl NativeScalar {
    pub const fn ty(self) -> Type {
        match self {
            Self::Bool(_) => Type::Bool,
            Self::U8(_) => Type::U8,
            Self::I64(_) => Type::I64,
            Self::F32(_) => Type::F32,
        }
    }

    fn encode(self) -> AbiValue {
        let mut encoded = AbiValue::default();
        encoded.words[0] = match self {
            Self::Bool(value) => value as u64,
            Self::U8(value) => value as u64,
            Self::I64(value) => value as u64,
            Self::F32(value) => value.to_bits() as u64,
        };
        encoded
    }

    fn decode(value: AbiValue, ty: Type) -> Self {
        let slot = value.words[0];
        match ty {
            Type::Bool => Self::Bool(slot != 0),
            Type::U8 => Self::U8(slot as u8),
            Type::I64 => Self::I64(slot as i64),
            Type::F32 => Self::F32(f32::from_bits(slot as u32)),
            _ => unreachable!("native scalar result has a scalar signature"),
        }
    }
}

#[derive(Debug)]
pub(crate) struct NativeImage {
    pub bytes: Vec<u8>,
    pub format: u32,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct NativeImageView<'a> {
    pub bytes: &'a [u8],
    pub format: u32,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
}

pub(crate) enum NativeArgument<'a> {
    Scalar(NativeScalar),
    Image(&'a mut NativeImage),
    ImageView(NativeImageView<'a>),
}

impl NativeArgument<'_> {
    fn ty(&self) -> Type {
        match self {
            Self::Scalar(value) => value.ty(),
            Self::Image(_) => Type::Image,
            Self::ImageView(_) => Type::ImageView,
        }
    }

    fn encode(&self) -> AbiValue {
        match self {
            Self::Scalar(value) => value.encode(),
            Self::Image(image) => image_value(
                image.bytes.as_ptr(),
                image.bytes.len(),
                image.bytes.capacity(),
                image.format,
                image.width,
                image.height,
                image.stride,
            ),
            Self::ImageView(image) => image_value(
                image.bytes.as_ptr(),
                image.bytes.len(),
                0,
                image.format,
                image.width,
                image.height,
                image.stride,
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum NativeResult {
    Scalar(NativeScalar),
    OwnedImageArgument(usize),
    ImageViewArgument(usize),
}

fn image_value(
    pointer: *const u8,
    length: usize,
    capacity: usize,
    format: u32,
    width: usize,
    height: usize,
    stride: usize,
) -> AbiValue {
    let mut value = AbiValue::default();
    value.words[ABI_POINTER_WORD] = pointer as usize as u64;
    value.words[ABI_LENGTH_WORD] = length as u64;
    value.words[ABI_CAPACITY_WORD] = capacity as u64;
    value.words[ABI_IMAGE_FORMAT_WORD] = u64::from(format);
    value.words[ABI_IMAGE_WIDTH_WORD] = width as u64;
    value.words[ABI_IMAGE_HEIGHT_WORD] = height as u64;
    value.words[ABI_IMAGE_STRIDE_WORD] = stride as u64;
    value
}

#[derive(Clone, Debug)]
struct NativeSignature {
    parameters: Vec<Type>,
    result: Type,
    span: Span,
}

#[derive(Clone, Copy)]
struct LoadedTransform {
    entry: NativeEntry,
}

/// A loadable AOT module for transforms admitted by the current backend.
/// Transform IDs absent from this module must be interpreted.
pub struct NativeModule {
    _library: Library,
    transforms: Vec<Option<LoadedTransform>>,
    signatures: Vec<Option<NativeSignature>>,
    artifact: CachedArtifact,
}

impl NativeModule {
    pub fn build(
        module: &TypedModule,
        identities: &TransformIdentities,
        cache_root: impl AsRef<Path>,
    ) -> Result<Option<Self>, Vec<Diagnostic>> {
        let selected = module
            .transforms
            .iter()
            .enumerate()
            .filter(|(_, transform)| CraneliftBackend::supports_transform(transform))
            .collect::<Vec<_>>();
        if selected.is_empty() {
            return Ok(None);
        }

        let selected_module = TypedModule {
            transforms: selected
                .iter()
                .map(|(_, transform)| (*transform).clone())
                .collect(),
        };
        let generated = CraneliftBackend.emit(&selected_module)?;
        let selected_identities = selected
            .iter()
            .map(|(index, _)| identities.get(TransformId(*index as u32)))
            .collect::<Vec<_>>();
        let artifact = NativeArtifactCache
            .store(&generated, &selected_identities, cache_root)
            .map_err(|error| vec![native_error(error.to_string())])?;
        let library_path = link_load_image(
            &artifact.artifact.artifact_path,
            selected_module.transforms.len(),
        )
        .map_err(|error| vec![error])?;
        let absolute_library = fs::canonicalize(&library_path).map_err(|error| {
            vec![native_error(format!(
                "could not resolve native load image {}: {error}",
                library_path.display()
            ))]
        })?;
        // SAFETY: this path was produced by the platform linker from the
        // backend-generated object in the validated artifact cache.
        let library = unsafe { Library::new(&absolute_library) }.map_err(|error| {
            vec![native_error(format!(
                "could not load native artifact {}: {error}",
                absolute_library.display()
            ))]
        })?;
        let mut transforms = vec![None; module.transforms.len()];
        let mut signatures = vec![None; module.transforms.len()];
        for (native_index, (original_index, transform)) in selected.iter().enumerate() {
            let symbol_name = format!("tima_transform_{native_index}\0");
            // SAFETY: the Cranelift backend emits every selected export with
            // `NativeEntry`'s ABI and the library remains owned by this module.
            let entry =
                unsafe { library.get::<NativeEntry>(symbol_name.as_bytes()) }.map_err(|error| {
                    vec![Diagnostic::error(
                        format!(
                            "native artifact is missing entry for transform `{}`: {error}",
                            transform.name
                        ),
                        transform.span,
                    )]
                })?;
            transforms[*original_index] = Some(LoadedTransform { entry: *entry });
            signatures[*original_index] = Some(NativeSignature {
                parameters: transform
                    .parameters
                    .iter()
                    .map(|parameter| parameter.ty)
                    .collect(),
                result: transform.return_type,
                span: transform.span,
            });
        }

        Ok(Some(Self {
            _library: library,
            transforms,
            signatures,
            artifact,
        }))
    }

    pub fn contains(&self, id: TransformId) -> bool {
        self.transforms
            .get(id.0 as usize)
            .is_some_and(Option::is_some)
    }

    pub fn artifact(&self) -> &CachedArtifact {
        &self.artifact
    }

    pub fn invoke_scalars(
        &self,
        id: TransformId,
        arguments: &[NativeScalar],
    ) -> Result<NativeScalar, Diagnostic> {
        let mut arguments = arguments
            .iter()
            .copied()
            .map(NativeArgument::Scalar)
            .collect::<Vec<_>>();
        let NativeResult::Scalar(result) = self.invoke(id, &mut arguments)? else {
            unreachable!("scalar signatures return scalar results")
        };
        Ok(result)
    }

    pub(crate) fn invoke(
        &self,
        id: TransformId,
        arguments: &mut [NativeArgument<'_>],
    ) -> Result<NativeResult, Diagnostic> {
        let Some(signature) = self.signatures.get(id.0 as usize).and_then(Option::as_ref) else {
            return Err(native_error(format!(
                "transform {} is not present in the native module",
                id.0
            )));
        };
        if arguments.len() != signature.parameters.len() {
            return Err(Diagnostic::error(
                format!(
                    "native transform expected {} argument(s), found {}",
                    signature.parameters.len(),
                    arguments.len()
                ),
                signature.span,
            ));
        }
        for (index, (argument, expected)) in arguments.iter().zip(&signature.parameters).enumerate()
        {
            if argument.ty() != *expected {
                return Err(Diagnostic::error(
                    format!(
                        "native argument {} expected {}, found {}",
                        index + 1,
                        expected.name(),
                        argument.ty().name()
                    ),
                    signature.span,
                ));
            }
        }
        let encoded = arguments
            .iter()
            .map(|argument| argument.encode())
            .collect::<Vec<_>>();
        let mut result = AbiValue::default();
        let entry = self.transforms[id.0 as usize]
            .expect("native signature and entry tables agree")
            .entry;
        // SAFETY: argument/result descriptors match the statically checked
        // signature, borrowed buffers outlive the call, and the library handle
        // outlives this copied function pointer.
        let status = unsafe { entry(std::ptr::null_mut(), encoded.as_ptr(), &mut result) };
        if status != ABI_STATUS_OK {
            return Err(Diagnostic::error(
                format!("native transform returned ABI status {status}"),
                signature.span,
            ));
        }
        if matches!(
            signature.result,
            Type::Bool | Type::U8 | Type::I64 | Type::F32
        ) {
            return Ok(NativeResult::Scalar(NativeScalar::decode(
                result,
                signature.result,
            )));
        }
        for (index, argument) in arguments.iter().enumerate() {
            if argument.encode() != result {
                continue;
            }
            return match (signature.result, argument) {
                (Type::Image, NativeArgument::Image(_)) => {
                    Ok(NativeResult::OwnedImageArgument(index))
                }
                (Type::ImageView, NativeArgument::ImageView(_)) => {
                    Ok(NativeResult::ImageViewArgument(index))
                }
                _ => continue,
            };
        }
        Err(Diagnostic::error(
            "native transform returned an image descriptor that does not identify a compatible input",
            signature.span,
        ))
    }
}

fn link_load_image(object_path: &Path, export_count: usize) -> Result<PathBuf, Diagnostic> {
    let directory = object_path
        .parent()
        .expect("cached native artifacts have a parent directory");
    let library_path = directory.join(host_library_file_name());
    if library_path.is_file() {
        return Ok(library_path);
    }

    let sequence = NEXT_LINK.fetch_add(1, Ordering::Relaxed);
    let temporary_name = format!(
        ".link-{}-{sequence}.{}",
        std::process::id(),
        host_library_extension()
    );
    let temporary_path = directory.join(&temporary_name);
    let compiler = clang_command();
    let mut command = Command::new(&compiler);
    if cfg!(target_os = "macos") {
        command.arg("-dynamiclib");
    } else {
        command.arg("-shared");
    }
    command
        .arg("-nostdlib")
        .arg(object_path)
        .arg("-o")
        .arg(&temporary_path);
    if cfg!(target_os = "windows") {
        command.arg("-Wl,/noentry");
        for index in 0..export_count {
            command.arg(format!("-Wl,/export:tima_transform_{index}"));
        }
    }
    let output = command.output().map_err(|error| {
        native_error(format!(
            "could not start native linker {}: {error}",
            compiler.display()
        ))
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(native_error(format!(
            "native linker {} failed with {}: {}",
            compiler.display(),
            output.status,
            stderr.trim()
        )));
    }
    match fs::rename(&temporary_path, &library_path) {
        Ok(()) => {}
        Err(_) if library_path.is_file() => {
            let _ = fs::remove_file(&temporary_path);
        }
        Err(error) => {
            return Err(native_error(format!(
                "could not publish native load image {}: {error}",
                library_path.display()
            )));
        }
    }
    remove_linker_sidecars(directory, &temporary_name);
    Ok(library_path)
}

fn clang_command() -> PathBuf {
    if let Some(configured) = std::env::var_os("TIMA_CLANG")
        && !configured.is_empty()
    {
        return configured.into();
    }
    if cfg!(target_os = "windows") {
        let llvm = PathBuf::from(r"C:\Program Files\LLVM\bin\clang.exe");
        if llvm.is_file() {
            return llvm;
        }
    }
    PathBuf::from("clang")
}

fn remove_linker_sidecars(directory: &Path, temporary_name: &str) {
    let stem = temporary_name
        .strip_suffix(&format!(".{}", host_library_extension()))
        .unwrap_or(temporary_name);
    for extension in ["lib", "exp"] {
        let _ = fs::remove_file(directory.join(format!("{stem}.{extension}")));
    }
}

const fn host_library_file_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "module.dll"
    } else if cfg!(target_os = "macos") {
        "module.dylib"
    } else {
        "module.so"
    }
}

const fn host_library_extension() -> &'static str {
    if cfg!(target_os = "windows") {
        "dll"
    } else if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    }
}

fn native_error(message: impl Into<String>) -> Diagnostic {
    Diagnostic::error(message, Span::default())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{
        NativeArgument, NativeImage, NativeImageView, NativeModule, NativeResult, NativeScalar,
    };
    use crate::backend::cache::ArtifactCacheStatus;
    use crate::ir::TransformId;

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    fn cache_root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("build")
            .join(format!(
                "native-loader-{}-{}",
                std::process::id(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed)
            ))
    }

    #[test]
    fn loads_and_executes_cached_scalar_artifacts() {
        let compiled = crate::compile(
            "native.tima",
            "transform choose(value: f32, threshold: f32) -> f32 {\n\
                 if value < threshold { return threshold } else { return value * 0.5 }\n\
             }\n\
             transform below(value: u8, threshold: u8) -> bool {\n\
                 return value < threshold\n\
             }\n\
             transform different(left: f32, right: f32) -> bool {\n\
                 return left != right\n\
             }\n\
             transform keep_integer(value: i64) -> i64 {\n\
                 return value\n\
             }\n\
             transform keep_byte(value: u8) -> u8 {\n\
                 return value\n\
             }\n",
        )
        .unwrap();
        let root = cache_root();
        let native = NativeModule::build(&compiled.transforms, &compiled.identities, &root)
            .unwrap()
            .unwrap();
        assert_eq!(native.artifact().status, ArtifactCacheStatus::Miss);
        assert_eq!(
            native
                .invoke_scalars(
                    TransformId(0),
                    &[NativeScalar::F32(8.0), NativeScalar::F32(3.0)]
                )
                .unwrap(),
            NativeScalar::F32(4.0)
        );
        assert_eq!(
            native
                .invoke_scalars(TransformId(1), &[NativeScalar::U8(2), NativeScalar::U8(3)])
                .unwrap(),
            NativeScalar::Bool(true)
        );
        assert_eq!(
            native
                .invoke_scalars(TransformId(3), &[NativeScalar::I64(-9_223_372_036)])
                .unwrap(),
            NativeScalar::I64(-9_223_372_036)
        );
        assert_eq!(
            native
                .invoke_scalars(TransformId(4), &[NativeScalar::U8(255)])
                .unwrap(),
            NativeScalar::U8(255)
        );
        assert_eq!(
            native
                .invoke_scalars(
                    TransformId(2),
                    &[NativeScalar::F32(f32::NAN), NativeScalar::F32(1.0)]
                )
                .unwrap(),
            NativeScalar::Bool(true)
        );
        drop(native);

        let cached = NativeModule::build(&compiled.transforms, &compiled.identities, &root)
            .unwrap()
            .unwrap();
        assert_eq!(cached.artifact().status, ArtifactCacheStatus::Hit);
    }

    #[test]
    fn selects_supported_leaf_transforms_and_leaves_fallbacks_absent() {
        let compiled = crate::compile(
            "hybrid.tima",
            "transform scale(value: f32, factor: f32) -> f32 { return value * factor }\n\
             transform checked(left: i64, right: i64) -> i64 { return left + right }\n",
        )
        .unwrap();
        let native = NativeModule::build(&compiled.transforms, &compiled.identities, cache_root())
            .unwrap()
            .unwrap();
        assert!(native.contains(TransformId(0)));
        assert!(!native.contains(TransformId(1)));
    }

    #[test]
    fn mutates_owned_images_and_returns_views_through_descriptors() {
        let compiled = crate::compile(
            "images.tima",
            "transform fill(img: Image, value: u8) -> Image { return image_fill(img, value) }\n\
             transform view(img: ImageView) -> ImageView { return img }\n\
             transform zero(img: Image) -> Image { return image_zero(img) }\n",
        )
        .unwrap();
        let native = NativeModule::build(&compiled.transforms, &compiled.identities, cache_root())
            .unwrap()
            .unwrap();

        let mut image = NativeImage {
            bytes: vec![1, 2, 3, 4],
            format: 0,
            width: 2,
            height: 2,
            stride: 2,
        };
        let owned_pointer = image.bytes.as_ptr();
        {
            let mut arguments = [
                NativeArgument::Image(&mut image),
                NativeArgument::Scalar(NativeScalar::U8(7)),
            ];
            assert_eq!(
                native.invoke(TransformId(0), &mut arguments).unwrap(),
                NativeResult::OwnedImageArgument(0)
            );
        }
        assert_eq!(image.bytes, vec![7, 7, 7, 7]);
        assert_eq!(image.bytes.as_ptr(), owned_pointer);

        let bytes = vec![9, 8, 7, 6];
        let mut arguments = [NativeArgument::ImageView(NativeImageView {
            bytes: &bytes,
            format: 0,
            width: 2,
            height: 2,
            stride: 2,
        })];
        assert_eq!(
            native.invoke(TransformId(1), &mut arguments).unwrap(),
            NativeResult::ImageViewArgument(0)
        );
        assert_eq!(bytes, vec![9, 8, 7, 6]);

        let mut image = NativeImage {
            bytes: vec![5, 4, 3, 2],
            format: 0,
            width: 2,
            height: 2,
            stride: 2,
        };
        {
            let mut arguments = [NativeArgument::Image(&mut image)];
            assert_eq!(
                native.invoke(TransformId(2), &mut arguments).unwrap(),
                NativeResult::OwnedImageArgument(0)
            );
        }
        assert_eq!(image.bytes, vec![0, 0, 0, 0]);
    }

    #[test]
    fn modules_without_supported_transforms_need_no_load_image() {
        let compiled = crate::compile(
            "interpreted.tima",
            "transform checked(left: i64, right: i64) -> i64 { return left + right }\n",
        )
        .unwrap();
        assert!(
            NativeModule::build(&compiled.transforms, &compiled.identities, cache_root())
                .unwrap()
                .is_none()
        );
    }
}
