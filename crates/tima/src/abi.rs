use std::ffi::c_void;

use crate::ir::Type;

/// ABI epoch for ahead-of-time native artifacts.
pub const TIMA_ABI_VERSION: u32 = 3;

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
pub const ABI_BUFFER_RANK_WORD: usize = 3;
pub const ABI_BUFFER_DIMENSION_0_WORD: usize = 4;
pub const ABI_BUFFER_DIMENSION_1_WORD: usize = 5;
pub const ABI_BUFFER_DIMENSION_2_WORD: usize = 6;
pub const ABI_BUFFER_OUTER_STRIDE_WORD: usize = 7;

pub const ABI_STATUS_OK: i32 = 0;
pub const ABI_STATUS_BUFFER_LAYOUT: i32 = 1;
pub const ABI_STATUS_RUNTIME: i32 = 2;

pub const ABI_ALLOCATION_STRING: u32 = 1;
pub const ABI_ALLOCATION_BYTES: u32 = 2;
pub const ABI_ALLOCATION_BUFFER: u32 = 3;

pub const ABI_WORLD_ENVIRONMENT_READ: u32 = 1;
pub const ABI_WORLD_FILE_READ: u32 = 2;
pub const ABI_WORLD_HTTP_GET: u32 = 3;

pub const ABI_FAILURE_CHECKED_INTEGER: u32 = 1;

pub type AbiAllocateFn = unsafe extern "C" fn(
    user_data: *mut c_void,
    kind: u32,
    length: u64,
    result: *mut AbiValue,
) -> i32;

pub type AbiWorldCallFn = unsafe extern "C" fn(
    user_data: *mut c_void,
    callsite: u64,
    operation: u32,
    key: *const u8,
    key_length: u64,
    result: *mut AbiValue,
) -> i32;

pub type AbiFailureFn =
    unsafe extern "C" fn(user_data: *mut c_void, callsite: u64, failure: u32) -> i32;

/// Host callbacks available to generated native transforms. Native code sees
/// only this C-compatible table and opaque user data, never Rust runtime
/// objects or outer values.
#[repr(C)]
pub struct AbiRuntimeContext {
    pub user_data: *mut c_void,
    pub allocate: AbiAllocateFn,
    pub world_call: AbiWorldCallFn,
    pub failure: AbiFailureFn,
}

pub const ABI_RUNTIME_USER_DATA_OFFSET: i32 =
    std::mem::offset_of!(AbiRuntimeContext, user_data) as i32;
pub const ABI_RUNTIME_ALLOCATE_OFFSET: i32 =
    std::mem::offset_of!(AbiRuntimeContext, allocate) as i32;
pub const ABI_RUNTIME_WORLD_CALL_OFFSET: i32 =
    std::mem::offset_of!(AbiRuntimeContext, world_call) as i32;
pub const ABI_RUNTIME_FAILURE_OFFSET: i32 = std::mem::offset_of!(AbiRuntimeContext, failure) as i32;

pub const fn abi_callsite(transform: u32, value: u32) -> u64 {
    ((transform as u64) << 32) | value as u64
}

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
        Type::String | Type::Bytes | Type::Buffer => AbiType {
            ownership: Ownership::Owned,
        },
        Type::StringView | Type::BytesView | Type::BufferView => AbiType {
            ownership: Ownership::ReadOnlyView,
        },
    }
}
