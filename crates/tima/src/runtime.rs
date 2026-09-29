use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use crate::CompiledProgram;
use crate::abi::{ABI_IMAGE_FORMAT_OPAQUE_BYTES, ABI_IMAGE_FORMAT_RGBA8};
use crate::ast::{Argument, BinaryOp, ExprId, ExprKind, Item};
use crate::backend::cache::CachedArtifact;
use crate::backend::native::{
    NativeArgument, NativeBuffer, NativeBufferView, NativeImage, NativeImageView, NativeModule,
    NativeResult, NativeScalar as AbiScalar,
};
use crate::cache::{ResultCache, TransformResultCache};
use crate::capability::{ASSET_CAPABILITY, CapabilitySession, World, observe_dependency};
use crate::diagnostic::Diagnostic;
use crate::identity::{ContentIdentity, content_identity};
use crate::ir::{Constant, RuntimeCall, Terminator, TransformId, Type, ValueId, ValueKind};
use crate::lineage::{Lineage, LineageArgument, LineageNode, RecordedValue};
use crate::registered::{RegisteredTransform, prepare_registered_invocation};
use crate::source::Span;

/// An immutable outer value. Composite payloads use immutable `Arc` storage;
/// there is no API that exposes mutable list, record, string, or asset data.
/// Semantic lineage is stored beside the payload and never crosses the native
/// ABI boundary.
#[derive(Clone, Debug, PartialEq)]
pub struct OuterValue {
    pub data: ValueData,
    pub lineage: Option<Lineage>,
}

impl OuterValue {
    pub fn plain(data: ValueData) -> Self {
        Self {
            data,
            lineage: None,
        }
    }

    pub fn image(image: ImageValue) -> Self {
        Self::plain(ValueData::Image(Arc::new(image)))
    }

    pub fn with_lineage(mut self, lineage: Lineage) -> Self {
        self.lineage = Some(lineage);
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ValueData {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f32),
    String(Arc<String>),
    Bytes(Arc<Vec<u8>>),
    List(Arc<[OuterValue]>),
    Record(Arc<BTreeMap<String, OuterValue>>),
    Asset(Arc<AssetValue>),
    Image(Arc<ImageValue>),
    Transform(TransformId),
    Lineage(Lineage),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetValue {
    /// A locator is not a content identity. Its source lineage remains
    /// unresolved until the host observes/materializes the asset.
    pub locator: Arc<str>,
}

#[derive(Clone, Debug)]
struct ImageStorage(Arc<Vec<u8>>);

impl ImageStorage {
    fn new(bytes: Vec<u8>) -> Self {
        Self(Arc::new(bytes))
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn with_bytes<R>(&self, operation: impl FnOnce(&[u8]) -> R) -> R {
        operation(self.0.as_slice())
    }

    fn to_vec(&self) -> Vec<u8> {
        self.with_bytes(<[u8]>::to_vec)
    }

    fn into_vec(self) -> Vec<u8> {
        Arc::try_unwrap(self.0).unwrap_or_else(|shared| (*shared).clone())
    }
}

impl PartialEq for ImageStorage {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_slice() == other.0.as_slice()
    }
}

impl Eq for ImageStorage {}

/// The semantic layout of an image's byte storage.
///
/// Opaque byte images preserve the initial runtime behavior and may be used by
/// byte-oriented transforms. RGBA8 is the first pixel-addressable layout: four
/// interleaved 8-bit channels per pixel, in red, green, blue, alpha order.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImageFormat {
    OpaqueBytes = ABI_IMAGE_FORMAT_OPAQUE_BYTES,
    Rgba8 = ABI_IMAGE_FORMAT_RGBA8,
}

impl ImageFormat {
    pub(crate) const fn abi_tag(self) -> u32 {
        self as u32
    }
}

impl fmt::Display for ImageFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpaqueBytes => formatter.write_str("opaque-bytes"),
            Self::Rgba8 => formatter.write_str("rgba8"),
        }
    }
}

/// Immutable outer image descriptor backed by shareable byte storage.
///
/// `width` and `height` are logical dimensions, while `stride` is the backing
/// byte count per row. The explicit format controls any pixel interpretation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageValue {
    storage: Arc<ImageStorage>,
    format: ImageFormat,
    width: usize,
    height: usize,
    stride: usize,
}

impl ImageValue {
    pub fn new(
        width: usize,
        height: usize,
        stride: usize,
        bytes: Vec<u8>,
    ) -> Result<Self, ImageLayoutError> {
        Self::with_format(ImageFormat::OpaqueBytes, width, height, stride, bytes)
    }

    pub fn new_rgba8(
        width: usize,
        height: usize,
        stride: usize,
        bytes: Vec<u8>,
    ) -> Result<Self, ImageLayoutError> {
        Self::with_format(ImageFormat::Rgba8, width, height, stride, bytes)
    }

    fn with_format(
        format: ImageFormat,
        width: usize,
        height: usize,
        stride: usize,
        bytes: Vec<u8>,
    ) -> Result<Self, ImageLayoutError> {
        validate_image_layout(format, width, height, stride, bytes.len())?;
        Ok(Self {
            storage: Arc::new(ImageStorage::new(bytes)),
            format,
            width,
            height,
            stride,
        })
    }

    pub fn format(&self) -> ImageFormat {
        self.format
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn stride(&self) -> usize {
        self.stride
    }

    pub fn byte_len(&self) -> usize {
        self.storage.len()
    }

    pub fn with_bytes<R>(&self, operation: impl FnOnce(&[u8]) -> R) -> R {
        self.storage.with_bytes(operation)
    }

    fn byte_slice(&self) -> &[u8] {
        self.storage.0.as_slice()
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.storage.to_vec()
    }

    #[cfg(test)]
    pub(crate) fn bytes(&self) -> Vec<u8> {
        self.to_vec()
    }

    pub fn shares_storage_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.storage, &other.storage)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageLayoutError {
    message: String,
}

impl fmt::Display for ImageLayoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ImageLayoutError {}

fn validate_image_layout(
    format: ImageFormat,
    width: usize,
    height: usize,
    stride: usize,
    byte_len: usize,
) -> Result<(), ImageLayoutError> {
    if format == ImageFormat::Rgba8 {
        let minimum_stride = width.checked_mul(4).ok_or_else(|| ImageLayoutError {
            message: "RGBA8 row byte length overflows usize".to_owned(),
        })?;
        if stride < minimum_stride {
            return Err(ImageLayoutError {
                message: format!(
                    "RGBA8 image stride {stride} is smaller than width {width} times 4 ({minimum_stride})"
                ),
            });
        }
    }
    let expected = height.checked_mul(stride).ok_or_else(|| ImageLayoutError {
        message: "image byte length overflows usize".to_owned(),
    })?;
    if byte_len != expected {
        return Err(ImageLayoutError {
            message: format!(
                "image storage has {byte_len} bytes, but height {height} and stride {stride} require {expected}"
            ),
        });
    }
    Ok(())
}

#[derive(Clone, Debug, Default)]
pub struct Execution {
    pub bindings: BTreeMap<String, OuterValue>,
    pub last_value: Option<OuterValue>,
}

#[derive(Clone, Debug)]
pub struct AotExecution {
    pub execution: Execution,
    pub artifact: Option<CachedArtifact>,
}

pub fn execute(program: &CompiledProgram) -> Result<Execution, Vec<Diagnostic>> {
    let engine = IrInterpreter {
        module: &program.transforms,
        capabilities: None,
    };
    execute_with(program, &engine, BTreeMap::new(), None)
}

pub fn execute_cached(
    program: &CompiledProgram,
    cache: &mut dyn ResultCache,
) -> Result<Execution, Vec<Diagnostic>> {
    let engine = IrInterpreter {
        module: &program.transforms,
        capabilities: None,
    };
    execute_with(program, &engine, BTreeMap::new(), Some(cache))
}

pub fn execute_with_capabilities(
    program: &CompiledProgram,
    capabilities: &dyn World,
) -> Result<Execution, Vec<Diagnostic>> {
    let engine = IrInterpreter {
        module: &program.transforms,
        capabilities: Some(capabilities),
    };
    execute_with(program, &engine, BTreeMap::new(), None)
}

pub fn execute_cached_with_capabilities(
    program: &CompiledProgram,
    cache: &mut dyn ResultCache,
    capabilities: &dyn World,
) -> Result<Execution, Vec<Diagnostic>> {
    let engine = IrInterpreter {
        module: &program.transforms,
        capabilities: Some(capabilities),
    };
    execute_with(program, &engine, BTreeMap::new(), Some(cache))
}

/// Executes AOT-compatible transforms from a cached native load image and
/// falls back to the typed-IR interpreter for every other transform.
pub fn execute_aot_cached_with_capabilities(
    program: &CompiledProgram,
    cache: &mut dyn ResultCache,
    capabilities: &dyn World,
    artifact_cache_root: impl AsRef<Path>,
) -> Result<AotExecution, Vec<Diagnostic>> {
    let native = NativeModule::build(
        &program.transforms,
        &program.identities,
        artifact_cache_root,
    )?;
    let artifact = native.as_ref().map(|module| module.artifact().clone());
    let interpreter = IrInterpreter {
        module: &program.transforms,
        capabilities: Some(capabilities),
    };
    let engine = HybridAotEngine {
        interpreter,
        native: native.as_ref(),
    };
    let execution = execute_with(program, &engine, BTreeMap::new(), Some(cache))?;
    Ok(AotExecution {
        execution,
        artifact,
    })
}

/// Host hook used to validate precise external observations before replay.
/// Replay without external observations does not require a resolver.
pub trait ReplayDependencyResolver {
    fn observe(&self, capability: &str, key: &[u8]) -> Result<ContentIdentity, String>;
}

struct CapabilityReplayResolver<'a>(&'a dyn World);

impl ReplayDependencyResolver for CapabilityReplayResolver<'_> {
    fn observe(&self, capability: &str, key: &[u8]) -> Result<ContentIdentity, String> {
        observe_dependency(self.0, capability, key)
    }
}

pub fn replay(
    program: &CompiledProgram,
    target: &OuterValue,
    cache: &mut dyn ResultCache,
) -> Result<OuterValue, Diagnostic> {
    replay_with_dependencies(program, target, cache, None)
}

pub fn replay_with_dependencies(
    program: &CompiledProgram,
    target: &OuterValue,
    cache: &mut dyn ResultCache,
    dependencies: Option<&dyn ReplayDependencyResolver>,
) -> Result<OuterValue, Diagnostic> {
    let engine = IrInterpreter {
        module: &program.transforms,
        capabilities: None,
    };
    replay_with(
        program,
        &engine,
        target,
        cache,
        dependencies,
        Span::default(),
    )
}

pub fn replay_with_capabilities(
    program: &CompiledProgram,
    target: &OuterValue,
    cache: &mut dyn ResultCache,
    capabilities: &dyn World,
) -> Result<OuterValue, Diagnostic> {
    let engine = IrInterpreter {
        module: &program.transforms,
        capabilities: Some(capabilities),
    };
    let resolver = CapabilityReplayResolver(capabilities);
    replay_with(
        program,
        &engine,
        target,
        cache,
        Some(&resolver),
        Span::default(),
    )
}

fn execute_with<'cache>(
    program: &CompiledProgram,
    engine: &dyn TransformEngine,
    bindings: BTreeMap<String, OuterValue>,
    cache: Option<&'cache mut (dyn ResultCache + 'cache)>,
) -> Result<Execution, Vec<Diagnostic>> {
    Interpreter {
        program,
        engine,
        execution: Execution {
            bindings,
            last_value: None,
        },
        cache,
    }
    .run()
    .map_err(|diagnostic| vec![diagnostic])
}

trait TransformEngine {
    fn invoke(
        &self,
        id: TransformId,
        arguments: Vec<(OuterValue, Span)>,
    ) -> Result<TransformOutcome, Diagnostic>;

    fn may_observe_dependencies(&self, _id: TransformId) -> bool {
        false
    }

    fn capabilities(&self) -> Option<&dyn World> {
        None
    }
}

struct TransformOutcome {
    value: OuterValue,
    observations: Vec<Lineage>,
}

