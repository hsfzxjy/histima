use crate::ast::BinaryOp;
use crate::source::Span;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TransformId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ValueId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StructId(pub u32);

/// Types represented by the backend-neutral inner semantic IR.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Type {
    Bool,
    U8,
    I64,
    F32,
    /// Uniquely owned UTF-8 storage.
    String,
    /// Read-only UTF-8 storage that may alias.
    StringView,
    /// Uniquely owned byte storage.
    Bytes,
    /// Read-only byte storage that may alias.
    BytesView,
    /// Uniquely owned shaped byte storage, mutable while a transform runs.
    Buffer,
    /// Read-only shaped byte storage, permitted to alias other views.
    BufferView,
    /// Immutable transform-local value with a statically declared field layout.
    Struct(StructId),
}

impl Type {
    pub fn name(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::U8 => "u8",
            Self::I64 => "i64",
            Self::F32 => "f32",
            Self::String => "String",
            Self::StringView => "StringView",
            Self::Bytes => "Bytes",
            Self::BytesView => "BytesView",
            Self::Buffer => "Buffer",
            Self::BufferView => "BufferView",
            Self::Struct(_) => "struct",
        }
    }

    pub fn is_numeric(self) -> bool {
        matches!(self, Self::I64 | Self::F32)
    }

    pub fn is_owned(self) -> bool {
        matches!(self, Self::String | Self::Bytes | Self::Buffer)
    }
}

#[derive(Clone, Debug, Default)]
pub struct TypedModule {
    pub structs: Vec<StructType>,
    pub transforms: Vec<Transform>,
}

impl TypedModule {
    pub fn get(&self, id: TransformId) -> &Transform {
        &self.transforms[id.0 as usize]
    }

    pub fn find(&self, name: &str) -> Option<(TransformId, &Transform)> {
        self.transforms
            .iter()
            .enumerate()
            .find(|(_, transform)| transform.name == name)
            .map(|(index, transform)| (TransformId(index as u32), transform))
    }

    pub fn struct_type(&self, id: StructId) -> &StructType {
        &self.structs[id.0 as usize]
    }

    pub fn find_struct(&self, name: &str) -> Option<(StructId, &StructType)> {
        self.structs
            .iter()
            .enumerate()
            .find(|(_, definition)| definition.name == name)
            .map(|(index, definition)| (StructId(index as u32), definition))
    }
}

#[derive(Clone, Debug)]
pub struct StructType {
    pub name: String,
    pub fields: Vec<StructField>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct StructField {
    pub name: String,
    pub ty: Type,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Transform {
    pub name: String,
    pub parameters: Vec<Parameter>,
    pub return_type: Type,
    pub capabilities: Vec<Capability>,
    pub values: Vec<Value>,
    pub blocks: Vec<BasicBlock>,
    pub entry: BlockId,
    pub span: Span,
}

/// Broad external read authorities declared by an inner transform. Precise
/// resource keys and observed content identities are recorded at runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Capability {
    EnvironmentRead,
    FileRead,
    HttpGet,
}

impl Capability {
    pub const fn name(self) -> &'static str {
        match self {
            Self::EnvironmentRead => "env.read",
            Self::FileRead => "file.read",
            Self::HttpGet => "http.get",
        }
    }
}

impl Transform {
    pub fn value(&self, id: ValueId) -> &Value {
        &self.values[id.0 as usize]
    }
}

#[derive(Clone, Debug)]
pub struct Parameter {
    pub name: String,
    pub ty: Type,
    pub value: ValueId,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Value {
    pub ty: Type,
    pub kind: ValueKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum ValueKind {
    Parameter {
        index: u32,
    },
    Constant(Constant),
    Binary {
        op: BinaryOp,
        left: ValueId,
        right: ValueId,
    },
    /// Multiplies a byte by an `f32`, then truncates and saturates to `u8`.
    U8Scale {
        value: ValueId,
        factor: ValueId,
    },
    /// Calls another checked transform. Arguments with owned types are
    /// consumed; their type makes the ownership transfer backend-neutral.
    Call {
        transform: TransformId,
        arguments: Vec<ValueId>,
    },
    /// Consumes one uniquely owned buffer, zeros its byte storage in place, and
    /// produces the same owned storage under a new value ID.
    BufferZero {
        buffer: ValueId,
    },
    /// Consumes one uniquely owned buffer, fills its byte storage in place, and
    /// produces the same owned storage under a new value ID.
    BufferFill {
        buffer: ValueId,
        value: ValueId,
    },
    /// Per-iteration `u8` value supplied by an enclosing `BufferByteMap`.
    /// It is not an ordinary block instruction or a native boundary value.
    BufferByteElement,
    /// Zero-based storage-byte offset supplied by an indexed
    /// `BufferByteMap`. Padding bytes participate in this index.
    BufferByteIndex,
    /// Consumes one uniquely owned buffer and evaluates a scalar instruction
    /// sequence once per byte, storing the `u8` result back into that byte.
    BufferByteMap {
        buffer: ValueId,
        element: ValueId,
        index: Option<ValueId>,
        instructions: Vec<ValueId>,
        result: ValueId,
    },
    /// Constructs an immutable struct value. Field values are in declaration
    /// order, independent of source constructor argument order.
    StructConstruct {
        struct_id: StructId,
        fields: Vec<ValueId>,
    },
    /// Projects one field by declaration-order index.
    StructField {
        value: ValueId,
        field: u32,
    },
    RuntimeCall(RuntimeCall),
}

/// Backend-neutral calls into the narrow Histima runtime capability boundary.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RuntimeCall {
    EnvironmentI64 { name: String },
    EnvironmentRead { name: ValueId },
    FileRead { path: ValueId },
    HttpGet { url: ValueId },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Constant {
    Bool(bool),
    I64(i64),
    F32(f32),
    String(String),
}

#[derive(Clone, Debug)]
pub struct BasicBlock {
    /// Values are listed in evaluation order. Backends need not infer order
    /// from arena allocation, leaving room for later CFG transformations.
    pub instructions: Vec<ValueId>,
    pub terminator: Terminator,
}

#[derive(Clone, Debug)]
pub enum Terminator {
    Return(ValueId),
    Jump(BlockId),
    Branch {
        condition: ValueId,
        then_block: BlockId,
        else_block: BlockId,
    },
}
