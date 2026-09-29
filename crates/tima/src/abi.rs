use crate::ir::Type;

/// ABI epoch for ahead-of-time native artifacts.
pub const TIMA_ABI_VERSION: u32 = 2;

pub const ABI_VALUE_WORDS: usize = 8;
pub const ABI_VALUE_BYTES: usize = ABI_VALUE_WORDS * size_of::<u64>();

/// Fixed-width call-boundary storage. Statically known signature types decide
/// which words are meaningful; this is an ABI carrier, not a dynamic object.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AbiValue {
    pub words: [u64; ABI_VALUE_WORDS],
}

pub const ABI_POINTER_WORD: usize = 0;
pub const ABI_LENGTH_WORD: usize = 1;
pub const ABI_CAPACITY_WORD: usize = 2;
pub const ABI_IMAGE_FORMAT_WORD: usize = 3;
pub const ABI_IMAGE_WIDTH_WORD: usize = 4;
pub const ABI_IMAGE_HEIGHT_WORD: usize = 5;
pub const ABI_IMAGE_STRIDE_WORD: usize = 6;

pub const ABI_IMAGE_FORMAT_OPAQUE_BYTES: u32 = 0;
pub const ABI_IMAGE_FORMAT_RGBA8: u32 = 1;

pub const ABI_STATUS_OK: i32 = 0;
pub const ABI_STATUS_IMAGE_FORMAT: i32 = 1;

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
