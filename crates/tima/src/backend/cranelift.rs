use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::immediates::Ieee32;
use cranelift_codegen::ir::{
    AbiParam, Function, InstBuilder, MemFlagsData, Signature, UserFuncName, types,
};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use object::write::{Object, StandardSection, Symbol, SymbolSection};
use object::{Architecture, BinaryFormat, Endianness, SymbolFlags, SymbolKind, SymbolScope};

use crate::abi::{ABI_LENGTH_WORD, ABI_POINTER_WORD, ABI_VALUE_BYTES, TIMA_ABI_VERSION};
use crate::ast::BinaryOp;
use crate::backend::{ArtifactBackend, BackendArtifact};
use crate::diagnostic::Diagnostic;
use crate::ir::{Constant, Terminator, Transform, Type, TypedModule, ValueId, ValueKind};

pub const CRANELIFT_BACKEND_VERSION: &str = "3";
pub const CRANELIFT_OPTIMIZATION: &str = "speed";

/// Ahead-of-time native object generation from backend-neutral Tima IR.
///
/// The initial slice accepts leaf transforms over scalars plus owned/view
/// image identity, zero, and fill operations. Other typed IR is interpreted.
#[derive(Clone, Copy, Debug, Default)]
pub struct CraneliftBackend;

impl CraneliftBackend {
    pub fn supports_transform(transform: &Transform) -> bool {
        validate_transform(transform).is_empty()
    }
}

impl ArtifactBackend for CraneliftBackend {
    fn emit(&self, module: &TypedModule) -> Result<BackendArtifact, Vec<Diagnostic>> {
        validate_module(module)?;

        let mut flag_builder = settings::builder();
        flag_builder
            .set("opt_level", CRANELIFT_OPTIMIZATION)
            .map_err(|error| {
                vec![backend_error(format!(
                    "invalid Cranelift settings: {error}"
                ))]
            })?;
        let isa_builder = cranelift_native::builder().map_err(|error| {
            vec![backend_error(format!(
                "Cranelift does not support the host target: {error}"
            ))]
        })?;
        let isa = isa_builder
            .finish(settings::Flags::new(flag_builder))
            .map_err(|error| {
                vec![backend_error(format!(
                    "could not build Cranelift ISA: {error}"
                ))]
            })?;

        let mut object = Object::new(
            BinaryFormat::native_object(),
            host_object_architecture()?,
            Endianness::default(),
        );
        let text = object.section_id(StandardSection::Text);

        for (index, transform) in module.transforms.iter().enumerate() {
            let function = lower_transform(transform, index as u32, isa.as_ref())?;
            let mut context = cranelift_codegen::Context::for_function(function);
            let mut control = ControlPlane::default();
            let compiled = context
                .compile(isa.as_ref(), &mut control)
                .map_err(|error| {
                    vec![Diagnostic::error(
                        format!(
                            "Cranelift could not compile transform `{}`: {error:?}",
                            transform.name
                        ),
                        transform.span,
                    )]
                })?;
            if !compiled.buffer.relocs().is_empty() {
                return Err(vec![Diagnostic::error(
                    format!(
                        "Cranelift emitted unsupported relocations for leaf transform `{}`",
                        transform.name
                    ),
                    transform.span,
                )]);
            }
            let code = compiled.code_buffer();
            let offset = object.append_section_data(text, code, 16);
            object.add_symbol(Symbol {
                name: format!("tima_transform_{index}").into_bytes(),
                value: offset,
                size: code.len() as u64,
                kind: SymbolKind::Text,
                scope: SymbolScope::Linkage,
                weak: false,
                section: SymbolSection::Section(text),
                flags: SymbolFlags::None,
            });
        }

        let bytes = object.write().map_err(|error| {
            vec![backend_error(format!(
                "could not serialize Cranelift object: {error}"
            ))]
        })?;
        Ok(BackendArtifact {
            backend: "cranelift",
            backend_version: CRANELIFT_BACKEND_VERSION,
            compiler_version: cranelift_native::VERSION,
            target: target_lexicon::Triple::host().to_string(),
            cpu_features: isa
                .isa_flags()
                .into_iter()
                .map(|value| value.to_string())
                .collect(),
            optimization: CRANELIFT_OPTIMIZATION,
            abi_version: TIMA_ABI_VERSION,
            bytes,
            static_size: 0,
        })
    }
}

