pub mod cache;
pub mod cranelift;
pub mod native;

use crate::diagnostic::Diagnostic;
use crate::ir::TypedModule;

/// A backend representation produced from backend-neutral typed IR.
///
/// Semantic transform identity belongs to the IR. Backend, target, compiler,
/// optimization settings, and `abi_version` belong to artifact identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendArtifact {
    pub backend: &'static str,
    pub backend_version: &'static str,
    pub compiler_version: &'static str,
    pub target: String,
    pub cpu_features: Vec<String>,
    pub optimization: &'static str,
    pub abi_version: u32,
    pub bytes: Vec<u8>,
    /// Bytes occupied by immutable data segments before dynamic allocations.
    pub static_size: u64,
}

pub trait ArtifactBackend {
    fn emit(&self, module: &TypedModule) -> Result<BackendArtifact, Vec<Diagnostic>>;
}
