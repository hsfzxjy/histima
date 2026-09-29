use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::immediates::Ieee32;
use cranelift_codegen::ir::{AbiParam, Function, InstBuilder, Signature, UserFuncName, types};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use object::write::{Object, StandardSection, Symbol, SymbolSection};
use object::{Architecture, BinaryFormat, Endianness, SymbolFlags, SymbolKind, SymbolScope};

use crate::abi::TIMA_ABI_VERSION;
use crate::ast::BinaryOp;
use crate::backend::{ArtifactBackend, BackendArtifact};
use crate::diagnostic::Diagnostic;
use crate::ir::{Constant, Terminator, Transform, Type, TypedModule, ValueId, ValueKind};

pub const CRANELIFT_BACKEND_VERSION: &str = "1";
pub const CRANELIFT_OPTIMIZATION: &str = "speed";

/// Ahead-of-time native object generation from backend-neutral Tima IR.
///
/// The first slice intentionally accepts only leaf scalar transforms. The
/// interpreter remains the execution engine until the native ABI and loader
/// cover all values admitted at a call boundary.
#[derive(Clone, Copy, Debug, Default)]
pub struct CraneliftBackend;

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
        if !transform
            .parameters
            .iter()
            .all(|parameter| scalar_type(parameter.ty))
            || !scalar_type(transform.return_type)
        {
            diagnostics.push(
                Diagnostic::error(
                    format!(
                        "Cranelift scalar AOT does not yet support the boundary of transform `{}`",
                        transform.name
                    ),
                    transform.span,
                )
                .with_note("supported boundary types are bool, u8, i64, and f32"),
            );
            continue;
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
                            "Cranelift scalar AOT does not yet lower checked i64 arithmetic",
                            value.span,
                        )
                        .with_note(
                            "the backend must preserve Tima overflow and division diagnostics",
                        ),
                    );
                }
                ValueKind::Binary { .. } => {}
                ValueKind::Call { .. } => diagnostics.push(
                    Diagnostic::error(
                        "Cranelift scalar AOT does not yet lower transform calls",
                        value.span,
                    )
                    .with_note("the first native artifact slice accepts leaf transforms only"),
                ),
                ValueKind::Constant(Constant::String(_))
                | ValueKind::ImageZero { .. }
                | ValueKind::ImageFill { .. }
                | ValueKind::ImageByteElement
                | ValueKind::ImageByteMap { .. }
                | ValueKind::ImageRgba8Scale { .. }
                | ValueKind::RuntimeCall(_) => diagnostics.push(Diagnostic::error(
                    "operation is outside the initial Cranelift scalar AOT subset",
                    value.span,
                )),
            }
        }
    }
    if diagnostics.is_empty() {
        Ok(())
    } else {
        Err(diagnostics)
    }
}

fn lower_transform(
    transform: &Transform,
    index: u32,
    isa: &dyn cranelift_codegen::isa::TargetIsa,
) -> Result<Function, Vec<Diagnostic>> {
    let mut signature = Signature::new(isa.default_call_conv());
    for parameter in &transform.parameters {
        signature
            .params
            .push(AbiParam::new(clif_type(parameter.ty)));
    }
    signature
        .returns
        .push(AbiParam::new(clif_type(transform.return_type)));
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
        let mut values = vec![None; transform.values.len()];
        for parameter in &transform.parameters {
            let ValueKind::Parameter { index } = transform.value(parameter.value).kind else {
                unreachable!("typed parameters reference parameter IR values")
            };
            values[parameter.value.0 as usize] = Some(builder.block_params(entry)[index as usize]);
        }

        let mut order = Vec::with_capacity(blocks.len());
        order.push(transform.entry.0 as usize);
        order.extend((0..blocks.len()).filter(|index| *index != transform.entry.0 as usize));
        for block_index in order {
            builder.switch_to_block(blocks[block_index]);
            for id in &transform.blocks[block_index].instructions {
                let value = &transform.values[id.0 as usize];
                let lowered = match &value.kind {
                    ValueKind::Parameter { .. } => continue,
                    ValueKind::Constant(constant) => lower_constant(&mut builder, constant),
                    ValueKind::Binary { op, left, right } => lower_binary(
                        &mut builder,
                        *op,
                        required_value(&values, *left),
                        required_value(&values, *right),
                        transform.value(*left).ty,
                    ),
                    _ => unreachable!("validation rejects non-scalar operations"),
                };
                values[id.0 as usize] = Some(lowered);
            }
            match transform.blocks[block_index].terminator {
                Terminator::Return(value) => {
                    builder.ins().return_(&[required_value(&values, value)]);
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
                        required_value(&values, condition),
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

fn lower_constant(
    builder: &mut FunctionBuilder<'_>,
    constant: &Constant,
) -> cranelift_codegen::ir::Value {
    match constant {
        Constant::Bool(value) => builder.ins().iconst(types::I8, i64::from(*value)),
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

fn required_value(
    values: &[Option<cranelift_codegen::ir::Value>],
    id: ValueId,
) -> cranelift_codegen::ir::Value {
    values[id.0 as usize].expect("typed IR values dominate their uses")
}

fn host_object_architecture() -> Result<Architecture, Vec<Diagnostic>> {
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
    fn rejects_semantics_not_yet_preserved_by_the_scalar_backend() {
        let checked_integer = crate::compile(
            "integer.tima",
            "transform add(left: i64, right: i64) -> i64 { return left + right }\n",
        )
        .unwrap();
        let diagnostics = CraneliftBackend
            .emit(&checked_integer.transforms)
            .unwrap_err();
        assert!(diagnostics[0].message.contains("checked i64 arithmetic"));

        let owned = crate::compile(
            "owned.tima",
            "transform own(img: Image) -> Image { return img }\n",
        )
        .unwrap();
        let diagnostics = CraneliftBackend.emit(&owned.transforms).unwrap_err();
        assert!(
            diagnostics[0]
                .message
                .contains("does not yet support the boundary")
        );
    }
}
