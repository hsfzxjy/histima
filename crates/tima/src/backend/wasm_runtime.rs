use std::collections::BTreeMap;
use std::error::Error;
use std::ffi::c_void;
use std::fmt;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use wasmtime::{
    Cache, CacheConfig, Caller, Config, Engine, Extern, Global, GlobalType, Linker, Memory,
    MemoryType, Module, Mutability, Store, StoreLimits, StoreLimitsBuilder, Val, ValType,
};

use crate::abi::TIMA_ABI_VERSION;
use crate::backend::BackendArtifact;
use crate::backend::wasm::WASM_TARGET;
use crate::diagnostic::Diagnostic;
use crate::identity::{
    ArtifactBundleIdentity, ArtifactConfiguration, ArtifactIdentity, TransformIdentity,
    artifact_bundle_identity, artifact_identity, byte_content_identity,
};
use crate::ir::{TransformId, Type, TypedModule};

pub const DEFAULT_MEMORY_LIMIT: u64 = 4 * 1024 * 1024 * 1024;
const PAGE_SIZE: u64 = 65_536;
const OPTIMIZATION: &str = "speed";
static NEXT_BUILD: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactCacheStatus {
    Hit,
    Miss,
}

#[derive(Clone, Debug)]
pub struct CompiledWasmArtifact {
    pub backend: &'static str,
    pub backend_version: &'static str,
    pub abi_version: u32,
    pub compiler_version: &'static str,
    pub target: &'static str,
    pub optimization: &'static str,
    pub module_path: PathBuf,
    pub execution_cache_dir: PathBuf,
    pub static_size: u64,
}

impl CompiledWasmArtifact {
    pub fn identity(&self, transform: TransformIdentity) -> ArtifactIdentity {
        artifact_identity(
            transform,
            &ArtifactConfiguration {
                backend: self.backend,
                backend_version: self.backend_version,
                compiler_version: self.compiler_version,
                target: self.target,
                cpu_features: &[],
                optimization: self.optimization,
                abi_version: self.abi_version,
            },
        )
    }
}

