use std::error::Error as StdError;
use std::fmt;

use tima::CompiledProgram;
use tima::ast::Item;
use tima::backend::ArtifactBackend;
use tima::backend::wasm::WasmBackend;
use tima::backend::wasm_runtime::{ArtifactCacheStatus, WasmArtifactCache, WasmError, WasmSession};
use tima::cache::{CacheError, CacheStats, ResultCache, TransformResultCache};
use tima::diagnostic::Diagnostic;
use tima::identity::{ContentIdentity, RecipeIdentity};
use tima::runtime::{Execution, OuterValue};

use crate::{ArtifactInfo, Workspace};

/// The observable result of one Tima program execution in a Histima workspace.
///
/// Result-cache statistics include both process-local and validated durable
/// workspace lookups. The portable artifact status reports its separate cache.
#[derive(Debug)]
pub struct ProgramExecution {
    pub execution: Execution,
    pub artifact_cache: ArtifactCacheStatus,
    pub artifact: ArtifactInfo,
    pub result_cache: CacheStats,
}

/// The result of replaying one durable semantic recipe.
#[derive(Debug)]
pub struct RecipeReplay {
    pub value: OuterValue,
    pub artifact_cache: ArtifactCacheStatus,
    pub artifact: ArtifactInfo,
    pub result_cache: CacheStats,
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
    CodeGeneration(Vec<Diagnostic>),
    Artifact(WasmError),
    Runtime(Vec<Diagnostic>),
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
        let execution =
            tima::runtime::execute_cached_with_capabilities(program, &mut result_cache, self)
                .map_err(RunError::Runtime)?;
        let value = execution.last_value.ok_or_else(|| {
            RunError::InvalidPipeline("the pipeline expression produced no value".to_owned())
        })?;
        Ok(PipelineExecution {
            value,
            result_cache: result_cache.stats,
        })
    }

    /// Executes checked Tima through the WebAssembly backend with this
    /// workspace as the only host capability provider.
    pub fn execute(&self, program: &CompiledProgram) -> Result<ProgramExecution, RunError> {
        let (artifact_cache, artifact, wasm) = self.load_wasm(program)?;
        let mut result_cache = WorkspaceResultCache::new(self);
        let execution = tima::runtime::execute_wasm_cached_with_capabilities(
            program,
            &wasm,
            &mut result_cache,
            self,
        )
        .map_err(RunError::Runtime)?;
        Ok(ProgramExecution {
            execution,
            artifact_cache,
            artifact,
            result_cache: result_cache.stats,
        })
    }

    /// Replays a durable recipe against current transform definitions and
    /// observed dependencies. Durable cached intermediates remain eligible,
    /// but only after the complete recorded lineage has been validated.
    pub fn replay_recipe(
        &self,
        program: &CompiledProgram,
        recipe: RecipeIdentity,
    ) -> Result<RecipeReplay, RunError> {
        let target = self.replay_target(recipe).map_err(RunError::Storage)?;
        let (artifact_cache, artifact, wasm) = self.load_wasm(program)?;
        let mut result_cache = WorkspaceResultCache::new(self);
        let value = tima::runtime::replay_wasm_with_capabilities(
            program,
            &wasm,
            &target,
            &mut result_cache,
            self,
        )
        .map_err(|diagnostic| RunError::Runtime(vec![diagnostic]))?;
        Ok(RecipeReplay {
            value,
            artifact_cache,
            artifact,
            result_cache: result_cache.stats,
        })
    }

    fn load_wasm(
        &self,
        program: &CompiledProgram,
    ) -> Result<(ArtifactCacheStatus, ArtifactInfo, WasmSession), RunError> {
        let generated = WasmBackend
            .emit(&program.transforms)
            .map_err(RunError::CodeGeneration)?;
        let transform_ids = program.identities.iter().collect::<Vec<_>>();
        let cached_artifact = WasmArtifactCache
            .store(&generated, &transform_ids, self.artifact_cache_root())
            .map_err(RunError::Artifact)?;
        let artifact_info = self
            .catalog
            .record_artifact(&cached_artifact, &transform_ids, self.root())
            .map_err(RunError::Storage)?;
        let wasm = WasmSession::instantiate(
            &cached_artifact.artifact,
            &program.transforms,
            self.wasm_memory_limit(),
        )
        .map_err(RunError::Artifact)?;
        Ok((cached_artifact.status, artifact_info, wasm))
    }
}

struct WorkspaceResultCache<'workspace> {
    workspace: &'workspace Workspace,
    memory: TransformResultCache,
    stats: CacheStats,
}

impl<'workspace> WorkspaceResultCache<'workspace> {
    fn new(workspace: &'workspace Workspace) -> Self {
        Self {
            workspace,
            memory: TransformResultCache::default(),
            stats: CacheStats::default(),
        }
    }
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
            Self::CodeGeneration(diagnostics) => write!(
                formatter,
                "WebAssembly code generation failed with {} diagnostic(s)",
                diagnostics.len()
            ),
            Self::Artifact(error) => error.fmt(formatter),
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
            Self::Artifact(error) => Some(error),
            Self::CodeGeneration(_) | Self::Runtime(_) | Self::InvalidPipeline(_) => None,
        }
    }
}
