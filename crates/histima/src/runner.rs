use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::str::FromStr;

use tima::CompiledProgram;
use tima::ast::Item;
use tima::backend::cache::{ArtifactCacheStatus, CachedArtifact};
use tima::cache::{CacheError, CacheStats, ResultCache, TransformResultCache};
use tima::capability::World;
use tima::identity::{ContentIdentity, RecipeIdentity, byte_content_identity};
use tima::lineage::{Lineage, LineageNode};
use tima::runtime::{Execution, OuterValue, ValueData};

use crate::{ArtifactInfo, RecordedResult, Workspace};

/// The observable result of one Tima program execution in a Histima workspace.
///
/// Result-cache statistics include both process-local and validated durable
/// workspace lookups. Interpreted execution does not produce a native artifact.
#[derive(Debug)]
pub struct ProgramExecution {
    pub execution: Execution,
    pub engine: ExecutionEngine,
    pub stock_policy: ResultStockPolicy,
    pub stocked_results: Vec<StockedResult>,
    pub artifact: Option<ArtifactInfo>,
    pub artifact_cache: Option<ArtifactCacheStatus>,
    pub result_cache: CacheStats,
}

/// Host storage policy applied only after a complete successful execution.
/// It does not participate in Tima semantics, lineage, or identity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ResultStockPolicy {
    #[default]
    None,
    ReachableInvocations,
}

impl ResultStockPolicy {
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ReachableInvocations => "reachable-invocations",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StockedResult {
    pub transform_name: String,
    pub recorded: RecordedResult,
}

/// Explicit execution policy for source-defined Tima transforms.
/// Registered host and Wasm transforms use their existing implementations in
/// either mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExecutionEngine {
    #[default]
    Interpreter,
    HybridAot,
}

impl ExecutionEngine {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Interpreter => "interpreter",
            Self::HybridAot => "hybrid-aot",
        }
    }
}

impl FromStr for ExecutionEngine {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "interpreter" => Ok(Self::Interpreter),
            "hybrid-aot" => Ok(Self::HybridAot),
            _ => Err(format!(
                "unknown execution engine {value:?}; expected interpreter or hybrid-aot"
            )),
        }
    }
}

/// The result of replaying one durable semantic recipe.
#[derive(Debug)]
pub struct RecipeReplay {
    pub value: OuterValue,
    pub policy: ReplayPolicy,
    pub artifact: Option<ArtifactInfo>,
    pub result_cache: CacheStats,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayPolicy {
    Strict,
    Snapshot,
}

impl ReplayPolicy {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Snapshot => "snapshot",
        }
    }
}

/// Result of evaluating one command-line outer pipeline expression.
///
/// This path deliberately has no compiled artifact: declarations are rejected,
/// so only the outer interpreter and versioned registered transforms are involved.
#[derive(Debug)]
pub struct PipelineExecution {
    pub value: OuterValue,
    pub result_cache: CacheStats,
}

#[derive(Debug)]
pub enum RunError {
    Storage(crate::Error),
    Runtime(Vec<tima::diagnostic::Diagnostic>),
    InvalidPipeline(String),
}

impl Workspace {
    /// Evaluates exactly one outer expression using workspace capabilities and
    /// the durable Recipe cache, without compiling an empty native module.
    pub fn evaluate_pipeline(
        &self,
        program: &CompiledProgram,
    ) -> Result<PipelineExecution, RunError> {
        if !matches!(program.syntax.items.as_slice(), [Item::Expression(_)]) {
            return Err(RunError::InvalidPipeline(
                "the pipeline command accepts exactly one expression; bindings, transform declarations, and multiple statements require a Tima source file"
                    .to_owned(),
            ));
        }
        let mut result_cache = WorkspaceResultCache::new(self);
        let execution = tima::runtime::execute_cached_with_capabilities_and_identity_prefixes(
            program,
            &mut result_cache,
            self,
            self,
        )
        .map_err(RunError::Runtime)?;
        let value = execution.last_value.ok_or_else(|| {
            RunError::InvalidPipeline("the pipeline expression produced no value".to_owned())
        })?;
        Ok(PipelineExecution {
            value,
            result_cache: result_cache.stats,
        })
    }

    /// Executes checked inner Tima through the typed-IR interpreter with this
    /// workspace as the only host capability provider.
    pub fn execute(&self, program: &CompiledProgram) -> Result<ProgramExecution, RunError> {
        self.execute_with_engine(program, ExecutionEngine::Interpreter)
    }