fn validate_module(module: &TypedModule) -> Result<(), Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();
    for transform in &module.transforms {
        diagnostics.extend(validate_transform(transform));
    }
    if diagnostics.is_empty() {
        Ok(())
    } else {
        Err(diagnostics)
    }
}

fn validate_transform(transform: &Transform) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    if !transform
        .parameters
        .iter()
        .all(|parameter| native_boundary_type(parameter.ty))
        || !native_boundary_type(transform.return_type)
    {
        diagnostics.push(
            Diagnostic::error(
                format!(
                    "Cranelift AOT does not yet support the boundary of transform `{}`",
                    transform.name
                ),
                transform.span,
            )
            .with_note("supported boundary types are bool, u8, i64, f32, Image, and ImageView"),
        );
        return diagnostics;
    }
    for value in &transform.values {
        match &value.kind {
            ValueKind::Parameter { .. }
            | ValueKind::Constant(Constant::Bool(_) | Constant::I64(_) | Constant::F32(_)) => {}
            ValueKind::Binary { op, left, .. }
                if value_type(transform, *left) == Type::I64 && op.is_arithmetic() =>
            {
                diagnostics.push(
                    Diagnostic::error(
                        "Cranelift AOT does not yet lower checked i64 arithmetic",
                        value.span,
                    )
                    .with_note("the backend must preserve Tima overflow and division diagnostics"),
                );
            }
            ValueKind::Binary { .. } => {}
            ValueKind::Call { .. } => diagnostics.push(
                Diagnostic::error(
                    "Cranelift AOT does not yet lower transform calls",
                    value.span,
                )
                .with_note("the first native artifact slice accepts leaf transforms only"),
            ),
            ValueKind::ImageZero { .. } | ValueKind::ImageFill { .. } => {}
            ValueKind::Constant(Constant::String(_))
            | ValueKind::ImageByteElement
            | ValueKind::ImageByteMap { .. }
            | ValueKind::ImageRgba8Scale { .. }
            | ValueKind::RuntimeCall(_) => diagnostics.push(Diagnostic::error(
                "operation is outside the initial Cranelift AOT subset",
                value.span,
            )),
        }
    }
    diagnostics
}