#[derive(Clone, Debug)]
pub struct CachedWasmArtifact {
    pub artifact: CompiledWasmArtifact,
    pub artifact_ids: Vec<ArtifactIdentity>,
    pub bundle_id: ArtifactBundleIdentity,
    pub status: ArtifactCacheStatus,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct WasmArtifactCache;

impl WasmArtifactCache {
    pub fn store(
        &self,
        generated: &BackendArtifact,
        transforms: &[TransformIdentity],
        cache_root: impl AsRef<Path>,
    ) -> Result<CachedWasmArtifact, WasmError> {
        validate_module(&generated.bytes)?;
        let configuration = ArtifactConfiguration {
            backend: generated.backend,
            backend_version: generated.backend_version,
            compiler_version: "wasm-encoder-0.259.0",
            target: WASM_TARGET,
            cpu_features: &[],
            optimization: OPTIMIZATION,
            abi_version: generated.abi_version,
        };
        let artifact_ids = transforms
            .iter()
            .map(|transform| artifact_identity(*transform, &configuration))
            .collect::<Vec<_>>();
        let bundle_id = artifact_bundle_identity(&artifact_ids);
        let root = cache_root.as_ref().join("artifacts").join("wasm");
        let directory = root.join(bundle_id.to_string());
        let module_path = directory.join("module.wasm");
        let checksum_path = directory.join("module.sha256");
        let expected_checksum = byte_content_identity(&generated.bytes).to_string();
        if fs::read(&module_path).is_ok_and(|bytes| bytes == generated.bytes)
            && fs::read_to_string(&checksum_path)
                .is_ok_and(|checksum| checksum == expected_checksum)
        {
            return Ok(CachedWasmArtifact {
                artifact: compiled(generated, module_path, cache_root.as_ref().join("wasmtime")),
                artifact_ids,
                bundle_id,
                status: ArtifactCacheStatus::Hit,
            });
        }

        fs::create_dir_all(&root).map_err(|error| {
            WasmError::new(format!("failed to create {}: {error}", root.display()))
        })?;
        let sequence = NEXT_BUILD.fetch_add(1, Ordering::Relaxed);
        let temporary = root.join(format!(".build-{}-{sequence}", std::process::id()));
        if temporary.exists() {
            fs::remove_dir_all(&temporary).map_err(|error| {
                WasmError::new(format!("failed to remove {}: {error}", temporary.display()))
            })?;
        }
        fs::create_dir_all(&temporary).map_err(|error| {
            WasmError::new(format!("failed to create {}: {error}", temporary.display()))
        })?;
        fs::write(temporary.join("module.wasm"), &generated.bytes).map_err(|error| {
            WasmError::new(format!("failed to write WebAssembly module: {error}"))
        })?;
        fs::write(temporary.join("module.sha256"), &expected_checksum).map_err(|error| {
            WasmError::new(format!("failed to write WebAssembly checksum: {error}"))
        })?;
        if directory.exists() {
            fs::remove_dir_all(&directory).map_err(|error| {
                WasmError::new(format!(
                    "failed to replace {}: {error}",
                    directory.display()
                ))
            })?;
        }
        fs::rename(&temporary, &directory).map_err(|error| {
            WasmError::new(format!(
                "failed to publish {} as {}: {error}",
                temporary.display(),
                directory.display()
            ))
        })?;
        Ok(CachedWasmArtifact {
            artifact: compiled(
                generated,
                directory.join("module.wasm"),
                cache_root.as_ref().join("wasmtime"),
            ),
            artifact_ids,
            bundle_id,
            status: ArtifactCacheStatus::Miss,
        })
    }
}

fn compiled(
    generated: &BackendArtifact,
    module_path: PathBuf,
    execution_cache_dir: PathBuf,
) -> CompiledWasmArtifact {
    CompiledWasmArtifact {
        backend: generated.backend,
        backend_version: generated.backend_version,
        abi_version: generated.abi_version,
        compiler_version: "wasm-encoder-0.259.0",
        target: WASM_TARGET,
        optimization: OPTIMIZATION,
        module_path,
        execution_cache_dir,
        static_size: generated.static_size,
    }
}

fn validate_module(bytes: &[u8]) -> Result<(), WasmError> {
    let mut config = Config::new();
    config.wasm_memory64(true);
    let engine = Engine::new(&config).map_err(|error| WasmError::new(error.to_string()))?;
    Module::validate(&engine, bytes).map_err(|error| WasmError::new(error.to_string()))
}

#[derive(Clone, Copy)]
pub struct InvocationBridge {
    pub context: *mut c_void,
    pub environment_i64: unsafe fn(*mut c_void, u32, u32, &[u8]) -> Result<i64, Diagnostic>,
}

unsafe impl Send for InvocationBridge {}
unsafe impl Sync for InvocationBridge {}

#[derive(Clone, Copy, Debug)]
pub struct WasmImage {
    pub offset: u64,
    pub byte_len: u64,
    pub width: u64,
    pub height: u64,
    pub stride: u64,
    pub format: u32,
}

#[derive(Clone, Copy, Debug)]
pub enum WasmValue {
    Bool(bool),
    U8(u8),
    I64(i64),
    F32(f32),
    Image(WasmImage),
}

#[derive(Clone)]
pub struct WasmSession {
    inner: Arc<Mutex<SessionInner>>,
}

impl fmt::Debug for WasmSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WasmSession")
            .field("identity", &(Arc::as_ptr(&self.inner) as usize))
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct WasmBuffer {
    session: WasmSession,
    lease: Arc<AllocationLease>,
    len: u64,
}

impl fmt::Debug for WasmBuffer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WasmBuffer")
            .field("offset", &self.lease.range.start)
            .field("len", &self.len)
            .finish()
    }
}

impl PartialEq for WasmBuffer {
    fn eq(&self, other: &Self) -> bool {
        if self.same_allocation(other) && self.len == other.len {
            return true;
        }
        if self.len != other.len {
            return false;
        }
        let left = self.to_vec();
        other.with_bytes(|right| left == right)
    }
}

impl Eq for WasmBuffer {}

impl WasmBuffer {
    pub fn offset(&self) -> u64 {
        self.lease.range.start
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn session(&self) -> WasmSession {
        self.session.clone()
    }

    pub fn is_unique(&self) -> bool {
        Arc::strong_count(&self.lease) == 1
    }

    pub fn same_allocation(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.lease, &other.lease)
    }

