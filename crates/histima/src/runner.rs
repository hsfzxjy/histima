use std::error::Error as StdError;
use std::fmt;

use tima::CompiledProgram;
use tima::backend::NativeBackend;
use tima::backend::c::CBackend;
use tima::backend::native::{
    ClangCompiler, NativeBuildError, NativeCacheStatus, NativeLoadError, NativeModule,
};
use tima::cache::{CacheError, CacheStats, ResultCache, TransformResultCache};
use tima::diagnostic::Diagnostic;
use tima::identity::{ContentIdentity, RecipeIdentity};
use tima::runtime::{Execution, OuterValue};

use crate::Workspace;

/// The observable result of one Tima program execution in a Histima workspace.
///
/// Result-cache statistics include both process-local and validated durable
/// workspace lookups. The native artifact status reports its separate cache.
#[derive(Debug)]
pub struct ProgramExecution {
    pub execution: Execution,
    pub native_cache: NativeCacheStatus,
    pub result_cache: CacheStats,
}

#[derive(Debug)]
pub enum RunError {
    CodeGeneration(Vec<Diagnostic>),
    NativeBuild(NativeBuildError),
    NativeLoad(NativeLoadError),
    Runtime(Vec<Diagnostic>),
}

impl Workspace {
    /// Executes checked Tima through the generated-C backend with this
    /// workspace as the only host capability provider.
    pub fn execute(&self, program: &CompiledProgram) -> Result<ProgramExecution, RunError> {
        let generated = CBackend
            .emit(&program.transforms)
            .map_err(RunError::CodeGeneration)?;
        let transform_ids = program.identities.iter().collect::<Vec<_>>();
        let cached_artifact = ClangCompiler::default()
            .compile_cached(&generated, &transform_ids, self.native_cache_root())
            .map_err(RunError::NativeBuild)?;
        let native = NativeModule::load(&cached_artifact.artifact, &program.transforms)
            .map_err(RunError::NativeLoad)?;
        let mut result_cache = WorkspaceResultCache::new(self);
        let execution = tima::runtime::execute_native_cached_with_capabilities(
            program,
            &native,
            &mut result_cache,
            self,
        )
        .map_err(RunError::Runtime)?;
        Ok(ProgramExecution {
            execution,
            native_cache: cached_artifact.status,
            result_cache: result_cache.stats,
        })
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
            Self::CodeGeneration(diagnostics) => write!(
                formatter,
                "generated-C code generation failed with {} diagnostic(s)",
                diagnostics.len()
            ),
            Self::NativeBuild(error) => error.fmt(formatter),
            Self::NativeLoad(error) => {
                write!(
                    formatter,
                    "could not load native transform artifact: {error}"
                )
            }
            Self::Runtime(diagnostics) => write!(
                formatter,
                "Tima execution failed with {} diagnostic(s)",
                diagnostics.len()
            ),
        }
    }
}

impl StdError for RunError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::NativeBuild(error) => Some(error),
            Self::NativeLoad(error) => Some(error),
            Self::CodeGeneration(_) | Self::Runtime(_) => None,
        }
    }
}
