use crate::ir::Type;

/// ABI epoch for core WebAssembly modules using imported memory64 linear
/// memory. C ABI versions used by older artifacts occupy a different backend
/// namespace and are intentionally not continued here.
pub const TIMA_ABI_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ownership {
    Scalar,
    Owned,
    ReadOnlyView,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbiType {
    pub ownership: Ownership,
}

pub fn lower_type(ty: Type) -> AbiType {
    match ty {
        Type::Bool => AbiType {
            ownership: Ownership::Scalar,
        },
        Type::U8 => AbiType {
            ownership: Ownership::Scalar,
        },
        Type::I64 => AbiType {
            ownership: Ownership::Scalar,
        },
        Type::F32 => AbiType {
            ownership: Ownership::Scalar,
        },
        Type::Image => AbiType {
            ownership: Ownership::Owned,
        },
        Type::ImageView => AbiType {
            ownership: Ownership::ReadOnlyView,
        },
    }
}
