//! Canonical semantic IR used only to derive Transform IDs.
//!
//! Version 1 is frozen. Its node tags and canonical byte encoding are part of
//! Tima's Transform-ID compatibility contract. Incompatible semantic changes
//! require a new format version; artifact/backend details and source-facing
//! names or spans do not belong here.

use crate::ast::BinaryOp as AstBinaryOp;
use crate::identity::TransformIdentity;
use crate::ir::{
    Capability as IrCapability, Constant as IrConstant, RuntimeCall as IrRuntimeCall,
    Terminator as IrTerminator, Transform as IrTransform, Type as IrType, ValueKind as IrValueKind,
};

pub const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Definition {
    Tima(TimaTransform),
    External(ExternalTransform),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimaTransform {
    pub capabilities: Vec<Capability>,
    pub parameters: Vec<Type>,
    pub result: Type,
    pub values: Vec<Value>,
    pub blocks: Vec<Block>,
    pub entry: u32,
}

/// A non-Tima semantic operation. `scheme` identifies how its version and
/// opaque contract bytes are interpreted; it is not an execution backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalTransform {
    pub scheme: String,
    pub name: String,
    pub semantic_version: u32,
    pub interface_version: Option<u32>,
    pub parameters: Vec<ExternalParameter>,
    pub result_type: Option<u8>,
}