    pub fn same_session(&self, session: &WasmSession) -> bool {
        Arc::ptr_eq(&self.session.inner, &session.inner)
    }

    pub fn with_bytes<R>(&self, operation: impl FnOnce(&[u8]) -> R) -> R {
        let inner = self
            .session
            .inner
            .lock()
            .expect("Wasm session lock poisoned");
        let start = self.offset() as usize;
        let end = start + self.len as usize;
        operation(&inner.memory.data(&inner.store)[start..end])
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.with_bytes(<[u8]>::to_vec)
    }
}

struct AllocationLease {
    owner: Weak<Mutex<SessionInner>>,
    range: Range<u64>,
}

impl Drop for AllocationLease {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade()
            && let Ok(mut inner) = owner.lock()
        {
            inner.allocator.free(self.range.clone());
        }
    }
}

struct StoreData {
    limits: StoreLimits,
    bridge: Option<InvocationBridge>,
    error: Option<Diagnostic>,
    fault: Option<(i32, u32, u32)>,
    memory: Option<Memory>,
}

struct SessionInner {
    store: Store<StoreData>,
    memory: Memory,
    invocations: Vec<wasmtime::Func>,
    signatures: Vec<(Vec<Type>, Type)>,
    allocator: ArenaAllocator,
    mirrors: BTreeMap<usize, Weak<AllocationLease>>,
}

#[derive(Debug)]
struct ArenaAllocator {
    next: u64,
    free: Vec<Range<u64>>,
    limit: u64,
}

impl ArenaAllocator {
    fn new(static_size: u64, limit: u64) -> Self {
        Self {
            next: align_up(static_size, 16),
            free: Vec::new(),
            limit,
        }
    }

    fn reserve(&mut self, len: u64) -> Result<Range<u64>, WasmError> {
        let physical = len.max(1);
        if let Some((index, range)) = self
            .free
            .iter()
            .enumerate()
            .find(|(_, range)| range.end - range.start >= physical)
            .map(|(index, range)| (index, range.clone()))
        {
            let allocated = range.start..range.start + physical;
            if allocated.end == range.end {
                self.free.remove(index);
            } else {
                self.free[index].start = allocated.end;
            }
            return Ok(allocated);
        }
        let start = align_up(self.next, 16);
        let end = start
            .checked_add(physical)
            .ok_or_else(|| WasmError::new("Wasm arena allocation overflow"))?;
        if end > self.limit {
            return Err(WasmError::new(format!(
                "Wasm memory limit of {} bytes exceeded",
                self.limit
            )));
        }
        self.next = end;
        Ok(start..end)
    }

    fn free(&mut self, range: Range<u64>) {
        let position = self
            .free
            .binary_search_by_key(&range.start, |candidate| candidate.start)
            .unwrap_or_else(|position| position);
        self.free.insert(position, range);
        let mut index = position.saturating_sub(1);
        while index + 1 < self.free.len() {
            if self.free[index].end == self.free[index + 1].start {
                let end = self.free[index + 1].end;
                self.free[index].end = end;
                self.free.remove(index + 1);
            } else {
                index += 1;
            }
        }
    }
}

