use std::error::Error as StdError;
use std::fmt;

use tima::CompiledProgram;
use tima::backend::NativeBackend;
use tima::backend::c::CBackend;
use tima::backend::native::{
    ClangCompiler, NativeBuildError, NativeCacheStatus, NativeLoadError, NativeModule,
};
use tima::cache::{CacheStats, TransformResultCache};
use tima::diagnostic::Diagnostic;
use tima::runtime::Execution;

use crate::Workspace;

/// The observable result of one Tima program execution in a Histima workspace.
///
/// Transform-result caching is intentionally process-local at this stage. The
/// native artifact status reports the separate durable workspace cache.
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
        let mut result_cache = TransformResultCache::default();
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
            result_cache: result_cache.stats(),
        })
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
