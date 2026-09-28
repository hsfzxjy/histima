use std::collections::BTreeMap;

use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, DataSection, EntityType, ExportKind, ExportSection,
    Function, FunctionSection, GlobalSection, GlobalType, ImportSection, Instruction, MemArg,
    MemoryType, Module, TypeSection, ValType,
};

use crate::abi::TIMA_ABI_VERSION;
use crate::ast::BinaryOp;
use crate::backend::{ArtifactBackend, BackendArtifact};
use crate::diagnostic::Diagnostic;
use crate::ir::{
    Constant, RuntimeCall, Terminator, Transform, Type, TypedModule, ValueId, ValueKind,
};

pub const WASM_BACKEND_VERSION: &str = "1";
pub const WASM_TARGET: &str = "wasm64-unknown-unknown";

const ENVIRONMENT_I64_IMPORT: u32 = 0;
const RUNTIME_ERROR_IMPORT: u32 = 1;
const IMPORTED_FUNCTIONS: u32 = 2;
const STATIC_BASE_GLOBAL: u32 = 0;
const ABI_VERSION_GLOBAL: u32 = 1;

#[derive(Clone, Copy, Debug, Default)]
pub struct WasmBackend;

impl ArtifactBackend for WasmBackend {
    fn emit(&self, module: &TypedModule) -> Result<BackendArtifact, Vec<Diagnostic>> {
        let bytes = emit_module(module);
        Ok(BackendArtifact {
            backend: "wasm",
            backend_version: WASM_BACKEND_VERSION,
            abi_version: TIMA_ABI_VERSION,
            bytes,
            static_size: static_size(module),
        })
    }
}

fn static_size(module: &TypedModule) -> u64 {
    module
        .transforms
        .iter()
        .flat_map(|transform| &transform.values)
        .filter_map(|value| match &value.kind {
            ValueKind::RuntimeCall(RuntimeCall::EnvironmentI64 { name }) => Some(name.len() as u64),
            _ => None,
        })
        .sum()
}

fn emit_module(module: &TypedModule) -> Vec<u8> {
    let mut wasm = Module::new();
    let mut types = TypeSection::new();
    types.ty().function(
        [ValType::I32, ValType::I32, ValType::I64, ValType::I64],
        [ValType::I64],
    );
    types
        .ty()
        .function([ValType::I32, ValType::I32, ValType::I32], []);
    for transform in &module.transforms {
        let parameters = transform
            .parameters
            .iter()
            .flat_map(|parameter| wasm_types(parameter.ty))
            .collect::<Vec<_>>();
        types
            .ty()
            .function(parameters, wasm_types(transform.return_type));
    }
    wasm.section(&types);

    let mut imports = ImportSection::new();
    imports.import("tima", "environment_i64", EntityType::Function(0));
    imports.import("tima", "runtime_error", EntityType::Function(1));
    imports.import(
        "tima",
        "memory",
        EntityType::Memory(MemoryType {
            minimum: 0,
            maximum: None,
            memory64: true,
            shared: false,
            page_size_log2: None,
        }),
    );
    imports.import(
        "tima",
        "static_base",
        EntityType::Global(GlobalType {
            val_type: ValType::I64,
            mutable: false,
            shared: false,
        }),
    );
    wasm.section(&imports);

    let mut functions = FunctionSection::new();
    for index in 0..module.transforms.len() {
        functions.function(2 + index as u32);
    }
    wasm.section(&functions);

    let mut globals = GlobalSection::new();
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: false,
            shared: false,
        },
        &ConstExpr::i32_const(TIMA_ABI_VERSION as i32),
    );
    wasm.section(&globals);

    let mut exports = ExportSection::new();
    exports.export("tima_abi_version", ExportKind::Global, ABI_VERSION_GLOBAL);
    for index in 0..module.transforms.len() {
        exports.export(
            &format!("tima_invoke_{index}"),
            ExportKind::Func,
            IMPORTED_FUNCTIONS + index as u32,
        );
    }
    wasm.section(&exports);

    let static_layout = StaticLayout::new(module);
    let mut code = CodeSection::new();
    for (index, transform) in module.transforms.iter().enumerate() {
        code.function(&emit_transform(transform, index as u32, &static_layout));
    }
    wasm.section(&code);

    if !static_layout.bytes.is_empty() {
        let mut data = DataSection::new();
        data.active(
            0,
            &ConstExpr::global_get(STATIC_BASE_GLOBAL),
            static_layout.bytes.iter().copied(),
        );
        wasm.section(&data);
    }

    wasm.finish()
}