fn lower_transform(
    transform: &Transform,
    index: u32,
    isa: &dyn cranelift_codegen::isa::TargetIsa,
) -> Result<Function, Vec<Diagnostic>> {
    let mut signature = Signature::new(isa.default_call_conv());
    let pointer_type = isa.pointer_type();
    // Opaque runtime context, fixed-width argument descriptors, and one result descriptor.
    signature.params.extend([
        AbiParam::new(pointer_type),
        AbiParam::new(pointer_type),
        AbiParam::new(pointer_type),
    ]);
    signature.returns.push(AbiParam::new(types::I32));
    let mut function = Function::with_name_signature(UserFuncName::user(0, index), signature);
    let mut frontend = FunctionBuilderContext::new();
    {
        let mut builder = FunctionBuilder::new(&mut function, &mut frontend);
        let blocks = transform
            .blocks
            .iter()
            .map(|_| builder.create_block())
            .collect::<Vec<_>>();
        let entry = blocks[transform.entry.0 as usize];
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        let arguments = builder.block_params(entry)[1];
        let result = builder.block_params(entry)[2];
        let mut values = vec![None; transform.values.len()];
        for parameter in &transform.parameters {
            let ValueKind::Parameter { index } = transform.value(parameter.value).kind else {
                unreachable!("typed parameters reference parameter IR values")
            };
            let offset = i32::try_from(u64::from(index) * ABI_VALUE_BYTES as u64)
                .expect("Tima transform arguments fit a Cranelift offset");
            values[parameter.value.0 as usize] = Some(lower_parameter(
                &mut builder,
                arguments,
                parameter.ty,
                offset,
            ));
        }

        let mut order = Vec::with_capacity(blocks.len());
        order.push(transform.entry.0 as usize);
        order.extend((0..blocks.len()).filter(|index| *index != transform.entry.0 as usize));
        for block_index in order {
            if block_index != transform.entry.0 as usize {
                builder.switch_to_block(blocks[block_index]);
            }
            for id in &transform.blocks[block_index].instructions {
                let value = &transform.values[id.0 as usize];
                let lowered = match &value.kind {
                    ValueKind::Parameter { .. } => continue,
                    ValueKind::Constant(constant) => {
                        LoweredValue::Scalar(lower_constant(&mut builder, constant, value.ty))
                    }
                    ValueKind::Binary { op, left, right } => LoweredValue::Scalar(lower_binary(
                        &mut builder,
                        *op,
                        required_scalar(&values, *left),
                        required_scalar(&values, *right),
                        transform.value(*left).ty,
                    )),
                    ValueKind::ImageZero { image } => {
                        let image = required_image(&values, *image);
                        emit_image_fill(&mut builder, image, None);
                        LoweredValue::Image(image)
                    }
                    ValueKind::ImageFill { image, value } => {
                        let image = required_image(&values, *image);
                        emit_image_fill(
                            &mut builder,
                            image,
                            Some(required_scalar(&values, *value)),
                        );
                        LoweredValue::Image(image)
                    }
                    _ => unreachable!("validation rejects unsupported operations"),
                };
                values[id.0 as usize] = Some(lowered);
            }
            match transform.blocks[block_index].terminator {
                Terminator::Return(value) => {
                    store_result(&mut builder, result, required_value(&values, value));
                    let status = builder
                        .ins()
                        .iconst(types::I32, i64::from(crate::abi::ABI_STATUS_OK));
                    builder.ins().return_(&[status]);
                }
                Terminator::Jump(target) => {
                    builder.ins().jump(blocks[target.0 as usize], &[]);
                }
                Terminator::Branch {
                    condition,
                    then_block,
                    else_block,
                } => {
                    builder.ins().brif(
                        required_scalar(&values, condition),
                        blocks[then_block.0 as usize],
                        &[],
                        blocks[else_block.0 as usize],
                        &[],
                    );
                }
            }
        }
        builder.seal_all_blocks();
        builder.finalize(isa.frontend_config());
    }
    Ok(function)
}

#[derive(Clone, Copy)]
enum LoweredValue {
    Scalar(cranelift_codegen::ir::Value),
    Image([cranelift_codegen::ir::Value; 7]),
}

fn lower_parameter(
    builder: &mut FunctionBuilder<'_>,
    arguments: cranelift_codegen::ir::Value,
    ty: Type,
    base: i32,
) -> LoweredValue {
    if scalar_type(ty) {
        return LoweredValue::Scalar(builder.ins().load(
            clif_type(ty),
            MemFlagsData::new(),
            arguments,
            base,
        ));
    }
    debug_assert!(matches!(ty, Type::Image | Type::ImageView));
    LoweredValue::Image(std::array::from_fn(|word| {
        builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            arguments,
            base + i32::try_from(word * 8).unwrap(),
        )
    }))
}

fn store_result(
    builder: &mut FunctionBuilder<'_>,
    result: cranelift_codegen::ir::Value,
    value: LoweredValue,
) {
    match value {
        LoweredValue::Scalar(value) => {
            builder.ins().store(MemFlagsData::new(), value, result, 0);
        }
        LoweredValue::Image(words) => {
            for (word, value) in words.into_iter().enumerate() {
                builder.ins().store(
                    MemFlagsData::new(),
                    value,
                    result,
                    i32::try_from(word * 8).unwrap(),
                );
            }
        }
    }
}