impl WasmSession {
    pub fn instantiate(
        artifact: &CompiledWasmArtifact,
        module: &TypedModule,
        memory_limit: u64,
    ) -> Result<Self, WasmError> {
        validate_memory_limit(memory_limit)?;
        if artifact.abi_version != TIMA_ABI_VERSION {
            return Err(WasmError::new(format!(
                "artifact ABI version {} does not match Wasm ABI version {}",
                artifact.abi_version, TIMA_ABI_VERSION
            )));
        }
        if artifact.static_size > memory_limit {
            return Err(WasmError::new(
                "module static data exceeds the Wasm memory limit",
            ));
        }
        let mut config = Config::new();
        config.wasm_memory64(true);
        config.memory_reservation(memory_limit);
        let cache_directory = if artifact.execution_cache_dir.is_absolute() {
            artifact.execution_cache_dir.clone()
        } else {
            std::env::current_dir()
                .map_err(|error| WasmError::new(error.to_string()))?
                .join(&artifact.execution_cache_dir)
        };
        let mut cache_config = CacheConfig::new();
        cache_config.with_directory(cache_directory);
        let cache = Cache::new(cache_config).map_err(|error| WasmError::new(error.to_string()))?;
        config.cache(Some(cache));
        let engine = Engine::new(&config).map_err(|error| WasmError::new(error.to_string()))?;
        let bytes = fs::read(&artifact.module_path).map_err(|error| {
            WasmError::new(format!(
                "failed to read {}: {error}",
                artifact.module_path.display()
            ))
        })?;
        let compiled =
            Module::new(&engine, bytes).map_err(|error| WasmError::new(error.to_string()))?;
        let initial_pages = artifact.static_size.div_ceil(PAGE_SIZE);
        let maximum_pages = memory_limit / PAGE_SIZE;
        let limits = StoreLimitsBuilder::new()
            .memory_size(memory_limit as usize)
            .build();
        let mut store = Store::new(
            &engine,
            StoreData {
                limits,
                bridge: None,
                error: None,
                fault: None,
                memory: None,
            },
        );
        store.limiter(|data| &mut data.limits);
        let memory = Memory::new(
            &mut store,
            MemoryType::new64(initial_pages, Some(maximum_pages)),
        )
        .map_err(|error| WasmError::new(error.to_string()))?;
        store.data_mut().memory = Some(memory);
        let mut linker = Linker::new(&engine);
        linker
            .func_wrap(
                "tima",
                "environment_i64",
                |mut caller: Caller<'_, StoreData>,
                 transform: i32,
                 callsite: i32,
                 offset: i64,
                 len: i64|
                 -> Result<i64, wasmtime::Error> {
                    let result = invoke_environment(
                        &mut caller,
                        transform as u32,
                        callsite as u32,
                        offset as u64,
                        len as u64,
                    );
                    match result {
                        Ok(value) => Ok(value),
                        Err(error) => {
                            caller.data_mut().error = Some(error);
                            Err(wasmtime::Error::msg("Tima environment capability failed"))
                        }
                    }
                },
            )
            .map_err(|error| WasmError::new(error.to_string()))?;
        linker
            .func_wrap(
                "tima",
                "runtime_error",
                |mut caller: Caller<'_, StoreData>, code: i32, transform: i32, value: i32| {
                    caller.data_mut().fault = Some((code, transform as u32, value as u32));
                    Err::<(), _>(wasmtime::Error::msg("Tima runtime boundary error"))
                },
            )
            .map_err(|error| WasmError::new(error.to_string()))?;
        linker
            .define(&mut store, "tima", "memory", Extern::Memory(memory))
            .map_err(|error| WasmError::new(error.to_string()))?;
        let static_base = Global::new(
            &mut store,
            GlobalType::new(ValType::I64, Mutability::Const),
            Val::I64(0),
        )
        .map_err(|error| WasmError::new(error.to_string()))?;
        linker
            .define(
                &mut store,
                "tima",
                "static_base",
                Extern::Global(static_base),
            )
            .map_err(|error| WasmError::new(error.to_string()))?;
        let instance = linker
            .instantiate(&mut store, &compiled)
            .map_err(|error| WasmError::new(error.to_string()))?;
        let abi = instance
            .get_global(&mut store, "tima_abi_version")
            .and_then(|global| global.get(&mut store).i32())
            .ok_or_else(|| WasmError::new("module does not export a valid ABI version"))?;
        if abi as u32 != TIMA_ABI_VERSION {
            return Err(WasmError::new(format!(
                "module ABI version {abi} does not match runtime version {TIMA_ABI_VERSION}"
            )));
        }
        let mut invocations = Vec::with_capacity(module.transforms.len());
        for index in 0..module.transforms.len() {
            invocations.push(
                instance
                    .get_func(&mut store, &format!("tima_invoke_{index}"))
                    .ok_or_else(|| WasmError::new(format!("missing tima_invoke_{index}")))?,
            );
        }
        let signatures = module
            .transforms
            .iter()
            .map(|transform| {
                (
                    transform
                        .parameters
                        .iter()
                        .map(|parameter| parameter.ty)
                        .collect(),
                    transform.return_type,
                )
            })
            .collect();
        Ok(Self {
            inner: Arc::new(Mutex::new(SessionInner {
                store,
                memory,
                invocations,
                signatures,
                allocator: ArenaAllocator::new(artifact.static_size, memory_limit),
                mirrors: BTreeMap::new(),
            })),
        })
    }

    pub fn allocate(&self, len: u64) -> Result<WasmBuffer, WasmError> {
        let mut inner = self.inner.lock().expect("Wasm session lock poisoned");
        let range = inner.allocator.reserve(len)?;
        ensure_memory(&mut inner, range.end)?;
        drop(inner);
        Ok(WasmBuffer {
            session: self.clone(),
            lease: Arc::new(AllocationLease {
                owner: Arc::downgrade(&self.inner),
                range,
            }),
            len,
        })
    }

    pub fn allocate_copy(&self, bytes: &[u8]) -> Result<WasmBuffer, WasmError> {
        let buffer = self.allocate(bytes.len() as u64)?;
        let mut inner = self.inner.lock().expect("Wasm session lock poisoned");
        let start = buffer.offset() as usize;
        let end = start + bytes.len();
        let memory = inner.memory;
        memory.data_mut(&mut inner.store)[start..end].copy_from_slice(bytes);
        drop(inner);
        Ok(buffer)
    }

    pub fn mirror_host(&self, identity: usize, bytes: &[u8]) -> Result<WasmBuffer, WasmError> {
        if let Some(lease) = self
            .inner
            .lock()
            .expect("Wasm session lock poisoned")
            .mirrors
            .get(&identity)
            .and_then(Weak::upgrade)
        {
            return Ok(WasmBuffer {
                session: self.clone(),
                lease,
                len: bytes.len() as u64,
            });
        }
        let buffer = self.allocate_copy(bytes)?;
        self.inner
            .lock()
            .expect("Wasm session lock poisoned")
            .mirrors
            .insert(identity, Arc::downgrade(&buffer.lease));
        Ok(buffer)
    }

    pub fn copy_buffer(&self, source: &WasmBuffer) -> Result<WasmBuffer, WasmError> {
        if source.same_session(self) {
            let destination = self.allocate(source.len())?;
            let mut inner = self.inner.lock().expect("Wasm session lock poisoned");
            let source_start = source.offset() as usize;
            let destination_start = destination.offset() as usize;
            let memory = inner.memory;
            memory.data_mut(&mut inner.store).copy_within(
                source_start..source_start + source.len() as usize,
                destination_start,
            );
            drop(inner);
            Ok(destination)
        } else {
            source.with_bytes(|bytes| self.allocate_copy(bytes))
        }
    }

    pub fn invoke(
        &self,
        transform: TransformId,
        arguments: &[WasmValue],
        bridge: Option<InvocationBridge>,
    ) -> Result<WasmValue, WasmInvokeError> {
        let mut inner = self.inner.lock().expect("Wasm session lock poisoned");
        let Some(function) = inner.invocations.get(transform.0 as usize).copied() else {
            return Err(WasmInvokeError::Runtime(WasmError::new(format!(
                "Wasm transform index {} is unavailable",
                transform.0
            ))));
        };
        let Some((parameter_types, return_type)) =
            inner.signatures.get(transform.0 as usize).cloned()
        else {
            unreachable!()
        };
        let mut params = Vec::new();
        for (argument, ty) in arguments.iter().zip(parameter_types) {
            flatten_value(*argument, ty, &mut params)?;
        }
        let mut results = default_results(return_type);
        inner.store.data_mut().bridge = bridge;
        inner.store.data_mut().error = None;
        inner.store.data_mut().fault = None;
        let called = function.call(&mut inner.store, &params, &mut results);
        inner.store.data_mut().bridge = None;
        if let Some(error) = inner.store.data_mut().error.take() {
            return Err(WasmInvokeError::Diagnostic(error));
        }
        if let Some(fault) = inner.store.data_mut().fault.take() {
            return Err(WasmInvokeError::Fault(fault));
        }
        called.map_err(|error| WasmInvokeError::Runtime(WasmError::new(error.to_string())))?;
        unflatten_value(return_type, &results)
    }
}