    /// Executes a program with an explicit source-transform engine. Hybrid AOT
    /// compiles supported definitions and interprets unsupported definitions.
    pub fn execute_with_engine(
        &self,
        program: &CompiledProgram,
        engine: ExecutionEngine,
    ) -> Result<ProgramExecution, RunError> {
        self.execute_with_engine_collecting(program, engine)
            .map(|(execution, _)| execution)
    }

    /// Executes a program and, after it has completed successfully, applies an
    /// explicit durable-result stocking policy. Reachability starts at final
    /// outer bindings and the final expression, so discarded expression
    /// results are not retained.
    pub fn execute_with_engine_and_stocking(
        &mut self,
        program: &CompiledProgram,
        engine: ExecutionEngine,
        policy: ResultStockPolicy,
    ) -> Result<ProgramExecution, RunError> {
        let (mut execution, candidates) = self.execute_with_engine_collecting(program, engine)?;
        execution.stock_policy = policy;
        if policy == ResultStockPolicy::ReachableInvocations {
            execution.stocked_results = candidates
                .into_iter()
                .map(|candidate| {
                    self.record_value(&candidate.value)
                        .map(|recorded| StockedResult {
                            transform_name: candidate.transform_name,
                            recorded,
                        })
                        .map_err(RunError::Storage)
                })
                .collect::<Result<Vec<_>, _>>()?;
        }
        Ok(execution)
    }

    fn execute_with_engine_collecting(
        &self,
        program: &CompiledProgram,
        engine: ExecutionEngine,
    ) -> Result<(ProgramExecution, Vec<StockCandidate>), RunError> {
        let mut result_cache = WorkspaceResultCache::new(self);
        let (execution, cached_artifact) = match engine {
            ExecutionEngine::Interpreter => (
                tima::runtime::execute_cached_with_capabilities_and_identity_prefixes(
                    program,
                    &mut result_cache,
                    self,
                    self,
                )
                .map_err(RunError::Runtime)?,
                None,
            ),
            ExecutionEngine::HybridAot => {
                let execution =
                    tima::runtime::execute_aot_cached_with_capabilities_and_identity_prefixes(
                        program,
                        &mut result_cache,
                        self,
                        self.artifact_cache_root(),
                        self,
                    )
                    .map_err(RunError::Runtime)?;
                (execution.execution, execution.artifact)
            }
        };
        let artifact_cache = cached_artifact.as_ref().map(|artifact| artifact.status);
        let artifact = cached_artifact
            .as_ref()
            .map(cached_artifact_info)
            .transpose()
            .map_err(RunError::Storage)?;
        let stats = result_cache.stats;
        let candidates = result_cache.reachable_candidates(&execution);
        Ok((
            ProgramExecution {
                execution,
                engine,
                stock_policy: ResultStockPolicy::None,
                stocked_results: vec![],
                artifact,
                artifact_cache,
                result_cache: stats,
            },
            candidates,
        ))
    }

    /// Replays a durable recipe against current transform definitions and
    /// observed dependencies. Durable cached intermediates remain eligible,
    /// but only after the complete recorded lineage has been validated.
    pub fn replay_recipe(
        &self,
        program: &CompiledProgram,
        recipe: RecipeIdentity,
    ) -> Result<RecipeReplay, RunError> {
        self.replay_recipe_with_policy(program, recipe, ReplayPolicy::Strict)
    }

    pub fn replay_recipe_with_policy(
        &self,
        program: &CompiledProgram,
        recipe: RecipeIdentity,
        policy: ReplayPolicy,
    ) -> Result<RecipeReplay, RunError> {
        let target = self.replay_target(recipe).map_err(RunError::Storage)?;
        let mut result_cache = WorkspaceResultCache::new(self);
        let snapshots;
        let world: &dyn World = match policy {
            ReplayPolicy::Strict => self,
            ReplayPolicy::Snapshot => {
                snapshots = SnapshotWorld::load(self, &target).map_err(RunError::Storage)?;
                &snapshots
            }
        };
        let value =
            tima::runtime::replay_with_capabilities(program, &target, &mut result_cache, world)
                .map_err(|diagnostic| RunError::Runtime(vec![diagnostic]))?;
        Ok(RecipeReplay {
            value,
            policy,
            artifact: None,
            result_cache: result_cache.stats,
        })
    }
}

