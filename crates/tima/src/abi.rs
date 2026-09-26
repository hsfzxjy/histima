use crate::ir::Type;

/// Increment this when the generated-transform C boundary changes incompatibly.
/// It is intentionally separate from language and backend versions so it can
/// later participate in native artifact identity.
pub const TIMA_ABI_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ownership {
    Scalar,
    Owned,
    ReadOnlyView,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbiType {
    pub c_name: &'static str,
    pub ownership: Ownership,
}

pub fn lower_type(ty: Type) -> AbiType {
    match ty {
        Type::Bool => AbiType {
            c_name: "bool",
            ownership: Ownership::Scalar,
        },
        Type::I64 => AbiType {
            c_name: "int64_t",
            ownership: Ownership::Scalar,
        },
        Type::F32 => AbiType {
            c_name: "float",
            ownership: Ownership::Scalar,
        },
        Type::Image => AbiType {
            c_name: "TimaImage",
            ownership: Ownership::Owned,
        },
        Type::ImageView => AbiType {
            c_name: "TimaImageView",
            ownership: Ownership::ReadOnlyView,
        },
    }
}