fn ensure_memory(inner: &mut SessionInner, required: u64) -> Result<(), WasmError> {
    let current = inner.memory.data_size(&inner.store) as u64;
    if required <= current {
        return Ok(());
    }
    let pages = (required - current).div_ceil(PAGE_SIZE);
    inner
        .memory
        .grow(&mut inner.store, pages)
        .map_err(|error| WasmError::new(format!("failed to grow Wasm memory: {error}")))?;
    Ok(())
}

fn invoke_environment(
    caller: &mut Caller<'_, StoreData>,
    transform: u32,
    callsite: u32,
    offset: u64,
    len: u64,
) -> Result<i64, Diagnostic> {
    let Some(bridge) = caller.data().bridge else {
        return Err(Diagnostic::error(
            "environment capability is unavailable in this transform context",
            crate::source::Span::default(),
        ));
    };
    let memory = caller.data().memory.expect("session memory is installed");
    let start = usize::try_from(offset).map_err(|_| {
        Diagnostic::error(
            "environment name offset is out of range",
            crate::source::Span::default(),
        )
    })?;
    let len = usize::try_from(len).map_err(|_| {
        Diagnostic::error(
            "environment name length is out of range",
            crate::source::Span::default(),
        )
    })?;
    let end = start.checked_add(len).ok_or_else(|| {
        Diagnostic::error(
            "environment name range overflows",
            crate::source::Span::default(),
        )
    })?;
    let data = memory.data(&*caller);
    let name = data.get(start..end).ok_or_else(|| {
        Diagnostic::error(
            "environment name is outside Wasm memory",
            crate::source::Span::default(),
        )
    })?;
    unsafe { (bridge.environment_i64)(bridge.context, transform, callsite, name) }
}