fn cached_artifact_info(artifact: &CachedArtifact) -> crate::Result<ArtifactInfo> {
    let path = &artifact.artifact.artifact_path;
    let bytes = fs::read(path)
        .map_err(|error| crate::Error::io("read compiled artifact metadata", path, error))?;
    Ok(ArtifactInfo {
        bundle_id: artifact.bundle_id,
        artifact_ids: artifact.artifact_ids.clone(),
        artifact_content_id: byte_content_identity(&bytes),
    })
}

struct SnapshotWorld<'workspace> {
    workspace: &'workspace Workspace,
    values: BTreeMap<(String, Vec<u8>), Vec<u8>>,
}

impl<'workspace> SnapshotWorld<'workspace> {
    fn load(workspace: &'workspace Workspace, target: &OuterValue) -> crate::Result<Self> {
        let mut values = BTreeMap::new();
        if let Some(lineage) = &target.lineage {
            collect_snapshots(workspace, lineage, &mut values)?;
        }
        Ok(Self { workspace, values })
    }

    fn value(&self, capability: &str, key: &[u8]) -> Result<Vec<u8>, String> {
        self.values
            .get(&(capability.to_owned(), key.to_vec()))
            .cloned()
            .ok_or_else(|| {
                format!(
                    "no retained snapshot is available for `{capability}` dependency {:?}",
                    String::from_utf8_lossy(key)
                )
            })
    }
}

impl World for SnapshotWorld<'_> {
    fn environment(&self, name: &str) -> Result<Vec<u8>, String> {
        self.value(tima::capability::ENVIRONMENT_CAPABILITY, name.as_bytes())
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        self.value(tima::capability::FILE_READ_CAPABILITY, path.as_bytes())
    }

    fn http_get(&self, url: &str) -> Result<Vec<u8>, String> {
        self.value(tima::capability::HTTP_GET_CAPABILITY, url.as_bytes())
    }

    fn read_asset(&self, locator: &str) -> Result<Vec<u8>, String> {
        World::read_asset(self.workspace, locator)
    }

    fn write_asset(&self, locator: &str, bytes: &[u8]) -> Result<(), String> {
        World::write_asset(self.workspace, locator, bytes)
    }
}

fn collect_snapshots(
    workspace: &Workspace,
    lineage: &Lineage,
    values: &mut BTreeMap<(String, Vec<u8>), Vec<u8>>,
) -> crate::Result<()> {
    match lineage.node() {
        LineageNode::Source(_) => {}
        LineageNode::ExternalObservation(observation) => {
            let bytes = workspace
                .world_snapshot(observation.dependency_id)
                .map_err(crate::Error::catalog)?
                .ok_or_else(|| {
                    crate::Error::catalog(format!(
                        "no retained snapshot is available for `{}` dependency {:?}",
                        observation.capability,
                        String::from_utf8_lossy(&observation.key)
                    ))
                })?;
            let observed = byte_content_identity(&bytes);
            if observed != observation.observed_content {
                return Err(crate::Error::catalog(format!(
                    "retained snapshot for dependency {} has content {observed}, expected {}",
                    observation.dependency_id, observation.observed_content
                )));
            }
            values.insert(
                (observation.capability.to_string(), observation.key.to_vec()),
                bytes,
            );
        }
        LineageNode::Invocation(invocation) => {
            for argument in invocation.arguments.iter() {
                if let Some(parent) = &argument.lineage {
                    collect_snapshots(workspace, parent, values)?;
                }
            }
            for observation in invocation.observations.iter() {
                collect_snapshots(workspace, observation, values)?;
            }
        }
    }
    Ok(())
}

struct WorkspaceResultCache<'workspace> {
    workspace: &'workspace Workspace,
    memory: TransformResultCache,
    produced: BTreeMap<RecipeIdentity, OuterValue>,
    stats: CacheStats,
}

impl<'workspace> WorkspaceResultCache<'workspace> {
    fn new(workspace: &'workspace Workspace) -> Self {
        Self {
            workspace,
            memory: TransformResultCache::default(),
            produced: BTreeMap::new(),
            stats: CacheStats::default(),
        }
    }

    fn reachable_candidates(&self, execution: &Execution) -> Vec<StockCandidate> {
        let mut seen = std::collections::BTreeSet::new();
        let mut candidates = Vec::new();
        for value in execution
            .bindings
            .values()
            .chain(execution.last_value.iter())
        {
            collect_value_stock_candidates(value, &self.produced, &mut seen, &mut candidates);
        }
        candidates
    }
}