fn wasm_types(ty: Type) -> Vec<ValType> {
    let values: &[ValType] = match ty {
        Type::Bool | Type::U8 => &[ValType::I32],
        Type::I64 => &[ValType::I64],
        Type::F32 => &[ValType::F32],
        Type::Image | Type::ImageView => &[
            ValType::I64,
            ValType::I64,
            ValType::I64,
            ValType::I64,
            ValType::I64,
            ValType::I32,
        ],
    };
    values.to_vec()
}

struct StaticLayout {
    bytes: Vec<u8>,
    environment_names: BTreeMap<(u32, u32), u64>,
}

impl StaticLayout {
    fn new(module: &TypedModule) -> Self {
        let mut bytes = Vec::new();
        let mut environment_names = BTreeMap::new();
        for (transform_index, transform) in module.transforms.iter().enumerate() {
            for (value_index, value) in transform.values.iter().enumerate() {
                let ValueKind::RuntimeCall(RuntimeCall::EnvironmentI64 { name }) = &value.kind
                else {
                    continue;
                };
                let offset = bytes.len() as u64;
                bytes.extend_from_slice(name.as_bytes());
                environment_names.insert((transform_index as u32, value_index as u32), offset);
            }
        }
        Self {
            bytes,
            environment_names,
        }
    }
}

#[derive(Clone)]
struct ValueLocals {
    indices: Vec<u32>,
}

struct FunctionLayout {
    values: Vec<ValueLocals>,
    local_types: Vec<ValType>,
    pc: u32,
    index: u32,
    end: u32,
    x: u32,
    y: u32,
    address: u32,
    scaled: u32,
}

impl FunctionLayout {
    fn new(transform: &Transform) -> Self {
        let mut next = 0_u32;
        let mut values = Vec::with_capacity(transform.values.len());
        for value in &transform.values {
            if let ValueKind::Parameter { .. } = value.kind {
                let count = wasm_types(value.ty).len() as u32;
                values.push(ValueLocals {
                    indices: (next..next + count).collect(),
                });
                next += count;
            } else {
                values.push(ValueLocals {
                    indices: Vec::new(),
                });
            }
        }
        let parameter_count = next;
        let mut local_types = Vec::new();
        for (value, locals) in transform.values.iter().zip(&mut values) {
            if matches!(value.kind, ValueKind::Parameter { .. }) {
                continue;
            }
            for ty in wasm_types(value.ty) {
                locals.indices.push(next);
                local_types.push(ty);
                next += 1;
            }
        }
        let mut scratch = |ty| {
            let index = next;
            next += 1;
            local_types.push(ty);
            index
        };
        let pc = scratch(ValType::I32);
        let index = scratch(ValType::I64);
        let end = scratch(ValType::I64);
        let x = scratch(ValType::I64);
        let y = scratch(ValType::I64);
        let address = scratch(ValType::I64);
        let scaled = scratch(ValType::F32);
        debug_assert_eq!(parameter_count + local_types.len() as u32, next);
        Self {
            values,
            local_types,
            pc,
            index,
            end,
            x,
            y,
            address,
            scaled,
        }
    }

    fn value(&self, id: ValueId) -> &[u32] {
        &self.values[id.0 as usize].indices
    }
}

