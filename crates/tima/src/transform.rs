//! Public, implementation-neutral descriptions of callable transforms.

use crate::identity::{ArtifactIdentity, ContentIdentity, TransformIdentity};
use crate::ir::{Capability, Type};

/// Where a transform definition entered the current callable namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransformOrigin {
    Source,
    Standard,
    Workspace,
}

impl TransformOrigin {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Standard => "standard",
            Self::Workspace => "workspace",
        }
    }
}

/// The semantic implementation family behind a transform.
///
/// Native compilation of Tima code is an execution choice and does not change
/// this value. A registered Wasm module remains distinct from its semantic
/// transform identity and artifact identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransformImplementation {
    Tima,
    RegisteredWasm,
}

impl TransformImplementation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tima => "tima",
            Self::RegisteredWasm => "registered-wasm",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransformDefaultValue {
    Integer(i64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransformParameterInfo {
    pub name: String,
    pub value_type: Type,
    pub default: Option<TransformDefaultValue>,
}

/// One common description for source Tima, standard, and workspace transforms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransformInfo {
    pub name: String,
    pub origin: TransformOrigin,
    pub implementation: TransformImplementation,
    pub semantic_version: Option<u32>,
    pub parameters: Vec<TransformParameterInfo>,
    pub result: Type,
    pub capabilities: Vec<Capability>,
    pub transform_id: TransformIdentity,
    pub abi_version: Option<u32>,
    pub artifact_id: Option<ArtifactIdentity>,
    pub module_content_id: Option<ContentIdentity>,
}

impl TransformInfo {
    pub fn signature(&self) -> String {
        let parameters = self
            .parameters
            .iter()
            .map(|parameter| {
                let value_type = parameter.value_type.name();
                match parameter.default {
                    Some(TransformDefaultValue::Integer(value)) => {
                        format!("{}: {value_type} = {value}", parameter.name)
                    }
                    None => format!("{}: {value_type}", parameter.name),
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("{}({parameters}) -> {}", self.name, self.result.name())
    }
}