fn invoke_transform_with_lineage<'cache>(
    program: &CompiledProgram,
    engine: &dyn TransformEngine,
    id: TransformId,
    arguments: Vec<(OuterValue, Span)>,
    mut cache: Option<&mut (dyn ResultCache + 'cache)>,
) -> Result<OuterValue, Diagnostic> {
    let transform = program.transforms.get(id);
    let recorded = transform
        .parameters
        .iter()
        .zip(&arguments)
        .map(|(parameter, (argument, span))| {
            LineageArgument::record(parameter.name.as_str(), argument).map_err(|error| {
                Diagnostic::error(
                    format!(
                        "cannot record argument `{}` for transform lineage: {error}",
                        parameter.name
                    ),
                    *span,
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(cache) = cache.as_deref_mut() {
        for (recorded, (argument, span)) in recorded.iter().zip(&arguments) {
            let RecordedValue::Materialized { content_id, .. } = recorded.value else {
                continue;
            };
            let remembered = cache
                .remember(argument)
                .map_err(|error| Diagnostic::error(error.to_string(), *span))?;
            if remembered != content_id {
                return Err(Diagnostic::error(
                    format!(
                        "recorded argument content identity {content_id} does not match stored content {remembered}"
                    ),
                    *span,
                ));
            }
        }
    }
    if !engine.may_observe_dependencies(id) {
        let lineage = Lineage::invocation(
            transform.name.as_str(),
            program.identities.get(id),
            recorded.clone(),
            vec![],
        )
        .map_err(|error| Diagnostic::error(error.to_string(), transform.span))?;
        let recipe = lineage
            .recipe_id()
            .expect("invocation lineage always has a recipe identity");
        if let Some(cache) = cache.as_deref_mut()
            && let Some(mut value) = cache
                .lookup(recipe)
                .map_err(|error| Diagnostic::error(error.to_string(), transform.span))?
        {
            value.lineage = Some(lineage);
            return Ok(value);
        }
    }
    let TransformOutcome {
        mut value,
        observations,
    } = engine.invoke(id, arguments)?;
    let lineage = Lineage::invocation(
        transform.name.as_str(),
        program.identities.get(id),
        recorded,
        observations,
    )
    .map_err(|error| Diagnostic::error(error.to_string(), transform.span))?;
    if let Some(cache) = cache {
        cache
            .store(
                lineage
                    .recipe_id()
                    .expect("invocation lineage always has a recipe identity"),
                &value,
            )
            .map_err(|error| Diagnostic::error(error.to_string(), transform.span))?;
    }
    value.lineage = Some(lineage);
    Ok(value)
}

fn invoke_registered_transform_with_lineage<'cache>(
    transform: &'static RegisteredTransform,
    arguments: Vec<(OuterValue, Span)>,
    mut cache: Option<&mut (dyn ResultCache + 'cache)>,
    span: Span,
) -> Result<OuterValue, Diagnostic> {
    let prepared = prepare_registered_invocation(transform, arguments, span)?;
    let recorded = transform
        .parameters()
        .iter()
        .zip(&prepared.arguments)
        .map(|(name, (argument, argument_span))| {
            LineageArgument::record(*name, argument).map_err(|error| {
                Diagnostic::error(
                    format!(
                        "cannot record argument `{name}` for registered transform lineage: {error}"
                    ),
                    *argument_span,
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(cache) = cache.as_deref_mut() {
        for (recorded, (argument, argument_span)) in recorded.iter().zip(&prepared.arguments) {
            let RecordedValue::Materialized { content_id, .. } = recorded.value else {
                continue;
            };
            let remembered = cache
                .remember(argument)
                .map_err(|error| Diagnostic::error(error.to_string(), *argument_span))?;
            if remembered != content_id {
                return Err(Diagnostic::error(
                    "recorded registered-transform argument does not match stored content",
                    *argument_span,
                ));
            }
        }
    }
    let lineage = Lineage::invocation(transform.name(), transform.identity(), recorded, vec![])
        .map_err(|error| Diagnostic::error(error.to_string(), span))?;
    let recipe = lineage
        .recipe_id()
        .expect("registered invocation lineage has a recipe identity");
    if let Some(cache) = cache.as_deref_mut()
        && let Some(mut value) = cache
            .lookup(recipe)
            .map_err(|error| Diagnostic::error(error.to_string(), span))?
    {
        value.lineage = Some(lineage);
        return Ok(value);
    }
    let mut value = prepared.execute()?;
    if let Some(cache) = cache {
        cache
            .store(recipe, &value)
            .map_err(|error| Diagnostic::error(error.to_string(), span))?;
    }
    value.lineage = Some(lineage);
    Ok(value)
}

fn replay_with(
    program: &CompiledProgram,
    engine: &dyn TransformEngine,
    target: &OuterValue,
    cache: &mut dyn ResultCache,
    dependencies: Option<&dyn ReplayDependencyResolver>,
    span: Span,
) -> Result<OuterValue, Diagnostic> {
    let Some(lineage) = &target.lineage else {
        return Err(Diagnostic::error(
            "replay expects a value produced by a recorded transform invocation",
            span,
        ));
    };
    let expected_content = content_identity(target).map_err(|error| {
        Diagnostic::error(
            format!("replay target has no materialized content identity: {error}"),
            span,
        )
    })?;
    replay_lineage(
        program,
        engine,
        lineage,
        Some(expected_content),
        cache,
        dependencies,
        span,
        0,
    )
}

#[derive(Clone, Copy)]
enum ReplayTransform {
    Inner(TransformId),
    Registered(&'static RegisteredTransform),
}

fn resolve_replay_transform(
    program: &CompiledProgram,
    invocation: &crate::lineage::InvocationLineage,
    span: Span,
) -> Result<ReplayTransform, Diagnostic> {
    let replay_transform = if let Some(transform_id) =
        program.identities.find_id(invocation.transform_id)
    {
        ReplayTransform::Inner(transform_id)
    } else if let Some(registered) = RegisteredTransform::from_identity(invocation.transform_id) {
        ReplayTransform::Registered(registered)
    } else {
        return Err(Diagnostic::error(
            format!(
                "recorded transform definition {} is unavailable or has changed",
                invocation.transform_id
            ),
            span,
        ));
    };
    let (transform_name, parameter_count) = match replay_transform {
        ReplayTransform::Inner(id) => {
            let transform = program.transforms.get(id);
            (transform.name.as_str(), transform.parameters.len())
        }
        ReplayTransform::Registered(registered) => {
            (registered.name(), registered.parameters().len())
        }
    };
    if parameter_count != invocation.arguments.len() {
        return Err(Diagnostic::error(
            format!(
                "recorded invocation has {} arguments, but transform `{}` now expects {}",
                invocation.arguments.len(),
                transform_name,
                parameter_count
            ),
            span,
        ));
    }
    Ok(replay_transform)
}

#[allow(clippy::too_many_arguments)]
fn replay_lineage(
    program: &CompiledProgram,
    engine: &dyn TransformEngine,
    lineage: &Lineage,
    expected_content: Option<ContentIdentity>,
    cache: &mut dyn ResultCache,
    dependencies: Option<&dyn ReplayDependencyResolver>,
    span: Span,
    depth: usize,
) -> Result<OuterValue, Diagnostic> {
    if depth >= 256 {
        return Err(Diagnostic::error(
            "replay lineage depth exceeded the runtime limit",
            span,
        ));
    }
    let LineageNode::Invocation(invocation) = lineage.node() else {
        return Err(Diagnostic::error(
            "replay expects transform invocation lineage",
            span,
        ));
    };
    let replay_transform = resolve_replay_transform(program, invocation, span)?;
    validate_replay_dependencies(program, invocation, dependencies, span, depth)?;

    if let Some(mut value) = cache
        .lookup(invocation.recipe_id)
        .map_err(|error| Diagnostic::error(error.to_string(), span))?
    {
        validate_replay_content(&value, expected_content, span)?;
        value.lineage = Some(lineage.clone());
        return Ok(value);
    }

    let mut arguments = Vec::with_capacity(invocation.arguments.len());
    for argument in invocation.arguments.iter() {
        arguments.push(replay_argument(
            program,
            engine,
            argument,
            cache,
            dependencies,
            span,
            depth + 1,
        )?);
    }
    let runtime_arguments = arguments
        .into_iter()
        .map(|argument| (argument, span))
        .collect::<Vec<_>>();
    let (mut value, observed_lineage) = match replay_transform {
        ReplayTransform::Inner(transform_id) => {
            let transform = program.transforms.get(transform_id);
            let TransformOutcome {
                value,
                observations,
            } = engine.invoke(transform_id, runtime_arguments)?;
            let lineage = Lineage::invocation(
                transform.name.as_str(),
                program.identities.get(transform_id),
                invocation.arguments.to_vec(),
                observations,
            )
            .map_err(|error| Diagnostic::error(error.to_string(), span))?;
            (value, lineage)
        }
        ReplayTransform::Registered(registered) => {
            let prepared = prepare_registered_invocation(registered, runtime_arguments, span)?;
            let arguments = registered
                .parameters()
                .iter()
                .zip(&prepared.arguments)
                .map(|(name, (value, argument_span))| {
                    LineageArgument::record(*name, value)
                        .map_err(|error| Diagnostic::error(error.to_string(), *argument_span))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let lineage =
                Lineage::invocation(registered.name(), registered.identity(), arguments, vec![])
                    .map_err(|error| Diagnostic::error(error.to_string(), span))?;
            (prepared.execute()?, lineage)
        }
    };
    let observed_recipe = observed_lineage
        .recipe_id()
        .expect("invocation lineage always has a recipe identity");
    if observed_recipe != invocation.recipe_id {
        return Err(Diagnostic::error(
            format!(
                "replay expected recipe {} but execution observed {}",
                invocation.recipe_id, observed_recipe
            ),
            span,
        )
        .with_note("recorded external dependencies or semantic arguments no longer match"));
    }
    validate_replay_content(&value, expected_content, span)?;
    cache
        .store(invocation.recipe_id, &value)
        .map_err(|error| Diagnostic::error(error.to_string(), span))?;
    value.lineage = Some(lineage.clone());
    Ok(value)
}

#[allow(clippy::too_many_arguments)]
fn replay_argument(
    program: &CompiledProgram,
    engine: &dyn TransformEngine,
    argument: &LineageArgument,
    cache: &mut dyn ResultCache,
    dependencies: Option<&dyn ReplayDependencyResolver>,
    span: Span,
    depth: usize,
) -> Result<OuterValue, Diagnostic> {
    let mut value = match &argument.value {
        RecordedValue::Null => OuterValue::plain(ValueData::Null),
        RecordedValue::Bool(value) => OuterValue::plain(ValueData::Bool(*value)),
        RecordedValue::Integer(value) => OuterValue::plain(ValueData::Integer(*value)),
        RecordedValue::Float(value) => OuterValue::plain(ValueData::Float(*value)),
        RecordedValue::String(value) => OuterValue::plain(ValueData::String(value.clone())),
        RecordedValue::Materialized { content_id, .. } => {
            if let Some(value) = cache
                .materialized(*content_id)
                .map_err(|error| Diagnostic::error(error.to_string(), span))?
            {
                value
            } else if let Some(lineage) = &argument.lineage
                && let LineageNode::Source(source) = lineage.node()
            {
                let Some(capabilities) = engine.capabilities() else {
                    return Err(Diagnostic::error(
                        format!(
                            "replay requires an asset reader to restore source {:?}",
                            source.locator
                        ),
                        span,
                    ));
                };
                let bytes = capabilities.read_asset(&source.locator).map_err(|error| {
                    Diagnostic::error(
                        format!("could not restore source {:?}: {error}", source.locator),
                        span,
                    )
                })?;
                OuterValue::plain(ValueData::Bytes(Arc::new(bytes)))
            } else if let Some(parent) = &argument.lineage {
                replay_lineage(
                    program,
                    engine,
                    parent,
                    Some(*content_id),
                    cache,
                    dependencies,
                    span,
                    depth,
                )?
            } else {
                return Err(Diagnostic::error(
                    format!(
                        "replay requires materialized argument content {content_id}, but it is unavailable"
                    ),
                    span,
                )
                .with_note("restore the content-addressed input before replaying this recipe"));
            }
        }
        RecordedValue::Source { locator, .. } => {
            OuterValue::plain(ValueData::Asset(Arc::new(AssetValue {
                locator: locator.clone(),
            })))
        }
    };
    value.lineage = argument.lineage.clone();
    let restored = LineageArgument::record(argument.name.clone(), &value).map_err(|error| {
        Diagnostic::error(
            format!(
                "cannot restore replay argument `{}`: {error}",
                argument.name
            ),
            span,
        )
    })?;
    if restored.semantic_identity != argument.semantic_identity {
        return Err(Diagnostic::error(
            format!(
                "replay argument `{}` expected semantic identity {} but restored {}",
                argument.name, argument.semantic_identity, restored.semantic_identity
            ),
            span,
        ));
    }
    Ok(value)
}

fn validate_replay_dependencies(
    program: &CompiledProgram,
    invocation: &crate::lineage::InvocationLineage,
    resolver: Option<&dyn ReplayDependencyResolver>,
    span: Span,
    depth: usize,
) -> Result<(), Diagnostic> {
    if depth >= 256 {
        return Err(Diagnostic::error(
            "replay lineage depth exceeded the runtime limit",
            span,
        ));
    }
    for observation in invocation.observations.iter() {
        let LineageNode::ExternalObservation(observation) = observation.node() else {
            return Err(Diagnostic::error(
                "recorded invocation contains invalid dependency lineage",
                span,
            ));
        };
        let Some(resolver) = resolver else {
            return Err(Diagnostic::error(
                format!(
                    "replay requires a resolver for external `{}` dependency {:?}",
                    observation.capability,
                    String::from_utf8_lossy(&observation.key)
                ),
                span,
            ));
        };
        let observed = resolver
            .observe(&observation.capability, &observation.key)
            .map_err(|error| {
                Diagnostic::error(
                    format!(
                        "could not validate external `{}` dependency {:?}: {error}",
                        observation.capability,
                        String::from_utf8_lossy(&observation.key)
                    ),
                    span,
                )
            })?;
        if observed != observation.observed_content {
            return Err(Diagnostic::error(
                format!(
                    "replay expected external `{}` dependency {:?} content {} but observed {}",
                    observation.capability,
                    String::from_utf8_lossy(&observation.key),
                    observation.observed_content,
                    observed
                ),
                span,
            ));
        }
    }
    for argument in invocation.arguments.iter() {
        let Some(lineage) = &argument.lineage else {
            continue;
        };
        match (&argument.value, lineage.node()) {
            (
                RecordedValue::Source { .. } | RecordedValue::Materialized { kind: "bytes", .. },
                LineageNode::Source(source),
            ) => {
                let Some(expected) = source.observed_content else {
                    return Err(Diagnostic::error(
                        format!("replay source {:?} was never observed", source.locator),
                        span,
                    ));
                };
                let Some(resolver) = resolver else {
                    return Err(Diagnostic::error(
                        format!(
                            "replay requires an asset resolver for source {:?}",
                            source.locator
                        ),
                        span,
                    ));
                };
                let observed = resolver
                    .observe(ASSET_CAPABILITY, source.locator.as_bytes())
                    .map_err(|error| {
                        Diagnostic::error(
                            format!("could not validate source {:?}: {error}", source.locator),
                            span,
                        )
                    })?;
                if observed != expected {
                    return Err(Diagnostic::error(
                        format!(
                            "replay expected source {:?} content {expected} but observed {observed}",
                            source.locator
                        ),
                        span,
                    ));
                }
            }
            (_, LineageNode::Invocation(parent)) => {
                resolve_replay_transform(program, parent, span)?;
                validate_replay_dependencies(program, parent, resolver, span, depth + 1)?;
            }
            (_, LineageNode::Source(_)) => {}
            (_, LineageNode::ExternalObservation(_)) => {
                return Err(Diagnostic::error(
                    "replay argument carries invalid observation lineage",
                    span,
                ));
            }
        }
    }
    Ok(())
}

fn validate_replay_content(
    value: &OuterValue,
    expected: Option<ContentIdentity>,
    span: Span,
) -> Result<(), Diagnostic> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let observed = content_identity(value).map_err(|error| {
        Diagnostic::error(
            format!("replayed value has no materialized content identity: {error}"),
            span,
        )
    })?;
    if observed != expected {
        return Err(Diagnostic::error(
            format!("replay expected content hash {expected} but observed {observed}"),
            span,
        ));
    }
    Ok(())
}

struct Interpreter<'program, 'engine, 'cache> {
    program: &'program CompiledProgram,
    engine: &'engine dyn TransformEngine,
    execution: Execution,
    cache: Option<&'cache mut (dyn ResultCache + 'cache)>,
}

impl Interpreter<'_, '_, '_> {
    fn run(mut self) -> Result<Execution, Diagnostic> {
        for item in &self.program.syntax.items {
            match item {
                Item::Transform(_) => {}
                Item::Binding(binding) => {
                    if self.execution.bindings.contains_key(&binding.name) {
                        return Err(Diagnostic::error(
                            format!(
                                "outer binding `{}` is already defined in this initial subset",
                                binding.name
                            ),
                            binding.name_span,
                        )
                        .with_note("binding rebinding semantics have not been chosen yet"));
                    }
                    let value = self.expression(binding.value)?;
                    self.execution
                        .bindings
                        .insert(binding.name.clone(), value.clone());
                    self.execution.last_value = Some(value);
                }
                Item::Expression(expression) => {
                    self.execution.last_value = Some(self.expression(*expression)?);
                }
            }
        }
        Ok(self.execution)
    }

    fn expression(&mut self, id: ExprId) -> Result<OuterValue, Diagnostic> {
        let expression = self.program.syntax.expr(id);
        let value = match &expression.kind {
            ExprKind::Null => OuterValue::plain(ValueData::Null),
            ExprKind::Bool(value) => OuterValue::plain(ValueData::Bool(*value)),
            ExprKind::Integer(value) => OuterValue::plain(ValueData::Integer(*value)),
            ExprKind::Float(value) => OuterValue::plain(ValueData::Float(*value as f32)),
            ExprKind::String(value) => {
                OuterValue::plain(ValueData::String(Arc::new(value.clone())))
            }
            ExprKind::Name(name) => self.resolve_name(name, expression.span)?,
            ExprKind::List(values) => {
                let values = values
                    .iter()
                    .map(|value| self.expression(*value))
                    .collect::<Result<Vec<_>, _>>()?;
                OuterValue::plain(ValueData::List(Arc::from(values)))
            }
            ExprKind::Record(fields) => {
                let mut values = BTreeMap::new();
                for field in fields {
                    if values.contains_key(&field.name) {
                        return Err(Diagnostic::error(
                            format!("record field `{}` is specified more than once", field.name),
                            field.name_span,
                        ));
                    }
                    values.insert(field.name.clone(), self.expression(field.value)?);
                }
                OuterValue::plain(ValueData::Record(Arc::new(values)))
            }
            ExprKind::Binary { op, left, right } => {
                let left = self.expression(*left)?;
                let right = self.expression(*right)?;
                outer_binary(*op, left, right, expression.span)?
            }
            ExprKind::Call { callee, arguments } => {
                self.call(*callee, arguments, None, expression.span)?
            }
            ExprKind::Member { .. } => {
                return Err(Diagnostic::error(
                    "member access is not yet executable in outer code",
                    expression.span,
                ));
            }
            ExprKind::IdentityQualified { callable, .. } => {
                let name = self.callable_name(*callable)?;
                let Some((id, _)) = self.program.transforms.find(&name) else {
                    return Err(Diagnostic::error(
                        "identity-qualified callable values currently require a user transform",
                        expression.span,
                    ));
                };
                OuterValue::plain(ValueData::Transform(id))
            }
            ExprKind::Pipeline { input, stage } => {
                let input = self.expression(*input)?;
                match &self.program.syntax.expr(*stage).kind {
                    ExprKind::Call { callee, arguments } => {
                        self.call(*callee, arguments, Some(input), expression.span)?
                    }
                    ExprKind::Name(_)
                    | ExprKind::Member { .. }
                    | ExprKind::IdentityQualified { .. } => {
                        self.call(*stage, &[], Some(input), expression.span)?
                    }
                    _ => {
                        return Err(Diagnostic::error(
                            "pipeline stage must be a transform name or call",
                            self.program.syntax.expr(*stage).span,
                        ));
                    }
                }
            }
        };
        Ok(value)
    }

    fn resolve_name(&self, name: &str, span: Span) -> Result<OuterValue, Diagnostic> {
        if let Some(value) = self.execution.bindings.get(name) {
            return Ok(value.clone());
        }
        if let Some((id, _)) = self.program.transforms.find(name) {
            return Ok(OuterValue::plain(ValueData::Transform(id)));
        }
        Err(Diagnostic::error(
            format!("unknown outer name `{name}`"),
            span,
        ))
    }

    fn call(
        &mut self,
        callee: ExprId,
        arguments: &[Argument],
        pipeline_input: Option<OuterValue>,
        span: Span,
    ) -> Result<OuterValue, Diagnostic> {
        let callee_expression = self.program.syntax.expr(callee);
        let name = self.callable_name(callee)?;
        let qualified = matches!(callee_expression.kind, ExprKind::IdentityQualified { .. });
        let mut evaluated = Vec::new();
        if let Some(input) = pipeline_input {
            evaluated.push((None, input, callee_expression.span));
        }
        for argument in arguments {
            evaluated.push((
                argument.name.as_ref().map(|(name, _)| name.clone()),
                self.expression(argument.value)?,
                argument.span,
            ));
        }
        if name == "asset" {
            if qualified {
                return Err(Diagnostic::error(
                    "outer builtins cannot use semantic identity qualifiers",
                    callee_expression.span,
                ));
            }
            return self.asset(evaluated, span);
        }
        if name == "read" {
            if qualified {
                return Err(Diagnostic::error(
                    "outer builtins cannot use semantic identity qualifiers",
                    callee_expression.span,
                ));
            }
            return self.read(evaluated, span);
        }
        if name == "trace" {
            if qualified {
                return Err(Diagnostic::error(
                    "outer builtins cannot use semantic identity qualifiers",
                    callee_expression.span,
                ));
            }
            return self.trace(evaluated, span);
        }
        if name == "replay" {
            if qualified {
                return Err(Diagnostic::error(
                    "outer builtins cannot use semantic identity qualifiers",
                    callee_expression.span,
                ));
            }
            return self.replay_call(evaluated, span);
        }
        if name == "save" {
            if qualified {
                return Err(Diagnostic::error(
                    "outer builtins cannot use semantic identity qualifiers",
                    callee_expression.span,
                ));
            }
            return self.save(evaluated, span);
        }
        if let Some(registered) = RegisteredTransform::find(&name) {
            let arguments = order_outer_arguments_with_defaults(
                registered.name(),
                registered.parameters(),
                evaluated,
                span,
                |index| registered.default_argument(index),
            )?;
            return invoke_registered_transform_with_lineage(
                registered,
                arguments,
                match &mut self.cache {
                    Some(cache) => Some(&mut **cache),
                    None => None,
                },
                span,
            );
        }
        let Some((id, transform)) = self.program.transforms.find(&name) else {
            return Err(Diagnostic::error(
                format!("unknown outer callable `{name}`"),
                callee_expression.span,
            ));
        };
        let parameters = transform
            .parameters
            .iter()
            .map(|parameter| parameter.name.as_str())
            .collect::<Vec<_>>();
        let values = order_outer_arguments(&transform.name, &parameters, evaluated, span)?;
        invoke_transform_with_lineage(
            self.program,
            self.engine,
            id,
            values,
            match &mut self.cache {
                Some(cache) => Some(&mut **cache),
                None => None,
            },
        )
    }

    fn callable_name(&self, callee: ExprId) -> Result<String, Diagnostic> {
        let expression = self.program.syntax.expr(callee);
        match &expression.kind {
            ExprKind::Name(name) => Ok(name.clone()),
            ExprKind::Member { receiver, name, .. } => {
                let ExprKind::Name(namespace) = &self.program.syntax.expr(*receiver).kind else {
                    return Err(Diagnostic::error(
                        "outer callable namespaces must be directly named",
                        expression.span,
                    ));
                };
                Ok(format!("{namespace}.{name}"))
            }
            ExprKind::IdentityQualified { callable, .. } => self.callable_name(*callable),
            _ => Err(Diagnostic::error(
                "outer calls require a named builtin or transform",
                expression.span,
            )),
        }
    }

    fn asset(
        &self,
        arguments: Vec<(Option<String>, OuterValue, Span)>,
        span: Span,
    ) -> Result<OuterValue, Diagnostic> {
        if arguments.len() != 1
            || arguments[0]
                .0
                .as_deref()
                .is_some_and(|name| name != "path" && name != "locator")
        {
            return Err(Diagnostic::error(
                "asset expects exactly one locator string",
                span,
            ));
        }
        let ValueData::String(locator) = &arguments[0].1.data else {
            return Err(Diagnostic::error(
                "asset locator must be a string",
                arguments[0].2,
            ));
        };
        let locator: Arc<str> = Arc::from(locator.as_str());
        Ok(OuterValue::plain(ValueData::Asset(Arc::new(AssetValue {
            locator: locator.clone(),
        })))
        .with_lineage(Lineage::source(locator, None)))
    }

    fn read(
        &self,
        arguments: Vec<(Option<String>, OuterValue, Span)>,
        span: Span,
    ) -> Result<OuterValue, Diagnostic> {
        let mut arguments = order_outer_arguments("read", &["asset"], arguments, span)?;
        let (asset, asset_span) = arguments.pop().expect("read has one asset argument");
        let ValueData::Asset(value) = &asset.data else {
            return Err(Diagnostic::error("read expects an asset value", asset_span));
        };
        let Some(capabilities) = self.engine.capabilities() else {
            return Err(Diagnostic::error(
                "asset materialization is unavailable in this runtime",
                asset_span,
            )
            .with_note("the Histima host must provide an asset-reading capability"));
        };
        let bytes = capabilities.read_asset(&value.locator).map_err(|error| {
            Diagnostic::error(
                format!("could not read asset {:?}: {error}", value.locator),
                asset_span,
            )
        })?;
        let observed = crate::identity::byte_content_identity(&bytes);
        if let Some(lineage) = &asset.lineage {
            match lineage.node() {
                LineageNode::Source(source) => {
                    if source.locator != value.locator {
                        return Err(Diagnostic::error(
                            "asset locator does not match its source lineage",
                            asset_span,
                        ));
                    }
                    if let Some(expected) = source.observed_content
                        && expected != observed
                    {
                        return Err(Diagnostic::error(
                            format!(
                                "asset {:?} changed: expected content {expected}, observed {observed}",
                                value.locator
                            ),
                            asset_span,
                        )
                        .with_note("replay requires the recorded source content"));
                    }
                }
                _ => {
                    return Err(Diagnostic::error(
                        "asset value carries non-source lineage",
                        asset_span,
                    ));
                }
            }
        }
        Ok(OuterValue::plain(ValueData::Bytes(Arc::new(bytes)))
            .with_lineage(Lineage::observed_source(value.locator.clone(), observed)))
    }

    fn trace(
        &self,
        arguments: Vec<(Option<String>, OuterValue, Span)>,
        span: Span,
    ) -> Result<OuterValue, Diagnostic> {
        if arguments.len() != 1
            || arguments[0]
                .0
                .as_deref()
                .is_some_and(|name| name != "value")
        {
            return Err(Diagnostic::error(
                "trace expects exactly one derived value",
                span,
            ));
        }
        let Some(lineage) = &arguments[0].1.lineage else {
            return Err(Diagnostic::error(
                "value has no semantic lineage to inspect",
                arguments[0].2,
            ));
        };
        Ok(OuterValue::plain(ValueData::Lineage(lineage.clone())))
    }

    fn replay_call(
        &mut self,
        arguments: Vec<(Option<String>, OuterValue, Span)>,
        span: Span,
    ) -> Result<OuterValue, Diagnostic> {
        if arguments.len() != 1
            || arguments[0]
                .0
                .as_deref()
                .is_some_and(|name| name != "value")
        {
            return Err(Diagnostic::error(
                "replay expects exactly one derived value",
                span,
            ));
        }
        let capabilities = self.engine.capabilities();
        let resolver = capabilities.map(CapabilityReplayResolver);
        let dependencies = resolver
            .as_ref()
            .map(|resolver| resolver as &dyn ReplayDependencyResolver);
        match &mut self.cache {
            Some(cache) => replay_with(
                self.program,
                self.engine,
                &arguments[0].1,
                &mut **cache,
                dependencies,
                span,
            ),
            None => {
                let mut cache = TransformResultCache::default();
                replay_with(
                    self.program,
                    self.engine,
                    &arguments[0].1,
                    &mut cache,
                    dependencies,
                    span,
                )
            }
        }
    }

    fn save(
        &self,
        arguments: Vec<(Option<String>, OuterValue, Span)>,
        span: Span,
    ) -> Result<OuterValue, Diagnostic> {
        let mut arguments = order_outer_arguments("save", &["value", "locator"], arguments, span)?;
        let (locator, locator_span) = arguments.pop().expect("save has a locator argument");
        let (value, value_span) = arguments.pop().expect("save has a value argument");
        let ValueData::Bytes(bytes) = &value.data else {
            return Err(Diagnostic::error(
                "save currently requires an encoded byte value",
                value_span,
            ));
        };
        let ValueData::String(locator) = &locator.data else {
            return Err(Diagnostic::error(
                "save output locator must be a string",
                locator_span,
            ));
        };
        let Some(capabilities) = self.engine.capabilities() else {
            return Err(
                Diagnostic::error("asset output is unavailable in this runtime", span)
                    .with_note("the Histima host must provide an asset-writing capability"),
            );
        };
        capabilities.write_asset(locator, bytes).map_err(|error| {
            Diagnostic::error(
                format!("could not write asset output {locator:?}: {error}"),
                locator_span,
            )
        })?;
        Ok(value)
    }
}

fn order_outer_arguments(
    callable: &str,
    parameters: &[&str],
    arguments: Vec<(Option<String>, OuterValue, Span)>,
    call_span: Span,
) -> Result<Vec<(OuterValue, Span)>, Diagnostic> {
    order_outer_arguments_with_defaults(callable, parameters, arguments, call_span, |_| None)
}

fn order_outer_arguments_with_defaults(
    callable: &str,
    parameters: &[&str],
    arguments: Vec<(Option<String>, OuterValue, Span)>,
    call_span: Span,
    default: impl Fn(usize) -> Option<OuterValue>,
) -> Result<Vec<(OuterValue, Span)>, Diagnostic> {
    let mut ordered: Vec<Option<(OuterValue, Span)>> = vec![None; parameters.len()];
    let mut next_positional = 0;
    for (name, value, argument_span) in arguments {
        let index = if let Some(name) = name {
            parameters
                .iter()
                .position(|parameter| *parameter == name)
                .ok_or_else(|| {
                    Diagnostic::error(
                        format!("{callable} has no parameter `{name}`"),
                        argument_span,
                    )
                })?
        } else {
            while next_positional < ordered.len() && ordered[next_positional].is_some() {
                next_positional += 1;
            }
            if next_positional == ordered.len() {
                return Err(Diagnostic::error(
                    format!("too many arguments for {callable}"),
                    argument_span,
                ));
            }
            let index = next_positional;
            next_positional += 1;
            index
        };
        if ordered[index].is_some() {
            return Err(Diagnostic::error(
                format!(
                    "parameter `{}` is supplied more than once",
                    parameters[index]
                ),
                argument_span,
            ));
        }
        ordered[index] = Some((value, argument_span));
    }
    ordered
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            value
                .or_else(|| default(index).map(|value| (value, call_span)))
                .ok_or_else(|| {
                    Diagnostic::error(
                        format!("missing argument `{}` for {callable}", parameters[index]),
                        call_span,
                    )
                })
        })
        .collect()
}

fn outer_binary(
    op: BinaryOp,
    left: OuterValue,
    right: OuterValue,
    span: Span,
) -> Result<OuterValue, Diagnostic> {
    let data = if op.is_arithmetic() {
        match (left.data, right.data) {
            (ValueData::Integer(left), ValueData::Integer(right)) => {
                ValueData::Integer(integer_binary(op, left, right, span)?)
            }
            (ValueData::Float(left), ValueData::Float(right)) => {
                ValueData::Float(float_arithmetic(op, left, right))
            }
            _ => {
                return Err(Diagnostic::error(
                    "outer arithmetic requires two integers or two floats",
                    span,
                ));
            }
        }
    } else if op.is_equality() {
        let equal = match (left.data, right.data) {
            (ValueData::Null, ValueData::Null) => true,
            (ValueData::Bool(left), ValueData::Bool(right)) => left == right,
            (ValueData::Integer(left), ValueData::Integer(right)) => left == right,
            (ValueData::Float(left), ValueData::Float(right)) => left == right,
            (ValueData::String(left), ValueData::String(right)) => left == right,
            _ => {
                return Err(Diagnostic::error(
                    "outer equality requires two values of the same scalar type",
                    span,
                ));
            }
        };
        ValueData::Bool(if op == BinaryOp::Equal { equal } else { !equal })
    } else {
        match (left.data, right.data) {
            (ValueData::Integer(left), ValueData::Integer(right)) => {
                ValueData::Bool(integer_comparison(op, left, right))
            }
            (ValueData::Float(left), ValueData::Float(right)) => {
                ValueData::Bool(float_comparison(op, left, right))
            }
            _ => {
                return Err(Diagnostic::error(
                    "outer ordering requires two integers or two floats",
                    span,
                ));
            }
        }
    };
    Ok(OuterValue::plain(data))
}

fn integer_binary(op: BinaryOp, left: i64, right: i64, span: Span) -> Result<i64, Diagnostic> {
    let result = match op {
        BinaryOp::Add => left.checked_add(right),
        BinaryOp::Subtract => left.checked_sub(right),
        BinaryOp::Multiply => left.checked_mul(right),
        BinaryOp::Divide => left.checked_div(right),
        _ => unreachable!("integer_binary is called only for arithmetic"),
    };
    result.ok_or_else(|| Diagnostic::error("integer arithmetic overflow or division by zero", span))
}

fn float_arithmetic(op: BinaryOp, left: f32, right: f32) -> f32 {
    match op {
        BinaryOp::Add => left + right,
        BinaryOp::Subtract => left - right,
        BinaryOp::Multiply => left * right,
        BinaryOp::Divide => left / right,
        _ => unreachable!("float_arithmetic is called only for arithmetic"),
    }
}

fn integer_comparison(op: BinaryOp, left: i64, right: i64) -> bool {
    match op {
        BinaryOp::Equal => left == right,
        BinaryOp::NotEqual => left != right,
        BinaryOp::Less => left < right,
        BinaryOp::LessEqual => left <= right,
        BinaryOp::Greater => left > right,
        BinaryOp::GreaterEqual => left >= right,
        _ => unreachable!("integer_comparison is called only for comparisons"),
    }
}

fn float_comparison(op: BinaryOp, left: f32, right: f32) -> bool {
    match op {
        BinaryOp::Equal => left == right,
        BinaryOp::NotEqual => left != right,
        BinaryOp::Less => left < right,
        BinaryOp::LessEqual => left <= right,
        BinaryOp::Greater => left > right,
        BinaryOp::GreaterEqual => left >= right,
        _ => unreachable!("float_comparison is called only for comparisons"),
    }
}

#[derive(Clone, Copy, Debug)]
enum NativeScalar {
    Bool(bool),
    U8(u8),
    I64(i64),
    F32(f32),
}

enum InterpretedValue {
    Scalar(NativeScalar),
    String(String),
    StringView(Arc<String>),
    Bytes(Vec<u8>),
    BytesView(Arc<Vec<u8>>),
    Image(InterpretedImage),
    ImageView(Arc<ImageValue>),
}

impl InterpretedValue {
    fn scalar(&self) -> NativeScalar {
        match self {
            Self::Scalar(value) => *value,
            Self::String(_)
            | Self::StringView(_)
            | Self::Bytes(_)
            | Self::BytesView(_)
            | Self::Image(_)
            | Self::ImageView(_) => {
                unreachable!("typed scalar operation received a composite value")
            }
        }
    }

    fn string(&self) -> &str {
        match self {
            Self::String(value) => value,
            Self::StringView(value) => value,
            _ => unreachable!("typed World operation received a non-string value"),
        }
    }
}

struct InterpretedImage {
    storage: Vec<u8>,
    format: ImageFormat,
    width: usize,
    height: usize,
    stride: usize,
}

struct IrInterpreter<'a> {
    module: &'a crate::ir::TypedModule,
    capabilities: Option<&'a dyn World>,
}

struct HybridAotEngine<'a> {
    interpreter: IrInterpreter<'a>,
    native: Option<&'a NativeModule>,
}

enum PreparedNativeArgument {
    Scalar(AbiScalar),
    String(NativeBuffer),
    StringView(Arc<String>),
    Bytes(NativeBuffer),
    BytesView(Arc<Vec<u8>>),
    Image(NativeImage),
    ImageView(Arc<ImageValue>),
}

impl TransformEngine for HybridAotEngine<'_> {
    fn invoke(
        &self,
        id: TransformId,
        arguments: Vec<(OuterValue, Span)>,
    ) -> Result<TransformOutcome, Diagnostic> {
        let Some(native) = self.native.filter(|native| native.contains(id)) else {
            return self.interpreter.invoke(id, arguments);
        };
        let transform = self.interpreter.module.get(id);
        let mut prepared = transform
            .parameters
            .iter()
            .zip(arguments)
            .map(|(parameter, (argument, span))| {
                prepare_native_argument(argument, parameter.ty, span)
            })
            .collect::<Result<Vec<_>, Diagnostic>>()?;
        let mut native_arguments = prepared
            .iter_mut()
            .map(|argument| match argument {
                PreparedNativeArgument::Scalar(value) => NativeArgument::Scalar(*value),
                PreparedNativeArgument::String(buffer) => NativeArgument::String(buffer),
                PreparedNativeArgument::StringView(value) => {
                    NativeArgument::StringView(NativeBufferView {
                        bytes: value.as_bytes(),
                    })
                }
                PreparedNativeArgument::Bytes(buffer) => NativeArgument::Bytes(buffer),
                PreparedNativeArgument::BytesView(value) => {
                    NativeArgument::BytesView(NativeBufferView {
                        bytes: value.as_slice(),
                    })
                }
                PreparedNativeArgument::Image(image) => NativeArgument::Image(image),
                PreparedNativeArgument::ImageView(image) => {
                    NativeArgument::ImageView(NativeImageView {
                        bytes: image.byte_slice(),
                        format: image.format().abi_tag(),
                        width: image.width(),
                        height: image.height(),
                        stride: image.stride(),
                    })
                }
            })
            .collect::<Vec<_>>();
        let result = native.invoke(id, &mut native_arguments)?;
        drop(native_arguments);
        let value = match result {
            NativeResult::Scalar(AbiScalar::Bool(value)) => {
                freeze_scalar(NativeScalar::Bool(value))
            }
            NativeResult::Scalar(AbiScalar::U8(value)) => freeze_scalar(NativeScalar::U8(value)),
            NativeResult::Scalar(AbiScalar::I64(value)) => freeze_scalar(NativeScalar::I64(value)),
            NativeResult::Scalar(AbiScalar::F32(value)) => freeze_scalar(NativeScalar::F32(value)),
            NativeResult::OwnedStringArgument(index) => {
                let PreparedNativeArgument::String(buffer) = std::mem::replace(
                    &mut prepared[index],
                    PreparedNativeArgument::Scalar(AbiScalar::Bool(false)),
                ) else {
                    unreachable!("native owned string result identifies an owned string argument")
                };
                freeze_native_string(buffer, transform.span)?
            }
            NativeResult::StringViewArgument(index) => {
                let PreparedNativeArgument::StringView(value) = &prepared[index] else {
                    unreachable!("native string view result identifies a string view argument")
                };
                OuterValue::plain(ValueData::String(value.clone()))
            }
            NativeResult::OwnedBytesArgument(index) => {
                let PreparedNativeArgument::Bytes(buffer) = std::mem::replace(
                    &mut prepared[index],
                    PreparedNativeArgument::Scalar(AbiScalar::Bool(false)),
                ) else {
                    unreachable!("native owned bytes result identifies an owned bytes argument")
                };
                OuterValue::plain(ValueData::Bytes(Arc::new(buffer.bytes)))
            }
            NativeResult::BytesViewArgument(index) => {
                let PreparedNativeArgument::BytesView(value) = &prepared[index] else {
                    unreachable!("native bytes view result identifies a bytes view argument")
                };
                OuterValue::plain(ValueData::Bytes(value.clone()))
            }
            NativeResult::OwnedImageArgument(index) => {
                let PreparedNativeArgument::Image(image) = std::mem::replace(
                    &mut prepared[index],
                    PreparedNativeArgument::Scalar(AbiScalar::Bool(false)),
                ) else {
                    unreachable!("native owned image result identifies an owned image argument")
                };
                freeze_native_image(image)
            }
            NativeResult::ImageViewArgument(index) => {
                let PreparedNativeArgument::ImageView(image) = &prepared[index] else {
                    unreachable!("native image view result identifies a view argument")
                };
                OuterValue::plain(ValueData::Image(image.clone()))
            }
        };
        Ok(TransformOutcome {
            value,
            observations: Vec::new(),
        })
    }

    fn may_observe_dependencies(&self, id: TransformId) -> bool {
        self.interpreter.may_observe_dependencies(id)
    }

    fn capabilities(&self) -> Option<&dyn World> {
        self.interpreter.capabilities()
    }
}

fn prepare_native_argument(
    value: OuterValue,
    expected: Type,
    span: Span,
) -> Result<PreparedNativeArgument, Diagnostic> {
    if matches!(expected, Type::Bool | Type::U8 | Type::I64 | Type::F32) {
        let scalar = lower_interpreted_value(value, expected, span)?.scalar();
        return Ok(PreparedNativeArgument::Scalar(match scalar {
            NativeScalar::Bool(value) => AbiScalar::Bool(value),
            NativeScalar::U8(value) => AbiScalar::U8(value),
            NativeScalar::I64(value) => AbiScalar::I64(value),
            NativeScalar::F32(value) => AbiScalar::F32(value),
        }));
    }
    match (expected, value.data) {
        (Type::String, ValueData::String(value)) => {
            let value = Arc::try_unwrap(value).unwrap_or_else(|shared| (*shared).clone());
            Ok(PreparedNativeArgument::String(NativeBuffer {
                bytes: value.into_bytes(),
            }))
        }
        (Type::StringView, ValueData::String(value)) => {
            Ok(PreparedNativeArgument::StringView(value))
        }
        (Type::Bytes, ValueData::Bytes(value)) => {
            let bytes = Arc::try_unwrap(value).unwrap_or_else(|shared| (*shared).clone());
            Ok(PreparedNativeArgument::Bytes(NativeBuffer { bytes }))
        }
        (Type::BytesView, ValueData::Bytes(value)) => Ok(PreparedNativeArgument::BytesView(value)),
        (Type::Image, ValueData::Image(image)) => {
            let image = Arc::try_unwrap(image).unwrap_or_else(|shared| (*shared).clone());
            let ImageValue {
                storage,
                format,
                width,
                height,
                stride,
            } = image;
            let bytes = match Arc::try_unwrap(storage) {
                Ok(storage) => storage.into_vec(),
                Err(shared) => shared.to_vec(),
            };
            Ok(PreparedNativeArgument::Image(NativeImage {
                bytes,
                format: format.abi_tag(),
                width,
                height,
                stride,
            }))
        }
        (Type::ImageView, ValueData::Image(image)) => Ok(PreparedNativeArgument::ImageView(image)),
        (expected, _) => Err(Diagnostic::error(
            format!(
                "outer value cannot cross into native parameter type {}",
                expected.name()
            ),
            span,
        )),
    }
}

fn freeze_native_string(buffer: NativeBuffer, span: Span) -> Result<OuterValue, Diagnostic> {
    let value = String::from_utf8(buffer.bytes).map_err(|_| {
        Diagnostic::error("native transform returned invalid UTF-8 for String", span)
    })?;
    Ok(OuterValue::plain(ValueData::String(Arc::new(value))))
}

fn freeze_native_image(image: NativeImage) -> OuterValue {
    let format = match image.format {
        ABI_IMAGE_FORMAT_OPAQUE_BYTES => ImageFormat::OpaqueBytes,
        ABI_IMAGE_FORMAT_RGBA8 => ImageFormat::Rgba8,
        _ => unreachable!("native image descriptors preserve validated input metadata"),
    };
    OuterValue::image(ImageValue {
        storage: Arc::new(ImageStorage::new(image.bytes)),
        format,
        width: image.width,
        height: image.height,
        stride: image.stride,
    })
}

impl IrInterpreter<'_> {
    fn invoke_at_depth(
        &self,
        id: TransformId,
        arguments: Vec<(OuterValue, Span)>,
        depth: usize,
        capabilities: &mut CapabilitySession<'_>,
    ) -> Result<OuterValue, Diagnostic> {
        let transform = self.module.get(id);
        let arguments = transform
            .parameters
            .iter()
            .zip(arguments)
            .map(|(parameter, (argument, span))| {
                lower_interpreted_value(argument, parameter.ty, span)
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.invoke_lowered_at_depth(id, arguments, depth, capabilities)
            .map(freeze_interpreted_value)
    }

    fn invoke_lowered_at_depth(
        &self,
        id: TransformId,
        arguments: Vec<InterpretedValue>,
        depth: usize,
        capabilities: &mut CapabilitySession<'_>,
    ) -> Result<InterpretedValue, Diagnostic> {
        let transform = self.module.get(id);
        if depth >= 256 {
            return Err(Diagnostic::error(
                "transform call depth exceeded the interpreter limit",
                transform.span,
            ));
        }
        let mut values = (0..transform.values.len())
            .map(|_| None)
            .collect::<Vec<_>>();
        for (parameter, argument) in transform.parameters.iter().zip(arguments) {
            values[parameter.value.0 as usize] = Some(argument);
        }
        let mut current = transform.entry;
        loop {
            let block = &transform.blocks[current.0 as usize];
            for id in &block.instructions {
                let evaluated =
                    self.evaluate_instruction(transform, *id, &mut values, depth, capabilities)?;
                values[id.0 as usize] = Some(evaluated);
            }
            match block.terminator {
                Terminator::Return(value) => {
                    return Ok(values[value.0 as usize].take().unwrap());
                }
                Terminator::Jump(target) => current = target,
                Terminator::Branch {
                    condition,
                    then_block,
                    else_block,
                } => {
                    current = match values[condition.0 as usize].as_ref().unwrap().scalar() {
                        NativeScalar::Bool(true) => then_block,
                        NativeScalar::Bool(false) => else_block,
                        _ => unreachable!("typed branch conditions are bool"),
                    };
                }
            }
        }
    }

    fn evaluate_instruction(
        &self,
        transform: &crate::ir::Transform,
        id: ValueId,
        values: &mut [Option<InterpretedValue>],
        depth: usize,
        capabilities: &mut CapabilitySession<'_>,
    ) -> Result<InterpretedValue, Diagnostic> {
        let value = transform.value(id);
        Ok(match &value.kind {
            ValueKind::Parameter { .. } | ValueKind::ImageByteElement => unreachable!(),
            ValueKind::Constant(constant) => match constant {
                Constant::Bool(value) => InterpretedValue::Scalar(NativeScalar::Bool(*value)),
                Constant::I64(value) => InterpretedValue::Scalar(NativeScalar::I64(*value)),
                Constant::F32(value) => InterpretedValue::Scalar(NativeScalar::F32(*value)),
                Constant::String(value) => InterpretedValue::StringView(Arc::new(value.clone())),
            },
            ValueKind::Binary { op, left, right } => InterpretedValue::Scalar(native_binary(
                *op,
                values[left.0 as usize].as_ref().unwrap().scalar(),
                values[right.0 as usize].as_ref().unwrap().scalar(),
                value.span,
            )?),
            ValueKind::Call {
                transform: callee,
                arguments,
            } => {
                let callee_transform = self.module.get(*callee);
                let call_arguments = arguments
                    .iter()
                    .zip(&callee_transform.parameters)
                    .map(|(argument, parameter)| {
                        transfer_interpreted_argument(
                            &mut values[argument.0 as usize],
                            parameter.ty,
                        )
                    })
                    .collect();
                self.invoke_lowered_at_depth(*callee, call_arguments, depth + 1, capabilities)?
            }
            ValueKind::ImageZero { image } => {
                let Some(InterpretedValue::Image(mut image)) = values[image.0 as usize].take()
                else {
                    unreachable!("typed image_zero input is an available owned image")
                };
                image.storage.fill(0);
                InterpretedValue::Image(image)
            }
            ValueKind::ImageFill { image, value: fill } => {
                let NativeScalar::U8(fill) = values[fill.0 as usize].as_ref().unwrap().scalar()
                else {
                    unreachable!("typed image_fill value is u8")
                };
                let Some(InterpretedValue::Image(mut image)) = values[image.0 as usize].take()
                else {
                    unreachable!("typed image_fill input is an available owned image")
                };
                image.storage.fill(fill);
                InterpretedValue::Image(image)
            }
            ValueKind::ImageByteMap {
                image,
                element,
                instructions,
                result,
            } => {
                let Some(InterpretedValue::Image(mut image)) = values[image.0 as usize].take()
                else {
                    unreachable!("typed image byte map input is an available owned image")
                };
                for index in 0..image.storage.len() {
                    values[element.0 as usize] = Some(InterpretedValue::Scalar(NativeScalar::U8(
                        image.storage[index],
                    )));
                    for instruction in instructions {
                        let evaluated = self.evaluate_instruction(
                            transform,
                            *instruction,
                            values,
                            depth,
                            capabilities,
                        )?;
                        values[instruction.0 as usize] = Some(evaluated);
                    }
                    let NativeScalar::U8(mapped) =
                        values[result.0 as usize].as_ref().unwrap().scalar()
                    else {
                        unreachable!("typed image byte map result is u8")
                    };
                    image.storage[index] = mapped;
                }
                InterpretedValue::Image(image)
            }
            ValueKind::ImageRgba8Scale { image, channels } => {
                let Some(InterpretedValue::Image(mut image)) = values[image.0 as usize].take()
                else {
                    unreachable!("typed RGBA8 scale input is an available owned image")
                };
                if image.format != ImageFormat::Rgba8 {
                    return Err(Diagnostic::error(
                        "image pixel iteration requires RGBA8 format",
                        value.span,
                    )
                    .with_note(format!("received {} image storage", image.format)));
                }
                for y in 0..image.height {
                    for x in 0..image.width {
                        let pixel = y * image.stride + x * 4;
                        for (channel, factor) in channels {
                            let NativeScalar::F32(factor) =
                                values[factor.0 as usize].as_ref().unwrap().scalar()
                            else {
                                unreachable!("typed RGBA8 scale factor is f32")
                            };
                            let offset = pixel + channel.offset();
                            image.storage[offset] =
                                scale_rgba8_channel(image.storage[offset], factor);
                        }
                    }
                }
                InterpretedValue::Image(image)
            }
            ValueKind::RuntimeCall(RuntimeCall::EnvironmentI64 { name }) => {
                InterpretedValue::Scalar(NativeScalar::I64(
                    capabilities.environment_i64(name, value.span)?,
                ))
            }
            ValueKind::RuntimeCall(RuntimeCall::EnvironmentRead { name }) => {
                let name = values[name.0 as usize]
                    .as_ref()
                    .unwrap()
                    .string()
                    .to_owned();
                InterpretedValue::String(capabilities.environment(&name, value.span)?)
            }
            ValueKind::RuntimeCall(RuntimeCall::FileRead { path }) => {
                let path = values[path.0 as usize]
                    .as_ref()
                    .unwrap()
                    .string()
                    .to_owned();
                InterpretedValue::Bytes(capabilities.read_file(&path, value.span)?)
            }
            ValueKind::RuntimeCall(RuntimeCall::HttpGet { url }) => {
                let url = values[url.0 as usize].as_ref().unwrap().string().to_owned();
                InterpretedValue::Bytes(capabilities.http_get(&url, value.span)?)
            }
        })
    }
}

impl TransformEngine for IrInterpreter<'_> {
    fn invoke(
        &self,
        id: TransformId,
        arguments: Vec<(OuterValue, Span)>,
    ) -> Result<TransformOutcome, Diagnostic> {
        let mut capabilities = CapabilitySession::new(self.capabilities);
        let value = self.invoke_at_depth(id, arguments, 0, &mut capabilities)?;
        Ok(TransformOutcome {
            value,
            observations: capabilities.finish(),
        })
    }

    fn may_observe_dependencies(&self, id: TransformId) -> bool {
        transform_may_observe_dependencies(self.module, id)
    }

    fn capabilities(&self) -> Option<&dyn World> {
        self.capabilities
    }
}

fn transform_may_observe_dependencies(module: &crate::ir::TypedModule, root: TransformId) -> bool {
    fn visit(
        module: &crate::ir::TypedModule,
        id: TransformId,
        visited: &mut BTreeSet<u32>,
    ) -> bool {
        if !visited.insert(id.0) {
            return false;
        }
        module.get(id).values.iter().any(|value| match &value.kind {
            ValueKind::RuntimeCall(_) => true,
            ValueKind::Call {
                transform: callee, ..
            } => visit(module, *callee, visited),
            _ => false,
        })
    }

    visit(module, root, &mut BTreeSet::new())
}

fn lower_interpreted_value(
    value: OuterValue,
    expected: Type,
    span: Span,
) -> Result<InterpretedValue, Diagnostic> {
    match (expected, value.data) {
        (Type::Bool, ValueData::Bool(value)) => {
            Ok(InterpretedValue::Scalar(NativeScalar::Bool(value)))
        }
        (Type::U8, ValueData::Integer(value)) => Ok(InterpretedValue::Scalar(NativeScalar::U8(
            checked_u8(value, span)?,
        ))),
        (Type::I64, ValueData::Integer(value)) => {
            Ok(InterpretedValue::Scalar(NativeScalar::I64(value)))
        }
        (Type::F32, ValueData::Float(value)) => {
            Ok(InterpretedValue::Scalar(NativeScalar::F32(value)))
        }
        (Type::String, ValueData::String(value)) => Ok(InterpretedValue::String(
            Arc::try_unwrap(value).unwrap_or_else(|shared| (*shared).clone()),
        )),
        (Type::StringView, ValueData::String(value)) => Ok(InterpretedValue::StringView(value)),
        (Type::Bytes, ValueData::Bytes(value)) => Ok(InterpretedValue::Bytes(
            Arc::try_unwrap(value).unwrap_or_else(|shared| (*shared).clone()),
        )),
        (Type::BytesView, ValueData::Bytes(value)) => Ok(InterpretedValue::BytesView(value)),
        (Type::Image, ValueData::Image(image)) => {
            let image = Arc::try_unwrap(image).unwrap_or_else(|shared| (*shared).clone());
            let ImageValue {
                storage,
                format,
                width,
                height,
                stride,
            } = image;
            let storage = match Arc::try_unwrap(storage) {
                Ok(storage) => storage.into_vec(),
                Err(shared) => shared.to_vec(),
            };
            Ok(InterpretedValue::Image(InterpretedImage {
                storage,
                format,
                width,
                height,
                stride,
            }))
        }
        (Type::ImageView, ValueData::Image(image)) => Ok(InterpretedValue::ImageView(image)),
        (expected, _) => Err(Diagnostic::error(
            format!(
                "outer value cannot cross into native parameter type {}",
                expected.name()
            ),
            span,
        )),
    }
}

fn transfer_interpreted_argument(
    value: &mut Option<InterpretedValue>,
    expected: Type,
) -> InterpretedValue {
    match (expected, value.as_ref().unwrap()) {
        (Type::Bool, InterpretedValue::Scalar(NativeScalar::Bool(value))) => {
            InterpretedValue::Scalar(NativeScalar::Bool(*value))
        }
        (Type::U8, InterpretedValue::Scalar(NativeScalar::U8(value))) => {
            InterpretedValue::Scalar(NativeScalar::U8(*value))
        }
        (Type::I64, InterpretedValue::Scalar(NativeScalar::I64(value))) => {
            InterpretedValue::Scalar(NativeScalar::I64(*value))
        }
        (Type::F32, InterpretedValue::Scalar(NativeScalar::F32(value))) => {
            InterpretedValue::Scalar(NativeScalar::F32(*value))
        }
        (Type::StringView, InterpretedValue::StringView(value)) => {
            InterpretedValue::StringView(value.clone())
        }
        (Type::BytesView, InterpretedValue::BytesView(value)) => {
            InterpretedValue::BytesView(value.clone())
        }
        (Type::String, InterpretedValue::String(_)) | (Type::Bytes, InterpretedValue::Bytes(_)) => {
            value
                .take()
                .expect("owned inner argument remains available until transferred")
        }
        (Type::ImageView, InterpretedValue::ImageView(image)) => {
            InterpretedValue::ImageView(image.clone())
        }
        (Type::Image, InterpretedValue::Image(_)) => value
            .take()
            .expect("owned inner argument remains available until transferred"),
        _ => unreachable!("typed inner call arguments match their parameter types"),
    }
}

fn freeze_interpreted_value(value: InterpretedValue) -> OuterValue {
    match value {
        InterpretedValue::Scalar(value) => freeze_scalar(value),
        InterpretedValue::String(value) => OuterValue::plain(ValueData::String(Arc::new(value))),
        InterpretedValue::StringView(value) => OuterValue::plain(ValueData::String(value)),
        InterpretedValue::Bytes(value) => OuterValue::plain(ValueData::Bytes(Arc::new(value))),
        InterpretedValue::BytesView(value) => OuterValue::plain(ValueData::Bytes(value)),
        InterpretedValue::Image(image) => OuterValue::image(ImageValue {
            storage: Arc::new(ImageStorage::new(image.storage)),
            format: image.format,
            width: image.width,
            height: image.height,
            stride: image.stride,
        }),
        InterpretedValue::ImageView(image) => OuterValue::plain(ValueData::Image(image)),
    }
}

fn freeze_scalar(value: NativeScalar) -> OuterValue {
    OuterValue::plain(match value {
        NativeScalar::Bool(value) => ValueData::Bool(value),
        NativeScalar::U8(value) => ValueData::Integer(i64::from(value)),
        NativeScalar::I64(value) => ValueData::Integer(value),
        NativeScalar::F32(value) => ValueData::Float(value),
    })
}

fn scale_rgba8_channel(channel: u8, factor: f32) -> u8 {
    let scaled = f32::from(channel) * factor;
    if !matches!(scaled.partial_cmp(&0.0), Some(std::cmp::Ordering::Greater)) {
        0
    } else if scaled >= 255.0 {
        255
    } else {
        scaled as u8
    }
}

fn native_binary(
    op: BinaryOp,
    left: NativeScalar,
    right: NativeScalar,
    span: Span,
) -> Result<NativeScalar, Diagnostic> {
    if op.is_arithmetic() {
        match (left, right) {
            (NativeScalar::I64(left), NativeScalar::I64(right)) => {
                Ok(NativeScalar::I64(integer_binary(op, left, right, span)?))
            }
            (NativeScalar::F32(left), NativeScalar::F32(right)) => {
                Ok(NativeScalar::F32(float_arithmetic(op, left, right)))
            }
            _ => unreachable!("typed arithmetic operands are matching numeric values"),
        }
    } else if op.is_equality() {
        let equal = match (left, right) {
            (NativeScalar::Bool(left), NativeScalar::Bool(right)) => left == right,
            (NativeScalar::U8(left), NativeScalar::U8(right)) => left == right,
            (NativeScalar::I64(left), NativeScalar::I64(right)) => left == right,
            (NativeScalar::F32(left), NativeScalar::F32(right)) => left == right,
            _ => unreachable!("typed equality operands are matching scalar values"),
        };
        Ok(NativeScalar::Bool(if op == BinaryOp::Equal {
            equal
        } else {
            !equal
        }))
    } else {
        match (left, right) {
            (NativeScalar::U8(left), NativeScalar::U8(right)) => Ok(NativeScalar::Bool(
                integer_comparison(op, i64::from(left), i64::from(right)),
            )),
            (NativeScalar::I64(left), NativeScalar::I64(right)) => {
                Ok(NativeScalar::Bool(integer_comparison(op, left, right)))
            }
            (NativeScalar::F32(left), NativeScalar::F32(right)) => {
                Ok(NativeScalar::Bool(float_comparison(op, left, right)))
            }
            _ => unreachable!("typed ordering operands are matching numeric values"),
        }
    }
}

fn checked_u8(value: i64, span: Span) -> Result<u8, Diagnostic> {
    u8::try_from(value).map_err(|_| {
        Diagnostic::error(
            format!("outer integer {value} cannot cross into native parameter type u8"),
            span,
        )
        .with_note("u8 values must be in the inclusive range 0..=255")
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Arc;

    use crate::backend::native::NativeModule;
    use crate::cache::TransformResultCache;
    use crate::capability::RuntimeCapabilities;
    use crate::identity::{ContentIdentity, byte_content_identity};
    use crate::ir::TransformId;
    use crate::lineage::{Lineage, LineageNode, RecordedValue};
    use crate::runtime::{
        HybridAotEngine, ImageFormat, ImageValue, IrInterpreter, OuterValue,
        ReplayDependencyResolver, TransformEngine, ValueData, execute,
        execute_aot_cached_with_capabilities, execute_cached, execute_cached_with_capabilities,
        execute_with, execute_with_capabilities, freeze_interpreted_value, lower_interpreted_value,
        replay, replay_with_capabilities, replay_with_dependencies, scale_rgba8_channel,
    };
    use crate::source::Span;

    struct FixedEnvironment(BTreeMap<String, Vec<u8>>);

    impl FixedEnvironment {
        fn one(name: &str, value: &[u8]) -> Self {
            Self(BTreeMap::from([(name.to_owned(), value.to_vec())]))
        }
    }

    impl RuntimeCapabilities for FixedEnvironment {
        fn environment(&self, name: &str) -> Result<Vec<u8>, String> {
            self.0
                .get(name)
                .cloned()
                .ok_or_else(|| format!("environment value `{name}` is unavailable"))
        }
    }

    struct FixedWorld {
        environment: BTreeMap<String, Vec<u8>>,
        files: BTreeMap<String, Vec<u8>>,
        urls: BTreeMap<String, Vec<u8>>,
    }

    impl RuntimeCapabilities for FixedWorld {
        fn environment(&self, name: &str) -> Result<Vec<u8>, String> {
            self.environment
                .get(name)
                .cloned()
                .ok_or_else(|| format!("environment value `{name}` is unavailable"))
        }

        fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| format!("file `{path}` is unavailable"))
        }

        fn http_get(&self, url: &str) -> Result<Vec<u8>, String> {
            self.urls
                .get(url)
                .cloned()
                .ok_or_else(|| format!("HTTP URL `{url}` is unavailable"))
        }
    }

    struct ChangingFile(RefCell<VecDeque<Vec<u8>>>);

    impl RuntimeCapabilities for ChangingFile {
        fn environment(&self, name: &str) -> Result<Vec<u8>, String> {
            Err(format!("environment value `{name}` is unavailable"))
        }

        fn read_file(&self, _path: &str) -> Result<Vec<u8>, String> {
            self.0
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| "no value remains".to_owned())
        }
    }

    #[test]
    fn runs_a_scalar_transform_through_a_pipeline() {
        let compiled = crate::compile(
            "test.tima",
            "transform scale(x: f32, factor: f32) -> f32 { return x * factor }\nout = 8.0 | scale(factor=0.25)\n",
        )
        .unwrap();
        let execution = execute(&compiled).unwrap();
        assert_eq!(execution.bindings["out"].data, ValueData::Float(2.0));
    }

    #[test]
    fn aot_executes_supported_scalars_and_interprets_the_rest() {
        let compiled = crate::compile(
            "hybrid.tima",
            "transform scale(x: f32, factor: f32) -> f32 { return x * factor }\n\
             transform checked(left: i64, right: i64) -> i64 { return left + right }\n\
             native_out = scale(8.0, 0.25)\n\
             interpreted_out = checked(20, 22)\n",
        )
        .unwrap();
        let world = FixedWorld {
            environment: BTreeMap::new(),
            files: BTreeMap::new(),
            urls: BTreeMap::new(),
        };
        let mut cache = TransformResultCache::default();
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("build")
            .join(format!("aot-runtime-{}", std::process::id()));
        let execution =
            execute_aot_cached_with_capabilities(&compiled, &mut cache, &world, root).unwrap();
        assert!(execution.artifact.is_some());
        assert_eq!(
            execution.execution.bindings["native_out"].data,
            ValueData::Float(2.0)
        );
        assert_eq!(
            execution.execution.bindings["interpreted_out"].data,
            ValueData::Integer(42)
        );
    }

    #[test]
    fn executes_an_identity_qualified_transform_reference() {
        let definition = "transform scale(x: f32, factor: f32) -> f32 { return x * factor }\n";
        let base = crate::compile("base.tima", definition).unwrap();
        let identity = base.identities.get(TransformId(0)).to_string();
        let compiled = crate::compile(
            "qualified.tima",
            format!(
                "{definition}out = 8.0 | scale#{}(factor=0.25)\n",
                &identity[..12]
            ),
        )
        .unwrap();

        let execution = execute(&compiled).unwrap();
        assert_eq!(execution.bindings["out"].data, ValueData::Float(2.0));
    }

    #[test]
    fn environment_capability_is_explicit_and_part_of_the_recipe() {
        let compiled = crate::compile(
            "test.tima",
            "transform read_mode() -> i64 uses env.read { return environment_i64(\"MODE\") }\n\
             transform configured() -> i64 { return read_mode() }\n\
             result = configured()\n",
        )
        .unwrap();

        let diagnostic = execute(&compiled).unwrap_err();
        assert!(
            diagnostic[0]
                .message
                .contains("environment access is unavailable")
        );

        let invalid_environment = FixedEnvironment::one("MODE", b"not-a-number");
        let mut invalid_cache = TransformResultCache::default();
        let diagnostic =
            execute_cached_with_capabilities(&compiled, &mut invalid_cache, &invalid_environment)
                .unwrap_err();
        assert!(diagnostic[0].message.contains("is not an i64"));
        assert_eq!(invalid_cache.stats().stores, 0);

        let first_environment = FixedEnvironment::one("MODE", b"41");
        let mut cache = TransformResultCache::default();
        let first =
            execute_cached_with_capabilities(&compiled, &mut cache, &first_environment).unwrap();
        assert_eq!(first.bindings["result"].data, ValueData::Integer(41));
        let first_lineage = first.bindings["result"].lineage.as_ref().unwrap();
        let LineageNode::Invocation(first_invocation) = first_lineage.node() else {
            panic!("expected invocation lineage")
        };
        assert_eq!(first_invocation.observations.len(), 1);
        let LineageNode::ExternalObservation(observation) = first_invocation.observations[0].node()
        else {
            panic!("expected external observation")
        };
        assert_eq!(observation.capability.as_ref(), "environment");
        assert_eq!(observation.key.as_ref(), b"MODE");
        assert_eq!(observation.observed_content, byte_content_identity(b"41"));

        let second_environment = FixedEnvironment::one("MODE", b"42");
        let second =
            execute_cached_with_capabilities(&compiled, &mut cache, &second_environment).unwrap();
        assert_eq!(second.bindings["result"].data, ValueData::Integer(42));
        assert_ne!(
            first_lineage.recipe_id(),
            second.bindings["result"]
                .lineage
                .as_ref()
                .unwrap()
                .recipe_id()
        );
        assert_eq!(cache.stats().stores, 2);
    }

    #[test]
    fn world_reads_return_frozen_values_and_record_precise_observations() {
        let compiled = crate::compile(
            "world.tima",
            "transform mode() -> String uses env.read { return env.read(\"MODE\") }\n\
             transform local() -> Bytes uses file.read { return file.read(\"assets/a.bin\") }\n\
             transform remote(url: StringView) -> Bytes uses http.get { return http.get(url) }\n\
             mode_value = mode()\n\
             file_value = local()\n\
             http_value = remote(\"https://example.test/data\")\n",
        )
        .unwrap();
        let world = FixedWorld {
            environment: BTreeMap::from([("MODE".to_owned(), b"dark".to_vec())]),
            files: BTreeMap::from([("assets/a.bin".to_owned(), b"local".to_vec())]),
            urls: BTreeMap::from([("https://example.test/data".to_owned(), b"remote".to_vec())]),
        };
        let mut cache = TransformResultCache::default();
        let execution = execute_cached_with_capabilities(&compiled, &mut cache, &world).unwrap();

        assert_eq!(
            execution.bindings["mode_value"].data,
            ValueData::String(Arc::new("dark".to_owned()))
        );
        assert_eq!(
            execution.bindings["file_value"].data,
            ValueData::Bytes(Arc::new(b"local".to_vec()))
        );
        assert_eq!(
            execution.bindings["http_value"].data,
            ValueData::Bytes(Arc::new(b"remote".to_vec()))
        );

        for (binding, capability, key, content) in [
            (
                "mode_value",
                "environment",
                b"MODE".as_slice(),
                b"dark".as_slice(),
            ),
            (
                "file_value",
                "file.read",
                b"assets/a.bin".as_slice(),
                b"local".as_slice(),
            ),
            (
                "http_value",
                "http.get",
                b"https://example.test/data".as_slice(),
                b"remote".as_slice(),
            ),
        ] {
            let LineageNode::Invocation(invocation) =
                execution.bindings[binding].lineage.as_ref().unwrap().node()
            else {
                panic!("expected invocation lineage")
            };
            let LineageNode::ExternalObservation(observation) = invocation.observations[0].node()
            else {
                panic!("expected external observation")
            };
            assert_eq!(observation.capability.as_ref(), capability);
            assert_eq!(observation.key.as_ref(), key);
            assert_eq!(observation.observed_content, byte_content_identity(content));
        }
    }

    #[test]
    fn world_dependency_must_remain_stable_during_one_invocation() {
        let compiled = crate::compile(
            "changing.tima",
            "transform load() -> Bytes uses file.read {\n\
                 first = file.read(\"config.bin\")\n\
                 return file.read(\"config.bin\")\n\
             }\n\
             result = load()\n",
        )
        .unwrap();
        let world = ChangingFile(RefCell::new(VecDeque::from([
            b"first".to_vec(),
            b"second".to_vec(),
        ])));
        let diagnostic = execute_with_capabilities(&compiled, &world).unwrap_err();
        assert!(
            diagnostic[0]
                .message
                .contains("changed during one transform invocation")
        );
    }

    #[test]
    fn replay_revalidates_and_reexecutes_environment_dependencies() {
        let compiled = crate::compile(
            "test.tima",
            "transform configured() -> i64 uses env.read { return environment_i64(\"MODE\") }\n\
             result = configured()\n",
        )
        .unwrap();
        let environment = FixedEnvironment::one("MODE", b"41");
        let mut cache = TransformResultCache::default();
        let execution =
            execute_cached_with_capabilities(&compiled, &mut cache, &environment).unwrap();
        let target = execution.bindings["result"].clone();
        cache.invalidate_recipe(target.lineage.as_ref().unwrap().recipe_id().unwrap());

        let replayed =
            replay_with_capabilities(&compiled, &target, &mut cache, &environment).unwrap();
        assert_eq!(replayed.data, ValueData::Integer(41));
        assert_eq!(replayed.lineage, target.lineage);

        let changed = FixedEnvironment::one("MODE", b"42");
        let diagnostic =
            replay_with_capabilities(&compiled, &target, &mut cache, &changed).unwrap_err();
        assert!(diagnostic.message.contains("expected external"));
    }

    #[test]
    fn transform_results_carry_stable_lineage_and_trace_is_first_class() {
        let compiled = crate::compile(
            "test.tima",
            "transform scale(x: f32, factor: f32) -> f32 { return x * factor }\n\
             out = 8.0 | scale(factor=0.25)\n\
             trace(out)\n",
        )
        .unwrap();
        let first = execute(&compiled).unwrap();
        let lineage = first.bindings["out"].lineage.as_ref().unwrap();
        let LineageNode::Invocation(invocation) = lineage.node() else {
            panic!("expected invocation lineage")
        };
        assert_eq!(invocation.transform_name.as_ref(), "scale");
        assert_eq!(
            invocation.transform_id,
            compiled.identities.get(TransformId(0))
        );
        assert_eq!(invocation.arguments.len(), 2);
        assert_eq!(invocation.arguments[0].value, RecordedValue::Float(8.0));
        assert_eq!(invocation.arguments[1].value, RecordedValue::Float(0.25));
        let ValueData::Lineage(inspected) = &first.last_value.as_ref().unwrap().data else {
            panic!("trace must return a first-class lineage value")
        };
        assert_eq!(inspected, lineage);
        assert!(inspected.render().contains("invoke scale"));

        let second = execute(&compiled).unwrap();
        let LineageNode::Invocation(second) =
            second.bindings["out"].lineage.as_ref().unwrap().node()
        else {
            unreachable!()
        };
        assert_eq!(invocation.recipe_id, second.recipe_id);
    }

    #[test]
    fn repeated_recipes_reuse_content_addressed_results() {
        let compiled = crate::compile(
            "test.tima",
            "transform scale(x: f32, factor: f32) -> f32 { return x * factor }\n\
             first = scale(8.0, 0.25)\n\
             second = scale(8.0, 0.25)\n",
        )
        .unwrap();
        let mut cache = TransformResultCache::default();
        let first_execution = execute_cached(&compiled, &mut cache).unwrap();
        assert_eq!(
            first_execution.bindings["first"].data,
            ValueData::Float(2.0)
        );
        assert_eq!(
            first_execution.bindings["second"].data,
            ValueData::Float(2.0)
        );
        assert_eq!(cache.stats().misses, 1);
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().stores, 1);
        assert_eq!(cache.content().len(), 1);

        let second_execution = execute_cached(&compiled, &mut cache).unwrap();
        assert_eq!(
            second_execution.bindings["second"].data,
            ValueData::Float(2.0)
        );
        assert_eq!(cache.stats().misses, 1);
        assert_eq!(cache.stats().hits, 3);
        let LineageNode::Invocation(first_lineage) = first_execution.bindings["first"]
            .lineage
            .as_ref()
            .unwrap()
            .node()
        else {
            unreachable!()
        };
        let LineageNode::Invocation(cached_lineage) = second_execution.bindings["second"]
            .lineage
            .as_ref()
            .unwrap()
            .node()
        else {
            unreachable!()
        };
        assert_eq!(first_lineage.recipe_id, cached_lineage.recipe_id);
    }

    #[test]
    fn replay_builtin_reuses_the_recorded_recipe() {
        let compiled = crate::compile(
            "test.tima",
            "transform scale(x: f32, factor: f32) -> f32 { return x * factor }\n\
             result = scale(8.0, 0.25)\n\
             replayed = replay(result)\n",
        )
        .unwrap();
        let mut cache = TransformResultCache::default();
        let execution = execute_cached(&compiled, &mut cache).unwrap();
        assert_eq!(execution.bindings["replayed"].data, ValueData::Float(2.0));
        assert_eq!(cache.stats().misses, 1);
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(
            execution.bindings["result"].lineage,
            execution.bindings["replayed"].lineage
        );
    }

    #[test]
    fn replay_reexecutes_invalidated_recipes_and_checks_content() {
        let compiled = crate::compile(
            "test.tima",
            "transform scale(x: f32, factor: f32) -> f32 { return x * factor }\n\
             result = scale(8.0, 0.25)\n",
        )
        .unwrap();
        let mut cache = TransformResultCache::default();
        let execution = execute_cached(&compiled, &mut cache).unwrap();
        let target = execution.bindings["result"].clone();
        let recipe = target.lineage.as_ref().unwrap().recipe_id().unwrap();
        cache.invalidate_recipe(recipe);
        let replayed = replay(&compiled, &target, &mut cache).unwrap();
        assert_eq!(replayed.data, ValueData::Float(2.0));
        assert_eq!(cache.stats().misses, 2);
        assert_eq!(cache.stats().stores, 2);

        cache.invalidate_recipe(recipe);
        let mut incorrect_recording = target;
        incorrect_recording.data = ValueData::Float(9.0);
        let diagnostic = replay(&compiled, &incorrect_recording, &mut cache).unwrap_err();
        assert!(diagnostic.message.contains("expected content hash"));
    }

    #[test]
    fn replay_rejects_changed_transform_definitions() {
        let original = crate::compile(
            "old.tima",
            "transform change(x: f32) -> f32 { return x * 2.0 }\nresult = change(4.0)\n",
        )
        .unwrap();
        let mut cache = TransformResultCache::default();
        let execution = execute_cached(&original, &mut cache).unwrap();
        let changed = crate::compile(
            "new.tima",
            "transform change(x: f32) -> f32 { return x * 3.0 }\n",
        )
        .unwrap();
        let diagnostic = replay(&changed, &execution.bindings["result"], &mut cache).unwrap_err();
        assert!(diagnostic.message.contains("unavailable or has changed"));
    }

    #[test]
    fn replay_validates_recorded_external_dependencies() {
        struct FixedDependency(ContentIdentity);

        impl ReplayDependencyResolver for FixedDependency {
            fn observe(&self, _capability: &str, _key: &[u8]) -> Result<ContentIdentity, String> {
                Ok(self.0)
            }
        }

        let compiled = crate::compile(
            "test.tima",
            "transform keep(x: f32) -> f32 { return x }\nresult = keep(4.0)\n",
        )
        .unwrap();
        let mut cache = TransformResultCache::default();
        let execution = execute_cached(&compiled, &mut cache).unwrap();
        let mut target = execution.bindings["result"].clone();
        let LineageNode::Invocation(invocation) = target.lineage.as_ref().unwrap().node() else {
            unreachable!()
        };
        let expected = byte_content_identity(b"font bytes");
        let observation =
            Lineage::external_observation("filesystem", b"font.ttf".as_slice(), expected);
        target.lineage = Some(
            Lineage::invocation(
                invocation.transform_name.clone(),
                invocation.transform_id,
                invocation.arguments.to_vec(),
                vec![observation],
            )
            .unwrap(),
        );
        let diagnostic = replay(&compiled, &target, &mut cache).unwrap_err();
        assert!(diagnostic.message.contains("requires a resolver"));

        let wrong = FixedDependency(byte_content_identity(b"changed font"));
        let diagnostic =
            replay_with_dependencies(&compiled, &target, &mut cache, Some(&wrong)).unwrap_err();
        assert!(diagnostic.message.contains("expected external"));

        let recipe = target.lineage.as_ref().unwrap().recipe_id().unwrap();
        cache.store(recipe, &target).unwrap();
        let matching = FixedDependency(expected);
        let replayed =
            replay_with_dependencies(&compiled, &target, &mut cache, Some(&matching)).unwrap();
        assert_eq!(replayed.data, ValueData::Float(4.0));
        assert_eq!(replayed.lineage, target.lineage);
    }

    #[test]
    fn outer_composites_are_shared_but_not_mutable() {
        let compiled = crate::compile("test.tima", "x = [1, {name: \"cat\"}]\ny = x\n").unwrap();
        let execution = execute(&compiled).unwrap();
        let ValueData::List(left) = &execution.bindings["x"].data else {
            panic!("expected list")
        };
        let ValueData::List(right) = &execution.bindings["y"].data else {
            panic!("expected list")
        };
        assert!(Arc::ptr_eq(left, right));
    }

    #[test]
    fn outer_scalar_comparisons_follow_shared_expression_precedence() {
        let compiled = crate::compile(
            "test.tima",
            "ordered = 1 + 2 < 4\n\
             same = \"cat\" == \"cat\"\n\
             different = true != false\n",
        )
        .unwrap();
        let execution = execute(&compiled).unwrap();
        assert_eq!(execution.bindings["ordered"].data, ValueData::Bool(true));
        assert_eq!(execution.bindings["same"].data, ValueData::Bool(true));
        assert_eq!(execution.bindings["different"].data, ValueData::Bool(true));

        let unsupported = crate::compile("bad.tima", "result = [1] == [1]\n").unwrap();
        let diagnostics = execute(&unsupported).unwrap_err();
        assert!(diagnostics[0].message.contains("same scalar type"));
    }

    #[test]
    fn asset_is_a_logical_locator_not_eager_io() {
        let compiled = crate::compile("test.tima", "img = asset(\"cat.png\")\n").unwrap();
        let execution = execute(&compiled).unwrap();
        let ValueData::Asset(asset) = &execution.bindings["img"].data else {
            panic!("expected asset")
        };
        assert_eq!(&*asset.locator, "cat.png");
        let LineageNode::Source(source) =
            execution.bindings["img"].lineage.as_ref().unwrap().node()
        else {
            panic!("asset must carry source lineage")
        };
        assert_eq!(source.locator.as_ref(), "cat.png");
        assert_eq!(source.observed_content, None);
    }

    #[test]
    fn save_requires_an_explicit_asset_output_capability() {
        let compiled =
            crate::compile("test.tima", "saved = encoded | save(\"out.ppm\")\n").unwrap();
        let engine = IrInterpreter {
            module: &compiled.transforms,
            capabilities: None,
        };
        let bindings = BTreeMap::from([(
            "encoded".to_owned(),
            OuterValue::plain(ValueData::Bytes(Arc::new(b"P3\n".to_vec()))),
        )]);

        let diagnostics = execute_with(&compiled, &engine, bindings, None).unwrap_err();

        assert!(diagnostics[0].message.contains("output is unavailable"));
    }

    #[test]
    fn rgba8_channel_scaling_saturates_and_truncates() {
        assert_eq!(scale_rgba8_channel(101, 0.5), 50);
        assert_eq!(scale_rgba8_channel(200, 2.0), 255);
        assert_eq!(scale_rgba8_channel(200, -1.0), 0);
        assert_eq!(scale_rgba8_channel(200, f32::NAN), 0);
        assert_eq!(scale_rgba8_channel(1, f32::INFINITY), 255);
    }

    #[test]
    fn unique_owned_image_storage_transfers_through_inner_calls_without_copying() {
        let compiled = crate::compile(
            "test.tima",
            "transform own(img: Image) -> Image { return img }\n\
             transform own_inner(img: Image) -> Image { return own(img) }\n",
        )
        .unwrap();
        let interpreter = IrInterpreter {
            module: &compiled.transforms,
            capabilities: None,
        };
        let image = ImageValue::new(2, 2, 2, vec![1, 2, 3, 4]).unwrap();
        let result = interpreter
            .invoke(
                crate::ir::TransformId(1),
                vec![(OuterValue::image(image), Span::default())],
            )
            .unwrap()
            .value;
        let ValueData::Image(result) = result.data else {
            panic!("expected image result")
        };
        assert_eq!(result.bytes(), &[1, 2, 3, 4]);
    }

    #[test]
    fn interpreted_owned_images_detach_while_views_alias() {
        let compiled = crate::compile(
            "test.tima",
            "transform clear(img: Image) -> Image { return image_zero(img) }\n\
             transform view(img: ImageView) -> ImageView { return img }\n\
             cleared = clear(img)\n\
             viewed = view(img)\n",
        )
        .unwrap();
        let engine = IrInterpreter {
            module: &compiled.transforms,
            capabilities: None,
        };
        let execution = execute_with(
            &compiled,
            &engine,
            BTreeMap::from([(
                "img".to_owned(),
                OuterValue::image(ImageValue::new(2, 2, 2, vec![1, 2, 3, 4]).unwrap()),
            )]),
            None,
        )
        .unwrap();
        let ValueData::Image(original) = &execution.bindings["img"].data else {
            panic!("expected original image")
        };
        let ValueData::Image(cleared) = &execution.bindings["cleared"].data else {
            panic!("expected owned result image")
        };
        let ValueData::Image(viewed) = &execution.bindings["viewed"].data else {
            panic!("expected image view")
        };

        assert_eq!(original.bytes(), &[1, 2, 3, 4]);
        assert_eq!(cleared.bytes(), &[0, 0, 0, 0]);
        assert!(!original.shares_storage_with(cleared));
        assert!(original.shares_storage_with(viewed));
    }

    #[test]
    fn native_owned_images_detach_while_views_alias() {
        let compiled = crate::compile(
            "native-images.tima",
            "transform fill(img: Image, value: u8) -> Image { return image_fill(img, value) }\n\
             transform view(img: ImageView) -> ImageView { return img }\n\
             filled = fill(img, 7)\n\
             viewed = view(img)\n",
        )
        .unwrap();
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("build")
            .join(format!("native-image-runtime-{}", std::process::id()));
        let native = NativeModule::build(&compiled.transforms, &compiled.identities, root)
            .unwrap()
            .unwrap();
        let engine = HybridAotEngine {
            interpreter: IrInterpreter {
                module: &compiled.transforms,
                capabilities: None,
            },
            native: Some(&native),
        };
        let execution = execute_with(
            &compiled,
            &engine,
            BTreeMap::from([(
                "img".to_owned(),
                OuterValue::image(ImageValue::new(2, 2, 2, vec![1, 2, 3, 4]).unwrap()),
            )]),
            None,
        )
        .unwrap();
        let ValueData::Image(original) = &execution.bindings["img"].data else {
            panic!("expected original image")
        };
        let ValueData::Image(filled) = &execution.bindings["filled"].data else {
            panic!("expected owned result image")
        };
        let ValueData::Image(viewed) = &execution.bindings["viewed"].data else {
            panic!("expected image view")
        };

        assert_eq!(original.bytes(), &[1, 2, 3, 4]);
        assert_eq!(filled.bytes(), &[7, 7, 7, 7]);
        assert!(!original.shares_storage_with(filled));
        assert!(original.shares_storage_with(viewed));
    }

    #[test]
    fn native_owned_buffers_detach_while_views_alias() {
        let compiled = crate::compile(
            "native-buffers.tima",
            "transform own_text(value: String) -> String { return value }\n\
             transform view_text(value: StringView) -> StringView { return value }\n\
             transform own_bytes(value: Bytes) -> Bytes { return value }\n\
             transform view_bytes(value: BytesView) -> BytesView { return value }\n\
             owned_text = own_text(text)\n\
             viewed_text = view_text(text)\n\
             owned_bytes = own_bytes(blob)\n\
             viewed_bytes = view_bytes(blob)\n",
        )
        .unwrap();
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("build")
            .join(format!("native-buffer-runtime-{}", std::process::id()));
        let native = NativeModule::build(&compiled.transforms, &compiled.identities, root)
            .unwrap()
            .unwrap();
        let engine = HybridAotEngine {
            interpreter: IrInterpreter {
                module: &compiled.transforms,
                capabilities: None,
            },
            native: Some(&native),
        };
        let execution = execute_with(
            &compiled,
            &engine,
            BTreeMap::from([
                (
                    "text".to_owned(),
                    OuterValue::plain(ValueData::String(Arc::new("hello".to_owned()))),
                ),
                (
                    "blob".to_owned(),
                    OuterValue::plain(ValueData::Bytes(Arc::new(vec![1, 2, 3, 4]))),
                ),
            ]),
            None,
        )
        .unwrap();

        let ValueData::String(text) = &execution.bindings["text"].data else {
            panic!("expected original string")
        };
        let ValueData::String(owned_text) = &execution.bindings["owned_text"].data else {
            panic!("expected owned string result")
        };
        let ValueData::String(viewed_text) = &execution.bindings["viewed_text"].data else {
            panic!("expected string view result")
        };
        assert_eq!(owned_text.as_str(), "hello");
        assert!(!Arc::ptr_eq(text, owned_text));
        assert!(Arc::ptr_eq(text, viewed_text));

        let ValueData::Bytes(blob) = &execution.bindings["blob"].data else {
            panic!("expected original bytes")
        };
        let ValueData::Bytes(owned_bytes) = &execution.bindings["owned_bytes"].data else {
            panic!("expected owned bytes result")
        };
        let ValueData::Bytes(viewed_bytes) = &execution.bindings["viewed_bytes"].data else {
            panic!("expected bytes view result")
        };
        assert_eq!(owned_bytes.as_slice(), &[1, 2, 3, 4]);
        assert!(!Arc::ptr_eq(blob, owned_bytes));
        assert!(Arc::ptr_eq(blob, viewed_bytes));
    }

    #[test]
    fn unique_owned_string_and_byte_allocations_cross_the_interpreter_without_copying() {
        let text = String::from("allocation stays put");
        let text_pointer = text.as_ptr();
        let lowered = lower_interpreted_value(
            OuterValue::plain(ValueData::String(Arc::new(text))),
            crate::ir::Type::String,
            Span::default(),
        )
        .unwrap();
        let frozen = freeze_interpreted_value(lowered);
        let ValueData::String(text) = frozen.data else {
            panic!("expected frozen string")
        };
        assert_eq!(text.as_ptr(), text_pointer);

        let bytes = vec![1, 2, 3, 4, 5];
        let bytes_pointer = bytes.as_ptr();
        let lowered = lower_interpreted_value(
            OuterValue::plain(ValueData::Bytes(Arc::new(bytes))),
            crate::ir::Type::Bytes,
            Span::default(),
        )
        .unwrap();
        let frozen = freeze_interpreted_value(lowered);
        let ValueData::Bytes(bytes) = frozen.data else {
            panic!("expected frozen bytes")
        };
        assert_eq!(bytes.as_ptr(), bytes_pointer);
    }

    #[test]
    fn native_rgba8_scaling_matches_interpreted_semantics() {
        let compiled = crate::compile(
            "native-rgba.tima",
            "transform adjust(img: Image, r: f32, g: f32, b: f32, a: f32) -> Image {\n\
                 for p in img.pixels {\n\
                     p.r *= r\n\
                     p.g *= g\n\
                     p.b *= b\n\
                     p.a *= a\n\
                 }\n\
                 return img\n\
             }\n\
             out = adjust(img, r, g, b, a)\n",
        )
        .unwrap();
        let bindings = BTreeMap::from([
            (
                "img".to_owned(),
                OuterValue::image(
                    ImageValue::new_rgba8(
                        2,
                        2,
                        10,
                        vec![
                            101, 200, 200, 1, 2, 3, 4, 5, 99, 100, 255, 10, 6, 10, 3, 4, 5, 6, 77,
                            88,
                        ],
                    )
                    .unwrap(),
                ),
            ),
            ("r".to_owned(), OuterValue::plain(ValueData::Float(0.5))),
            ("g".to_owned(), OuterValue::plain(ValueData::Float(2.0))),
            ("b".to_owned(), OuterValue::plain(ValueData::Float(-1.0))),
            (
                "a".to_owned(),
                OuterValue::plain(ValueData::Float(f32::INFINITY)),
            ),
        ]);
        let interpreted = execute_with(
            &compiled,
            &IrInterpreter {
                module: &compiled.transforms,
                capabilities: None,
            },
            bindings.clone(),
            None,
        )
        .unwrap();

        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("build")
            .join(format!("native-rgba-runtime-{}", std::process::id()));
        let native = NativeModule::build(&compiled.transforms, &compiled.identities, root)
            .unwrap()
            .unwrap();
        let native = execute_with(
            &compiled,
            &HybridAotEngine {
                interpreter: IrInterpreter {
                    module: &compiled.transforms,
                    capabilities: None,
                },
                native: Some(&native),
            },
            bindings,
            None,
        )
        .unwrap();

        assert_eq!(native.bindings["out"], interpreted.bindings["out"]);
        let ValueData::Image(result) = &native.bindings["out"].data else {
            panic!("expected native RGBA8 image")
        };
        assert_eq!(
            result.bytes(),
            &[
                50, 255, 0, 255, 1, 6, 0, 255, 99, 100, 127, 20, 0, 255, 1, 8, 0, 255, 77, 88,
            ]
        );
    }

    #[test]
    fn native_byte_map_calls_match_interpreted_semantics() {
        let compiled = crate::compile(
            "native-byte-map.tima",
            "transform choose(current: u8, target: u8, replacement: u8) -> u8 {\n\
                 if current == target { return replacement } else { return current }\n\
             }\n\
             transform replace(img: Image, target: u8, replacement: u8) -> Image {\n\
                 for byte in img.bytes { byte = choose(byte, target, replacement) }\n\
                 return img\n\
             }\n\
             out = replace(img, target, replacement)\n",
        )
        .unwrap();
        let bindings = BTreeMap::from([
            (
                "img".to_owned(),
                OuterValue::image(ImageValue::new(3, 2, 4, vec![1, 2, 1, 8, 3, 1, 4, 1]).unwrap()),
            ),
            (
                "target".to_owned(),
                OuterValue::plain(ValueData::Integer(1)),
            ),
            (
                "replacement".to_owned(),
                OuterValue::plain(ValueData::Integer(9)),
            ),
        ]);
        let interpreted = execute_with(
            &compiled,
            &IrInterpreter {
                module: &compiled.transforms,
                capabilities: None,
            },
            bindings.clone(),
            None,
        )
        .unwrap();

        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("build")
            .join(format!("native-byte-map-runtime-{}", std::process::id()));
        let native_module = NativeModule::build(&compiled.transforms, &compiled.identities, root)
            .unwrap()
            .unwrap();
        let native = execute_with(
            &compiled,
            &HybridAotEngine {
                interpreter: IrInterpreter {
                    module: &compiled.transforms,
                    capabilities: None,
                },
                native: Some(&native_module),
            },
            bindings,
            None,
        )
        .unwrap();

        assert_eq!(native.bindings["out"], interpreted.bindings["out"]);
        let ValueData::Image(original) = &native.bindings["img"].data else {
            panic!("expected original byte image")
        };
        let ValueData::Image(result) = &native.bindings["out"].data else {
            panic!("expected native byte-map image")
        };
        assert_eq!(original.bytes(), &[1, 2, 1, 8, 3, 1, 4, 1]);
        assert_eq!(result.bytes(), &[9, 2, 9, 8, 3, 9, 4, 9]);
    }

    #[test]
    fn validates_outer_image_layouts() {
        let error = ImageValue::new(2, 2, 2, vec![0; 3]).unwrap_err();
        assert!(error.to_string().contains("require 4"));

        let rgba = ImageValue::new_rgba8(2, 1, 8, vec![0; 8]).unwrap();
        assert_eq!(rgba.format(), ImageFormat::Rgba8);
        let error = ImageValue::new_rgba8(2, 1, 7, vec![0; 7]).unwrap_err();
        assert!(error.to_string().contains("width 2 times 4"));
    }
}