fn emit_transform(
    transform: &Transform,
    transform_index: u32,
    static_layout: &StaticLayout,
) -> Function {
    let layout = FunctionLayout::new(transform);
    let mut function = Function::new(layout.local_types.iter().copied().map(|ty| (1, ty)));
    function.instruction(&Instruction::I32Const(transform.entry.0 as i32));
    function.instruction(&Instruction::LocalSet(layout.pc));
    function.instruction(&Instruction::Loop(BlockType::Empty));
    for (block_index, block) in transform.blocks.iter().enumerate() {
        function.instruction(&Instruction::LocalGet(layout.pc));
        function.instruction(&Instruction::I32Const(block_index as i32));
        function.instruction(&Instruction::I32Eq);
        function.instruction(&Instruction::If(BlockType::Empty));
        for id in &block.instructions {
            emit_instruction(
                &mut function,
                transform,
                transform_index,
                *id,
                &layout,
                static_layout,
            );
        }
        match block.terminator {
            Terminator::Return(value) => {
                for local in layout.value(value) {
                    function.instruction(&Instruction::LocalGet(*local));
                }
                function.instruction(&Instruction::Return);
            }
            Terminator::Jump(target) => {
                function.instruction(&Instruction::I32Const(target.0 as i32));
                function.instruction(&Instruction::LocalSet(layout.pc));
                function.instruction(&Instruction::Br(1));
            }
            Terminator::Branch {
                condition,
                then_block,
                else_block,
            } => {
                function.instruction(&Instruction::LocalGet(layout.value(condition)[0]));
                function.instruction(&Instruction::If(BlockType::Empty));
                function.instruction(&Instruction::I32Const(then_block.0 as i32));
                function.instruction(&Instruction::LocalSet(layout.pc));
                function.instruction(&Instruction::Else);
                function.instruction(&Instruction::I32Const(else_block.0 as i32));
                function.instruction(&Instruction::LocalSet(layout.pc));
                function.instruction(&Instruction::End);
                function.instruction(&Instruction::Br(1));
            }
        }
        function.instruction(&Instruction::End);
    }
    function.instruction(&Instruction::Unreachable);
    function.instruction(&Instruction::End);
    function.instruction(&Instruction::Unreachable);
    function.instruction(&Instruction::End);
    function
}

