use crate::ir::Type;

/// ABI epoch for ahead-of-time native artifacts.
pub const TIMA_ABI_VERSION: u32 = 1;

/// One scalar call-boundary slot. Statically known signature types determine
/// how its low bits are interpreted; dynamic outer tags never enter native
/// transform code.
pub type AbiSlot = u64;

pub const ABI_STATUS_OK: i32 = 0;

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
        Type::String | Type::Bytes | Type::Image => AbiType {
            ownership: Ownership::Owned,
        },
        Type::StringView | Type::BytesView | Type::ImageView => AbiType {
            ownership: Ownership::ReadOnlyView,
        },
    }
}