fn emit_image_fill(
    builder: &mut FunctionBuilder<'_>,
    image: [cranelift_codegen::ir::Value; 7],
    fill: Option<cranelift_codegen::ir::Value>,
) {
    let header = builder.create_block();
    let body = builder.create_block();
    let done = builder.create_block();
    builder.append_block_param(header, types::I64);
    let zero = builder.ins().iconst(types::I64, 0);
    builder.ins().jump(header, &[zero.into()]);

    builder.switch_to_block(header);
    let index = builder.block_params(header)[0];
    let finished = builder.ins().icmp(
        IntCC::UnsignedGreaterThanOrEqual,
        index,
        image[ABI_LENGTH_WORD],
    );
    builder.ins().brif(finished, done, &[], body, &[]);

    builder.switch_to_block(body);
    let address = builder.ins().iadd(image[ABI_POINTER_WORD], index);
    let fill = fill.unwrap_or_else(|| builder.ins().iconst(types::I8, 0));
    builder.ins().store(MemFlagsData::new(), fill, address, 0);
    let one = builder.ins().iconst(types::I64, 1);
    let next = builder.ins().iadd(index, one);
    builder.ins().jump(header, &[next.into()]);

    builder.switch_to_block(done);
}

fn lower_constant(
    builder: &mut FunctionBuilder<'_>,
    constant: &Constant,
    ty: Type,
) -> cranelift_codegen::ir::Value {
    match constant {
        Constant::Bool(value) => builder.ins().iconst(types::I8, i64::from(*value)),
        Constant::I64(value) if ty == Type::U8 => builder.ins().iconst(types::I8, *value),
        Constant::I64(value) => builder.ins().iconst(types::I64, *value),
        Constant::F32(value) => builder.ins().f32const(Ieee32::with_bits(value.to_bits())),
        Constant::String(_) => unreachable!("validation rejects string constants"),
    }
}

fn lower_binary(
    builder: &mut FunctionBuilder<'_>,
    op: BinaryOp,
    left: cranelift_codegen::ir::Value,
    right: cranelift_codegen::ir::Value,
    ty: Type,
) -> cranelift_codegen::ir::Value {
    if ty == Type::F32 {
        return match op {
            BinaryOp::Add => builder.ins().fadd(left, right),
            BinaryOp::Subtract => builder.ins().fsub(left, right),
            BinaryOp::Multiply => builder.ins().fmul(left, right),
            BinaryOp::Divide => builder.ins().fdiv(left, right),
            _ => builder.ins().fcmp(float_condition(op), left, right),
        };
    }
    builder.ins().icmp(integer_condition(op, ty), left, right)
}

fn float_condition(op: BinaryOp) -> FloatCC {
    match op {
        BinaryOp::Equal => FloatCC::Equal,
        BinaryOp::NotEqual => FloatCC::NotEqual,
        BinaryOp::Less => FloatCC::LessThan,
        BinaryOp::LessEqual => FloatCC::LessThanOrEqual,
        BinaryOp::Greater => FloatCC::GreaterThan,
        BinaryOp::GreaterEqual => FloatCC::GreaterThanOrEqual,
        _ => unreachable!("arithmetic is lowered separately"),
    }
}

fn integer_condition(op: BinaryOp, ty: Type) -> IntCC {
    match op {
        BinaryOp::Equal => IntCC::Equal,
        BinaryOp::NotEqual => IntCC::NotEqual,
        BinaryOp::Less if ty == Type::U8 => IntCC::UnsignedLessThan,
        BinaryOp::LessEqual if ty == Type::U8 => IntCC::UnsignedLessThanOrEqual,
        BinaryOp::Greater if ty == Type::U8 => IntCC::UnsignedGreaterThan,
        BinaryOp::GreaterEqual if ty == Type::U8 => IntCC::UnsignedGreaterThanOrEqual,
        BinaryOp::Less => IntCC::SignedLessThan,
        BinaryOp::LessEqual => IntCC::SignedLessThanOrEqual,
        BinaryOp::Greater => IntCC::SignedGreaterThan,
        BinaryOp::GreaterEqual => IntCC::SignedGreaterThanOrEqual,
        _ => unreachable!("checked integer arithmetic is rejected by validation"),
    }
}

fn scalar_type(ty: Type) -> bool {
    matches!(ty, Type::Bool | Type::U8 | Type::I64 | Type::F32)
}

fn native_boundary_type(ty: Type) -> bool {
    scalar_type(ty) || matches!(ty, Type::Image | Type::ImageView)
}