fn emit_instruction(
    function: &mut Function,
    transform: &Transform,
    transform_index: u32,
    id: ValueId,
    layout: &FunctionLayout,
    static_layout: &StaticLayout,
) {
    match &transform.value(id).kind {
        ValueKind::ImageZero { image } => {
            copy_image_locals(function, layout.value(*image), layout.value(id));
            emit_fill_loop(function, layout, id, None);
        }
        ValueKind::ImageFill { image, value } => {
            copy_image_locals(function, layout.value(*image), layout.value(id));
            emit_fill_loop(function, layout, id, Some(layout.value(*value)[0]));
        }
        ValueKind::ImageByteMap {
            image,
            element,
            instructions,
            result,
        } => {
            copy_image_locals(function, layout.value(*image), layout.value(id));
            function.instruction(&Instruction::I64Const(0));
            function.instruction(&Instruction::LocalSet(layout.index));
            function.instruction(&Instruction::LocalGet(layout.value(id)[1]));
            function.instruction(&Instruction::LocalSet(layout.end));
            function.instruction(&Instruction::Block(BlockType::Empty));
            function.instruction(&Instruction::Loop(BlockType::Empty));
            emit_loop_exit(function, layout);
            emit_byte_address(function, layout, id);
            function.instruction(&Instruction::I32Load8U(memarg()));
            function.instruction(&Instruction::LocalSet(layout.value(*element)[0]));
            for instruction in instructions {
                emit_instruction(
                    function,
                    transform,
                    transform_index,
                    *instruction,
                    layout,
                    static_layout,
                );
            }
            emit_byte_address(function, layout, id);
            function.instruction(&Instruction::LocalGet(layout.value(*result)[0]));
            function.instruction(&Instruction::I32Store8(memarg()));
            emit_loop_continue(function, layout);
            function.instruction(&Instruction::End);
            function.instruction(&Instruction::End);
        }
        ValueKind::ImageRgba8Scale { image, channels } => {
            copy_image_locals(function, layout.value(*image), layout.value(id));
            function.instruction(&Instruction::LocalGet(layout.value(id)[5]));
            function.instruction(&Instruction::I32Const(1));
            function.instruction(&Instruction::I32Ne);
            function.instruction(&Instruction::If(BlockType::Empty));
            function.instruction(&Instruction::I32Const(1));
            function.instruction(&Instruction::I32Const(transform_index as i32));
            function.instruction(&Instruction::I32Const(id.0 as i32));
            function.instruction(&Instruction::Call(RUNTIME_ERROR_IMPORT));
            function.instruction(&Instruction::Unreachable);
            function.instruction(&Instruction::End);
            function.instruction(&Instruction::I64Const(0));
            function.instruction(&Instruction::LocalSet(layout.y));
            function.instruction(&Instruction::Block(BlockType::Empty));
            function.instruction(&Instruction::Loop(BlockType::Empty));
            function.instruction(&Instruction::LocalGet(layout.y));
            function.instruction(&Instruction::LocalGet(layout.value(id)[3]));
            function.instruction(&Instruction::I64GeU);
            function.instruction(&Instruction::BrIf(1));
            function.instruction(&Instruction::I64Const(0));
            function.instruction(&Instruction::LocalSet(layout.x));
            function.instruction(&Instruction::Block(BlockType::Empty));
            function.instruction(&Instruction::Loop(BlockType::Empty));
            function.instruction(&Instruction::LocalGet(layout.x));
            function.instruction(&Instruction::LocalGet(layout.value(id)[2]));
            function.instruction(&Instruction::I64GeU);
            function.instruction(&Instruction::BrIf(1));
            for (channel, factor) in channels {
                function.instruction(&Instruction::LocalGet(layout.value(id)[0]));
                function.instruction(&Instruction::LocalGet(layout.y));
                function.instruction(&Instruction::LocalGet(layout.value(id)[4]));
                function.instruction(&Instruction::I64Mul);
                function.instruction(&Instruction::I64Add);
                function.instruction(&Instruction::LocalGet(layout.x));
                function.instruction(&Instruction::I64Const(4));
                function.instruction(&Instruction::I64Mul);
                function.instruction(&Instruction::I64Add);
                function.instruction(&Instruction::I64Const(channel.offset() as i64));
                function.instruction(&Instruction::I64Add);
                function.instruction(&Instruction::LocalTee(layout.address));
                function.instruction(&Instruction::I32Load8U(memarg()));
                function.instruction(&Instruction::F32ConvertI32U);
                function.instruction(&Instruction::LocalGet(layout.value(*factor)[0]));
                function.instruction(&Instruction::F32Mul);
                function.instruction(&Instruction::LocalSet(layout.scaled));
                function.instruction(&Instruction::LocalGet(layout.address));
                emit_scaled_channel(function, layout.scaled);
                function.instruction(&Instruction::I32Store8(memarg()));
            }
            function.instruction(&Instruction::LocalGet(layout.x));
            function.instruction(&Instruction::I64Const(1));
            function.instruction(&Instruction::I64Add);
            function.instruction(&Instruction::LocalSet(layout.x));
            function.instruction(&Instruction::Br(0));
            function.instruction(&Instruction::End);
            function.instruction(&Instruction::End);
            function.instruction(&Instruction::LocalGet(layout.y));
            function.instruction(&Instruction::I64Const(1));
            function.instruction(&Instruction::I64Add);
            function.instruction(&Instruction::LocalSet(layout.y));
            function.instruction(&Instruction::Br(0));
            function.instruction(&Instruction::End);
            function.instruction(&Instruction::End);
        }
        _ => emit_simple_value(
            function,
            transform,
            transform_index,
            id,
            layout,
            static_layout,
        ),
    }
}