fn flatten_value(
    value: WasmValue,
    expected: Type,
    output: &mut Vec<Val>,
) -> Result<(), WasmInvokeError> {
    match (value, expected) {
        (WasmValue::Bool(value), Type::Bool) => output.push(Val::I32(i32::from(value))),
        (WasmValue::U8(value), Type::U8) => output.push(Val::I32(i32::from(value))),
        (WasmValue::I64(value), Type::I64) => output.push(Val::I64(value)),
        (WasmValue::F32(value), Type::F32) => output.push(Val::F32(value.to_bits())),
        (WasmValue::Image(value), Type::Image | Type::ImageView) => {
            output.extend([
                Val::I64(value.offset as i64),
                Val::I64(value.byte_len as i64),
                Val::I64(value.width as i64),
                Val::I64(value.height as i64),
                Val::I64(value.stride as i64),
                Val::I32(value.format as i32),
            ]);
        }
        _ => {
            return Err(WasmInvokeError::Runtime(WasmError::new(
                "Wasm ABI argument type mismatch",
            )));
        }
    }
    Ok(())
}

fn default_results(ty: Type) -> Vec<Val> {
    match ty {
        Type::Bool | Type::U8 => vec![Val::I32(0)],
        Type::I64 => vec![Val::I64(0)],
        Type::F32 => vec![Val::F32(0)],
        Type::Image | Type::ImageView => vec![
            Val::I64(0),
            Val::I64(0),
            Val::I64(0),
            Val::I64(0),
            Val::I64(0),
            Val::I32(0),
        ],
    }
}

fn unflatten_value(ty: Type, values: &[Val]) -> Result<WasmValue, WasmInvokeError> {
    let mismatch = || WasmInvokeError::Runtime(WasmError::new("Wasm ABI result type mismatch"));
    Ok(match ty {
        Type::Bool => WasmValue::Bool(values[0].i32().ok_or_else(mismatch)? != 0),
        Type::U8 => WasmValue::U8(values[0].i32().ok_or_else(mismatch)? as u8),
        Type::I64 => WasmValue::I64(values[0].i64().ok_or_else(mismatch)?),
        Type::F32 => WasmValue::F32(values[0].f32().ok_or_else(mismatch)?),
        Type::Image | Type::ImageView => WasmValue::Image(WasmImage {
            offset: values[0].i64().ok_or_else(mismatch)? as u64,
            byte_len: values[1].i64().ok_or_else(mismatch)? as u64,
            width: values[2].i64().ok_or_else(mismatch)? as u64,
            height: values[3].i64().ok_or_else(mismatch)? as u64,
            stride: values[4].i64().ok_or_else(mismatch)? as u64,
            format: values[5].i32().ok_or_else(mismatch)? as u32,
        }),
    })
}

