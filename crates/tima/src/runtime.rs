use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::error::Error;
use std::ffi::c_void;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use crate::CompiledProgram;
use crate::ast::{Argument, BinaryOp, ExprId, ExprKind, Item};
use crate::backend::native::{AbiImage, AbiImageView, AbiRuntime, AbiValue, NativeModule};
use crate::cache::TransformResultCache;
use crate::capability::{CapabilitySession, RuntimeCapabilities, observe_dependency};
use crate::diagnostic::Diagnostic;
use crate::identity::{ContentIdentity, content_identity};
use crate::ir::{Constant, RuntimeCall, Terminator, TransformId, Type, ValueKind};
use crate::lineage::{Lineage, LineageArgument, LineageNode, RecordedValue};
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
    String(Arc<str>),
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

#[derive(Debug, PartialEq, Eq)]
struct ImageStorage {
    bytes: Vec<u8>,
}

impl ImageStorage {
    fn new(mut bytes: Vec<u8>) -> Self {
        // Empty Vec pointers are shared dangling sentinels, which cannot serve
        // as invocation-local storage identities. Reserve one byte without
        // changing the logical image length so even empty images have a live,
        // unique allocation while crossing the native boundary.
        if bytes.is_empty() {
            bytes.reserve_exact(1);
        }
        Self { bytes }
    }
}

/// Immutable outer image descriptor backed by shareable byte storage.
///
/// `width` and `height` are logical dimensions, while `stride` is the backing
/// byte count per row. Pixel format is deliberately outside this first
/// ownership milestone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageValue {
    storage: Arc<ImageStorage>,
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
        validate_image_layout(height, stride, bytes.len())?;
        Ok(Self {
            storage: Arc::new(ImageStorage::new(bytes)),
            width,
            height,
            stride,
        })
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

    pub fn bytes(&self) -> &[u8] {
        &self.storage.bytes
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
    height: usize,
    stride: usize,
    byte_len: usize,
) -> Result<(), ImageLayoutError> {
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

pub fn execute(program: &CompiledProgram) -> Result<Execution, Vec<Diagnostic>> {
    let engine = IrInterpreter {
        module: &program.transforms,
        capabilities: None,
    };
    execute_with(program, &engine, BTreeMap::new(), None)
}

pub fn execute_cached(
    program: &CompiledProgram,
    cache: &mut TransformResultCache,
) -> Result<Execution, Vec<Diagnostic>> {
    let engine = IrInterpreter {
        module: &program.transforms,
        capabilities: None,
    };
    execute_with(program, &engine, BTreeMap::new(), Some(cache))
}

pub fn execute_with_capabilities(
    program: &CompiledProgram,
    capabilities: &dyn RuntimeCapabilities,
) -> Result<Execution, Vec<Diagnostic>> {
    let engine = IrInterpreter {
        module: &program.transforms,
        capabilities: Some(capabilities),
    };
    execute_with(program, &engine, BTreeMap::new(), None)
}

pub fn execute_cached_with_capabilities(
    program: &CompiledProgram,
    cache: &mut TransformResultCache,
    capabilities: &dyn RuntimeCapabilities,
) -> Result<Execution, Vec<Diagnostic>> {
    let engine = IrInterpreter {
        module: &program.transforms,
        capabilities: Some(capabilities),
    };
    execute_with(program, &engine, BTreeMap::new(), Some(cache))
}

/// Executes outer code while dispatching transform calls to a loaded native
/// artifact. Outer expressions remain interpreted; only checked inner
/// transforms cross this boundary.
pub fn execute_native(
    program: &CompiledProgram,
    native: &NativeModule,
) -> Result<Execution, Vec<Diagnostic>> {
    let engine = NativeEngine {
        module: &program.transforms,
        native,
        capabilities: None,
    };
    execute_with(program, &engine, BTreeMap::new(), None)
}

pub fn execute_native_cached(
    program: &CompiledProgram,
    native: &NativeModule,
    cache: &mut TransformResultCache,
) -> Result<Execution, Vec<Diagnostic>> {
    let engine = NativeEngine {
        module: &program.transforms,
        native,
        capabilities: None,
    };
    execute_with(program, &engine, BTreeMap::new(), Some(cache))
}

pub fn execute_native_with_capabilities(
    program: &CompiledProgram,
    native: &NativeModule,
    capabilities: &dyn RuntimeCapabilities,
) -> Result<Execution, Vec<Diagnostic>> {
    let engine = NativeEngine {
        module: &program.transforms,
        native,
        capabilities: Some(capabilities),
    };
    execute_with(program, &engine, BTreeMap::new(), None)
}

pub fn execute_native_cached_with_capabilities(
    program: &CompiledProgram,
    native: &NativeModule,
    cache: &mut TransformResultCache,
    capabilities: &dyn RuntimeCapabilities,
) -> Result<Execution, Vec<Diagnostic>> {
    let engine = NativeEngine {
        module: &program.transforms,
        native,
        capabilities: Some(capabilities),
    };
    execute_with(program, &engine, BTreeMap::new(), Some(cache))
}

/// Runs a program with immutable values supplied by the Histima host runtime.
/// This is the initial integration point for materialized asset-native values;
/// it avoids inventing source-language image literal semantics.
pub fn execute_native_with_bindings(
    program: &CompiledProgram,
    native: &NativeModule,
    bindings: BTreeMap<String, OuterValue>,
) -> Result<Execution, Vec<Diagnostic>> {
    let engine = NativeEngine {
        module: &program.transforms,
        native,
        capabilities: None,
    };
    execute_with(program, &engine, bindings, None)
}

pub fn execute_native_with_bindings_cached(
    program: &CompiledProgram,
    native: &NativeModule,
    bindings: BTreeMap<String, OuterValue>,
    cache: &mut TransformResultCache,
) -> Result<Execution, Vec<Diagnostic>> {
    let engine = NativeEngine {
        module: &program.transforms,
        native,
        capabilities: None,
    };
    execute_with(program, &engine, bindings, Some(cache))
}

/// Invokes one checked transform with owned outer arguments. This makes the
/// ownership transition directly usable by Histima and testable independently
/// of outer binding liveness.
pub fn invoke_native_transform(
    program: &CompiledProgram,
    native: &NativeModule,
    transform: TransformId,
    arguments: Vec<OuterValue>,
) -> Result<OuterValue, Diagnostic> {
    let definition = program.transforms.get(transform);
    if arguments.len() != definition.parameters.len() {
        return Err(Diagnostic::error(
            format!(
                "transform `{}` expects {} arguments, but {} were supplied",
                definition.name,
                definition.parameters.len(),
                arguments.len()
            ),
            definition.span,
        ));
    }
    let arguments = arguments
        .into_iter()
        .map(|value| (value, definition.span))
        .collect();
    let engine = NativeEngine {
        module: &program.transforms,
        native,
        capabilities: None,
    };
    invoke_transform_with_lineage(program, &engine, transform, arguments, None)
}

pub fn invoke_native_transform_cached(
    program: &CompiledProgram,
    native: &NativeModule,
    transform: TransformId,
    arguments: Vec<OuterValue>,
    cache: &mut TransformResultCache,
) -> Result<OuterValue, Diagnostic> {
    let definition = program.transforms.get(transform);
    if arguments.len() != definition.parameters.len() {
        return Err(Diagnostic::error(
            format!(
                "transform `{}` expects {} arguments, but {} were supplied",
                definition.name,
                definition.parameters.len(),
                arguments.len()
            ),
            definition.span,
        ));
    }
    let arguments = arguments
        .into_iter()
        .map(|value| (value, definition.span))
        .collect();
    let engine = NativeEngine {
        module: &program.transforms,
        native,
        capabilities: None,
    };
    invoke_transform_with_lineage(program, &engine, transform, arguments, Some(cache))
}

/// Host hook used to validate precise external observations before replay.
/// Replay without external observations does not require a resolver.
pub trait ReplayDependencyResolver {
    fn observe(&self, capability: &str, key: &[u8]) -> Result<ContentIdentity, String>;
}

struct CapabilityReplayResolver<'a>(&'a dyn RuntimeCapabilities);