struct StockCandidate {
    transform_name: String,
    value: OuterValue,
}

fn collect_value_stock_candidates(
    value: &OuterValue,
    produced: &BTreeMap<RecipeIdentity, OuterValue>,
    seen: &mut std::collections::BTreeSet<RecipeIdentity>,
    candidates: &mut Vec<StockCandidate>,
) {
    if let Some(lineage) = &value.lineage {
        collect_stock_candidates(lineage, produced, seen, candidates);
    }
    match &value.data {
        ValueData::List(values) => {
            for value in values.iter() {
                collect_value_stock_candidates(value, produced, seen, candidates);
            }
        }
        ValueData::Record(values) => {
            for value in values.values() {
                collect_value_stock_candidates(value, produced, seen, candidates);
            }
        }
        _ => {}
    }
}

fn collect_stock_candidates(
    lineage: &Lineage,
    produced: &BTreeMap<RecipeIdentity, OuterValue>,
    seen: &mut std::collections::BTreeSet<RecipeIdentity>,
    candidates: &mut Vec<StockCandidate>,
) {
    let LineageNode::Invocation(invocation) = lineage.node() else {
        return;
    };
    for argument in invocation.arguments.iter() {
        if let Some(parent) = &argument.lineage {
            collect_stock_candidates(parent, produced, seen, candidates);
        }
    }
    if !seen.insert(invocation.recipe_id) {
        return;
    }
    let Some(value) = produced.get(&invocation.recipe_id) else {
        return;
    };
    if !matches!(value.data, ValueData::Bytes(_) | ValueData::Buffer(_)) {
        return;
    }
    candidates.push(StockCandidate {
        transform_name: invocation.transform_name.to_string(),
        value: value.clone().with_lineage(lineage.clone()),
    });
}

impl ResultCache for WorkspaceResultCache<'_> {
    fn remember(&mut self, value: &OuterValue) -> Result<ContentIdentity, CacheError> {
        self.memory.remember(value)
    }

    fn lookup(&mut self, recipe: RecipeIdentity) -> Result<Option<OuterValue>, CacheError> {
        let before = self.memory.stats();
        if let Some(value) = self.memory.lookup(recipe)? {
            self.stats.hits += 1;
            self.stats.invalidations += self
                .memory
                .stats()
                .invalidations
                .saturating_sub(before.invalidations);
            return Ok(Some(value));
        }
        self.stats.invalidations += self
            .memory
            .stats()
            .invalidations
            .saturating_sub(before.invalidations);
        match self
            .workspace
            .cached_value(recipe)
            .map_err(|error| CacheError::storage(error.to_string()))?
        {
            Some(value) => {
                self.memory.store(recipe, &value)?;
                self.stats.hits += 1;
                Ok(Some(value))
            }
            None => {
                self.stats.misses += 1;
                Ok(None)
            }
        }
    }

    fn store(
        &mut self,
        recipe: RecipeIdentity,
        value: &OuterValue,
    ) -> Result<ContentIdentity, CacheError> {
        let before = self.memory.stats().stores;
        let identity = self.memory.store(recipe, value)?;
        self.produced
            .entry(recipe)
            .or_insert_with(|| OuterValue::plain(value.data.clone()));
        self.stats.stores += self.memory.stats().stores.saturating_sub(before);
        Ok(identity)
    }

    fn materialized(
        &mut self,
        identity: ContentIdentity,
    ) -> Result<Option<OuterValue>, CacheError> {
        if let Some(value) = self.memory.content().get(identity) {
            return Ok(Some(value));
        }
        let value = self
            .workspace
            .typed_value(identity)
            .map_err(|error| CacheError::storage(error.to_string()))?;
        if let Some(value) = &value {
            self.memory.remember(value)?;
        }
        Ok(value)
    }
}

impl fmt::Display for RunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => error.fmt(formatter),
            Self::Runtime(diagnostics) => write!(
                formatter,
                "Tima execution failed with {} diagnostic(s)",
                diagnostics.len()
            ),
            Self::InvalidPipeline(message) => formatter.write_str(message),
        }
    }
}

impl StdError for RunError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            Self::Runtime(_) | Self::InvalidPipeline(_) => None,
        }
    }
}
