use std::collections::BTreeMap;
use std::ffi::c_void;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use libloading::Library;

use crate::abi::{
    ABI_ALLOCATION_BYTES, ABI_ALLOCATION_IMAGE, ABI_ALLOCATION_STRING, ABI_CAPACITY_WORD,
    ABI_IMAGE_FORMAT_WORD, ABI_IMAGE_HEIGHT_WORD, ABI_IMAGE_STRIDE_WORD, ABI_IMAGE_WIDTH_WORD,
    ABI_LENGTH_WORD, ABI_POINTER_WORD, ABI_STATUS_IMAGE_FORMAT, ABI_STATUS_OK, ABI_STATUS_RUNTIME,
    ABI_WORLD_ENVIRONMENT_READ, ABI_WORLD_FILE_READ, ABI_WORLD_HTTP_GET, AbiRuntimeContext,
    AbiValue, abi_callsite,
};
use crate::backend::ArtifactBackend;
use crate::backend::cache::{CachedArtifact, NativeArtifactCache};
use crate::backend::cranelift::CraneliftBackend;
use crate::capability::CapabilitySession;
use crate::diagnostic::Diagnostic;
use crate::identity::TransformIdentities;
use crate::ir::{TransformId, Type, TypedModule, ValueKind};
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

#[derive(Debug, PartialEq)]
pub(crate) struct NativeBuffer {
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct NativeBufferView<'a> {
    pub bytes: &'a [u8],
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
    String(&'a mut NativeBuffer),
    StringView(NativeBufferView<'a>),
    Bytes(&'a mut NativeBuffer),
    BytesView(NativeBufferView<'a>),
    Image(&'a mut NativeImage),
    ImageView(NativeImageView<'a>),
}

impl NativeArgument<'_> {
    fn ty(&self) -> Type {
        match self {
            Self::Scalar(value) => value.ty(),
            Self::String(_) => Type::String,
            Self::StringView(_) => Type::StringView,
            Self::Bytes(_) => Type::Bytes,
            Self::BytesView(_) => Type::BytesView,
            Self::Image(_) => Type::Image,
            Self::ImageView(_) => Type::ImageView,
        }
    }

    fn encode(&self) -> AbiValue {
        match self {
            Self::Scalar(value) => value.encode(),
            Self::String(buffer) | Self::Bytes(buffer) => buffer_value(
                buffer.bytes.as_ptr(),
                buffer.bytes.len(),
                buffer.bytes.capacity(),
            ),
            Self::StringView(buffer) | Self::BytesView(buffer) => {
                buffer_value(buffer.bytes.as_ptr(), buffer.bytes.len(), 0)
            }
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

#[derive(Debug, PartialEq)]
pub(crate) enum NativeResult {
    Scalar(NativeScalar),
    OwnedStringArgument(usize),
    OwnedStringAllocation(NativeBuffer),
    StringViewArgument(usize),
    OwnedBytesArgument(usize),
    OwnedBytesAllocation(NativeBuffer),
    BytesViewArgument(usize),
    OwnedImageArgument(usize),
    ImageViewArgument(usize),
}

fn buffer_value(pointer: *const u8, length: usize, capacity: usize) -> AbiValue {
    let mut value = AbiValue::default();
    value.words[ABI_POINTER_WORD] = pointer as usize as u64;
    value.words[ABI_LENGTH_WORD] = length as u64;
    value.words[ABI_CAPACITY_WORD] = capacity as u64;
    value
}

struct NativeAllocation {
    ty: Type,
    buffer: NativeBuffer,
}

struct NativeCallState<'a, 'world> {
    capabilities: &'a mut CapabilitySession<'world>,
    call_spans: &'a BTreeMap<u64, Span>,
    allocations: Vec<NativeAllocation>,
    diagnostic: Option<Diagnostic>,
}

impl NativeCallState<'_, '_> {
    fn span(&self, callsite: u64) -> Span {
        self.call_spans.get(&callsite).copied().unwrap_or_default()
    }

    fn fail(&mut self, diagnostic: Diagnostic) -> i32 {
        if self.diagnostic.is_none() {
            self.diagnostic = Some(diagnostic);
        }
        ABI_STATUS_RUNTIME
    }

    fn register_buffer(&mut self, ty: Type, bytes: Vec<u8>) -> AbiValue {
        self.allocations.push(NativeAllocation {
            ty,
            buffer: NativeBuffer { bytes },
        });
        let allocation = self.allocations.last().unwrap();
        buffer_value(
            allocation.buffer.bytes.as_ptr(),
            allocation.buffer.bytes.len(),
            allocation.buffer.bytes.capacity(),
        )
    }

    fn take_buffer(&mut self, ty: Type, descriptor: AbiValue) -> Option<NativeBuffer> {
        let index = self.allocations.iter().position(|allocation| {
            allocation.ty == ty
                && buffer_value(
                    allocation.buffer.bytes.as_ptr(),
                    allocation.buffer.bytes.len(),
                    allocation.buffer.bytes.capacity(),
                ) == descriptor
        })?;
        Some(self.allocations.swap_remove(index).buffer)
    }
}

unsafe extern "C" fn abi_allocate(
    user_data: *mut c_void,
    kind: u32,
    length: u64,
    result: *mut AbiValue,
) -> i32 {
    // SAFETY: `NativeModule::invoke_with_capabilities` installs this exact
    // state for the duration of the native call.
    let state = unsafe { &mut *user_data.cast::<NativeCallState<'_, '_>>() };
    let ty = match kind {
        ABI_ALLOCATION_STRING => Type::String,
        ABI_ALLOCATION_BYTES => Type::Bytes,
        ABI_ALLOCATION_IMAGE => {
            return state.fail(Diagnostic::error(
                "native image allocation requires layout metadata and is not available yet",
                Span::default(),
            ));
        }
        _ => {
            return state.fail(Diagnostic::error(
                format!("native transform requested unknown allocation kind {kind}"),
                Span::default(),
            ));
        }
    };
    let Ok(length) = usize::try_from(length) else {
        return state.fail(Diagnostic::error(
            "native allocation length does not fit the host address space",
            Span::default(),
        ));
    };
    let mut bytes = Vec::new();
    if let Err(error) = bytes.try_reserve_exact(length) {
        return state.fail(Diagnostic::error(
            format!("native allocation of {length} bytes failed: {error}"),
            Span::default(),
        ));
    }
    bytes.resize(length, 0);
    // SAFETY: the generated backend supplies a valid result descriptor.
    unsafe { result.write(state.register_buffer(ty, bytes)) };
    ABI_STATUS_OK
}

unsafe extern "C" fn abi_world_call(
    user_data: *mut c_void,
    callsite: u64,
    operation: u32,
    key: *const u8,
    key_length: u64,
    result: *mut AbiValue,
) -> i32 {
    // SAFETY: `NativeModule::invoke_with_capabilities` installs this exact
    // state for the duration of the native call.
    let state = unsafe { &mut *user_data.cast::<NativeCallState<'_, '_>>() };
    let span = state.span(callsite);
    let Ok(key_length) = usize::try_from(key_length) else {
        return state.fail(Diagnostic::error(
            "native World key length does not fit the host address space",
            span,
        ));
    };
    let key = if key_length == 0 {
        &[]
    } else {
        if key.is_null() {
            return state.fail(Diagnostic::error(
                "native World call supplied a null key pointer",
                span,
            ));
        }
        // SAFETY: the typed backend forwards a live StringView descriptor and
        // the call cannot outlive its retained outer storage.
        unsafe { std::slice::from_raw_parts(key, key_length) }
    };
    let Ok(key) = std::str::from_utf8(key) else {
        return state.fail(Diagnostic::error(
            "native World call key is not valid UTF-8",
            span,
        ));
    };
    let (ty, bytes) = match operation {
        ABI_WORLD_ENVIRONMENT_READ => match state.capabilities.environment(key, span) {
            Ok(value) => (Type::String, value.into_bytes()),
            Err(diagnostic) => return state.fail(diagnostic),
        },
        ABI_WORLD_FILE_READ => match state.capabilities.read_file(key, span) {
            Ok(value) => (Type::Bytes, value),
            Err(diagnostic) => return state.fail(diagnostic),
        },
        ABI_WORLD_HTTP_GET => match state.capabilities.http_get(key, span) {
            Ok(value) => (Type::Bytes, value),
            Err(diagnostic) => return state.fail(diagnostic),
        },
        _ => {
            return state.fail(Diagnostic::error(
                format!("native transform requested unknown World operation {operation}"),
                span,
            ));
        }
    };
    // SAFETY: the generated backend supplies a valid result descriptor.
    unsafe { result.write(state.register_buffer(ty, bytes)) };
    ABI_STATUS_OK
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
    rgba8_failure_span: Option<Span>,
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
    call_spans: BTreeMap<u64, Span>,
    artifact: CachedArtifact,
}

impl NativeModule {
    pub fn build(
        module: &TypedModule,
        identities: &TransformIdentities,
        cache_root: impl AsRef<Path>,
    ) -> Result<Option<Self>, Vec<Diagnostic>> {
        let supported = CraneliftBackend::supported_transforms(module);
        let selected = module
            .transforms
            .iter()
            .enumerate()
            .filter(|(index, _)| supported[*index])
            .collect::<Vec<_>>();
        if selected.is_empty() {
            return Ok(None);
        }

        let mut native_ids = vec![None; module.transforms.len()];
        for (native_index, (original_index, _)) in selected.iter().enumerate() {
            native_ids[*original_index] = Some(TransformId(native_index as u32));
        }
        let selected_module = TypedModule {
            transforms: selected
                .iter()
                .map(|(_, transform)| {
                    let mut transform = (*transform).clone();
                    for value in &mut transform.values {
                        if let ValueKind::Call {
                            transform: callee, ..
                        } = &mut value.kind
                        {
                            *callee = native_ids[callee.0 as usize]
                                .expect("native callers have native callees");
                        }
                    }
                    transform
                })
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
        let mut call_spans = BTreeMap::new();
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
                rgba8_failure_span: reachable_rgba8_span(
                    module,
                    TransformId(*original_index as u32),
                    &mut vec![false; module.transforms.len()],
                ),
                span: transform.span,
            });
            for (value_index, value) in transform.values.iter().enumerate() {
                if matches!(&value.kind, ValueKind::RuntimeCall(_)) {
                    call_spans.insert(
                        abi_callsite(native_index as u32, value_index as u32),
                        value.span,
                    );
                }
            }
        }

        Ok(Some(Self {
            _library: library,
            transforms,
            signatures,
            call_spans,
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
        let mut capabilities = CapabilitySession::new(None);
        self.invoke_with_capabilities(id, arguments, &mut capabilities)
    }

    pub(crate) fn invoke_with_capabilities(
        &self,
        id: TransformId,
        arguments: &mut [NativeArgument<'_>],
        capabilities: &mut CapabilitySession<'_>,
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
        let mut state = NativeCallState {
            capabilities,
            call_spans: &self.call_spans,
            allocations: Vec::new(),
            diagnostic: None,
        };
        let mut context = AbiRuntimeContext {
            user_data: std::ptr::from_mut(&mut state).cast(),
            allocate: abi_allocate,
            world_call: abi_world_call,
        };
        // SAFETY: argument/result descriptors match the statically checked
        // signature, borrowed buffers and the runtime callback table outlive
        // the call, and the library handle outlives this copied function
        // pointer.
        let status = unsafe {
            entry(
                std::ptr::from_mut(&mut context).cast(),
                encoded.as_ptr(),
                &mut result,
            )
        };
        if let Some(diagnostic) = state.diagnostic.take() {
            return Err(diagnostic);
        }
        if status == ABI_STATUS_IMAGE_FORMAT {
            return Err(Diagnostic::error(
                "image pixel iteration requires RGBA8 format",
                signature.rgba8_failure_span.unwrap_or(signature.span),
            ));
        }
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
                (Type::String, NativeArgument::String(_)) => {
                    Ok(NativeResult::OwnedStringArgument(index))
                }
                (Type::StringView, NativeArgument::StringView(_)) => {
                    Ok(NativeResult::StringViewArgument(index))
                }
                (Type::Bytes, NativeArgument::Bytes(_)) => {
                    Ok(NativeResult::OwnedBytesArgument(index))
                }
                (Type::BytesView, NativeArgument::BytesView(_)) => {
                    Ok(NativeResult::BytesViewArgument(index))
                }
                (Type::Image, NativeArgument::Image(_)) => {
                    Ok(NativeResult::OwnedImageArgument(index))
                }
                (Type::ImageView, NativeArgument::ImageView(_)) => {
                    Ok(NativeResult::ImageViewArgument(index))
                }
                _ => continue,
            };
        }
        match signature.result {
            Type::String => {
                if let Some(buffer) = state.take_buffer(Type::String, result) {
                    return Ok(NativeResult::OwnedStringAllocation(buffer));
                }
            }
            Type::Bytes => {
                if let Some(buffer) = state.take_buffer(Type::Bytes, result) {
                    return Ok(NativeResult::OwnedBytesAllocation(buffer));
                }
            }
            _ => {}
        }
        Err(Diagnostic::error(
            "native transform returned a descriptor that does not identify a compatible input",
            signature.span,
        ))
    }
}

fn reachable_rgba8_span(
    module: &TypedModule,
    id: TransformId,
    visiting: &mut [bool],
) -> Option<Span> {
    let index = id.0 as usize;
    if visiting[index] {
        return None;
    }
    visiting[index] = true;
    let transform = module.get(id);
    let span = transform.values.iter().find_map(|value| match &value.kind {
        ValueKind::ImageRgba8Scale { .. } => Some(value.span),
        ValueKind::Call {
            transform: callee, ..
        } => reachable_rgba8_span(module, *callee, visiting),
        _ => None,
    });
    visiting[index] = false;
    span
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
        NativeArgument, NativeBuffer, NativeBufferView, NativeCallState, NativeImage,
        NativeImageView, NativeModule, NativeResult, NativeScalar, abi_allocate,
    };
    use crate::abi::{
        ABI_ALLOCATION_BYTES, ABI_IMAGE_FORMAT_OPAQUE_BYTES, ABI_IMAGE_FORMAT_RGBA8,
        ABI_POINTER_WORD, ABI_STATUS_OK, AbiValue,
    };
    use crate::backend::cache::ArtifactCacheStatus;
    use crate::capability::{CapabilitySession, World};
    use crate::ir::{TransformId, Type};
    use crate::lineage::LineageNode;

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
    fn passes_owned_and_view_strings_and_bytes_without_descriptor_copies() {
        let compiled = crate::compile(
            "buffers.tima",
            "transform text(value: String) -> String { return value }\n\
             transform text_call(value: String) -> String { return text(value) }\n\
             transform text_view(value: StringView) -> StringView { return value }\n\
             transform bytes(value: Bytes) -> Bytes { return value }\n\
             transform bytes_call(value: Bytes) -> Bytes { return bytes(value) }\n\
             transform bytes_view(value: BytesView) -> BytesView { return value }\n",
        )
        .unwrap();
        let native = NativeModule::build(&compiled.transforms, &compiled.identities, cache_root())
            .unwrap()
            .unwrap();

        let mut text = NativeBuffer {
            bytes: "hello".as_bytes().to_vec(),
        };
        let text_pointer = text.bytes.as_ptr();
        let mut arguments = [NativeArgument::String(&mut text)];
        assert_eq!(
            native.invoke(TransformId(1), &mut arguments).unwrap(),
            NativeResult::OwnedStringArgument(0)
        );
        assert_eq!(text.bytes.as_ptr(), text_pointer);

        let text = "view";
        let mut arguments = [NativeArgument::StringView(NativeBufferView {
            bytes: text.as_bytes(),
        })];
        assert_eq!(
            native.invoke(TransformId(2), &mut arguments).unwrap(),
            NativeResult::StringViewArgument(0)
        );

        let mut bytes = NativeBuffer {
            bytes: vec![1, 2, 3, 4],
        };
        let bytes_pointer = bytes.bytes.as_ptr();
        let mut arguments = [NativeArgument::Bytes(&mut bytes)];
        assert_eq!(
            native.invoke(TransformId(4), &mut arguments).unwrap(),
            NativeResult::OwnedBytesArgument(0)
        );
        assert_eq!(bytes.bytes.as_ptr(), bytes_pointer);

        let bytes = [9, 8, 7];
        let mut arguments = [NativeArgument::BytesView(NativeBufferView {
            bytes: &bytes,
        })];
        assert_eq!(
            native.invoke(TransformId(5), &mut arguments).unwrap(),
            NativeResult::BytesViewArgument(0)
        );
    }

    #[test]
    fn host_allocator_registers_an_exactly_adoptable_buffer() {
        let mut capabilities = CapabilitySession::new(None);
        let spans = std::collections::BTreeMap::new();
        let mut state = NativeCallState {
            capabilities: &mut capabilities,
            call_spans: &spans,
            allocations: Vec::new(),
            diagnostic: None,
        };
        let mut descriptor = AbiValue::default();
        // SAFETY: this test supplies the callback's expected state and result
        // storage for the complete call.
        let status = unsafe {
            abi_allocate(
                std::ptr::from_mut(&mut state).cast(),
                ABI_ALLOCATION_BYTES,
                4,
                &mut descriptor,
            )
        };
        assert_eq!(status, ABI_STATUS_OK);
        // SAFETY: the successful allocator returned a live four-byte buffer.
        unsafe {
            std::slice::from_raw_parts_mut(descriptor.words[ABI_POINTER_WORD] as *mut u8, 4)
                .copy_from_slice(&[1, 2, 3, 4]);
        }
        assert_eq!(
            state.take_buffer(Type::Bytes, descriptor).unwrap().bytes,
            vec![1, 2, 3, 4]
        );
    }

    #[test]
    fn executes_world_calls_and_adopts_host_buffers() {
        struct FixedWorld;

        impl World for FixedWorld {
            fn environment(&self, name: &str) -> Result<Vec<u8>, String> {
                Ok(format!("env:{name}").into_bytes())
            }

            fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
                Ok(format!("file:{path}").into_bytes())
            }

            fn http_get(&self, url: &str) -> Result<Vec<u8>, String> {
                Ok(format!("http:{url}").into_bytes())
            }
        }

        let compiled = crate::compile(
            "world.tima",
            "transform environment(key: StringView) -> String uses env.read {
                 return env.read(key)
             }
             transform file(path: StringView) -> Bytes uses file.read {
                 return file.read(path)
             }
             transform http(url: StringView) -> Bytes uses http.get {
                 return http.get(url)
             }
             transform file_through_call(path: StringView) -> Bytes uses file.read {
                 return file(path)
             }
",
        )
        .unwrap();
        let native = NativeModule::build(&compiled.transforms, &compiled.identities, cache_root())
            .unwrap()
            .unwrap();
        for id in 0..4 {
            assert!(native.contains(TransformId(id)));
        }

        let world = FixedWorld;
        let mut capabilities = CapabilitySession::new(Some(&world));
        for (id, key, expected) in [
            (0, "MODE", b"env:MODE".as_slice()),
            (1, "asset.bin", b"file:asset.bin".as_slice()),
            (
                2,
                "https://example.test/a",
                b"http:https://example.test/a".as_slice(),
            ),
            (3, "nested.bin", b"file:nested.bin".as_slice()),
        ] {
            let mut arguments = [NativeArgument::StringView(NativeBufferView {
                bytes: key.as_bytes(),
            })];
            let result = native
                .invoke_with_capabilities(TransformId(id), &mut arguments, &mut capabilities)
                .unwrap();
            let bytes = match result {
                NativeResult::OwnedStringAllocation(buffer)
                | NativeResult::OwnedBytesAllocation(buffer) => buffer.bytes,
                other => panic!("expected allocated World result, found {other:?}"),
            };
            assert_eq!(bytes, expected);
        }
        let observations = capabilities.finish();
        assert_eq!(observations.len(), 4);
        assert!(
            observations.iter().all(|observation| matches!(
                observation.node(),
                LineageNode::ExternalObservation(_)
            ))
        );

        let key = "denied";
        let mut arguments = [NativeArgument::StringView(NativeBufferView {
            bytes: key.as_bytes(),
        })];
        let error = native.invoke(TransformId(1), &mut arguments).unwrap_err();
        assert!(error.message.contains("filesystem access is unavailable"));
        let expected_span = compiled.transforms.get(TransformId(1)).values[1].span;
        assert_eq!(error.labels[0].span, expected_span);
    }

    #[test]
    fn selects_supported_transforms_and_leaves_fallbacks_absent() {
        let compiled = crate::compile(
            "hybrid.tima",
            "transform scale(value: f32, factor: f32) -> f32 { return value * factor }\n\
             transform checked(left: i64, right: i64) -> i64 { return left + right }\n\
             transform checked_wrapper(left: i64, right: i64) -> i64 {\n\
                 return checked(left, right)\n\
             }\n",
        )
        .unwrap();
        let native = NativeModule::build(&compiled.transforms, &compiled.identities, cache_root())
            .unwrap()
            .unwrap();
        assert!(native.contains(TransformId(0)));
        assert!(!native.contains(TransformId(1)));
        assert!(!native.contains(TransformId(2)));
    }

    #[test]
    fn links_native_calls_and_maps_image_bytes_through_a_scalar_callee() {
        let compiled = crate::compile(
            "calls.tima",
            "transform checked(left: i64, right: i64) -> i64 { return left + right }\n\
             transform choose(current: u8, target: u8, replacement: u8) -> u8 {\n\
                 if current == target { return replacement } else { return current }\n\
             }\n\
             transform replace(img: Image, target: u8, replacement: u8) -> Image {\n\
                 for byte in img.bytes { byte = choose(byte, target, replacement) }\n\
                 return img\n\
             }\n\
             transform choose_once(current: u8, target: u8, replacement: u8) -> u8 {\n\
                 return choose(current, target, replacement)\n\
             }\n\
             transform clear(img: Image) -> Image { return image_zero(img) }\n\
             transform clear_through_call(img: Image) -> Image { return clear(img) }\n",
        )
        .unwrap();
        let native = NativeModule::build(&compiled.transforms, &compiled.identities, cache_root())
            .unwrap()
            .unwrap();
        assert!(!native.contains(TransformId(0)));
        for id in 1..=5 {
            assert!(native.contains(TransformId(id)));
        }

        let mut image = NativeImage {
            bytes: vec![1, 2, 1, 3],
            format: ABI_IMAGE_FORMAT_OPAQUE_BYTES,
            width: 2,
            height: 2,
            stride: 2,
        };
        {
            let mut arguments = [
                NativeArgument::Image(&mut image),
                NativeArgument::Scalar(NativeScalar::U8(1)),
                NativeArgument::Scalar(NativeScalar::U8(9)),
            ];
            assert_eq!(
                native.invoke(TransformId(2), &mut arguments).unwrap(),
                NativeResult::OwnedImageArgument(0)
            );
        }
        assert_eq!(image.bytes, vec![9, 2, 9, 3]);

        assert_eq!(
            native
                .invoke_scalars(
                    TransformId(3),
                    &[
                        NativeScalar::U8(4),
                        NativeScalar::U8(4),
                        NativeScalar::U8(7),
                    ],
                )
                .unwrap(),
            NativeScalar::U8(7)
        );

        let mut image = NativeImage {
            bytes: vec![8, 7, 6, 5],
            format: ABI_IMAGE_FORMAT_OPAQUE_BYTES,
            width: 2,
            height: 2,
            stride: 2,
        };
        {
            let mut arguments = [NativeArgument::Image(&mut image)];
            assert_eq!(
                native.invoke(TransformId(5), &mut arguments).unwrap(),
                NativeResult::OwnedImageArgument(0)
            );
        }
        assert_eq!(image.bytes, vec![0, 0, 0, 0]);
    }

    #[test]
    fn executes_owned_and_view_image_operations_through_descriptors() {
        let compiled = crate::compile(
            "images.tima",
            "transform fill(img: Image, value: u8) -> Image { return image_fill(img, value) }\n\
             transform view(img: ImageView) -> ImageView { return img }\n\
             transform zero(img: Image) -> Image { return image_zero(img) }\n\
             transform adjust(img: Image, r: f32, g: f32, b: f32, a: f32) -> Image {\n\
                 for p in img.pixels {\n\
                     p.r *= r\n\
                     p.g *= g\n\
                     p.b *= b\n\
                     p.a *= a\n\
                 }\n\
                 return img\n\
             }\n\
             transform adjust_through_call(img: Image, factor: f32) -> Image {\n\
                 return adjust(img, factor, factor, factor, factor)\n\
             }\n\
             transform prepare_and_adjust(img: Image, factor: f32) -> Image {\n\
                 prepared = zero(img)\n\
                 for p in prepared.pixels { p.r *= factor }\n\
                 return prepared\n\
             }\n",
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

        let mut image = NativeImage {
            bytes: vec![101, 200, 200, 1, 2, 3, 4, 5, 99, 100],
            format: ABI_IMAGE_FORMAT_RGBA8,
            width: 2,
            height: 1,
            stride: 10,
        };
        {
            let mut arguments = [
                NativeArgument::Image(&mut image),
                NativeArgument::Scalar(NativeScalar::F32(0.5)),
                NativeArgument::Scalar(NativeScalar::F32(2.0)),
                NativeArgument::Scalar(NativeScalar::F32(f32::NAN)),
                NativeArgument::Scalar(NativeScalar::F32(f32::INFINITY)),
            ];
            assert_eq!(
                native.invoke(TransformId(3), &mut arguments).unwrap(),
                NativeResult::OwnedImageArgument(0)
            );
        }
        assert_eq!(image.bytes, vec![50, 255, 0, 255, 1, 6, 0, 255, 99, 100]);

        let mut opaque = NativeImage {
            bytes: vec![1, 2, 3, 4],
            format: ABI_IMAGE_FORMAT_OPAQUE_BYTES,
            width: 1,
            height: 1,
            stride: 4,
        };
        let mut arguments = [
            NativeArgument::Image(&mut opaque),
            NativeArgument::Scalar(NativeScalar::F32(1.0)),
            NativeArgument::Scalar(NativeScalar::F32(1.0)),
            NativeArgument::Scalar(NativeScalar::F32(1.0)),
            NativeArgument::Scalar(NativeScalar::F32(1.0)),
        ];
        let error = native.invoke(TransformId(3), &mut arguments).unwrap_err();
        assert!(error.message.contains("requires RGBA8 format"));

        let mut opaque = NativeImage {
            bytes: vec![1, 2, 3, 4],
            format: ABI_IMAGE_FORMAT_OPAQUE_BYTES,
            width: 1,
            height: 1,
            stride: 4,
        };
        let mut arguments = [
            NativeArgument::Image(&mut opaque),
            NativeArgument::Scalar(NativeScalar::F32(0.5)),
        ];
        let error = native.invoke(TransformId(4), &mut arguments).unwrap_err();
        assert!(error.message.contains("requires RGBA8 format"));

        let mut opaque = NativeImage {
            bytes: vec![1, 2, 3, 4],
            format: ABI_IMAGE_FORMAT_OPAQUE_BYTES,
            width: 1,
            height: 1,
            stride: 4,
        };
        let mut arguments = [
            NativeArgument::Image(&mut opaque),
            NativeArgument::Scalar(NativeScalar::F32(0.5)),
        ];
        let error = native.invoke(TransformId(5), &mut arguments).unwrap_err();
        assert!(error.message.contains("requires RGBA8 format"));
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