fn emit_simple_value(
    function: &mut Function,
    transform: &Transform,
    transform_index: u32,
    id: ValueId,
    layout: &FunctionLayout,
    static_layout: &StaticLayout,
) {
    let value = transform.value(id);
    match &value.kind {
        ValueKind::Parameter { .. } | ValueKind::ImageByteElement => return,
        ValueKind::Constant(Constant::Bool(value)) => {
            function.instruction(&Instruction::I32Const(i32::from(*value)));
        }
        ValueKind::Constant(Constant::I64(value)) => {
            function.instruction(&Instruction::I64Const(*value));
        }
        ValueKind::Constant(Constant::F32(value)) => {
            function.instruction(&Instruction::F32Const((*value).into()));
        }
        ValueKind::Binary { op, left, right } => {
            function.instruction(&Instruction::LocalGet(layout.value(*left)[0]));
            function.instruction(&Instruction::LocalGet(layout.value(*right)[0]));
            function.instruction(&binary_instruction(*op, transform.value(*left).ty));
        }
        ValueKind::Call {
            transform: callee,
            arguments,
        } => {
            for argument in arguments {
                for local in layout.value(*argument) {
                    function.instruction(&Instruction::LocalGet(*local));
                }
            }
            function.instruction(&Instruction::Call(IMPORTED_FUNCTIONS + callee.0));
        }
        ValueKind::RuntimeCall(RuntimeCall::EnvironmentI64 { name }) => {
            let offset = static_layout.environment_names[&(transform_index, id.0)];
            function.instruction(&Instruction::I32Const(transform_index as i32));
            function.instruction(&Instruction::I32Const(id.0 as i32));
            function.instruction(&Instruction::GlobalGet(STATIC_BASE_GLOBAL));
            function.instruction(&Instruction::I64Const(offset as i64));
            function.instruction(&Instruction::I64Add);
            function.instruction(&Instruction::I64Const(name.len() as i64));
            function.instruction(&Instruction::Call(ENVIRONMENT_I64_IMPORT));
        }
        ValueKind::ImageZero { .. }
        | ValueKind::ImageFill { .. }
        | ValueKind::ImageByteMap { .. }
        | ValueKind::ImageRgba8Scale { .. } => unreachable!(),
    }
    for local in layout.value(id).iter().rev() {
        function.instruction(&Instruction::LocalSet(*local));
    }
}

fn binary_instruction(op: BinaryOp, operand: Type) -> Instruction<'static> {
    match (operand, op) {
        (Type::F32, BinaryOp::Add) => Instruction::F32Add,
        (Type::F32, BinaryOp::Subtract) => Instruction::F32Sub,
        (Type::F32, BinaryOp::Multiply) => Instruction::F32Mul,
        (Type::F32, BinaryOp::Divide) => Instruction::F32Div,
        (Type::F32, BinaryOp::Equal) => Instruction::F32Eq,
        (Type::F32, BinaryOp::NotEqual) => Instruction::F32Ne,
        (Type::F32, BinaryOp::Less) => Instruction::F32Lt,
        (Type::F32, BinaryOp::LessEqual) => Instruction::F32Le,
        (Type::F32, BinaryOp::Greater) => Instruction::F32Gt,
        (Type::F32, BinaryOp::GreaterEqual) => Instruction::F32Ge,
        (Type::I64, BinaryOp::Add) => Instruction::I64Add,
        (Type::I64, BinaryOp::Subtract) => Instruction::I64Sub,
        (Type::I64, BinaryOp::Multiply) => Instruction::I64Mul,
        (Type::I64, BinaryOp::Divide) => Instruction::I64DivS,
        (Type::I64, BinaryOp::Equal) => Instruction::I64Eq,
        (Type::I64, BinaryOp::NotEqual) => Instruction::I64Ne,
        (Type::I64, BinaryOp::Less) => Instruction::I64LtS,
        (Type::I64, BinaryOp::LessEqual) => Instruction::I64LeS,
        (Type::I64, BinaryOp::Greater) => Instruction::I64GtS,
        (Type::I64, BinaryOp::GreaterEqual) => Instruction::I64GeS,
        (Type::Bool | Type::U8, BinaryOp::Equal) => Instruction::I32Eq,
        (Type::Bool | Type::U8, BinaryOp::NotEqual) => Instruction::I32Ne,
        (Type::U8, BinaryOp::Less) => Instruction::I32LtU,
        (Type::U8, BinaryOp::LessEqual) => Instruction::I32LeU,
        (Type::U8, BinaryOp::Greater) => Instruction::I32GtU,
        (Type::U8, BinaryOp::GreaterEqual) => Instruction::I32GeU,
        _ => unreachable!("semantic checking admits only typed binary operations"),
    }
}

fn copy_image_locals(function: &mut Function, source: &[u32], destination: &[u32]) {
    for (source, destination) in source.iter().zip(destination) {
        function.instruction(&Instruction::LocalGet(*source));
        function.instruction(&Instruction::LocalSet(*destination));
    }
}

