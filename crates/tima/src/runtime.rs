mod buffer;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use buffer::BufferStorage;
pub use buffer::{BufferLayoutError, BufferValue};

use crate::ast::{Argument, BinaryOp, ExprId, ExprKind, Item};
use crate::backend::cache::CachedArtifact;
use crate::backend::cranelift::{CraneliftBackend, CraneliftTransformPlan};
use crate::backend::native::{
    NativeArgument, NativeBuffer, NativeBufferView, NativeModule, NativeResult,
    NativeScalar as AbiScalar, NativeShapedBuffer, NativeShapedBufferView,
};
use crate::cache::{ResultCache, TransformResultCache};
use crate::capability::{ASSET_CAPABILITY, CapabilitySession, World, observe_dependency};
use crate::diagnostic::Diagnostic;
use crate::fraction::{Fraction, FractionError};
use crate::identity::{
    ContentIdentity, IdentityDomain, IdentityPrefixResolver, SemanticValueIdentity,
    TransformIdentity, content_identity,
};
use crate::ir::{
    Constant, RuntimeCall, Terminator, Transform, TransformId, Type, ValueId, ValueKind,
};
use crate::lineage::{
    Lineage, LineageArgument, LineageNode, RecordedValue, semantic_value_identity,
};
use crate::plugin::{PluginTransform, PreparedPluginInvocation, prepare_plugin_invocation};
use crate::registered::{
    PreparedRegisteredInvocation, RegisteredTransform, prepare_registered_invocation,
};
use crate::source::Span;
use crate::{CompiledProgram, available_transform_identities};

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

    pub fn buffer(buffer: BufferValue) -> Self {
        Self::plain(ValueData::Buffer(Arc::new(buffer)))
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
    Fraction(Fraction),
    String(Arc<String>),
    Bytes(Arc<Vec<u8>>),
    List(Arc<[OuterValue]>),
    Record(Arc<BTreeMap<String, OuterValue>>),
    Asset(Arc<AssetValue>),
    Buffer(Arc<BufferValue>),
    Transform(TransformId),
    Lineage(Lineage),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetValue {
    /// A locator is not a content identity. Its source lineage remains
    /// unresolved until the host observes/materializes the asset.
    pub locator: Arc<str>,
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
    pub transform_plan: Vec<CraneliftTransformPlan>,
}

pub fn execute(program: &CompiledProgram) -> Result<Execution, Vec<Diagnostic>> {
    let engine = IrInterpreter {
        module: &program.transforms,
        capabilities: None,
    };
    execute_with(program, &engine, BTreeMap::new(), None, None)
}

/// Invokes any named transform through the same positional Rust interface used
/// by outer Tima calls. The name may resolve to source-defined Tima IR, a
/// standard transform, or a configured registered-Wasm transform.
pub fn invoke_transform(
    program: &CompiledProgram,
    name: &str,
    arguments: Vec<OuterValue>,
) -> Result<OuterValue, Diagnostic> {
    let engine = IrInterpreter {
        module: &program.transforms,
        capabilities: None,
    };
    invoke_transform_with_engine(program, &engine, name, arguments)
}

/// Equivalent to [`invoke_transform`], with an explicit World for a
/// source-defined transform that declares runtime capabilities.
pub fn invoke_transform_with_capabilities(
    program: &CompiledProgram,
    name: &str,
    arguments: Vec<OuterValue>,
    capabilities: &dyn World,
) -> Result<OuterValue, Diagnostic> {
    let engine = IrInterpreter {
        module: &program.transforms,
        capabilities: Some(capabilities),
    };
    invoke_transform_with_engine(program, &engine, name, arguments)
}

fn invoke_transform_with_engine(
    program: &CompiledProgram,
    engine: &dyn TransformEngine,
    name: &str,
    arguments: Vec<OuterValue>,
) -> Result<OuterValue, Diagnostic> {
    let span = Span::default();
    let transform = CallableTransform::find(program, name)
        .ok_or_else(|| Diagnostic::error(format!("unknown transform `{name}`"), span))?;
    let parameters = (0..transform.parameter_count())
        .map(|index| transform.parameter_name(index))
        .collect::<Vec<_>>();
    let evaluated = arguments
        .into_iter()
        .map(|argument| (None, argument, span))
        .collect::<Vec<_>>();
    let arguments = order_outer_arguments_with_defaults(
        transform.name(),
        &parameters,
        evaluated,
        span,
        |index| transform.default_argument(index),
    )?;
    invoke_transform_with_lineage(engine, transform, arguments, None, span)
}

pub fn execute_cached(
    program: &CompiledProgram,
    cache: &mut dyn ResultCache,
) -> Result<Execution, Vec<Diagnostic>> {
    let engine = IrInterpreter {
        module: &program.transforms,
        capabilities: None,
    };
    execute_with(program, &engine, BTreeMap::new(), Some(cache), None)
}

pub fn execute_with_capabilities(
    program: &CompiledProgram,
    capabilities: &dyn World,
) -> Result<Execution, Vec<Diagnostic>> {
    let engine = IrInterpreter {
        module: &program.transforms,
        capabilities: Some(capabilities),
    };
    execute_with(program, &engine, BTreeMap::new(), None, None)
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
    execute_with(program, &engine, BTreeMap::new(), Some(cache), None)
}

pub fn execute_cached_with_capabilities_and_identity_prefixes(
    program: &CompiledProgram,
    cache: &mut dyn ResultCache,
    capabilities: &dyn World,
    identity_prefixes: &dyn IdentityPrefixResolver,
) -> Result<Execution, Vec<Diagnostic>> {
    let engine = IrInterpreter {
        module: &program.transforms,
        capabilities: Some(capabilities),
    };
    execute_with(
        program,
        &engine,
        BTreeMap::new(),
        Some(cache),
        Some(identity_prefixes),
    )
}

/// Executes AOT-compatible transforms from a cached native load image and
/// falls back to the typed-IR interpreter for every other transform.
pub fn execute_aot_cached_with_capabilities(
    program: &CompiledProgram,
    cache: &mut dyn ResultCache,
    capabilities: &dyn World,
    artifact_cache_root: impl AsRef<Path>,
) -> Result<AotExecution, Vec<Diagnostic>> {
    execute_aot_with(
        program,
        cache,
        capabilities,
        artifact_cache_root.as_ref(),
        None,
    )
}

/// Hybrid-AOT execution with host-local semantic identity-prefix resolution.
/// Backend selection remains execution metadata and does not enter lineage.
pub fn execute_aot_cached_with_capabilities_and_identity_prefixes(
    program: &CompiledProgram,
    cache: &mut dyn ResultCache,
    capabilities: &dyn World,
    artifact_cache_root: impl AsRef<Path>,
    identity_prefixes: &dyn IdentityPrefixResolver,
) -> Result<AotExecution, Vec<Diagnostic>> {
    execute_aot_with(
        program,
        cache,
        capabilities,
        artifact_cache_root.as_ref(),
        Some(identity_prefixes),
    )
}

fn execute_aot_with(
    program: &CompiledProgram,
    cache: &mut dyn ResultCache,
    capabilities: &dyn World,
    artifact_cache_root: &Path,
    identity_prefixes: Option<&dyn IdentityPrefixResolver>,
) -> Result<AotExecution, Vec<Diagnostic>> {
    let transform_plan = CraneliftBackend::transform_plan(&program.transforms);
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
    let execution = execute_with(
        program,
        &engine,
        BTreeMap::new(),
        Some(cache),
        identity_prefixes,
    )?;
    Ok(AotExecution {
        execution,
        artifact,
        transform_plan,
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
    identity_prefixes: Option<&dyn IdentityPrefixResolver>,
) -> Result<Execution, Vec<Diagnostic>> {
    Interpreter {
        program,
        engine,
        execution: Execution {
            bindings,
            last_value: None,
        },
        cache,
        identity_prefixes,
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

#[derive(Clone, Copy)]
enum CallableTransform<'a> {
    Tima {
        id: TransformId,
        definition: &'a Transform,
        identity: TransformIdentity,
    },
    Builtin(&'static RegisteredTransform),
    Wasm(&'a PluginTransform),
}

impl<'a> CallableTransform<'a> {
    fn find(program: &'a CompiledProgram, name: &str) -> Option<Self> {
        program
            .transforms
            .find(name)
            .map(|(id, definition)| Self::Tima {
                id,
                definition,
                identity: program.identities.get(id),
            })
            .or_else(|| RegisteredTransform::find(name).map(Self::Builtin))
            .or_else(|| program.plugins.find(name).map(Self::Wasm))
    }

    fn from_identity(program: &'a CompiledProgram, identity: TransformIdentity) -> Option<Self> {
        program
            .identities
            .find_id(identity)
            .map(|id| Self::Tima {
                id,
                definition: program.transforms.get(id),
                identity,
            })
            .or_else(|| RegisteredTransform::from_identity(identity).map(Self::Builtin))
            .or_else(|| program.plugins.find_by_identity(identity).map(Self::Wasm))
    }

    fn name(self) -> &'a str {
        match self {
            Self::Tima { definition, .. } => definition.name.as_str(),
            Self::Builtin(transform) => transform.name(),
            Self::Wasm(transform) => transform.name(),
        }
    }

    fn identity(self) -> TransformIdentity {
        match self {
            Self::Tima { identity, .. } => identity,
            Self::Builtin(transform) => transform.identity(),
            Self::Wasm(transform) => transform.identity(),
        }
    }

    fn parameter_count(self) -> usize {
        match self {
            Self::Tima { definition, .. } => definition.parameters.len(),
            Self::Builtin(transform) => transform.parameters().len(),
            Self::Wasm(transform) => transform.parameters().len(),
        }
    }

    fn parameter_name(self, index: usize) -> &'a str {
        match self {
            Self::Tima { definition, .. } => definition.parameters[index].name.as_str(),
            Self::Builtin(transform) => transform.parameters()[index].name,
            Self::Wasm(transform) => transform.parameters()[index].name.as_str(),
        }
    }

    fn default_argument(self, index: usize) -> Option<OuterValue> {
        match self {
            Self::Builtin(transform) => transform.default_argument(index),
            Self::Tima { .. } | Self::Wasm(_) => None,
        }
    }

    fn diagnostic_span(self, call_span: Span) -> Span {
        match self {
            Self::Tima { definition, .. } => definition.span,
            Self::Builtin(_) | Self::Wasm(_) => call_span,
        }
    }

    fn may_observe_dependencies(self, engine: &dyn TransformEngine) -> bool {
        match self {
            Self::Tima { id, .. } => engine.may_observe_dependencies(id),
            Self::Builtin(_) | Self::Wasm(_) => false,
        }
    }

    fn prepare(
        self,
        arguments: Vec<(OuterValue, Span)>,
        span: Span,
    ) -> Result<PreparedTransformInvocation<'a>, Diagnostic> {
        match self {
            Self::Tima { id, .. } => {
                if arguments.len() != self.parameter_count() {
                    return Err(Diagnostic::error(
                        format!(
                            "{} expects exactly {} arguments",
                            self.name(),
                            self.parameter_count()
                        ),
                        span,
                    ));
                }
                Ok(PreparedTransformInvocation::Tima { id, arguments })
            }
            Self::Builtin(transform) => prepare_registered_invocation(transform, arguments, span)
                .map(PreparedTransformInvocation::Builtin),
            Self::Wasm(transform) => prepare_plugin_invocation(transform, arguments, span)
                .map(PreparedTransformInvocation::Wasm),
        }
    }
}

enum PreparedTransformInvocation<'a> {
    Tima {
        id: TransformId,
        arguments: Vec<(OuterValue, Span)>,
    },
    Builtin(PreparedRegisteredInvocation),
    Wasm(PreparedPluginInvocation<'a>),
}

impl PreparedTransformInvocation<'_> {
    fn arguments(&self) -> &[(OuterValue, Span)] {
        match self {
            Self::Tima { arguments, .. } => arguments,
            Self::Builtin(prepared) => &prepared.arguments,
            Self::Wasm(prepared) => &prepared.arguments,
        }
    }

    fn execute(self, engine: &dyn TransformEngine) -> Result<TransformOutcome, Diagnostic> {
        match self {
            Self::Tima { id, arguments } => engine.invoke(id, arguments),
            Self::Builtin(prepared) => Ok(TransformOutcome {
                value: prepared.execute()?,
                observations: vec![],
            }),
            Self::Wasm(prepared) => Ok(TransformOutcome {
                value: prepared.execute()?,
                observations: vec![],
            }),
        }
    }
}

fn record_transform_arguments(
    transform: CallableTransform<'_>,
    arguments: &[(OuterValue, Span)],
) -> Result<Vec<LineageArgument>, Diagnostic> {
    arguments
        .iter()
        .enumerate()
        .map(|(index, (argument, span))| {
            let name = transform.parameter_name(index);
            LineageArgument::record(name, argument).map_err(|error| {
                Diagnostic::error(
                    format!("cannot record argument `{name}` for transform lineage: {error}"),
                    *span,
                )
            })
        })
        .collect()
}

fn remember_materialized_arguments(
    cache: &mut dyn ResultCache,
    recorded: &[LineageArgument],
    arguments: &[(OuterValue, Span)],
) -> Result<(), Diagnostic> {
    for (recorded, (argument, span)) in recorded.iter().zip(arguments) {
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
    Ok(())
}

fn invoke_transform_with_lineage<'cache>(
    engine: &dyn TransformEngine,
    transform: CallableTransform<'_>,
    arguments: Vec<(OuterValue, Span)>,
    mut cache: Option<&mut (dyn ResultCache + 'cache)>,
    span: Span,
) -> Result<OuterValue, Diagnostic> {
    let diagnostic_span = transform.diagnostic_span(span);
    let prepared = transform.prepare(arguments, span)?;
    let recorded = record_transform_arguments(transform, prepared.arguments())?;
    if let Some(cache) = cache.as_deref_mut() {
        remember_materialized_arguments(cache, &recorded, prepared.arguments())?;
    }
    if !transform.may_observe_dependencies(engine) {
        let lineage = Lineage::invocation(
            transform.name(),
            transform.identity(),
            recorded.clone(),
            vec![],
        )
        .map_err(|error| Diagnostic::error(error.to_string(), diagnostic_span))?;
        let recipe = lineage
            .recipe_id()
            .expect("invocation lineage always has a recipe identity");
        if let Some(cache) = cache.as_deref_mut()
            && let Some(mut value) = cache
                .lookup(recipe)
                .map_err(|error| Diagnostic::error(error.to_string(), diagnostic_span))?
        {
            value.lineage = Some(lineage);
            return Ok(value);
        }
    }
    let TransformOutcome {
        mut value,
        observations,
    } = prepared.execute(engine)?;
    let lineage = Lineage::invocation(
        transform.name(),
        transform.identity(),
        recorded,
        observations,
    )
    .map_err(|error| Diagnostic::error(error.to_string(), diagnostic_span))?;
    if let Some(cache) = cache {
        cache
            .store(
                lineage
                    .recipe_id()
                    .expect("invocation lineage always has a recipe identity"),
                &value,
            )
            .map_err(|error| Diagnostic::error(error.to_string(), diagnostic_span))?;
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

fn resolve_replay_transform<'a>(
    program: &'a CompiledProgram,
    invocation: &crate::lineage::InvocationLineage,
    span: Span,
) -> Result<CallableTransform<'a>, Diagnostic> {
    let Some(replay_transform) = CallableTransform::from_identity(program, invocation.transform_id)
    else {
        return Err(Diagnostic::error(
            format!(
                "recorded transform definition {} is unavailable or has changed",
                invocation.transform_id
            ),
            span,
        ));
    };
    let transform_name = replay_transform.name();
    let parameter_count = replay_transform.parameter_count();
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
    let prepared = replay_transform.prepare(runtime_arguments, span)?;
    let recorded = record_transform_arguments(replay_transform, prepared.arguments())?;
    let TransformOutcome {
        mut value,
        observations,
    } = prepared.execute(engine)?;
    let observed_lineage = Lineage::invocation(
        replay_transform.name(),
        replay_transform.identity(),
        recorded,
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
        RecordedValue::Fraction(value) => OuterValue::plain(ValueData::Fraction(*value)),
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

struct Interpreter<'program, 'engine, 'cache, 'identities> {
    program: &'program CompiledProgram,
    engine: &'engine dyn TransformEngine,
    execution: Execution,
    cache: Option<&'cache mut (dyn ResultCache + 'cache)>,
    identity_prefixes: Option<&'identities dyn IdentityPrefixResolver>,
}

impl Interpreter<'_, '_, '_, '_> {
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
            ExprKind::Fraction(numerator, denominator) => {
                let value = Fraction::new(*numerator, *denominator)
                    .map_err(|error| Diagnostic::error(error.to_string(), expression.span))?;
                OuterValue::plain(ValueData::Fraction(value))
            }
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
            ExprKind::IdentityAsserted {
                value,
                prefix,
                prefix_span,
            } => {
                let value = self.expression(*value)?;
                self.assert_value_identity(value, prefix, *prefix_span)?
            }
            ExprKind::Pipeline { input, stage } => {
                let input = self.expression(*input)?;
                match &self.program.syntax.expr(*stage).kind {
                    ExprKind::Call { callee, arguments } => {
                        self.call(*callee, arguments, Some(input), expression.span)?
                    }
                    ExprKind::Name(_)
                    | ExprKind::Member { .. }
                    | ExprKind::IdentityAsserted { .. } => {
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

    fn assert_value_identity(
        &self,
        value: OuterValue,
        prefix: &str,
        span: Span,
    ) -> Result<OuterValue, Diagnostic> {
        let (domain, actual) = match &value.data {
            ValueData::Transform(id) => (
                IdentityDomain::Transform,
                self.program.identities.get(*id).to_string(),
            ),
            _ => match semantic_value_identity(&value).map_err(|error| {
                Diagnostic::error(
                    format!("value has no assertable semantic identity: {error}"),
                    span,
                )
            })? {
                SemanticValueIdentity::Content(identity) => {
                    (IdentityDomain::Content, identity.to_string())
                }
                SemanticValueIdentity::Source(identity) => {
                    (IdentityDomain::Source, identity.to_string())
                }
                SemanticValueIdentity::Recipe(identity) => {
                    (IdentityDomain::Recipe, identity.to_string())
                }
            },
        };
        if !actual.starts_with(prefix) {
            return Err(Diagnostic::error(
                format!(
                    "value has {} {actual}, which does not match identity assertion `#{prefix}`",
                    domain.name()
                ),
                span,
            )
            .with_note("update or remove the identity assertion to use this value"));
        }
        self.assert_identity_prefix_unique(domain, &actual, prefix, span)?;
        Ok(value)
    }

    fn assert_identity_prefix_unique(
        &self,
        domain: IdentityDomain,
        actual: &str,
        prefix: &str,
        span: Span,
    ) -> Result<(), Diagnostic> {
        let mut identities = BTreeSet::from([actual.to_owned()]);
        if domain == IdentityDomain::Transform {
            identities.extend(
                available_transform_identities(&self.program.identities, &self.program.plugins)
                    .into_iter()
                    .map(|identity| identity.to_string()),
            );
        }
        for value in self
            .execution
            .bindings
            .values()
            .chain(self.execution.last_value.iter())
        {
            if let Some((candidate_domain, identity)) = self.value_identity(value)
                && candidate_domain == domain
            {
                identities.insert(identity);
            }
        }
        if let Some(resolver) = self.identity_prefixes {
            identities.extend(
                resolver
                    .matching_identities(domain, prefix)
                    .map_err(|error| {
                        Diagnostic::error(
                            format!(
                                "could not check local {} prefix collisions: {error}",
                                domain.name()
                            ),
                            span,
                        )
                    })?,
            );
        }
        let mut matches = identities
            .into_iter()
            .filter(|identity| identity.starts_with(prefix));
        let first = matches.next();
        let second = matches.next();
        if let (Some(first), Some(second)) = (first, second) {
            return Err(Diagnostic::error(
                format!(
                    "{} assertion `#{prefix}` is ambiguous locally; it matches {first} and {second}",
                    domain.name()
                ),
                span,
            )
            .with_note(format!(
                "use a longer prefix or the full {}",
                domain.name()
            )));
        }
        Ok(())
    }

    fn value_identity(&self, value: &OuterValue) -> Option<(IdentityDomain, String)> {
        if let ValueData::Transform(id) = &value.data {
            return Some((
                IdentityDomain::Transform,
                self.program.identities.get(*id).to_string(),
            ));
        }
        match semantic_value_identity(value).ok()? {
            SemanticValueIdentity::Content(identity) => {
                Some((IdentityDomain::Content, identity.to_string()))
            }
            SemanticValueIdentity::Source(identity) => {
                Some((IdentityDomain::Source, identity.to_string()))
            }
            SemanticValueIdentity::Recipe(identity) => {
                Some((IdentityDomain::Recipe, identity.to_string()))
            }
        }
    }

    fn transform_identity(&self, name: &str) -> Option<TransformIdentity> {
        CallableTransform::find(self.program, name).map(CallableTransform::identity)
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
        let asserted = matches!(callee_expression.kind, ExprKind::IdentityAsserted { .. });
        if let ExprKind::IdentityAsserted {
            prefix,
            prefix_span,
            ..
        } = &callee_expression.kind
            && let Some(identity) = self.transform_identity(&name)
        {
            self.assert_identity_prefix_unique(
                IdentityDomain::Transform,
                &identity.to_string(),
                prefix,
                *prefix_span,
            )?;
        }
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
            if asserted {
                return Err(Diagnostic::error(
                    "outer builtins cannot use semantic identity assertions",
                    callee_expression.span,
                ));
            }
            return self.asset(evaluated, span);
        }
        if name == "f32.from_bits" {
            if asserted {
                return Err(Diagnostic::error(
                    "outer builtins cannot use semantic identity assertions",
                    callee_expression.span,
                ));
            }
            return self.f32_from_bits(evaluated, span);
        }
        if name == "f32.from_fraction" {
            if asserted {
                return Err(Diagnostic::error(
                    "outer builtins cannot use semantic identity assertions",
                    callee_expression.span,
                ));
            }
            return self.f32_from_fraction(evaluated, span);
        }
        if name == "fraction" {
            if asserted {
                return Err(Diagnostic::error(
                    "outer builtins cannot use semantic identity assertions",
                    callee_expression.span,
                ));
            }
            return self.fraction(evaluated, span);
        }
        if name == "read" {
            if asserted {
                return Err(Diagnostic::error(
                    "outer builtins cannot use semantic identity assertions",
                    callee_expression.span,
                ));
            }
            return self.read(evaluated, span);
        }
        if name == "trace" {
            if asserted {
                return Err(Diagnostic::error(
                    "outer builtins cannot use semantic identity assertions",
                    callee_expression.span,
                ));
            }
            return self.trace(evaluated, span);
        }
        if name == "replay" {
            if asserted {
                return Err(Diagnostic::error(
                    "outer builtins cannot use semantic identity assertions",
                    callee_expression.span,
                ));
            }
            return self.replay_call(evaluated, span);
        }
        if name == "save" {
            if asserted {
                return Err(Diagnostic::error(
                    "outer builtins cannot use semantic identity assertions",
                    callee_expression.span,
                ));
            }
            return self.save(evaluated, span);
        }
        let Some(transform) = CallableTransform::find(self.program, &name) else {
            return Err(Diagnostic::error(
                format!("unknown outer callable `{name}`"),
                callee_expression.span,
            ));
        };
        let parameters = (0..transform.parameter_count())
            .map(|index| transform.parameter_name(index))
            .collect::<Vec<_>>();
        let arguments = order_outer_arguments_with_defaults(
            transform.name(),
            &parameters,
            evaluated,
            span,
            |index| transform.default_argument(index),
        )?;
        invoke_transform_with_lineage(
            self.engine,
            transform,
            arguments,
            match &mut self.cache {
                Some(cache) => Some(&mut **cache),
                None => None,
            },
            span,
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
            ExprKind::IdentityAsserted { value, .. } => self.callable_name(*value),
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

    fn f32_from_bits(
        &self,
        arguments: Vec<(Option<String>, OuterValue, Span)>,
        span: Span,
    ) -> Result<OuterValue, Diagnostic> {
        let mut arguments = order_outer_arguments("f32.from_bits", &["bits"], arguments, span)?;
        let (argument, argument_span) = arguments
            .pop()
            .expect("f32.from_bits has one normalized argument");
        let ValueData::Integer(bits) = argument.data else {
            return Err(Diagnostic::error(
                "f32.from_bits expects an integer bit pattern",
                argument_span,
            ));
        };
        let bits = u32::try_from(bits).map_err(|_| {
            Diagnostic::error(
                "f32.from_bits requires an integer in the range 0..=4294967295",
                argument_span,
            )
        })?;
        Ok(OuterValue::plain(ValueData::Float(f32::from_bits(bits))))
    }

    fn f32_from_fraction(
        &self,
        arguments: Vec<(Option<String>, OuterValue, Span)>,
        span: Span,
    ) -> Result<OuterValue, Diagnostic> {
        let mut arguments =
            order_outer_arguments("f32.from_fraction", &["value"], arguments, span)?;
        let (argument, argument_span) = arguments
            .pop()
            .expect("f32.from_fraction has one normalized argument");
        let ValueData::Fraction(value) = argument.data else {
            return Err(Diagnostic::error(
                "f32.from_fraction expects a fraction value",
                argument_span,
            ));
        };
        Ok(OuterValue::plain(ValueData::Float(value.to_f32())))
    }

    fn fraction(
        &self,
        arguments: Vec<(Option<String>, OuterValue, Span)>,
        span: Span,
    ) -> Result<OuterValue, Diagnostic> {
        let mut arguments =
            order_outer_arguments("fraction", &["numerator", "denominator"], arguments, span)?;
        let (denominator, denominator_span) = arguments
            .pop()
            .expect("fraction has two normalized arguments");
        let (numerator, numerator_span) = arguments
            .pop()
            .expect("fraction has two normalized arguments");
        let ValueData::Integer(numerator) = numerator.data else {
            return Err(Diagnostic::error(
                "fraction numerator must be an integer",
                numerator_span,
            ));
        };
        let ValueData::Integer(denominator) = denominator.data else {
            return Err(Diagnostic::error(
                "fraction denominator must be an integer",
                denominator_span,
            ));
        };
        let value = Fraction::new(numerator, denominator)
            .map_err(|error| Diagnostic::error(error.to_string(), denominator_span))?;
        Ok(OuterValue::plain(ValueData::Fraction(value)))
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
            (ValueData::Fraction(left), ValueData::Fraction(right)) => {
                ValueData::Fraction(fraction_binary(op, left, right, span)?)
            }
            _ => {
                return Err(Diagnostic::error(
                    "outer arithmetic requires two integers, two floats, or two fractions",
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
            (ValueData::Fraction(left), ValueData::Fraction(right)) => left == right,
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
            (ValueData::Fraction(left), ValueData::Fraction(right)) => {
                ValueData::Bool(fraction_comparison(op, left, right))
            }
            _ => {
                return Err(Diagnostic::error(
                    "outer ordering requires two integers, two floats, or two fractions",
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

fn fraction_binary(
    op: BinaryOp,
    left: Fraction,
    right: Fraction,
    span: Span,
) -> Result<Fraction, Diagnostic> {
    let result = match op {
        BinaryOp::Add => left.checked_add(right),
        BinaryOp::Subtract => left.checked_sub(right),
        BinaryOp::Multiply => left.checked_mul(right),
        BinaryOp::Divide => left.checked_div(right),
        _ => unreachable!("fraction_binary is called only for arithmetic"),
    };
    result.map_err(|error| {
        let message = match error {
            FractionError::DivisionByZero => "fraction division by zero",
            FractionError::Overflow => "fraction arithmetic overflow",
            FractionError::NonPositiveDenominator => {
                unreachable!("canonical fraction arithmetic keeps a positive denominator")
            }
        };
        Diagnostic::error(message, span)
    })
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

fn fraction_comparison(op: BinaryOp, left: Fraction, right: Fraction) -> bool {
    match op {
        BinaryOp::Equal => left == right,
        BinaryOp::NotEqual => left != right,
        BinaryOp::Less => left < right,
        BinaryOp::LessEqual => left <= right,
        BinaryOp::Greater => left > right,
        BinaryOp::GreaterEqual => left >= right,
        _ => unreachable!("fraction_comparison is called only for comparisons"),
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
    Buffer(InterpretedBuffer),
    BufferView(Arc<BufferValue>),
}

impl InterpretedValue {
    fn scalar(&self) -> NativeScalar {
        match self {
            Self::Scalar(value) => *value,
            Self::String(_)
            | Self::StringView(_)
            | Self::Bytes(_)
            | Self::BytesView(_)
            | Self::Buffer(_)
            | Self::BufferView(_) => {
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

struct InterpretedBuffer {
    storage: Vec<u8>,
    shape: Arc<[usize]>,
    outer_stride: usize,
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
    Buffer(NativeShapedBuffer),
    BufferView(Arc<BufferValue>),
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
                PreparedNativeArgument::Buffer(buffer) => NativeArgument::Buffer(buffer),
                PreparedNativeArgument::BufferView(value) => {
                    NativeArgument::BufferView(NativeShapedBufferView {
                        bytes: value.as_bytes(),
                        shape: value.shape(),
                        outer_stride: value.outer_stride(),
                    })
                }
            })
            .collect::<Vec<_>>();
        let mut capabilities = CapabilitySession::new(self.interpreter.capabilities);
        let result =
            native.invoke_with_capabilities(id, &mut native_arguments, &mut capabilities)?;
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
            NativeResult::OwnedStringAllocation(buffer) => {
                freeze_native_string(buffer, transform.span)?
            }
            NativeResult::StringViewArgument(index) => {
                let PreparedNativeArgument::StringView(value) = &prepared[index] else {
                    unreachable!("native string view result identifies a string view argument")
                };
                OuterValue::plain(ValueData::String(value.clone()))
            }
            NativeResult::CopiedStringView(value) => {
                OuterValue::plain(ValueData::String(Arc::new(value)))
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
            NativeResult::OwnedBytesAllocation(buffer) => {
                OuterValue::plain(ValueData::Bytes(Arc::new(buffer.bytes)))
            }
            NativeResult::BytesViewArgument(index) => {
                let PreparedNativeArgument::BytesView(value) = &prepared[index] else {
                    unreachable!("native bytes view result identifies a bytes view argument")
                };
                OuterValue::plain(ValueData::Bytes(value.clone()))
            }
            NativeResult::OwnedBufferArgument(index) => {
                let PreparedNativeArgument::Buffer(buffer) = std::mem::replace(
                    &mut prepared[index],
                    PreparedNativeArgument::Scalar(AbiScalar::Bool(false)),
                ) else {
                    unreachable!("native owned Buffer result identifies an owned Buffer argument")
                };
                freeze_native_buffer(buffer, transform.span)?
            }
            NativeResult::BufferViewArgument(index) => {
                let PreparedNativeArgument::BufferView(value) = &prepared[index] else {
                    unreachable!("native BufferView result identifies a BufferView argument")
                };
                OuterValue::plain(ValueData::Buffer(value.clone()))
            }
        };
        Ok(TransformOutcome {
            value,
            observations: capabilities.finish(),
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
        (Type::Buffer, ValueData::Buffer(value)) => {
            let value = Arc::try_unwrap(value).unwrap_or_else(|shared| (*shared).clone());
            let (bytes, shape, outer_stride) = value.into_parts();
            Ok(PreparedNativeArgument::Buffer(NativeShapedBuffer {
                bytes,
                shape,
                outer_stride,
            }))
        }
        (Type::BufferView, ValueData::Buffer(value)) => {
            Ok(PreparedNativeArgument::BufferView(value))
        }
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

fn freeze_native_buffer(buffer: NativeShapedBuffer, span: Span) -> Result<OuterValue, Diagnostic> {
    BufferValue::new(buffer.shape, buffer.outer_stride, buffer.bytes)
        .map(OuterValue::buffer)
        .map_err(|error| {
            Diagnostic::error(
                format!("native transform returned an invalid Buffer: {error}"),
                span,
            )
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
            ValueKind::Parameter { .. }
            | ValueKind::BufferByteElement
            | ValueKind::BufferByteIndex => unreachable!(),
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
            ValueKind::U8Scale { value, factor } => {
                let NativeScalar::U8(value) = values[value.0 as usize].as_ref().unwrap().scalar()
                else {
                    unreachable!("typed u8.scale value is u8")
                };
                let NativeScalar::F32(factor) =
                    values[factor.0 as usize].as_ref().unwrap().scalar()
                else {
                    unreachable!("typed u8.scale factor is f32")
                };
                InterpretedValue::Scalar(NativeScalar::U8(scale_u8(value, factor)))
            }
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
            ValueKind::BufferZero { buffer } => {
                let Some(InterpretedValue::Buffer(mut buffer)) = values[buffer.0 as usize].take()
                else {
                    unreachable!("typed buffer_zero input is an available owned buffer")
                };
                buffer.storage.fill(0);
                InterpretedValue::Buffer(buffer)
            }
            ValueKind::BufferFill {
                buffer,
                value: fill,
            } => {
                let NativeScalar::U8(fill) = values[fill.0 as usize].as_ref().unwrap().scalar()
                else {
                    unreachable!("typed buffer_fill value is u8")
                };
                let Some(InterpretedValue::Buffer(mut buffer)) = values[buffer.0 as usize].take()
                else {
                    unreachable!("typed buffer_fill input is an available owned buffer")
                };
                buffer.storage.fill(fill);
                InterpretedValue::Buffer(buffer)
            }
            ValueKind::BufferByteMap {
                buffer,
                element,
                index: byte_index,
                instructions,
                result,
            } => {
                let Some(InterpretedValue::Buffer(mut buffer)) = values[buffer.0 as usize].take()
                else {
                    unreachable!("typed buffer byte map input is an available owned buffer")
                };
                for index in 0..buffer.storage.len() {
                    values[element.0 as usize] = Some(InterpretedValue::Scalar(NativeScalar::U8(
                        buffer.storage[index],
                    )));
                    if let Some(byte_index) = byte_index {
                        let index = i64::try_from(index).map_err(|_| {
                            Diagnostic::error(
                                "buffer byte offset exceeds the inner i64 index range",
                                value.span,
                            )
                        })?;
                        values[byte_index.0 as usize] =
                            Some(InterpretedValue::Scalar(NativeScalar::I64(index)));
                    }
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
                        unreachable!("typed buffer byte map result is u8")
                    };
                    buffer.storage[index] = mapped;
                }
                InterpretedValue::Buffer(buffer)
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
        (Type::Buffer, ValueData::Buffer(buffer)) => {
            let buffer = Arc::try_unwrap(buffer).unwrap_or_else(|shared| (*shared).clone());
            let (storage, shape, outer_stride) = buffer.into_parts();
            Ok(InterpretedValue::Buffer(InterpretedBuffer {
                storage,
                shape,
                outer_stride,
            }))
        }
        (Type::BufferView, ValueData::Buffer(buffer)) => Ok(InterpretedValue::BufferView(buffer)),
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
        (Type::BufferView, InterpretedValue::BufferView(buffer)) => {
            InterpretedValue::BufferView(buffer.clone())
        }
        (Type::Buffer, InterpretedValue::Buffer(_)) => value
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
        InterpretedValue::Buffer(buffer) => OuterValue::buffer(BufferValue {
            storage: Arc::new(BufferStorage::new(buffer.storage)),
            shape: buffer.shape,
            outer_stride: buffer.outer_stride,
        }),
        InterpretedValue::BufferView(buffer) => OuterValue::plain(ValueData::Buffer(buffer)),
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

fn scale_u8(value: u8, factor: f32) -> u8 {
    let scaled = f32::from(value) * factor;
    if scaled.is_nan() || scaled <= 0.0 {
        0
    } else if scaled >= 255.0 {
        255
    } else {
        scaled.trunc() as u8
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
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::backend::native::NativeModule;
    use crate::cache::TransformResultCache;
    use crate::capability::{RuntimeCapabilities, World};
    use crate::diagnostic::Diagnostic;
    use crate::fraction::Fraction;
    use crate::identity::{
        ContentIdentity, IdentityDomain, IdentityPrefixResolver, byte_content_identity,
        content_identity, source_identity,
    };
    use crate::ir::TransformId;
    use crate::lineage::{Lineage, LineageNode, RecordedValue};
    use crate::plugin::{PluginDefinition, PluginParameter, PluginRegistry, PluginValueType};
    use crate::runtime::{
        BufferValue, HybridAotEngine, IrInterpreter, OuterValue, ReplayDependencyResolver,
        TransformEngine, ValueData, execute, execute_cached, execute_cached_with_capabilities,
        execute_cached_with_capabilities_and_identity_prefixes, execute_with,
        execute_with_capabilities, freeze_interpreted_value, invoke_transform,
        lower_interpreted_value, replay, replay_with_capabilities, replay_with_dependencies,
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

    impl FixedWorld {
        fn empty() -> Self {
            Self {
                environment: BTreeMap::new(),
                files: BTreeMap::new(),
                urls: BTreeMap::new(),
            }
        }
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

        fn read_asset(&self, locator: &str) -> Result<Vec<u8>, String> {
            self.files
                .get(locator)
                .cloned()
                .ok_or_else(|| format!("asset `{locator}` is unavailable"))
        }

        fn http_get(&self, url: &str) -> Result<Vec<u8>, String> {
            self.urls
                .get(url)
                .cloned()
                .ok_or_else(|| format!("HTTP URL `{url}` is unavailable"))
        }
    }

    struct EngineExecutions {
        interpreted: super::Execution,
        hybrid: super::Execution,
    }

    static CONFORMANCE_ARTIFACT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn conformance_artifact_root(case: &str) -> PathBuf {
        let sequence = CONFORMANCE_ARTIFACT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("build")
            .join(format!(
                "engine-conformance-{case}-{}-{sequence}",
                std::process::id()
            ))
    }

    /// Runs the typed-IR interpreter as the reference semantics and the hybrid
    /// engine over the same compiled program, inputs, and World. The support
    /// assertions keep native coverage distinct from intentional fallback.
    fn execute_engine_pair(
        case: &str,
        source: &str,
        bindings: BTreeMap<String, OuterValue>,
        world: &dyn World,
        native_transforms: &[&str],
        interpreted_transforms: &[&str],
    ) -> (
        Result<super::Execution, Vec<Diagnostic>>,
        Result<super::Execution, Vec<Diagnostic>>,
    ) {
        let compiled = crate::compile(format!("{case}.tima"), source).unwrap();
        let native = NativeModule::build(
            &compiled.transforms,
            &compiled.identities,
            conformance_artifact_root(case),
        )
        .unwrap();

        for name in native_transforms {
            let (id, _) = compiled
                .transforms
                .find(name)
                .unwrap_or_else(|| panic!("missing conformance transform `{name}`"));
            assert!(
                native.as_ref().is_some_and(|module| module.contains(id)),
                "conformance transform `{name}` must execute through Cranelift"
            );
        }
        for name in interpreted_transforms {
            let (id, _) = compiled
                .transforms
                .find(name)
                .unwrap_or_else(|| panic!("missing conformance transform `{name}`"));
            assert!(
                !native.as_ref().is_some_and(|module| module.contains(id)),
                "conformance transform `{name}` must exercise the documented interpreter fallback"
            );
        }

        let interpreted = execute_with(
            &compiled,
            &IrInterpreter {
                module: &compiled.transforms,
                capabilities: Some(world),
            },
            bindings.clone(),
            None,
            None,
        );
        let hybrid = execute_with(
            &compiled,
            &HybridAotEngine {
                interpreter: IrInterpreter {
                    module: &compiled.transforms,
                    capabilities: Some(world),
                },
                native: native.as_ref(),
            },
            bindings,
            None,
            None,
        );
        (interpreted, hybrid)
    }

    fn assert_successful_engine_conformance(
        case: &str,
        source: &str,
        bindings: BTreeMap<String, OuterValue>,
        world: &dyn World,
        native_transforms: &[&str],
        interpreted_transforms: &[&str],
    ) -> EngineExecutions {
        let (interpreted, hybrid) = execute_engine_pair(
            case,
            source,
            bindings,
            world,
            native_transforms,
            interpreted_transforms,
        );
        let interpreted = interpreted.unwrap_or_else(|diagnostics| {
            panic!("reference interpreter failed for {case}: {diagnostics:#?}")
        });
        let hybrid = hybrid
            .unwrap_or_else(|diagnostics| panic!("hybrid AOT failed for {case}: {diagnostics:#?}"));
        assert_execution_semantics(case, &interpreted, &hybrid);
        EngineExecutions {
            interpreted,
            hybrid,
        }
    }

    fn assert_execution_semantics(
        case: &str,
        interpreted: &super::Execution,
        hybrid: &super::Execution,
    ) {
        assert_eq!(
            interpreted.bindings.keys().collect::<Vec<_>>(),
            hybrid.bindings.keys().collect::<Vec<_>>(),
            "binding sets differ for {case}"
        );
        for (name, interpreted) in &interpreted.bindings {
            assert_outer_value_semantics(
                &format!("{case} binding `{name}`"),
                interpreted,
                &hybrid.bindings[name],
            );
        }
        match (&interpreted.last_value, &hybrid.last_value) {
            (Some(interpreted), Some(hybrid)) => {
                assert_outer_value_semantics(&format!("{case} last value"), interpreted, hybrid);
            }
            (None, None) => {}
            _ => panic!("last-value presence differs for {case}"),
        }
    }

    fn assert_outer_value_semantics(label: &str, left: &OuterValue, right: &OuterValue) {
        assert_value_data_semantics(label, &left.data, &right.data);
        match (&left.lineage, &right.lineage) {
            (Some(left), Some(right)) => assert_lineage_semantics(label, left, right),
            (None, None) => {}
            _ => panic!("lineage presence differs for {label}"),
        }
        match (content_identity(left), content_identity(right)) {
            (Ok(left), Ok(right)) => assert_eq!(left, right, "Content ID differs for {label}"),
            (Err(left), Err(right)) => assert_eq!(left.to_string(), right.to_string()),
            _ => panic!("content identity availability differs for {label}"),
        }
    }

    fn assert_value_data_semantics(label: &str, left: &ValueData, right: &ValueData) {
        match (left, right) {
            (ValueData::Null, ValueData::Null) => {}
            (ValueData::Bool(left), ValueData::Bool(right)) => assert_eq!(left, right, "{label}"),
            (ValueData::Integer(left), ValueData::Integer(right)) => {
                assert_eq!(left, right, "{label}");
            }
            (ValueData::Float(left), ValueData::Float(right)) => {
                assert_eq!(
                    left.to_bits(),
                    right.to_bits(),
                    "f32 bits differ for {label}"
                );
            }
            (ValueData::Fraction(left), ValueData::Fraction(right)) => {
                assert_eq!(left, right, "{label}");
            }
            (ValueData::String(left), ValueData::String(right)) => {
                assert_eq!(left, right, "{label}");
            }
            (ValueData::Bytes(left), ValueData::Bytes(right)) => {
                assert_eq!(left, right, "{label}");
            }
            (ValueData::List(left), ValueData::List(right)) => {
                assert_eq!(left.len(), right.len(), "list length differs for {label}");
                for (index, (left, right)) in left.iter().zip(right.iter()).enumerate() {
                    assert_outer_value_semantics(&format!("{label}[{index}]"), left, right);
                }
            }
            (ValueData::Record(left), ValueData::Record(right)) => {
                assert_eq!(
                    left.keys().collect::<Vec<_>>(),
                    right.keys().collect::<Vec<_>>(),
                    "record fields differ for {label}"
                );
                for (name, left) in left.iter() {
                    assert_outer_value_semantics(&format!("{label}.{name}"), left, &right[name]);
                }
            }
            (ValueData::Asset(left), ValueData::Asset(right)) => {
                assert_eq!(left, right, "{label}");
            }
            (ValueData::Buffer(left), ValueData::Buffer(right)) => {
                assert_eq!(
                    left.shape(),
                    right.shape(),
                    "Buffer shape differs for {label}"
                );
                assert_eq!(
                    left.outer_stride(),
                    right.outer_stride(),
                    "Buffer stride differs for {label}"
                );
                assert_eq!(
                    left.to_vec(),
                    right.to_vec(),
                    "Buffer bytes differ for {label}"
                );
            }
            (ValueData::Transform(left), ValueData::Transform(right)) => {
                assert_eq!(left, right, "{label}");
            }
            (ValueData::Lineage(left), ValueData::Lineage(right)) => {
                assert_lineage_semantics(label, left, right);
            }
            _ => panic!(
                "result types differ for {label}: {} versus {}",
                value_kind(left),
                value_kind(right)
            ),
        }
    }

    fn value_kind(value: &ValueData) -> &'static str {
        match value {
            ValueData::Null => "Null",
            ValueData::Bool(_) => "bool",
            ValueData::Integer(_) => "i64",
            ValueData::Float(_) => "f32",
            ValueData::Fraction(_) => "Fraction",
            ValueData::String(_) => "String",
            ValueData::Bytes(_) => "Bytes",
            ValueData::List(_) => "List",
            ValueData::Record(_) => "Record",
            ValueData::Asset(_) => "Asset",
            ValueData::Buffer(_) => "Buffer",
            ValueData::Transform(_) => "Transform",
            ValueData::Lineage(_) => "Lineage",
        }
    }

    fn assert_lineage_semantics(label: &str, left: &Lineage, right: &Lineage) {
        match (left.node(), right.node()) {
            (LineageNode::Source(left), LineageNode::Source(right)) => {
                assert_eq!(left, right, "source lineage differs for {label}");
            }
            (LineageNode::ExternalObservation(left), LineageNode::ExternalObservation(right)) => {
                assert_eq!(left, right, "observation lineage differs for {label}");
            }
            (LineageNode::Invocation(left), LineageNode::Invocation(right)) => {
                assert_eq!(left.transform_name, right.transform_name, "{label}");
                assert_eq!(left.transform_id, right.transform_id, "{label}");
                assert_eq!(
                    left.recipe_id, right.recipe_id,
                    "Recipe ID differs for {label}"
                );
                assert_eq!(left.arguments.len(), right.arguments.len(), "{label}");
                for (index, (left, right)) in left
                    .arguments
                    .iter()
                    .zip(right.arguments.iter())
                    .enumerate()
                {
                    let argument_label = format!("{label} argument {index}");
                    assert_eq!(left.name, right.name, "{argument_label}");
                    assert_eq!(
                        left.semantic_identity, right.semantic_identity,
                        "{argument_label}"
                    );
                    assert_recorded_value_semantics(&argument_label, &left.value, &right.value);
                    match (&left.lineage, &right.lineage) {
                        (Some(left), Some(right)) => {
                            assert_lineage_semantics(&argument_label, left, right);
                        }
                        (None, None) => {}
                        _ => panic!("argument lineage presence differs for {argument_label}"),
                    }
                }
                assert_eq!(left.observations.len(), right.observations.len(), "{label}");
                for (index, (left, right)) in left
                    .observations
                    .iter()
                    .zip(right.observations.iter())
                    .enumerate()
                {
                    assert_lineage_semantics(&format!("{label} observation {index}"), left, right);
                }
            }
            _ => panic!("lineage node types differ for {label}"),
        }
    }

    fn assert_recorded_value_semantics(label: &str, left: &RecordedValue, right: &RecordedValue) {
        match (left, right) {
            (RecordedValue::Float(left), RecordedValue::Float(right)) => {
                assert_eq!(
                    left.to_bits(),
                    right.to_bits(),
                    "f32 bits differ for {label}"
                );
            }
            _ => assert_eq!(left, right, "recorded value differs for {label}"),
        }
    }

    struct ChangingFile(RefCell<VecDeque<Vec<u8>>>);

    struct FixedIdentityPrefixes {
        domain: IdentityDomain,
        identities: Vec<String>,
    }

    impl IdentityPrefixResolver for FixedIdentityPrefixes {
        fn matching_identities(
            &self,
            domain: IdentityDomain,
            prefix: &str,
        ) -> Result<Vec<String>, String> {
            Ok(if domain == self.domain {
                self.identities
                    .iter()
                    .filter(|identity| identity.starts_with(prefix))
                    .cloned()
                    .collect()
            } else {
                Vec::new()
            })
        }
    }

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
    fn rust_invocation_dispatches_tima_builtin_and_workspace_wasm_transforms() {
        let module = include_bytes!("../../../plugins/ppm-decode/ppm_decode.wasm").to_vec();
        let plugins = PluginRegistry::new([PluginDefinition {
            name: "fixture.decode".to_owned(),
            semantic_version: 1,
            abi_version: crate::registered_wasm::PLUGIN_ABI_VERSION,
            parameters: vec![PluginParameter {
                name: "bytes".to_owned(),
                value_type: PluginValueType::Bytes,
            }],
            result: PluginValueType::Buffer,
            expected_module_content: byte_content_identity(&module),
            module_bytes: module,
        }])
        .unwrap();
        let program = crate::compile_with_plugins(
            "invoke.tima",
            "transform echo(value: i64) -> i64 { return value }\n",
            Arc::new(plugins),
        )
        .unwrap();

        let echoed = invoke_transform(
            &program,
            "echo",
            vec![OuterValue::plain(ValueData::Integer(7))],
        )
        .unwrap();
        assert_eq!(echoed.data, ValueData::Integer(7));

        let bytes = OuterValue::plain(ValueData::Bytes(Arc::new(
            b"P3\n1 1\n255\n2 4 8\n".to_vec(),
        )));
        let builtin = invoke_transform(&program, "ppm.decode", vec![bytes.clone()]).unwrap();
        let wasm = invoke_transform(&program, "fixture.decode", vec![bytes]).unwrap();
        assert_eq!(builtin.data, wasm.data);
        assert!(matches!(
            builtin.lineage.as_ref().map(Lineage::node),
            Some(LineageNode::Invocation(invocation)) if invocation.transform_name.as_ref() == "ppm.decode"
        ));
        assert!(matches!(
            wasm.lineage.as_ref().map(Lineage::node),
            Some(LineageNode::Invocation(invocation)) if invocation.transform_name.as_ref() == "fixture.decode"
        ));
    }

    #[test]
    fn outer_f32_from_bits_preserves_every_bit_in_special_values() {
        let compiled = crate::compile(
            "float-bits.tima",
            "negative_zero = f32.from_bits(2147483648)\n\
             payload_nan = f32.from_bits(2143294004)\n",
        )
        .unwrap();
        let execution = execute(&compiled).unwrap();
        let ValueData::Float(negative_zero) = execution.bindings["negative_zero"].data else {
            panic!("expected f32")
        };
        let ValueData::Float(payload_nan) = execution.bindings["payload_nan"].data else {
            panic!("expected f32")
        };
        assert_eq!(negative_zero.to_bits(), 0x8000_0000);
        assert_eq!(payload_nan.to_bits(), 0x7fc0_1234);

        let compiled = crate::compile(
            "invalid-float-bits.tima",
            "out = f32.from_bits(4294967296)\n",
        )
        .unwrap();
        let diagnostics = execute(&compiled).unwrap_err();
        assert!(diagnostics[0].message.contains("0..=4294967295"));
    }

    #[test]
    fn outer_fractions_are_exact_canonical_values() {
        let compiled = crate::compile(
            "fractions.tima",
            "half = fraction(2, 4)\n\
             literal = 2/4\n\
             literal_sum = 1/2 + 1/3\n\
             integer_division = 1 / 3\n\
             sum = half + fraction(1, 3)\n\
             difference = fraction(1, 3) - half\n\
             product = half * fraction(2, 3)\n\
             quotient = half / fraction(2, 3)\n\
             same = half == fraction(3, 6)\n\
             ordered = fraction(1, 3) < half\n\
             rounded = f32.from_fraction(fraction(1, 3))\n",
        )
        .unwrap();
        let execution = execute(&compiled).unwrap();

        assert_eq!(
            execution.bindings["half"].data,
            ValueData::Fraction(Fraction::new(1, 2).unwrap())
        );
        assert_eq!(
            execution.bindings["literal"].data,
            ValueData::Fraction(Fraction::new(1, 2).unwrap())
        );
        assert_eq!(
            execution.bindings["literal_sum"].data,
            ValueData::Fraction(Fraction::new(5, 6).unwrap())
        );
        assert_eq!(
            execution.bindings["integer_division"].data,
            ValueData::Integer(0)
        );
        assert_eq!(
            execution.bindings["sum"].data,
            ValueData::Fraction(Fraction::new(5, 6).unwrap())
        );
        assert_eq!(
            execution.bindings["difference"].data,
            ValueData::Fraction(Fraction::new(-1, 6).unwrap())
        );
        assert_eq!(
            execution.bindings["product"].data,
            ValueData::Fraction(Fraction::new(1, 3).unwrap())
        );
        assert_eq!(
            execution.bindings["quotient"].data,
            ValueData::Fraction(Fraction::new(3, 4).unwrap())
        );
        assert_eq!(execution.bindings["same"].data, ValueData::Bool(true));
        assert_eq!(execution.bindings["ordered"].data, ValueData::Bool(true));
        assert_eq!(
            execution.bindings["rounded"].data,
            ValueData::Float(1.0f32 / 3.0)
        );
    }

    #[test]
    fn outer_fractions_reject_invalid_and_mixed_arithmetic() {
        let invalid = crate::compile("invalid-fraction.tima", "out = fraction(1, 0)\n").unwrap();
        let diagnostic = execute(&invalid).unwrap_err();
        assert!(
            diagnostic
                .iter()
                .any(|diagnostic| diagnostic.message.contains("denominator must be positive"))
        );

        let invalid_literal = crate::compile("invalid-literal.tima", "out = 1/0\n").unwrap();
        let diagnostic = execute(&invalid_literal).unwrap_err();
        assert!(
            diagnostic
                .iter()
                .any(|diagnostic| diagnostic.message.contains("denominator must be positive"))
        );

        let divided = crate::compile(
            "divide-fraction.tima",
            "out = fraction(1, 2) / fraction(0, 1)\n",
        )
        .unwrap();
        let diagnostic = execute(&divided).unwrap_err();
        assert!(
            diagnostic
                .iter()
                .any(|diagnostic| diagnostic.message.contains("division by zero"))
        );

        let mixed = crate::compile("mixed-fraction.tima", "out = fraction(1, 2) + 1\n").unwrap();
        let diagnostic = execute(&mixed).unwrap_err();
        assert!(
            diagnostic
                .iter()
                .any(|diagnostic| diagnostic.message.contains("two fractions"))
        );
    }

    #[test]
    fn interpreter_and_aot_share_scalar_call_and_boundary_semantics() {
        let world = FixedWorld::empty();
        let executions = assert_successful_engine_conformance(
            "scalar-boundary",
            "transform bool_identity(value: bool) -> bool { return value }
             transform int_identity(value: i64) -> i64 { return value }
             transform int_add(left: i64, right: i64) -> i64 { return left + right }
             transform float_identity(value: f32) -> f32 { return value }
             transform float_math(left: f32, right: f32) -> f32 {
                 return left * right + right
             }
             transform float_less(left: f32, right: f32) -> bool { return left < right }
             transform choose(flag: bool, left: f32, right: f32) -> f32 {
                 if flag { return left } else { return right }
             }
             transform halve(value: f32) -> f32 { return value * 0.5 }
             transform through_call(value: f32) -> f32 { return halve(value) }
             transform own_text(value: String) -> String { return value }
             transform view_text(value: StringView) -> StringView { return value }
             transform own_bytes(value: Bytes) -> Bytes { return value }
             transform view_bytes(value: BytesView) -> BytesView { return value }
             bool_out = bool_identity(true)
             integer_out = int_identity(42)
             integer_sum = int_add(20, 22)
             float_out = float_math(8.0, 0.25)
             less_out = float_less(0.25, 0.5)
             branch_out = choose(false, 1.0, 2.0)
             call_out = through_call(8.0)
             negative_zero_out = float_identity(negative_zero)
             nan_out = float_identity(payload_nan)
             owned_text = own_text(text)
             viewed_text = view_text(text)
             owned_bytes = own_bytes(blob)
             viewed_bytes = view_bytes(blob)
            ",
            BTreeMap::from([
                (
                    "negative_zero".to_owned(),
                    OuterValue::plain(ValueData::Float(f32::from_bits(0x8000_0000))),
                ),
                (
                    "payload_nan".to_owned(),
                    OuterValue::plain(ValueData::Float(f32::from_bits(0x7fc0_1234))),
                ),
                (
                    "text".to_owned(),
                    OuterValue::plain(ValueData::String(Arc::new("hello".to_owned()))),
                ),
                (
                    "blob".to_owned(),
                    OuterValue::plain(ValueData::Bytes(Arc::new(vec![1, 2, 3, 4]))),
                ),
            ]),
            &world,
            &[
                "bool_identity",
                "int_identity",
                "float_identity",
                "float_math",
                "float_less",
                "choose",
                "halve",
                "through_call",
                "own_text",
                "view_text",
                "own_bytes",
                "view_bytes",
            ],
            &["int_add"],
        );

        for execution in [&executions.interpreted, &executions.hybrid] {
            let ValueData::Float(negative_zero) = execution.bindings["negative_zero_out"].data
            else {
                panic!("expected f32 result")
            };
            assert_eq!(negative_zero.to_bits(), 0x8000_0000);
            let ValueData::Float(payload_nan) = execution.bindings["nan_out"].data else {
                panic!("expected f32 result")
            };
            assert_eq!(payload_nan.to_bits(), 0x7fc0_1234);

            let ValueData::String(text) = &execution.bindings["text"].data else {
                panic!("expected original String")
            };
            let ValueData::String(owned_text) = &execution.bindings["owned_text"].data else {
                panic!("expected owned String result")
            };
            let ValueData::String(viewed_text) = &execution.bindings["viewed_text"].data else {
                panic!("expected StringView result")
            };
            assert!(!Arc::ptr_eq(text, owned_text));
            assert!(Arc::ptr_eq(text, viewed_text));

            let ValueData::Bytes(blob) = &execution.bindings["blob"].data else {
                panic!("expected original Bytes")
            };
            let ValueData::Bytes(owned_bytes) = &execution.bindings["owned_bytes"].data else {
                panic!("expected owned Bytes result")
            };
            let ValueData::Bytes(viewed_bytes) = &execution.bindings["viewed_bytes"].data else {
                panic!("expected BytesView result")
            };
            assert!(!Arc::ptr_eq(blob, owned_bytes));
            assert!(Arc::ptr_eq(blob, viewed_bytes));
        }
    }

    #[test]
    fn interpreter_and_aot_share_world_observations_lineage_and_content() {
        let world = FixedWorld {
            environment: BTreeMap::from([("MODE".to_owned(), b"release".to_vec())]),
            files: BTreeMap::from([("asset.bin".to_owned(), vec![1, 2, 3, 4])]),
            urls: BTreeMap::from([("https://example.test/data".to_owned(), vec![5, 6, 7, 8])]),
        };
        let executions = assert_successful_engine_conformance(
            "world-operations",
            "transform environment(key: StringView) -> String uses env.read {
                 return env.read(key)
             }
             transform file(path: StringView) -> Bytes uses file.read {
                 return file.read(path)
             }
             transform http(url: StringView) -> Bytes uses http.get {
                 return http.get(url)
             }
             mode = environment(\"MODE\")
             local = file(\"asset.bin\")
             remote = http(\"https://example.test/data\")
            ",
            BTreeMap::new(),
            &world,
            &["environment", "file", "http"],
            &[],
        );

        assert_eq!(
            executions.hybrid.bindings["local"].data,
            ValueData::Bytes(Arc::new(vec![1, 2, 3, 4]))
        );
        assert_eq!(
            executions.hybrid.bindings["mode"].data,
            ValueData::String(Arc::new("release".to_owned()))
        );
        let lineage = executions.hybrid.bindings["local"]
            .lineage
            .as_ref()
            .unwrap();
        let LineageNode::Invocation(invocation) = lineage.node() else {
            panic!("expected invocation lineage")
        };
        assert_eq!(invocation.observations.len(), 1);
        let LineageNode::ExternalObservation(observation) = invocation.observations[0].node()
        else {
            panic!("expected external observation")
        };
        assert_eq!(observation.capability.as_ref(), "file.read");
        assert_eq!(observation.key.as_ref(), b"asset.bin");
        assert_eq!(
            observation.observed_content,
            byte_content_identity(&[1, 2, 3, 4])
        );
    }

    #[test]
    fn interpreter_and_aot_report_the_same_world_callback_failure() {
        let world = FixedWorld::empty();
        let (interpreted, hybrid) = execute_engine_pair(
            "world-failure",
            "transform load() -> Bytes uses file.read {
                 return file.read(\"missing.bin\")
             }
             out = load()
            ",
            BTreeMap::new(),
            &world,
            &["load"],
            &[],
        );
        assert_eq!(interpreted.unwrap_err(), hybrid.unwrap_err());
    }

    #[test]
    fn hybrid_fallback_preserves_checked_integer_failures() {
        let world = FixedWorld::empty();
        for (case, expression) in [
            ("integer-overflow", "value + 1"),
            ("integer-division-by-zero", "value / 0"),
        ] {
            let source = format!(
                "transform checked(value: i64) -> i64 {{ return {expression} }}\n\
                 out = checked(9223372036854775807)\n"
            );
            let (interpreted, hybrid) =
                execute_engine_pair(case, &source, BTreeMap::new(), &world, &[], &["checked"]);
            assert_eq!(interpreted.unwrap_err(), hybrid.unwrap_err());
        }
    }

    #[test]
    fn executes_an_identity_asserted_transform_call() {
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
    fn identity_assertions_accept_first_class_transform_values() {
        let definition = "transform keep(x: i64) -> i64 { return x }\n";
        let base = crate::compile("base.tima", definition).unwrap();
        let identity = base.identities.get(TransformId(0)).to_string();
        let compiled = crate::compile(
            "transform-value-identity.tima",
            format!("{definition}pinned = keep#{}\n", &identity[..14]),
        )
        .unwrap();

        let execution = execute(&compiled).unwrap();
        assert_eq!(
            execution.bindings["pinned"].data,
            ValueData::Transform(TransformId(0))
        );
    }

    #[test]
    fn identity_assertions_accept_plain_outer_values_by_content_id() {
        let input = OuterValue::plain(ValueData::List(Arc::from([
            OuterValue::plain(ValueData::Integer(1)),
            OuterValue::plain(ValueData::Integer(2)),
        ])));
        let identity = content_identity(&input).unwrap().to_string();
        let compiled = crate::compile(
            "value-identity.tima",
            format!("out = input#{}\n", &identity[..16]),
        )
        .unwrap();

        let execution = execute_with(
            &compiled,
            &IrInterpreter {
                module: &compiled.transforms,
                capabilities: None,
            },
            BTreeMap::from([("input".to_owned(), input.clone())]),
            None,
            None,
        )
        .unwrap();

        assert_eq!(execution.bindings["out"], input);
    }

    #[test]
    fn value_identity_prefixes_must_be_unique_in_outer_bindings() {
        let mut first_by_prefix = BTreeMap::new();
        let (first, second, prefix) = (0_i64..17)
            .find_map(|value| {
                let identity = content_identity(&OuterValue::plain(ValueData::Integer(value)))
                    .unwrap()
                    .to_string();
                let prefix = identity[..1].to_owned();
                first_by_prefix
                    .insert(prefix.clone(), value)
                    .map(|first| (first, value, prefix))
            })
            .expect("17 distinct Content IDs collide in one hexadecimal digit");
        let compiled = crate::compile(
            "value-collision.tima",
            format!("first = {first}\nsecond = {second}\nout = first#{prefix}\n"),
        )
        .unwrap();
        let diagnostics = execute(&compiled).unwrap_err();

        assert!(diagnostics[0].message.contains("Content ID"));
        assert!(diagnostics[0].message.contains("ambiguous locally"));
        assert!(diagnostics[0].notes[0].contains("longer prefix"));
    }

    #[test]
    fn value_identity_prefixes_include_host_local_identities() {
        let value = OuterValue::plain(ValueData::Integer(42));
        let actual = content_identity(&value).unwrap().to_string();
        let prefix = &actual[..1];
        let collision = (0_i64..)
            .map(|candidate| {
                content_identity(&OuterValue::plain(ValueData::Integer(candidate)))
                    .unwrap()
                    .to_string()
            })
            .find(|candidate| candidate != &actual && candidate.starts_with(prefix))
            .unwrap();
        let compiled =
            crate::compile("host-collision.tima", format!("out = 42#{prefix}\n")).unwrap();
        let mut cache = TransformResultCache::default();
        let world = FixedWorld {
            environment: BTreeMap::new(),
            files: BTreeMap::new(),
            urls: BTreeMap::new(),
        };
        let resolver = FixedIdentityPrefixes {
            domain: IdentityDomain::Content,
            identities: vec![actual, collision],
        };
        let diagnostics = execute_cached_with_capabilities_and_identity_prefixes(
            &compiled, &mut cache, &world, &resolver,
        )
        .unwrap_err();

        assert!(diagnostics[0].message.contains("ambiguous locally"));
    }

    #[test]
    fn identity_assertions_use_recipe_id_for_derived_values() {
        let definition = "transform keep(x: i64) -> i64 { return x }\n";
        let base = crate::compile("base.tima", format!("{definition}out = keep(42)\n")).unwrap();
        let base_execution = execute(&base).unwrap();
        let recipe = base_execution.bindings["out"]
            .lineage
            .as_ref()
            .and_then(Lineage::recipe_id)
            .unwrap()
            .to_string();
        let compiled = crate::compile(
            "recipe-identity.tima",
            format!("{definition}out = keep(42)#{}\n", &recipe[..20]),
        )
        .unwrap();

        let execution = execute(&compiled).unwrap();
        assert_eq!(execution.bindings["out"].data, ValueData::Integer(42));
    }

    #[test]
    fn identity_assertions_use_source_id_for_observed_assets() {
        let bytes = b"source bytes";
        let identity = source_identity("cat.bin", byte_content_identity(bytes)).to_string();
        let compiled = crate::compile(
            "source-identity.tima",
            format!("out = read(asset(\"cat.bin\"))#{}\n", &identity[..18]),
        )
        .unwrap();
        let world = FixedWorld {
            environment: BTreeMap::new(),
            files: BTreeMap::from([("cat.bin".to_owned(), bytes.to_vec())]),
            urls: BTreeMap::new(),
        };

        let execution = execute_with_capabilities(&compiled, &world).unwrap();
        assert_eq!(
            execution.bindings["out"].data,
            ValueData::Bytes(Arc::new(bytes.to_vec()))
        );
    }

    #[test]
    fn identity_assertion_mismatches_report_the_identity_domain() {
        let compiled = crate::compile("mismatch.tima", "out = 42#00000000\n").unwrap();
        let diagnostics = execute(&compiled).unwrap_err();

        assert!(diagnostics[0].message.contains("Content ID"));
        assert!(diagnostics[0].message.contains("does not match"));
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

        let diagnostics = execute_with(&compiled, &engine, bindings, None, None).unwrap_err();

        assert!(diagnostics[0].message.contains("output is unavailable"));
    }

    #[test]
    fn unique_owned_buffer_storage_transfers_through_inner_calls_without_copying() {
        let compiled = crate::compile(
            "test.tima",
            "transform own(img: Buffer) -> Buffer { return img }\n\
             transform own_inner(img: Buffer) -> Buffer { return own(img) }\n",
        )
        .unwrap();
        let interpreter = IrInterpreter {
            module: &compiled.transforms,
            capabilities: None,
        };
        let buffer = BufferValue::new(vec![2, 2], 2, vec![1, 2, 3, 4]).unwrap();
        let result = interpreter
            .invoke(
                crate::ir::TransformId(1),
                vec![(OuterValue::buffer(buffer), Span::default())],
            )
            .unwrap()
            .value;
        let ValueData::Buffer(result) = result.data else {
            panic!("expected Buffer result")
        };
        assert_eq!(result.bytes(), &[1, 2, 3, 4]);
    }

    #[test]
    fn interpreted_owned_buffers_detach_while_views_alias() {
        let compiled = crate::compile(
            "test.tima",
            "transform clear(img: Buffer) -> Buffer { return buffer_zero(img) }\n\
             transform view(img: BufferView) -> BufferView { return img }\n\
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
                OuterValue::buffer(BufferValue::new(vec![2, 2], 2, vec![1, 2, 3, 4]).unwrap()),
            )]),
            None,
            None,
        )
        .unwrap();
        let ValueData::Buffer(original) = &execution.bindings["img"].data else {
            panic!("expected original Buffer")
        };
        let ValueData::Buffer(cleared) = &execution.bindings["cleared"].data else {
            panic!("expected owned Buffer result")
        };
        let ValueData::Buffer(viewed) = &execution.bindings["viewed"].data else {
            panic!("expected Buffer view")
        };

        assert_eq!(original.bytes(), &[1, 2, 3, 4]);
        assert_eq!(cleared.bytes(), &[0, 0, 0, 0]);
        assert!(!original.shares_storage_with(cleared));
        assert!(original.shares_storage_with(viewed));
    }

    #[test]
    fn interpreter_and_aot_share_buffer_fill_and_view_ownership() {
        let world = FixedWorld::empty();
        let executions = assert_successful_engine_conformance(
            "buffer-boundary",
            "transform clear(buffer: Buffer) -> Buffer { return buffer_zero(buffer) }\n\
             transform fill(buffer: Buffer, value: u8) -> Buffer { return buffer_fill(buffer, value) }\n\
             transform view(buffer: BufferView) -> BufferView { return buffer }\n\
             cleared = clear(input)\n\
             filled = fill(input, 7)\n\
             viewed = view(input)\n",
            BTreeMap::from([(
                "input".to_owned(),
                OuterValue::buffer(
                    BufferValue::new(vec![2, 3], 4, vec![1, 2, 3, 99, 4, 5, 6, 100]).unwrap(),
                ),
            )]),
            &world,
            &["clear", "fill", "view"],
            &[],
        );

        for execution in [&executions.interpreted, &executions.hybrid] {
            let ValueData::Buffer(original) = &execution.bindings["input"].data else {
                panic!("expected original Buffer")
            };
            let ValueData::Buffer(cleared) = &execution.bindings["cleared"].data else {
                panic!("expected cleared Buffer")
            };
            let ValueData::Buffer(filled) = &execution.bindings["filled"].data else {
                panic!("expected filled Buffer")
            };
            let ValueData::Buffer(viewed) = &execution.bindings["viewed"].data else {
                panic!("expected viewed Buffer")
            };
            assert_eq!(cleared.shape(), &[2, 3]);
            assert_eq!(cleared.outer_stride(), 4);
            assert_eq!(cleared.bytes(), &[0; 8]);
            assert_eq!(filled.shape(), &[2, 3]);
            assert_eq!(filled.outer_stride(), 4);
            assert_eq!(filled.bytes(), &[7; 8]);
            assert!(!original.shares_storage_with(cleared));
            assert!(!original.shares_storage_with(filled));
            assert!(original.shares_storage_with(viewed));
        }
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
    fn interpreter_and_aot_share_unindexed_buffer_map_semantics() {
        let world = FixedWorld::empty();
        let executions = assert_successful_engine_conformance(
            "buffer-map-fallback",
            "transform choose(current: u8, target: u8, replacement: u8) -> u8 {\n\
                 if current == target { return replacement } else { return current }\n\
             }\n\
             transform replace(img: Buffer, target: u8, replacement: u8) -> Buffer {\n\
                 for byte in img.bytes { byte = choose(byte, target, replacement) }\n\
                 return img\n\
             }\n\
             out = replace(img, target, replacement)\n",
            BTreeMap::from([
                (
                    "img".to_owned(),
                    OuterValue::buffer(
                        BufferValue::new(vec![2, 3], 4, vec![1, 2, 1, 8, 3, 1, 4, 1]).unwrap(),
                    ),
                ),
                (
                    "target".to_owned(),
                    OuterValue::plain(ValueData::Integer(1)),
                ),
                (
                    "replacement".to_owned(),
                    OuterValue::plain(ValueData::Integer(9)),
                ),
            ]),
            &world,
            &["choose", "replace"],
            &[],
        );

        for execution in [&executions.interpreted, &executions.hybrid] {
            let ValueData::Buffer(original) = &execution.bindings["img"].data else {
                panic!("expected original Buffer")
            };
            let ValueData::Buffer(result) = &execution.bindings["out"].data else {
                panic!("expected mapped Buffer")
            };
            assert_eq!(original.shape(), &[2, 3]);
            assert_eq!(original.outer_stride(), 4);
            assert_eq!(original.bytes(), &[1, 2, 1, 8, 3, 1, 4, 1]);
            assert_eq!(result.shape(), &[2, 3]);
            assert_eq!(result.outer_stride(), 4);
            assert_eq!(result.bytes(), &[9, 2, 9, 8, 3, 9, 4, 9]);
            assert!(!original.shares_storage_with(result));
        }
    }

    #[test]
    fn interpreter_and_aot_share_u8_scale_saturation_inside_buffer_maps() {
        let world = FixedWorld::empty();
        let executions = assert_successful_engine_conformance(
            "buffer-scale",
            "transform scale_byte(value: u8, factor: f32) -> u8 {\n\
                 return u8.scale(value, factor)\n\
             }\n\
             transform scale(buffer: Buffer, factor: f32) -> Buffer {\n\
                 for byte in buffer.bytes { byte = scale_byte(byte, factor) }\n\
                 return buffer\n\
             }\n\
             half = scale(input, 0.5)\n\
             not_a_number = scale(input, nan_factor)\n\
             positive_infinity = scale(input, positive_factor)\n\
             negative_infinity = scale(input, negative_factor)\n",
            BTreeMap::from([
                (
                    "input".to_owned(),
                    OuterValue::buffer(BufferValue::new(vec![4], 4, vec![0, 1, 200, 255]).unwrap()),
                ),
                (
                    "nan_factor".to_owned(),
                    OuterValue::plain(ValueData::Float(f32::NAN)),
                ),
                (
                    "positive_factor".to_owned(),
                    OuterValue::plain(ValueData::Float(f32::INFINITY)),
                ),
                (
                    "negative_factor".to_owned(),
                    OuterValue::plain(ValueData::Float(f32::NEG_INFINITY)),
                ),
            ]),
            &world,
            &["scale_byte", "scale"],
            &[],
        );

        for execution in [&executions.interpreted, &executions.hybrid] {
            let ValueData::Buffer(half) = &execution.bindings["half"].data else {
                panic!("expected scaled Buffer")
            };
            let ValueData::Buffer(not_a_number) = &execution.bindings["not_a_number"].data else {
                panic!("expected scaled Buffer")
            };
            let ValueData::Buffer(positive_infinity) =
                &execution.bindings["positive_infinity"].data
            else {
                panic!("expected scaled Buffer")
            };
            let ValueData::Buffer(negative_infinity) =
                &execution.bindings["negative_infinity"].data
            else {
                panic!("expected scaled Buffer")
            };
            assert_eq!(half.bytes(), &[0, 0, 100, 127]);
            assert_eq!(not_a_number.bytes(), &[0, 0, 0, 0]);
            assert_eq!(positive_infinity.bytes(), &[0, 255, 255, 255]);
            assert_eq!(negative_infinity.bytes(), &[0, 0, 0, 0]);
        }
    }

    #[test]
    fn indexed_buffer_map_darkens_rgba_storage_without_mutating_the_outer_input() {
        let world = FixedWorld::empty();
        let executions = assert_successful_engine_conformance(
            "indexed-buffer-darken",
            "transform darken_byte(value: u8, offset: i64, factor: f32) -> u8 {\n\
                 pixel = offset / 4\n\
                 alpha = pixel * 4 + 3\n\
                 if offset == alpha { return value } else { return u8.scale(value, factor) }\n\
             }\n\
             transform darken(buffer: Buffer, factor: f32) -> Buffer {\n\
                 for byte, offset in buffer.bytes {\n\
                     byte = darken_byte(byte, offset, factor)\n\
                 }\n\
                 return buffer\n\
             }\n\
             out = darken(img, 0.5)\n",
            BTreeMap::from([(
                "img".to_owned(),
                OuterValue::buffer(
                    BufferValue::new(vec![1, 2, 4], 8, vec![100, 50, 20, 255, 200, 100, 50, 128])
                        .unwrap(),
                ),
            )]),
            &world,
            &[],
            &["darken_byte", "darken"],
        );

        for execution in [&executions.interpreted, &executions.hybrid] {
            let ValueData::Buffer(original) = &execution.bindings["img"].data else {
                panic!("expected original Buffer")
            };
            let ValueData::Buffer(darkened) = &execution.bindings["out"].data else {
                panic!("expected darkened Buffer")
            };
            assert_eq!(original.bytes(), &[100, 50, 20, 255, 200, 100, 50, 128]);
            assert_eq!(darkened.bytes(), &[50, 25, 10, 255, 100, 50, 25, 128]);
            assert_eq!(darkened.shape(), &[1, 2, 4]);
            assert_eq!(darkened.outer_stride(), 8);
            assert!(!original.shares_storage_with(darkened));
            assert!(execution.bindings["out"].lineage.is_some());
        }
    }

    #[test]
    fn u8_scale_truncates_saturates_and_defines_non_finite_results() {
        assert_eq!(super::scale_u8(101, 0.5), 50);
        assert_eq!(super::scale_u8(200, 2.0), 255);
        assert_eq!(super::scale_u8(200, -1.0), 0);
        assert_eq!(super::scale_u8(200, f32::INFINITY), 255);
        assert_eq!(super::scale_u8(200, f32::NEG_INFINITY), 0);
        assert_eq!(super::scale_u8(200, f32::NAN), 0);
    }
}
