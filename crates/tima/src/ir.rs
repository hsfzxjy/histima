use crate::ast::BinaryOp;
use crate::source::Span;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TransformId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ValueId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockId(pub u32);

/// Types admitted by the native transform boundary in the first milestone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Type {
    Bool,
    U8,
    I64,
    F32,
    /// Uniquely owned and mutable while an inner transform runs.
    Image,
    /// Read-only and permitted to alias other views.
    ImageView,
}

impl Type {
    pub fn name(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::U8 => "u8",
            Self::I64 => "i64",
            Self::F32 => "f32",
            Self::Image => "Image",
            Self::ImageView => "ImageView",
        }
    }

    pub fn is_numeric(self) -> bool {
        matches!(self, Self::I64 | Self::F32)
    }
}

#[derive(Clone, Debug, Default)]
pub struct TypedModule {
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
}

#[derive(Clone, Debug)]
pub struct Transform {
    pub name: String,
    pub parameters: Vec<Parameter>,
    pub return_type: Type,
    pub values: Vec<Value>,
    pub blocks: Vec<BasicBlock>,
    pub entry: BlockId,
    pub span: Span,
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
    /// Calls another checked transform. Arguments with owned types are
    /// consumed; their type makes the ownership transfer backend-neutral.
    Call {
        transform: TransformId,
        arguments: Vec<ValueId>,
    },
    /// Consumes one uniquely owned image, zeros its byte storage in place, and
    /// produces the same owned storage under a new value ID.
    ImageZero {
        image: ValueId,
    },
    /// Consumes one uniquely owned image, fills its byte storage in place, and
    /// produces the same owned storage under a new value ID.
    ImageFill {
        image: ValueId,
        value: ValueId,
    },
    RuntimeCall(RuntimeCall),
}

/// Backend-neutral calls into the narrow Histima runtime capability boundary.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RuntimeCall {
    EnvironmentI64 { name: String },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Constant {
    Bool(bool),
    I64(i64),
    F32(f32),
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