fn emit_fill_loop(
    function: &mut Function,
    layout: &FunctionLayout,
    image: ValueId,
    fill: Option<u32>,
) {
    function.instruction(&Instruction::I64Const(0));
    function.instruction(&Instruction::LocalSet(layout.index));
    function.instruction(&Instruction::LocalGet(layout.value(image)[1]));
    function.instruction(&Instruction::LocalSet(layout.end));
    function.instruction(&Instruction::Block(BlockType::Empty));
    function.instruction(&Instruction::Loop(BlockType::Empty));
    emit_loop_exit(function, layout);
    emit_byte_address(function, layout, image);
    match fill {
        Some(local) => function.instruction(&Instruction::LocalGet(local)),
        None => function.instruction(&Instruction::I32Const(0)),
    };
    function.instruction(&Instruction::I32Store8(memarg()));
    emit_loop_continue(function, layout);
    function.instruction(&Instruction::End);
    function.instruction(&Instruction::End);
}

fn emit_loop_exit(function: &mut Function, layout: &FunctionLayout) {
    function.instruction(&Instruction::LocalGet(layout.index));
    function.instruction(&Instruction::LocalGet(layout.end));
    function.instruction(&Instruction::I64GeU);
    function.instruction(&Instruction::BrIf(1));
}

fn emit_loop_continue(function: &mut Function, layout: &FunctionLayout) {
    function.instruction(&Instruction::LocalGet(layout.index));
    function.instruction(&Instruction::I64Const(1));
    function.instruction(&Instruction::I64Add);
    function.instruction(&Instruction::LocalSet(layout.index));
    function.instruction(&Instruction::Br(0));
}

fn emit_byte_address(function: &mut Function, layout: &FunctionLayout, image: ValueId) {
    function.instruction(&Instruction::LocalGet(layout.value(image)[0]));
    function.instruction(&Instruction::LocalGet(layout.index));
    function.instruction(&Instruction::I64Add);
}

fn emit_scaled_channel(function: &mut Function, scaled: u32) {
    function.instruction(&Instruction::LocalGet(scaled));
    function.instruction(&Instruction::F32Const(0.0_f32.into()));
    function.instruction(&Instruction::F32Gt);
    function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
    function.instruction(&Instruction::LocalGet(scaled));
    function.instruction(&Instruction::F32Const(255.0_f32.into()));
    function.instruction(&Instruction::F32Ge);
    function.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
    function.instruction(&Instruction::I32Const(255));
    function.instruction(&Instruction::Else);
    function.instruction(&Instruction::LocalGet(scaled));
    function.instruction(&Instruction::I32TruncSatF32U);
    function.instruction(&Instruction::End);
    function.instruction(&Instruction::Else);
    function.instruction(&Instruction::I32Const(0));
    function.instruction(&Instruction::End);
}

fn memarg() -> MemArg {
    MemArg {
        offset: 0,
        align: 0,
        memory_index: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_deterministic_memory64_module() {
        let compiled = crate::compile(
            "test.tima",
            "transform scale(x: f32, factor: f32) -> f32 { return x * factor }\n",
        )
        .unwrap();
        let first = WasmBackend.emit(&compiled.transforms).unwrap();
        let second = WasmBackend.emit(&compiled.transforms).unwrap();
        assert_eq!(first, second);
        assert_eq!(&first.bytes[..4], b"\0asm");
        wasmtime::Module::validate(&wasmtime::Engine::default(), &first.bytes).unwrap();
    }

    #[test]
    fn semantic_renames_do_not_change_the_portable_artifact() {
        let first = crate::compile(
            "first.tima",
            "transform scale(x: f32, factor: f32) -> f32 { return x * factor }\n",
        )
        .unwrap();
        let renamed = crate::compile(
            "renamed.tima",
            "transform shade(value: f32, amount: f32) -> f32 { return value * amount }\n",
        )
        .unwrap();

        let first = WasmBackend.emit(&first.transforms).unwrap();
        let renamed = WasmBackend.emit(&renamed.transforms).unwrap();
        assert_eq!(first.bytes, renamed.bytes);
    }
}