fn clif_type(ty: Type) -> cranelift_codegen::ir::Type {
    match ty {
        Type::Bool | Type::U8 => types::I8,
        Type::I64 => types::I64,
        Type::F32 => types::F32,
        _ => unreachable!("validation rejects non-scalar boundary types"),
    }
}

fn value_type(transform: &Transform, id: ValueId) -> Type {
    transform.values[id.0 as usize].ty
}

fn required_value(values: &[Option<LoweredValue>], id: ValueId) -> LoweredValue {
    values[id.0 as usize].expect("typed IR values dominate their uses")
}

fn required_scalar(values: &[Option<LoweredValue>], id: ValueId) -> cranelift_codegen::ir::Value {
    let LoweredValue::Scalar(value) = required_value(values, id) else {
        unreachable!("typed scalar operation has a scalar operand")
    };
    value
}

fn required_image(
    values: &[Option<LoweredValue>],
    id: ValueId,
) -> [cranelift_codegen::ir::Value; 7] {
    let LoweredValue::Image(value) = required_value(values, id) else {
        unreachable!("typed image operation has an image operand")
    };
    value
}

fn host_object_architecture() -> Result<Architecture, Vec<Diagnostic>> {
    if !cfg!(target_endian = "little") {
        return Err(vec![backend_error(
            "the initial native value ABI supports little-endian hosts only",
        )]);
    }
    if cfg!(target_arch = "x86_64") {
        Ok(Architecture::X86_64)
    } else if cfg!(target_arch = "aarch64") {
        Ok(Architecture::Aarch64)
    } else {
        Err(vec![backend_error(
            "the initial Cranelift object writer supports x86-64 and AArch64 hosts",
        )])
    }
}

pub const fn host_object_file_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "module.obj"
    } else {
        "module.o"
    }
}

fn backend_error(message: impl Into<String>) -> Diagnostic {
    Diagnostic::error(message, crate::source::Span::default())
}

#[cfg(test)]
mod tests {
    use object::{Object as _, ObjectSymbol as _};

    use super::CraneliftBackend;
    use crate::backend::ArtifactBackend;

    #[test]
    fn emits_deterministic_host_objects_for_leaf_scalar_transforms() {
        let compiled = crate::compile(
            "scalar.tima",
            "transform choose(value: f32, threshold: f32) -> f32 {\n\
                 if value < threshold { return threshold } else { return value * 0.5 }\n\
             }\n",
        )
        .unwrap();
        let first = CraneliftBackend.emit(&compiled.transforms).unwrap();
        let second = CraneliftBackend.emit(&compiled.transforms).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.backend, "cranelift");
        assert_eq!(first.target, target_lexicon::Triple::host().to_string());

        let object = object::File::parse(first.bytes.as_slice()).unwrap();
        let symbols = object
            .symbols()
            .filter_map(|symbol| symbol.name().ok())
            .collect::<Vec<_>>();
        assert!(symbols.contains(&"tima_transform_0"));

        let renamed = crate::compile(
            "renamed.tima",
            "transform select_renamed(input: f32, limit: f32) -> f32 {\n\
                 if input < limit { return limit } else { return input * 0.5 }\n\
             }\n",
        )
        .unwrap();
        assert_eq!(
            first.bytes,
            CraneliftBackend.emit(&renamed.transforms).unwrap().bytes
        );
    }

    #[test]
    fn rejects_semantics_not_yet_preserved_by_the_native_backend() {
        let checked_integer = crate::compile(
            "integer.tima",
            "transform add(left: i64, right: i64) -> i64 { return left + right }\n",
        )
        .unwrap();
        let diagnostics = CraneliftBackend
            .emit(&checked_integer.transforms)
            .unwrap_err();
        assert!(diagnostics[0].message.contains("checked i64 arithmetic"));

        let unsupported_boundary = crate::compile(
            "owned.tima",
            "transform own(bytes: Bytes) -> Bytes { return bytes }\n",
        )
        .unwrap();
        let diagnostics = CraneliftBackend
            .emit(&unsupported_boundary.transforms)
            .unwrap_err();
        assert!(
            diagnostics[0]
                .message
                .contains("does not yet support the boundary")
        );
    }
}