impl ReplayDependencyResolver for CapabilityReplayResolver<'_> {
    fn observe(&self, capability: &str, key: &[u8]) -> Result<ContentIdentity, String> {
        observe_dependency(self.0, capability, key)
    }
}

pub fn replay(
    program: &CompiledProgram,
    target: &OuterValue,
    cache: &mut TransformResultCache,
) -> Result<OuterValue, Diagnostic> {
    replay_with_dependencies(program, target, cache, None)
}

pub fn replay_with_dependencies(
    program: &CompiledProgram,
    target: &OuterValue,
    cache: &mut TransformResultCache,
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
    cache: &mut TransformResultCache,
    capabilities: &dyn RuntimeCapabilities,
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

pub fn replay_native(
    program: &CompiledProgram,
    native: &NativeModule,
    target: &OuterValue,
    cache: &mut TransformResultCache,
) -> Result<OuterValue, Diagnostic> {
    replay_native_with_dependencies(program, native, target, cache, None)
}

pub fn replay_native_with_dependencies(
    program: &CompiledProgram,
    native: &NativeModule,
    target: &OuterValue,
    cache: &mut TransformResultCache,
    dependencies: Option<&dyn ReplayDependencyResolver>,
) -> Result<OuterValue, Diagnostic> {
    let engine = NativeEngine {
        module: &program.transforms,
        native,
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

pub fn replay_native_with_capabilities(
    program: &CompiledProgram,
    native: &NativeModule,
    target: &OuterValue,
    cache: &mut TransformResultCache,
    capabilities: &dyn RuntimeCapabilities,
) -> Result<OuterValue, Diagnostic> {
    let engine = NativeEngine {
        module: &program.transforms,
        native,
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

fn execute_with(
    program: &CompiledProgram,
    engine: &dyn TransformEngine,
    bindings: BTreeMap<String, OuterValue>,
    cache: Option<&mut TransformResultCache>,
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
}

struct TransformOutcome {
    value: OuterValue,
    observations: Vec<Lineage>,
}

fn invoke_transform_with_lineage(
    program: &CompiledProgram,
    engine: &dyn TransformEngine,
    id: TransformId,
    arguments: Vec<(OuterValue, Span)>,
    mut cache: Option<&mut TransformResultCache>,
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

fn replay_with(
    program: &CompiledProgram,
    engine: &dyn TransformEngine,
    target: &OuterValue,
    cache: &mut TransformResultCache,
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

#[allow(clippy::too_many_arguments)]
fn replay_lineage(
    program: &CompiledProgram,
    engine: &dyn TransformEngine,
    lineage: &Lineage,
    expected_content: Option<ContentIdentity>,
    cache: &mut TransformResultCache,
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
    let Some(transform_id) = program.identities.find_id(invocation.transform_id) else {
        return Err(Diagnostic::error(
            format!(
                "recorded transform definition {} is unavailable or has changed",
                invocation.transform_id
            ),
            span,
        ));
    };
    let transform = program.transforms.get(transform_id);
    if transform.parameters.len() != invocation.arguments.len() {
        return Err(Diagnostic::error(
            format!(
                "recorded invocation has {} arguments, but transform `{}` now expects {}",
                invocation.arguments.len(),
                transform.name,
                transform.parameters.len()
            ),
            span,
        ));
    }
    validate_replay_dependencies(invocation, dependencies, span)?;

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
    let TransformOutcome {
        mut value,
        observations,
    } = engine.invoke(transform_id, runtime_arguments)?;
    let observed_lineage = Lineage::invocation(
        transform.name.as_str(),
        program.identities.get(transform_id),
        invocation.arguments.to_vec(),
        observations,
    )
    .map_err(|error| Diagnostic::error(error.to_string(), span))?;
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
    cache: &mut TransformResultCache,
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
            if let Some(value) = cache.content().get(*content_id) {
                value
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
    invocation: &crate::lineage::InvocationLineage,
    resolver: Option<&dyn ReplayDependencyResolver>,
    span: Span,
) -> Result<(), Diagnostic> {
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
    cache: Option<&'cache mut TransformResultCache>,
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
                OuterValue::plain(ValueData::String(Arc::from(value.as_str())))
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
            ExprKind::Pipeline { input, stage } => {
                let input = self.expression(*input)?;
                match &self.program.syntax.expr(*stage).kind {
                    ExprKind::Call { callee, arguments } => {
                        self.call(*callee, arguments, Some(input), expression.span)?
                    }
                    ExprKind::Name(_) => self.call(*stage, &[], Some(input), expression.span)?,
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
        let ExprKind::Name(name) = &callee_expression.kind else {
            return Err(Diagnostic::error(
                "outer calls require a directly named builtin or transform",
                callee_expression.span,
            ));
        };
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
            return self.asset(evaluated, span);
        }
        if name == "trace" {
            return self.trace(evaluated, span);
        }
        if name == "replay" {
            return self.replay_call(evaluated, span);
        }
        let Some((id, transform)) = self.program.transforms.find(name) else {
            return Err(Diagnostic::error(
                format!("unknown outer callable `{name}`"),
                callee_expression.span,
            ));
        };
        let mut ordered: Vec<Option<(OuterValue, Span)>> = vec![None; transform.parameters.len()];
        let mut next_positional = 0;
        for (name, value, argument_span) in evaluated {
            let index = if let Some(name) = name {
                transform
                    .parameters
                    .iter()
                    .position(|parameter| parameter.name == name)
                    .ok_or_else(|| {
                        Diagnostic::error(
                            format!("transform `{}` has no parameter `{name}`", transform.name),
                            argument_span,
                        )
                    })?
            } else {
                while next_positional < ordered.len() && ordered[next_positional].is_some() {
                    next_positional += 1;
                }
                if next_positional == ordered.len() {
                    return Err(Diagnostic::error(
                        format!("too many arguments for transform `{}`", transform.name),
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
                        transform.parameters[index].name
                    ),
                    argument_span,
                ));
            }
            ordered[index] = Some((value, argument_span));
        }
        let mut values = Vec::new();
        for (index, value) in ordered.into_iter().enumerate() {
            let Some(value) = value else {
                return Err(Diagnostic::error(
                    format!(
                        "missing argument `{}` for transform `{}`",
                        transform.parameters[index].name, transform.name
                    ),
                    span,
                ));
            };
            values.push(value);
        }
        invoke_transform_with_lineage(
            self.program,
            self.engine,
            id,
            values,
            self.cache.as_deref_mut(),
        )
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
        Ok(OuterValue::plain(ValueData::Asset(Arc::new(AssetValue {
            locator: locator.clone(),
        })))
        .with_lineage(Lineage::source(locator.clone(), None)))
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
        if let Some(cache) = self.cache.as_deref_mut() {
            replay_with(
                self.program,
                self.engine,
                &arguments[0].1,
                cache,
                None,
                span,
            )
        } else {
            let mut cache = TransformResultCache::default();
            replay_with(
                self.program,
                self.engine,
                &arguments[0].1,
                &mut cache,
                None,
                span,
            )
        }
    }
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
    I64(i64),
    F32(f32),
}

enum InterpretedValue {
    Scalar(NativeScalar),
    Image(InterpretedImage),
    ImageView(Arc<ImageValue>),
}

impl InterpretedValue {
    fn scalar(&self) -> NativeScalar {
        match self {
            Self::Scalar(value) => *value,
            Self::Image(_) | Self::ImageView(_) => {
                unreachable!("typed scalar operation received an image")
            }
        }
    }
}

struct InterpretedImage {
    storage: ImageStorage,
    width: usize,
    height: usize,
    stride: usize,
}

struct IrInterpreter<'a> {
    module: &'a crate::ir::TypedModule,
    capabilities: Option<&'a dyn RuntimeCapabilities>,
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
        if depth >= 256 {
            return Err(Diagnostic::error(
                "transform call depth exceeded the interpreter limit",
                transform.span,
            ));
        }
        let mut values = (0..transform.values.len())
            .map(|_| None)
            .collect::<Vec<_>>();
        for (parameter, (argument, span)) in transform.parameters.iter().zip(arguments) {
            values[parameter.value.0 as usize] =
                Some(lower_interpreted_value(argument, parameter.ty, span)?);
        }
        let mut current = transform.entry;
        loop {
            let block = &transform.blocks[current.0 as usize];
            for id in &block.instructions {
                let value = transform.value(*id);
                let evaluated = match &value.kind {
                    ValueKind::Parameter { .. } => unreachable!(),
                    ValueKind::Constant(constant) => InterpretedValue::Scalar(match constant {
                        Constant::Bool(value) => NativeScalar::Bool(*value),
                        Constant::I64(value) => NativeScalar::I64(*value),
                        Constant::F32(value) => NativeScalar::F32(*value),
                    }),
                    ValueKind::Binary { op, left, right } => {
                        InterpretedValue::Scalar(native_binary(
                            *op,
                            values[left.0 as usize].as_ref().unwrap().scalar(),
                            values[right.0 as usize].as_ref().unwrap().scalar(),
                            value.span,
                        )?)
                    }
                    ValueKind::Call {
                        transform: callee,
                        arguments,
                    } => {
                        let callee_transform = self.module.get(*callee);
                        let call_span = value.span;
                        let call_arguments = arguments
                            .iter()
                            .zip(&callee_transform.parameters)
                            .map(|(argument, parameter)| {
                                clone_interpreted_argument(
                                    values[argument.0 as usize].as_ref().unwrap(),
                                    parameter.ty,
                                    value.span,
                                )
                                .map(|value| (value, call_span))
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        let result =
                            self.invoke_at_depth(*callee, call_arguments, depth + 1, capabilities)?;
                        lower_interpreted_value(result, callee_transform.return_type, value.span)?
                    }
                    ValueKind::ImageZero { image } => {
                        let Some(InterpretedValue::Image(mut image)) =
                            values[image.0 as usize].take()
                        else {
                            unreachable!("typed image_zero input is an available owned image")
                        };
                        image.storage.bytes.fill(0);
                        InterpretedValue::Image(image)
                    }
                    ValueKind::RuntimeCall(RuntimeCall::EnvironmentI64 { name }) => {
                        InterpretedValue::Scalar(NativeScalar::I64(
                            capabilities.environment_i64(name, value.span)?,
                        ))
                    }
                };
                values[id.0 as usize] = Some(evaluated);
            }
            match block.terminator {
                Terminator::Return(value) => {
                    return Ok(freeze_interpreted_value(
                        values[value.0 as usize].take().unwrap(),
                    ));
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
}

struct NativeEngine<'a> {
    module: &'a crate::ir::TypedModule,
    native: &'a NativeModule,
    capabilities: Option<&'a dyn RuntimeCapabilities>,
}

impl TransformEngine for NativeEngine<'_> {
    fn invoke(
        &self,
        id: TransformId,
        arguments: Vec<(OuterValue, Span)>,
    ) -> Result<TransformOutcome, Diagnostic> {
        self.validate_path(id, &mut BTreeSet::new(), &mut BTreeSet::new())?;
        let transform = self.module.get(id);
        let mut lowered = Vec::with_capacity(arguments.len());
        for (parameter, (argument, span)) in transform.parameters.iter().zip(arguments) {
            lowered.push(lower_native_argument(argument, parameter.ty, span)?);
        }
        let abi_arguments = lowered
            .iter()
            .map(|argument| argument.abi)
            .collect::<Vec<_>>();
        let mut capability_context = NativeCapabilityContext {
            session: CapabilitySession::new(self.capabilities),
            module: self.module,
            error: None,
        };
        let mut abi_runtime = AbiRuntime {
            context: (&mut capability_context as *mut NativeCapabilityContext<'_>).cast(),
            environment_i64: native_environment_i64,
            status: 0,
        };
        let result = match self.native.invoke(id, &mut abi_runtime, &abi_arguments) {
            Ok(result) => result,
            Err(error) => {
                return Err(capability_context
                    .error
                    .take()
                    .unwrap_or_else(|| Diagnostic::error(error.to_string(), transform.span)));
            }
        };
        let observations = capability_context.session.finish();
        Ok(TransformOutcome {
            value: freeze_native_result(
                result,
                transform.return_type,
                &mut lowered,
                transform.span,
            )?,
            observations,
        })
    }

    fn may_observe_dependencies(&self, id: TransformId) -> bool {
        transform_may_observe_dependencies(self.module, id)
    }
}

struct NativeCapabilityContext<'a> {
    session: CapabilitySession<'a>,
    module: &'a crate::ir::TypedModule,
    error: Option<Diagnostic>,
}

unsafe extern "C" fn native_environment_i64(
    context: *mut c_void,
    transform: u32,
    callsite: u32,
    name: *const u8,
    name_len: usize,
    result: *mut i64,
) -> i32 {
    if context.is_null() || name.is_null() || result.is_null() {
        return -3;
    }
    // SAFETY: generated C passes back the invocation-local context pointer and
    // a name byte slice that remains live for the synchronous callback.
    let context = unsafe { &mut *context.cast::<NativeCapabilityContext<'_>>() };
    let span = context
        .module
        .transforms
        .get(transform as usize)
        .and_then(|transform| transform.values.get(callsite as usize))
        .map_or(Span::default(), |value| value.span);
    let name = unsafe { std::slice::from_raw_parts(name, name_len) };
    let name = match std::str::from_utf8(name) {
        Ok(name) => name,
        Err(_) => {
            context.error = Some(Diagnostic::error(
                "native transform requested an invalid UTF-8 environment name",
                span,
            ));
            return -4;
        }
    };
    match catch_unwind(AssertUnwindSafe(|| {
        context.session.environment_i64(name, span)
    })) {
        Ok(Ok(value)) => {
            unsafe { *result = value };
            0
        }
        Ok(Err(error)) => {
            context.error = Some(error);
            -5
        }
        Err(_) => {
            context.error = Some(
                Diagnostic::error(
                    format!("environment capability panicked while reading `{name}`"),
                    span,
                )
                .with_note("runtime capability providers must not unwind across the native ABI"),
            );
            -6
        }
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

impl NativeEngine<'_> {
    fn validate_path(
        &self,
        id: TransformId,
        visiting: &mut BTreeSet<u32>,
        visited: &mut BTreeSet<u32>,
    ) -> Result<(), Diagnostic> {
        if visited.contains(&id.0) {
            return Ok(());
        }
        let transform = self.module.get(id);
        if !visiting.insert(id.0) {
            return Err(Diagnostic::error(
                "recursive transform calls are not executable in the initial native runtime",
                transform.span,
            )
            .with_note("the reference interpreter retains a bounded recursion guard"));
        }
        for value in &transform.values {
            match &value.kind {
                ValueKind::Binary { .. } if value.ty == Type::I64 => {
                    return Err(
                        Diagnostic::error(
                            "i64 arithmetic is not executable in the initial native runtime",
                            value.span,
                        )
                        .with_note(
                            "Tima integer overflow and division-error semantics must be chosen before mapping them to C",
                        ),
                    );
                }
                ValueKind::Call {
                    transform: callee, ..
                } => self.validate_path(*callee, visiting, visited)?,
                _ => {}
            }
        }
        visiting.remove(&id.0);
        visited.insert(id.0);
        Ok(())
    }
}

struct LoweredArgument {
    abi: AbiValue,
    keep_alive: BoundaryStorage,
}

enum BoundaryStorage {
    Scalar,
    Owned(Option<OwnedImage>),
    View(Arc<ImageValue>),
}

struct OwnedImage {
    storage: ImageStorage,
}

fn lower_native_argument(
    value: OuterValue,
    expected: Type,
    span: Span,
) -> Result<LoweredArgument, Diagnostic> {
    match (expected, value.data) {
        (Type::Bool, ValueData::Bool(value)) => Ok(LoweredArgument {
            abi: AbiValue {
                boolean: u8::from(value),
            },
            keep_alive: BoundaryStorage::Scalar,
        }),
        (Type::I64, ValueData::Integer(value)) => Ok(LoweredArgument {
            abi: AbiValue { i64_value: value },
            keep_alive: BoundaryStorage::Scalar,
        }),
        (Type::F32, ValueData::Float(value)) => Ok(LoweredArgument {
            abi: AbiValue { f32_value: value },
            keep_alive: BoundaryStorage::Scalar,
        }),
        (Type::Image, ValueData::Image(image)) => {
            let image = Arc::try_unwrap(image).unwrap_or_else(|shared| (*shared).clone());
            let ImageValue {
                storage,
                width,
                height,
                stride,
            } = image;
            let mut storage = Arc::try_unwrap(storage)
                .unwrap_or_else(|shared| ImageStorage::new(shared.bytes.clone()));
            let abi = AbiValue {
                image: AbiImage {
                    data: storage.bytes.as_mut_ptr(),
                    width,
                    height,
                    stride,
                },
            };
            Ok(LoweredArgument {
                abi,
                keep_alive: BoundaryStorage::Owned(Some(OwnedImage { storage })),
            })
        }
        (Type::ImageView, ValueData::Image(image)) => Ok(LoweredArgument {
            abi: AbiValue {
                image_view: AbiImageView {
                    data: image.storage.bytes.as_ptr(),
                    width: image.width,
                    height: image.height,
                    stride: image.stride,
                },
            },
            keep_alive: BoundaryStorage::View(image),
        }),
        (expected, _) => Err(Diagnostic::error(
            format!(
                "outer value cannot cross into native parameter type {}",
                expected.name()
            ),
            span,
        )),
    }
}

fn freeze_native_result(
    result: AbiValue,
    ty: Type,
    arguments: &mut [LoweredArgument],
    span: Span,
) -> Result<OuterValue, Diagnostic> {
    // SAFETY: the generated adapter writes the union field selected by the
    // statically checked transform return type.
    unsafe {
        match ty {
            Type::Bool => Ok(freeze_scalar(NativeScalar::Bool(result.boolean != 0))),
            Type::I64 => Ok(freeze_scalar(NativeScalar::I64(result.i64_value))),
            Type::F32 => Ok(freeze_scalar(NativeScalar::F32(result.f32_value))),
            Type::Image => freeze_owned_image(result.image, arguments, span),
            Type::ImageView => freeze_image_view(result.image_view, arguments, span),
        }
    }
}

fn freeze_owned_image(
    returned: AbiImage,
    arguments: &mut [LoweredArgument],
    span: Span,
) -> Result<OuterValue, Diagnostic> {
    for argument in arguments {
        let BoundaryStorage::Owned(owned) = &mut argument.keep_alive else {
            continue;
        };
        let Some(candidate) = owned.as_ref() else {
            continue;
        };
        if candidate.storage.bytes.as_ptr() != returned.data.cast_const() {
            continue;
        }
        validate_returned_layout(
            returned.width,
            returned.height,
            returned.stride,
            candidate.storage.bytes.len(),
            span,
        )?;
        let owned = owned.take().expect("matched owned image remains available");
        return Ok(OuterValue::image(ImageValue {
            storage: Arc::new(owned.storage),
            width: returned.width,
            height: returned.height,
            stride: returned.stride,
        }));
    }
    Err(Diagnostic::error(
        "returned owned image does not reference storage acquired by this invocation",
        span,
    )
    .with_note("native allocation will require an explicit runtime allocator capability"))
}

fn freeze_image_view(
    returned: AbiImageView,
    arguments: &mut [LoweredArgument],
    span: Span,
) -> Result<OuterValue, Diagnostic> {
    for argument in arguments {
        let BoundaryStorage::View(image) = &argument.keep_alive else {
            continue;
        };
        if image.storage.bytes.as_ptr() != returned.data {
            continue;
        }
        validate_returned_layout(
            returned.width,
            returned.height,
            returned.stride,
            image.storage.bytes.len(),
            span,
        )?;
        return Ok(OuterValue::image(ImageValue {
            storage: image.storage.clone(),
            width: returned.width,
            height: returned.height,
            stride: returned.stride,
        }));
    }
    Err(Diagnostic::error(
        "returned image view does not reference a live input view",
        span,
    ))
}

fn validate_returned_layout(
    _width: usize,
    height: usize,
    stride: usize,
    byte_len: usize,
    span: Span,
) -> Result<(), Diagnostic> {
    validate_image_layout(height, stride, byte_len).map_err(|error| {
        Diagnostic::error(
            format!("native transform returned an invalid image: {error}"),
            span,
        )
    })
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
        (Type::I64, ValueData::Integer(value)) => {
            Ok(InterpretedValue::Scalar(NativeScalar::I64(value)))
        }
        (Type::F32, ValueData::Float(value)) => {
            Ok(InterpretedValue::Scalar(NativeScalar::F32(value)))
        }
        (Type::Image, ValueData::Image(image)) => {
            let image = Arc::try_unwrap(image).unwrap_or_else(|shared| (*shared).clone());
            let ImageValue {
                storage,
                width,
                height,
                stride,
            } = image;
            let storage = Arc::try_unwrap(storage)
                .unwrap_or_else(|shared| ImageStorage::new(shared.bytes.clone()));
            Ok(InterpretedValue::Image(InterpretedImage {
                storage,
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

fn clone_interpreted_argument(
    value: &InterpretedValue,
    expected: Type,
    span: Span,
) -> Result<OuterValue, Diagnostic> {
    match (expected, value) {
        (Type::Bool, InterpretedValue::Scalar(NativeScalar::Bool(value))) => {
            Ok(freeze_scalar(NativeScalar::Bool(*value)))
        }
        (Type::I64, InterpretedValue::Scalar(NativeScalar::I64(value))) => {
            Ok(freeze_scalar(NativeScalar::I64(*value)))
        }
        (Type::F32, InterpretedValue::Scalar(NativeScalar::F32(value))) => {
            Ok(freeze_scalar(NativeScalar::F32(*value)))
        }
        (Type::ImageView, InterpretedValue::ImageView(image)) => {
            Ok(OuterValue::image((**image).clone()))
        }
        (Type::Image, InterpretedValue::Image(_)) => Err(Diagnostic::error(
            "owned Image cannot be copied into an inner transform call",
            span,
        )
        .with_note("owned inner-to-inner calls require explicit move lowering")),
        _ => unreachable!("typed inner call arguments match their parameter types"),
    }
}

fn freeze_interpreted_value(value: InterpretedValue) -> OuterValue {
    match value {
        InterpretedValue::Scalar(value) => freeze_scalar(value),
        InterpretedValue::Image(image) => OuterValue::image(ImageValue {
            storage: Arc::new(image.storage),
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
        NativeScalar::I64(value) => ValueData::Integer(value),
        NativeScalar::F32(value) => ValueData::Float(value),
    })
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    use crate::backend::NativeBackend;
    use crate::backend::c::CBackend;
    use crate::backend::native::{ClangCompiler, NativeModule};
    use crate::cache::TransformResultCache;
    use crate::capability::RuntimeCapabilities;
    use crate::identity::{ContentIdentity, byte_content_identity, content_identity};
    use crate::ir::{TransformId, Type};
    use crate::lineage::{Lineage, LineageNode, RecordedValue};
    use crate::runtime::{
        ImageValue, IrInterpreter, OuterValue, ReplayDependencyResolver, ValueData, execute,
        execute_cached, execute_cached_with_capabilities, execute_native,
        execute_native_cached_with_capabilities, execute_native_with_bindings_cached, execute_with,
        invoke_native_transform, lower_native_argument, replay, replay_native,
        replay_with_capabilities, replay_with_dependencies,
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
    fn environment_capability_is_explicit_and_part_of_the_recipe() {
        let compiled = crate::compile(
            "test.tima",
            "transform read_mode() -> i64 { return environment_i64(\"MODE\") }\n\
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
    fn replay_revalidates_and_reexecutes_environment_dependencies() {
        let compiled = crate::compile(
            "test.tima",
            "transform configured() -> i64 { return environment_i64(\"MODE\") }\n\
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
    fn compiles_loads_and_runs_nested_transforms_as_native_code() {
        let compiled = crate::compile(
            "test.tima",
            "transform double(x: f32) -> f32 {\n doubled = x * 2.0\n return doubled\n}\n\
             transform scale_twice(x: f32, factor: f32) -> f32 { return double(x * factor) }\n\
             transform keep_i64(x: i64) -> i64 { return x }\n\
             transform keep_bool(x: bool) -> bool { return x }\n\
             transform choose(flag: bool, left: f32, right: f32) -> f32 {\n\
                 if flag { return left } else {}\n\
                 return right\n\
             }\n\
             transform minimum(left: f32, right: f32) -> f32 {\n\
                 if left < right { return left } else { return right }\n\
             }\n\
             transform less_i64(left: i64, right: i64) -> bool { return left < right }\n\
             out = 8.0 | scale_twice(factor=0.25)\n\
             count = keep_i64(7)\n\
             flag = keep_bool(true)\n\
             chosen = choose(false, 3.0, 7.0)\n\
             chosen_true = choose(true, 3.0, 7.0)\n\
             smaller = minimum(7.0, 3.0)\n\
             ordered = less_i64(3, 7)\n",
        )
        .unwrap();
        let reference = execute(&compiled).unwrap();
        let LineageNode::Invocation(reference_lineage) =
            reference.bindings["out"].lineage.as_ref().unwrap().node()
        else {
            unreachable!()
        };
        let generated = CBackend.emit(&compiled.transforms).unwrap();
        let build_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("build");
        let artifact = ClangCompiler::default()
            .compile(&generated, build_root)
            .unwrap();
        assert!(artifact.compiler_version.contains("clang"));
        assert!(!artifact.target.is_empty());
        assert_ne!(
            artifact.identity(compiled.identities.get(TransformId(0))),
            artifact.identity(compiled.identities.get(TransformId(1)))
        );
        let native = NativeModule::load(&artifact, &compiled.transforms).unwrap();
        let execution = execute_native(&compiled, &native).unwrap();
        assert_eq!(execution.bindings["out"].data, ValueData::Float(4.0));
        assert_eq!(execution.bindings["count"].data, ValueData::Integer(7));
        assert_eq!(execution.bindings["flag"].data, ValueData::Bool(true));
        assert_eq!(reference.bindings["chosen"].data, ValueData::Float(7.0));
        assert_eq!(execution.bindings["chosen"].data, ValueData::Float(7.0));
        assert_eq!(
            execution.bindings["chosen_true"].data,
            ValueData::Float(3.0)
        );
        assert_eq!(execution.bindings["smaller"].data, ValueData::Float(3.0));
        assert_eq!(execution.bindings["ordered"].data, ValueData::Bool(true));
        let LineageNode::Invocation(native_lineage) =
            execution.bindings["out"].lineage.as_ref().unwrap().node()
        else {
            unreachable!()
        };
        assert_eq!(reference_lineage.recipe_id, native_lineage.recipe_id);
    }

    #[test]
    fn generated_c_reads_environment_through_the_host_capability() {
        let compiled = crate::compile(
            "test.tima",
            "transform read_mode() -> i64 { return environment_i64(\"MODE\") }\n\
             transform configured() -> i64 { return read_mode() }\n\
             result = configured()\n",
        )
        .unwrap();
        let generated = CBackend.emit(&compiled.transforms).unwrap();
        let build_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("build");
        let artifact = ClangCompiler::default()
            .compile(&generated, build_root)
            .unwrap();
        let native = NativeModule::load(&artifact, &compiled.transforms).unwrap();

        let diagnostic = execute_native(&compiled, &native).unwrap_err();
        assert!(
            diagnostic[0]
                .message
                .contains("environment access is unavailable")
        );
        assert_eq!(
            diagnostic[0].labels[0].span,
            compiled.transforms.transforms[0].values[0].span
        );

        let environment = FixedEnvironment::one("MODE", b"73");
        let mut reference_cache = TransformResultCache::default();
        let reference =
            execute_cached_with_capabilities(&compiled, &mut reference_cache, &environment)
                .unwrap();
        let mut native_cache = TransformResultCache::default();
        let execution = execute_native_cached_with_capabilities(
            &compiled,
            &native,
            &mut native_cache,
            &environment,
        )
        .unwrap();
        assert_eq!(execution.bindings["result"].data, ValueData::Integer(73));
        assert_eq!(
            execution.bindings["result"]
                .lineage
                .as_ref()
                .unwrap()
                .recipe_id(),
            reference.bindings["result"]
                .lineage
                .as_ref()
                .unwrap()
                .recipe_id()
        );
    }

    #[test]
    fn untaken_branches_do_not_observe_runtime_capabilities() {
        let compiled = crate::compile(
            "test.tima",
            "transform guarded(flag: bool) -> i64 {\n\
                 if flag {\n\
                     observed = environment_i64(\"MODE\")\n\
                 } else {}\n\
                 return 7\n\
             }\n\
             result = guarded(false)\n",
        )
        .unwrap();
        let reference = execute(&compiled).unwrap();
        assert_eq!(reference.bindings["result"].data, ValueData::Integer(7));

        let generated = CBackend.emit(&compiled.transforms).unwrap();
        let build_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("build");
        let artifact = ClangCompiler::default()
            .compile(&generated, build_root)
            .unwrap();
        let native = NativeModule::load(&artifact, &compiled.transforms).unwrap();
        let execution = execute_native(&compiled, &native).unwrap();
        assert_eq!(execution.bindings["result"].data, ValueData::Integer(7));
    }

    #[test]
    fn native_runtime_defers_unspecified_i64_arithmetic_semantics() {
        let compiled = crate::compile(
            "test.tima",
            "transform add(x: i64, y: i64) -> i64 { return x + y }\n\
             out = add(1, 2)\n",
        )
        .unwrap();
        let generated = CBackend.emit(&compiled.transforms).unwrap();
        let build_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("build");
        let artifact = ClangCompiler::default()
            .compile(&generated, build_root)
            .unwrap();
        let native = NativeModule::load(&artifact, &compiled.transforms).unwrap();
        let diagnostics = execute_native(&compiled, &native).unwrap_err();
        assert!(diagnostics[0].message.contains("i64 arithmetic"));
    }

    #[test]
    fn owned_image_arguments_detach_when_storage_is_shared() {
        let image = OuterValue::image(ImageValue::new(2, 2, 2, vec![1, 2, 3, 4]).unwrap());
        let original_pointer = match &image.data {
            ValueData::Image(image) => image.storage.bytes.as_ptr(),
            _ => unreachable!(),
        };
        let retained_outer_alias = image.clone();
        let first = lower_native_argument(image.clone(), Type::Image, Span::default()).unwrap();
        let second = lower_native_argument(image, Type::Image, Span::default()).unwrap();
        // SAFETY: each union was initialized with its `image` field.
        let first_pointer = unsafe { first.abi.image.data.cast_const() };
        // SAFETY: each union was initialized with its `image` field.
        let second_pointer = unsafe { second.abi.image.data.cast_const() };
        assert_ne!(first_pointer, original_pointer);
        assert_ne!(second_pointer, original_pointer);
        assert_ne!(first_pointer, second_pointer);
        drop(retained_outer_alias);
    }

    #[test]
    fn image_views_alias_shared_storage_without_copying() {
        let image = OuterValue::image(ImageValue::new(2, 2, 2, vec![1, 2, 3, 4]).unwrap());
        let original_pointer = match &image.data {
            ValueData::Image(image) => image.storage.bytes.as_ptr(),
            _ => unreachable!(),
        };
        let first = lower_native_argument(image.clone(), Type::ImageView, Span::default()).unwrap();
        let second = lower_native_argument(image, Type::ImageView, Span::default()).unwrap();
        // SAFETY: each union was initialized with its `image_view` field.
        assert_eq!(unsafe { first.abi.image_view.data }, original_pointer);
        // SAFETY: each union was initialized with its `image_view` field.
        assert_eq!(unsafe { second.abi.image_view.data }, original_pointer);
    }

    #[test]
    fn owned_image_detaches_from_a_simultaneous_view() {
        let image = OuterValue::image(ImageValue::new(2, 2, 2, vec![1, 2, 3, 4]).unwrap());
        let view = lower_native_argument(image.clone(), Type::ImageView, Span::default()).unwrap();
        let owned = lower_native_argument(image, Type::Image, Span::default()).unwrap();
        // SAFETY: the unions were initialized with the fields read here.
        let view_pointer = unsafe { view.abi.image_view.data };
        // SAFETY: the unions were initialized with the fields read here.
        let owned_pointer = unsafe { owned.abi.image.data.cast_const() };
        assert_ne!(owned_pointer, view_pointer);
    }

    #[test]
    fn native_image_boundary_detaches_owned_values_and_freezes_returns() {
        let compiled = crate::compile(
            "test.tima",
            "transform own(img: Image) -> Image { return img }\n\
             transform view(img: ImageView) -> ImageView { return img }\n\
             owned = img | own\n\
             viewed = img | view\n",
        )
        .unwrap();
        let generated = CBackend.emit(&compiled.transforms).unwrap();
        let build_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("build");
        let artifact = ClangCompiler::default()
            .compile(&generated, build_root)
            .unwrap();
        let native = NativeModule::load(&artifact, &compiled.transforms).unwrap();
        let input = OuterValue::image(ImageValue::new(2, 2, 2, vec![1, 2, 3, 4]).unwrap());
        let input_content = content_identity(&input).unwrap();
        let input = input.with_lineage(Lineage::observed_source("cat.raw", input_content));
        let mut cache = TransformResultCache::default();
        let execution = execute_native_with_bindings_cached(
            &compiled,
            &native,
            BTreeMap::from([("img".to_owned(), input)]),
            &mut cache,
        )
        .unwrap();
        let ValueData::Image(original) = &execution.bindings["img"].data else {
            panic!("expected input image")
        };
        let ValueData::Image(owned) = &execution.bindings["owned"].data else {
            panic!("expected owned result image")
        };
        let ValueData::Image(viewed) = &execution.bindings["viewed"].data else {
            panic!("expected viewed result image")
        };
        assert!(!original.shares_storage_with(owned));
        assert!(original.shares_storage_with(viewed));
        assert_eq!(original.bytes(), owned.bytes());
        assert_eq!(original.bytes(), viewed.bytes());
        let owned_trace = execution.bindings["owned"]
            .lineage
            .as_ref()
            .unwrap()
            .render();
        assert!(owned_trace.contains("source \"cat.raw\""));
        assert!(owned_trace.contains("invoke own"));
        assert!(owned_trace.contains("from=#0"));
        let owned_value = execution.bindings["owned"].clone();
        let recipe = owned_value.lineage.as_ref().unwrap().recipe_id().unwrap();
        cache.invalidate_recipe(recipe);
        let replayed = replay_native(&compiled, &native, &owned_value, &mut cache).unwrap();
        let ValueData::Image(replayed) = replayed.data else {
            panic!("expected replayed image")
        };
        assert_eq!(replayed.bytes(), original.bytes());
    }

    #[test]
    fn owned_image_zero_mutates_detached_storage_in_both_engines() {
        let compiled = crate::compile(
            "test.tima",
            "transform clear(img: Image) -> Image { return image_zero(img) }\n\
             cleared = img | clear\n",
        )
        .unwrap();
        let input = OuterValue::image(ImageValue::new(2, 2, 2, vec![1, 2, 3, 4]).unwrap());
        let input_content = content_identity(&input).unwrap();
        let input = input.with_lineage(Lineage::observed_source("cat.raw", input_content));

        let reference_engine = IrInterpreter {
            module: &compiled.transforms,
            capabilities: None,
        };
        let reference = execute_with(
            &compiled,
            &reference_engine,
            BTreeMap::from([("img".to_owned(), input.clone())]),
            None,
        )
        .unwrap();
        let ValueData::Image(reference_result) = &reference.bindings["cleared"].data else {
            panic!("expected reference image")
        };
        assert_eq!(reference_result.bytes(), &[0, 0, 0, 0]);

        let generated = CBackend.emit(&compiled.transforms).unwrap();
        let build_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("build");
        let artifact = ClangCompiler::default()
            .compile(&generated, build_root)
            .unwrap();
        let native = NativeModule::load(&artifact, &compiled.transforms).unwrap();
        let mut cache = TransformResultCache::default();
        let execution = execute_native_with_bindings_cached(
            &compiled,
            &native,
            BTreeMap::from([("img".to_owned(), input.clone())]),
            &mut cache,
        )
        .unwrap();
        let ValueData::Image(original) = &execution.bindings["img"].data else {
            panic!("expected original image")
        };
        let ValueData::Image(native_result) = &execution.bindings["cleared"].data else {
            panic!("expected native image")
        };
        assert_eq!(original.bytes(), &[1, 2, 3, 4]);
        assert_eq!(native_result.bytes(), &[0, 0, 0, 0]);
        assert!(!original.shares_storage_with(native_result));
        assert_eq!(
            reference.bindings["cleared"]
                .lineage
                .as_ref()
                .unwrap()
                .recipe_id(),
            execution.bindings["cleared"]
                .lineage
                .as_ref()
                .unwrap()
                .recipe_id()
        );

        let recorded = execution.bindings["cleared"].clone();
        let recipe = recorded.lineage.as_ref().unwrap().recipe_id().unwrap();
        cache.invalidate_recipe(recipe);
        let replayed = replay_native(&compiled, &native, &recorded, &mut cache).unwrap();
        let ValueData::Image(replayed) = replayed.data else {
            panic!("expected replayed image")
        };
        assert_eq!(replayed.bytes(), &[0, 0, 0, 0]);
    }

    #[test]
    fn unique_owned_image_storage_transfers_without_copying() {
        let compiled = crate::compile(
            "test.tima",
            "transform own(img: Image) -> Image { return img }\n",
        )
        .unwrap();
        let generated = CBackend.emit(&compiled.transforms).unwrap();
        let build_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("build");
        let artifact = ClangCompiler::default()
            .compile(&generated, build_root)
            .unwrap();
        let native = NativeModule::load(&artifact, &compiled.transforms).unwrap();
        let image = ImageValue::new(2, 2, 2, vec![1, 2, 3, 4]).unwrap();
        let original_pointer = image.storage.bytes.as_ptr();
        let result = invoke_native_transform(
            &compiled,
            &native,
            crate::ir::TransformId(0),
            vec![OuterValue::image(image)],
        )
        .unwrap();
        let ValueData::Image(result) = result.data else {
            panic!("expected image result")
        };
        assert_eq!(result.storage.bytes.as_ptr(), original_pointer);
        assert_eq!(result.bytes(), &[1, 2, 3, 4]);
    }

    #[test]
    fn validates_outer_image_layouts() {
        let error = ImageValue::new(2, 2, 2, vec![0; 3]).unwrap_err();
        assert!(error.to_string().contains("require 4"));
    }
}