/// A scheme-defined external parameter. The type code is interpreted by the
/// named scheme; this keeps external ABI epochs out of the Tima type enum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalParameter {
    pub name: String,
    pub type_code: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capability {
    EnvironmentRead,
    FileRead,
    HttpGet,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    Bool,
    U8,
    I64,
    F32,
    String,
    StringView,
    Bytes,
    BytesView,
    Buffer,
    BufferView,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Value {
    pub ty: Type,
    pub kind: ValueKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValueKind {
    Parameter(u32),
    Constant(Constant),
    Binary {
        op: BinaryOp,
        left: u32,
        right: u32,
    },
    Call {
        transform: TransformIdentity,
        arguments: Vec<u32>,
    },
    RuntimeCall(RuntimeCall),
    BufferZero(u32),
    BufferFill {
        buffer: u32,
        value: u32,
    },
    BufferByteElement,
    BufferByteMap {
        buffer: u32,
        element: u32,
        instructions: Vec<u32>,
        result: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Constant {
    Bool(bool),
    I64(i64),
    F32Bits(u32),
    String(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeCall {
    EnvironmentI64(String),
    EnvironmentRead(u32),
    FileRead(u32),
    HttpGet(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub instructions: Vec<u32>,
    pub terminator: Terminator,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Terminator {
    Return(u32),
    Branch {
        condition: u32,
        then_block: u32,
        else_block: u32,
    },
    Jump(u32),
}

impl Definition {
    /// Returns the canonical Hash IR v1 encoding.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut encoder = Encoder::default();
        encoder.raw(b"TIMA-HASH-IR\0");
        encoder.u32(FORMAT_VERSION);
        match self {
            Self::Tima(transform) => {
                encoder.u8(0);
                transform.encode(&mut encoder);
            }
            Self::External(transform) => {
                encoder.u8(1);
                transform.encode(&mut encoder);
            }
        }
        encoder.bytes
    }
}

impl TimaTransform {
    fn encode(&self, encoder: &mut Encoder) {
        let mut capabilities = self.capabilities.clone();
        capabilities.sort_unstable();
        capabilities.dedup();
        encoder.sequence(&capabilities, |encoder, capability| {
            encoder.u8(match capability {
                Capability::EnvironmentRead => 0,
                Capability::FileRead => 1,
                Capability::HttpGet => 2,
            });
        });
        encoder.sequence(&self.parameters, |encoder, ty| ty.encode(encoder));
        self.result.encode(encoder);
        encoder.sequence(&self.values, |encoder, value| value.encode(encoder));
        encoder.sequence(&self.blocks, |encoder, block| block.encode(encoder));
        encoder.u32(self.entry);
    }
}

impl ExternalTransform {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.text(&self.scheme);
        encoder.text(&self.name);
        encoder.u32(self.semantic_version);
        match self.interface_version {
            Some(version) => {
                encoder.u8(1);
                encoder.u32(version);
            }
            None => encoder.u8(0),
        }
        encoder.sequence(&self.parameters, |encoder, parameter| {
            encoder.text(&parameter.name);
            encoder.u8(parameter.type_code);
        });
        match self.result_type {
            Some(result) => {
                encoder.u8(1);
                encoder.u8(result);
            }
            None => encoder.u8(0),
        }
    }
}

impl Type {
    fn encode(self, encoder: &mut Encoder) {
        encoder.u8(match self {
            Self::Bool => 0,
            Self::U8 => 1,
            Self::I64 => 2,
            Self::F32 => 3,
            Self::String => 4,
            Self::StringView => 5,
            Self::Bytes => 6,
            Self::BytesView => 7,
            Self::Buffer => 8,
            Self::BufferView => 9,
        });
    }
}

impl Value {
    fn encode(&self, encoder: &mut Encoder) {
        self.ty.encode(encoder);
        match &self.kind {
            ValueKind::Parameter(index) => {
                encoder.u8(0);
                encoder.u32(*index);
            }
            ValueKind::Constant(constant) => {
                encoder.u8(1);
                constant.encode(encoder);
            }
            ValueKind::Binary { op, left, right } => {
                encoder.u8(2);
                op.encode(encoder);
                encoder.u32(*left);
                encoder.u32(*right);
            }
            ValueKind::Call {
                transform,
                arguments,
            } => {
                encoder.u8(3);
                encoder.raw(transform.as_bytes());
                encoder.sequence(arguments, |encoder, value| encoder.u32(*value));
            }
            ValueKind::RuntimeCall(call) => {
                encoder.u8(4);
                call.encode(encoder);
            }
            ValueKind::BufferZero(buffer) => {
                encoder.u8(5);
                encoder.u32(*buffer);
            }
            ValueKind::BufferFill { buffer, value } => {
                encoder.u8(6);
                encoder.u32(*buffer);
                encoder.u32(*value);
            }
            ValueKind::BufferByteElement => encoder.u8(7),
            ValueKind::BufferByteMap {
                buffer,
                element,
                instructions,
                result,
            } => {
                encoder.u8(8);
                encoder.u32(*buffer);
                encoder.u32(*element);
                encoder.sequence(instructions, |encoder, value| encoder.u32(*value));
                encoder.u32(*result);
            }
        }
    }
}

impl Constant {
    fn encode(&self, encoder: &mut Encoder) {
        match self {
            Self::Bool(value) => {
                encoder.u8(0);
                encoder.u8(u8::from(*value));
            }
            Self::I64(value) => {
                encoder.u8(1);
                encoder.i64(*value);
            }
            Self::F32Bits(value) => {
                encoder.u8(2);
                encoder.u32(*value);
            }
            Self::String(value) => {
                encoder.u8(3);
                encoder.text(value);
            }
        }
    }
}

impl BinaryOp {
    fn encode(self, encoder: &mut Encoder) {
        encoder.u8(match self {
            Self::Add => 0,
            Self::Subtract => 1,
            Self::Multiply => 2,
            Self::Divide => 3,
            Self::Equal => 4,
            Self::NotEqual => 5,
            Self::Less => 6,
            Self::LessEqual => 7,
            Self::Greater => 8,
            Self::GreaterEqual => 9,
        });
    }
}

impl RuntimeCall {
    fn encode(&self, encoder: &mut Encoder) {
        match self {
            Self::EnvironmentI64(name) => {
                encoder.u8(0);
                encoder.text(name);
            }
            Self::EnvironmentRead(name) => {
                encoder.u8(1);
                encoder.u32(*name);
            }
            Self::FileRead(path) => {
                encoder.u8(2);
                encoder.u32(*path);
            }
            Self::HttpGet(url) => {
                encoder.u8(3);
                encoder.u32(*url);
            }
        }
    }
}

impl Block {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.sequence(&self.instructions, |encoder, value| encoder.u32(*value));
        match self.terminator {
            Terminator::Return(value) => {
                encoder.u8(0);
                encoder.u32(value);
            }
            Terminator::Branch {
                condition,
                then_block,
                else_block,
            } => {
                encoder.u8(1);
                encoder.u32(condition);
                encoder.u32(then_block);
                encoder.u32(else_block);
            }
            Terminator::Jump(target) => {
                encoder.u8(2);
                encoder.u32(target);
            }
        }
    }
}

/// Lowers typed Tima IR into the hash-only representation. `references` is
/// parallel to the typed value arena and contains resolved IDs for call nodes.
pub(crate) fn lower_tima(
    transform: &IrTransform,
    references: &[Option<TransformIdentity>],
) -> Definition {
    assert_eq!(transform.values.len(), references.len());
    Definition::Tima(TimaTransform {
        capabilities: transform
            .capabilities
            .iter()
            .map(|capability| match capability {
                IrCapability::EnvironmentRead => Capability::EnvironmentRead,
                IrCapability::FileRead => Capability::FileRead,
                IrCapability::HttpGet => Capability::HttpGet,
            })
            .collect(),
        parameters: transform
            .parameters
            .iter()
            .map(|parameter| Type::from(parameter.ty))
            .collect(),
        result: Type::from(transform.return_type),
        values: transform
            .values
            .iter()
            .zip(references)
            .map(|(value, reference)| Value {
                ty: Type::from(value.ty),
                kind: lower_value(&value.kind, *reference),
            })
            .collect(),
        blocks: transform
            .blocks
            .iter()
            .map(|block| Block {
                instructions: block.instructions.iter().map(|value| value.0).collect(),
                terminator: match block.terminator {
                    IrTerminator::Return(value) => Terminator::Return(value.0),
                    IrTerminator::Branch {
                        condition,
                        then_block,
                        else_block,
                    } => Terminator::Branch {
                        condition: condition.0,
                        then_block: then_block.0,
                        else_block: else_block.0,
                    },
                    IrTerminator::Jump(target) => Terminator::Jump(target.0),
                },
            })
            .collect(),
        entry: transform.entry.0,
    })
}

pub(crate) fn external(scheme: &str, name: &str, semantic_version: u32) -> Definition {
    Definition::External(ExternalTransform {
        scheme: scheme.to_owned(),
        name: name.to_owned(),
        semantic_version,
        interface_version: None,
        parameters: Vec::new(),
        result_type: None,
    })
}

pub(crate) fn registered_wasm_external(
    name: &str,
    semantic_version: u32,
    abi_version: u32,
    parameters: &[(&str, u8)],
    result: u8,
) -> Definition {
    Definition::External(ExternalTransform {
        scheme: "tima.registered-wasm-contract".to_owned(),
        name: name.to_owned(),
        semantic_version,
        interface_version: Some(abi_version),
        parameters: parameters
            .iter()
            .map(|(name, type_code)| ExternalParameter {
                name: (*name).to_owned(),
                type_code: *type_code,
            })
            .collect(),
        result_type: Some(result),
    })
}

fn lower_value(kind: &IrValueKind, reference: Option<TransformIdentity>) -> ValueKind {
    match kind {
        IrValueKind::Parameter { index } => ValueKind::Parameter(*index),
        IrValueKind::Constant(constant) => ValueKind::Constant(match constant {
            IrConstant::Bool(value) => Constant::Bool(*value),
            IrConstant::I64(value) => Constant::I64(*value),
            IrConstant::F32(value) => Constant::F32Bits(value.to_bits()),
            IrConstant::String(value) => Constant::String(value.clone()),
        }),
        IrValueKind::Binary { op, left, right } => ValueKind::Binary {
            op: BinaryOp::from(*op),
            left: left.0,
            right: right.0,
        },
        IrValueKind::Call { arguments, .. } => ValueKind::Call {
            transform: reference.expect("call values have a resolved Transform ID"),
            arguments: arguments.iter().map(|value| value.0).collect(),
        },
        IrValueKind::RuntimeCall(call) => ValueKind::RuntimeCall(match call {
            IrRuntimeCall::EnvironmentI64 { name } => RuntimeCall::EnvironmentI64(name.clone()),
            IrRuntimeCall::EnvironmentRead { name } => RuntimeCall::EnvironmentRead(name.0),
            IrRuntimeCall::FileRead { path } => RuntimeCall::FileRead(path.0),
            IrRuntimeCall::HttpGet { url } => RuntimeCall::HttpGet(url.0),
        }),
        IrValueKind::BufferZero { buffer } => ValueKind::BufferZero(buffer.0),
        IrValueKind::BufferFill { buffer, value } => ValueKind::BufferFill {
            buffer: buffer.0,
            value: value.0,
        },
        IrValueKind::BufferByteElement => ValueKind::BufferByteElement,
        IrValueKind::BufferByteMap {
            buffer,
            element,
            instructions,
            result,
        } => ValueKind::BufferByteMap {
            buffer: buffer.0,
            element: element.0,
            instructions: instructions.iter().map(|value| value.0).collect(),
            result: result.0,
        },
    }
}

impl From<IrType> for Type {
    fn from(value: IrType) -> Self {
        match value {
            IrType::Bool => Self::Bool,
            IrType::U8 => Self::U8,
            IrType::I64 => Self::I64,
            IrType::F32 => Self::F32,
            IrType::String => Self::String,
            IrType::StringView => Self::StringView,
            IrType::Bytes => Self::Bytes,
            IrType::BytesView => Self::BytesView,
            IrType::Buffer => Self::Buffer,
            IrType::BufferView => Self::BufferView,
        }
    }
}

impl From<AstBinaryOp> for BinaryOp {
    fn from(value: AstBinaryOp) -> Self {
        match value {
            AstBinaryOp::Add => Self::Add,
            AstBinaryOp::Subtract => Self::Subtract,
            AstBinaryOp::Multiply => Self::Multiply,
            AstBinaryOp::Divide => Self::Divide,
            AstBinaryOp::Equal => Self::Equal,
            AstBinaryOp::NotEqual => Self::NotEqual,
            AstBinaryOp::Less => Self::Less,
            AstBinaryOp::LessEqual => Self::LessEqual,
            AstBinaryOp::Greater => Self::Greater,
            AstBinaryOp::GreaterEqual => Self::GreaterEqual,
        }
    }
}

#[derive(Default)]
struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    fn raw(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    fn data(&mut self, value: &[u8]) {
        self.u32(length(value.len()));
        self.raw(value);
    }

    fn text(&mut self, value: &str) {
        self.data(value.as_bytes());
    }

    fn sequence<T>(&mut self, values: &[T], encode: impl Fn(&mut Self, &T)) {
        self.u32(length(values.len()));
        for value in values {
            encode(self, value);
        }
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.raw(&value.to_le_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.raw(&value.to_le_bytes());
    }
}

fn length(value: usize) -> u32 {
    u32::try_from(value).expect("Hash IR collections fit in u32 arenas")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_encoding_is_self_identifying_and_length_delimited() {
        let first = external("a", "bc", 1);
        let second = external("ab", "c", 1);

        assert_ne!(first.canonical_bytes(), second.canonical_bytes());
        assert!(first.canonical_bytes().starts_with(b"TIMA-HASH-IR\0"));
        assert_eq!(
            first
                .canonical_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            "54494d412d484153482d4952000100000001010000006102000000626301000000000000000000"
        );
    }
}