pub fn validate_memory_limit(limit: u64) -> Result<(), WasmError> {
    if limit == 0 || !limit.is_multiple_of(PAGE_SIZE) {
        return Err(WasmError::new(
            "Wasm memory limit must be a positive multiple of 64KiB",
        ));
    }
    if usize::try_from(limit).is_err() {
        return Err(WasmError::new(
            "Wasm memory limit exceeds host address space",
        ));
    }
    Ok(())
}

pub fn parse_memory_limit(text: &str) -> Result<u64, WasmError> {
    let (number, multiplier) = [
        ("TiB", 1024_u64.pow(4)),
        ("GiB", 1024_u64.pow(3)),
        ("MiB", 1024_u64.pow(2)),
        ("KiB", 1024_u64),
        ("B", 1_u64),
    ]
    .into_iter()
    .find_map(|(suffix, multiplier)| text.strip_suffix(suffix).map(|number| (number, multiplier)))
    .ok_or_else(|| WasmError::new("Wasm memory limit must use B, KiB, MiB, GiB, or TiB"))?;
    let number = number
        .parse::<u64>()
        .map_err(|_| WasmError::new("Wasm memory limit has an invalid unsigned integer"))?;
    let limit = number
        .checked_mul(multiplier)
        .ok_or_else(|| WasmError::new("Wasm memory limit overflows u64"))?;
    validate_memory_limit(limit)?;
    Ok(limit)
}

fn align_up(value: u64, alignment: u64) -> u64 {
    value.div_ceil(alignment) * alignment
}

#[derive(Debug)]
pub enum WasmInvokeError {
    Diagnostic(Diagnostic),
    Fault((i32, u32, u32)),
    Runtime(WasmError),
}

#[derive(Clone, Debug)]
pub struct WasmError {
    details: String,
}

impl WasmError {
    pub fn new(details: impl Into<String>) -> Self {
        Self {
            details: details.into(),
        }
    }
}

impl fmt::Display for WasmError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.details)
    }
}

impl Error for WasmError {}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::backend::ArtifactBackend;
    use crate::backend::wasm::WasmBackend;

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn parses_page_aligned_memory_limits() {
        assert_eq!(parse_memory_limit("64KiB").unwrap(), 65_536);
        assert_eq!(parse_memory_limit("4GiB").unwrap(), DEFAULT_MEMORY_LIMIT);
        assert!(parse_memory_limit("1MiB ").is_err());
        assert!(parse_memory_limit("1KB").is_err());
        assert!(parse_memory_limit("1B").is_err());
        assert!(parse_memory_limit("0GiB").is_err());
    }

    #[test]
    fn portable_artifact_cache_hits_and_repairs_corruption() {
        let root = test_directory("artifact-cache");
        let compiled = crate::compile(
            "cache.tima",
            "transform scale(value: f32, factor: f32) -> f32 { return value * factor }\n",
        )
        .unwrap();
        let generated = WasmBackend.emit(&compiled.transforms).unwrap();
        let transform_ids = compiled.identities.iter().collect::<Vec<_>>();
        let cache = WasmArtifactCache;

        let first = cache.store(&generated, &transform_ids, &root).unwrap();
        assert_eq!(first.status, ArtifactCacheStatus::Miss);
        assert_eq!(
            fs::read(&first.artifact.module_path).unwrap(),
            generated.bytes
        );

        let second = cache.store(&generated, &transform_ids, &root).unwrap();
        assert_eq!(second.status, ArtifactCacheStatus::Hit);
        assert_eq!(second.bundle_id, first.bundle_id);

        fs::write(&second.artifact.module_path, b"not wasm").unwrap();
        let repaired = cache.store(&generated, &transform_ids, &root).unwrap();
        assert_eq!(repaired.status, ArtifactCacheStatus::Miss);
        assert_eq!(
            fs::read(&repaired.artifact.module_path).unwrap(),
            generated.bytes
        );

        fs::remove_dir_all(root).unwrap();
    }

    fn test_directory(label: &str) -> PathBuf {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../build/tima-wasm-tests")
            .join(format!("{}-{label}-{sequence}", std::process::id()));
        if path.exists() {
            fs::remove_dir_all(&path).unwrap();
        }
        fs::create_dir_all(&path).unwrap();
        path
    }
}
