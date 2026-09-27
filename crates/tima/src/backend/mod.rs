pub mod c;
pub mod native;

use crate::diagnostic::Diagnostic;
use crate::ir::TypedModule;

/// A backend representation produced from backend-neutral typed IR.
///
/// Semantic transform identity belongs to the IR. Backend, target, compiler,
/// optimization settings, and `abi_version` belong to artifact identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeArtifact {
    pub backend: &'static str,
    pub backend_version: &'static str,
    pub abi_version: u32,
    pub source: String,
}

pub trait NativeBackend {
    fn emit(&self, module: &TypedModule) -> Result<NativeArtifact, Vec<Diagnostic>>;
}
