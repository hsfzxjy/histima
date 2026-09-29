use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::immediates::Ieee32;
use cranelift_codegen::ir::{
    AbiParam, Function, InstBuilder, MemFlagsData, Signature, StackSlotData, StackSlotKind,
    UserFuncName, types,
};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{FuncId, Linkage, Module, default_libcall_names};
use cranelift_object::{ObjectBuilder, ObjectModule};

use crate::abi::{
    ABI_IMAGE_FORMAT_RGBA8, ABI_IMAGE_FORMAT_WORD, ABI_IMAGE_HEIGHT_WORD, ABI_IMAGE_STRIDE_WORD,
    ABI_IMAGE_WIDTH_WORD, ABI_LENGTH_WORD, ABI_POINTER_WORD, ABI_STATUS_IMAGE_FORMAT,
    ABI_VALUE_BYTES, TIMA_ABI_VERSION,
};
use crate::ast::BinaryOp;
use crate::backend::{ArtifactBackend, BackendArtifact};
use crate::diagnostic::Diagnostic;
use crate::ir::{Constant, Terminator, Transform, Type, TypedModule, ValueId, ValueKind};

pub const CRANELIFT_BACKEND_VERSION: &str = "5";
pub const CRANELIFT_OPTIMIZATION: &str = "speed";

/// Ahead-of-time native object generation from backend-neutral Tima IR.
///
/// The initial slice accepts scalar and owned/view image transforms, including
/// native calls, byte maps, and RGBA8 scaling. Other typed IR is interpreted.
#[derive(Clone, Copy, Debug, Default)]
pub struct CraneliftBackend;

impl CraneliftBackend {
    pub fn supported_transforms(module: &TypedModule) -> Vec<bool> {
        supported_transforms(module)
    }
}

impl ArtifactBackend for CraneliftBackend {
    fn emit(&self, module: &TypedModule) -> Result<BackendArtifact, Vec<Diagnostic>> {
        validate_module(module)?;
        validate_host()?;

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

        let cpu_features = isa
            .isa_flags()
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>();
        let object_builder =
            ObjectBuilder::new(isa, "tima", default_libcall_names()).map_err(|error| {
                vec![backend_error(format!(
                    "could not create object module: {error}"
                ))]
            })?;
        let mut object = ObjectModule::new(object_builder);
        let signature = native_signature(&object);
        let mut function_ids = Vec::with_capacity(module.transforms.len());
        for index in 0..module.transforms.len() {
            function_ids.push(
                object
                    .declare_function(
                        &format!("tima_transform_{index}"),
                        Linkage::Export,
                        &signature,
                    )
                    .map_err(|error| {
                        vec![backend_error(format!(
                            "could not declare native transform {index}: {error}"
                        ))]
                    })?,
            );
        }
        for (index, transform) in module.transforms.iter().enumerate() {
            let function = lower_transform(transform, index as u32, &mut object, &function_ids)?;
            let mut context = cranelift_codegen::Context::for_function(function);
            object
                .define_function(function_ids[index], &mut context)
                .map_err(|error| {
                    vec![Diagnostic::error(
                        format!(
                            "Cranelift could not compile transform `{}`: {error}",
                            transform.name
                        ),
                        transform.span,
                    )]
                })?;
        }
        let bytes = object.finish().emit().map_err(|error| {
            vec![backend_error(format!(
                "could not serialize Cranelift object: {error}"
            ))]
        })?;
        Ok(BackendArtifact {
            backend: "cranelift",
            backend_version: CRANELIFT_BACKEND_VERSION,
            compiler_version: cranelift_native::VERSION,
            target: target_lexicon::Triple::host().to_string(),
            cpu_features,
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
    let supported = supported_transforms(module);
    for (transform_index, transform) in module.transforms.iter().enumerate() {
        if !validate_transform(transform).is_empty() || supported[transform_index] {
            continue;
        }
        for value in &transform.values {
            let ValueKind::Call {
                transform: callee, ..
            } = value.kind
            else {
                continue;
            };
            if !supported[callee.0 as usize] {
                diagnostics.push(
                    Diagnostic::error(
                        format!(
                            "Cranelift AOT cannot lower call to unsupported transform `{}`",
                            module.get(callee).name
                        ),
                        value.span,
                    )
                    .with_note(
                        "the caller remains interpreted until its callee is native-compatible",
                    ),
                );
            }
        }
    }
    if diagnostics.is_empty() {
        Ok(())
    } else {
        Err(diagnostics)
    }
}

fn supported_transforms(module: &TypedModule) -> Vec<bool> {
    fn visit(
        module: &TypedModule,
        index: usize,
        states: &mut [u8],
        supported: &mut [bool],
    ) -> bool {
        match states[index] {
            1 => return false,
            2 => return supported[index],
            _ => {}
        }
        states[index] = 1;
        let transform = &module.transforms[index];
        let mut accepted = validate_transform(transform).is_empty();
        if accepted {
            for value in &transform.values {
                if let ValueKind::Call {
                    transform: callee, ..
                } = value.kind
                    && !visit(module, callee.0 as usize, states, supported)
                {
                    accepted = false;
                    break;
                }
            }
        }
        supported[index] = accepted;
        states[index] = 2;
        accepted
    }

    let mut states = vec![0; module.transforms.len()];
    let mut supported = vec![false; module.transforms.len()];
    for index in 0..module.transforms.len() {
        visit(module, index, &mut states, &mut supported);
    }
    supported
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
            ValueKind::Call { .. } => {}
            ValueKind::ImageZero { .. }
            | ValueKind::ImageFill { .. }
            | ValueKind::ImageByteElement
            | ValueKind::ImageByteMap { .. }
            | ValueKind::ImageRgba8Scale { .. } => {}
            ValueKind::Constant(Constant::String(_)) | ValueKind::RuntimeCall(_) => diagnostics
                .push(Diagnostic::error(
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
    module: &mut ObjectModule,
    function_ids: &[FuncId],
) -> Result<Function, Vec<Diagnostic>> {
    let signature = native_signature(module);
    let frontend_config = module.target_config();
    let mut function = Function::with_name_signature(UserFuncName::user(0, index), signature);
    let function_refs = function_ids
        .iter()
        .map(|callee| module.declare_func_in_func(*callee, &mut function))
        .collect::<Vec<_>>();
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
        let runtime_context = builder.block_params(entry)[0];
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
                    ValueKind::Call {
                        transform: callee,
                        arguments,
                    } => emit_transform_call(
                        &mut builder,
                        runtime_context,
                        function_refs[callee.0 as usize],
                        arguments,
                        value.ty,
                        &values,
                    ),
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
                    ValueKind::ImageByteMap {
                        image,
                        element,
                        instructions,
                        result,
                    } => {
                        let image = required_image(&values, *image);
                        emit_image_byte_map(
                            &mut builder,
                            runtime_context,
                            image,
                            *element,
                            instructions,
                            *result,
                            transform,
                            &function_refs,
                            &mut values,
                        );
                        LoweredValue::Image(image)
                    }
                    ValueKind::ImageRgba8Scale { image, channels } => {
                        let image = required_image(&values, *image);
                        let channels = channels
                            .iter()
                            .map(|(channel, factor)| {
                                (channel.offset(), required_scalar(&values, *factor))
                            })
                            .collect::<Vec<_>>();
                        emit_image_rgba8_scale(&mut builder, image, &channels);
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
        builder.finalize(frontend_config);
    }
    Ok(function)
}

fn native_signature(module: &ObjectModule) -> Signature {
    let mut signature = module.make_signature();
    let pointer_type = module.target_config().pointer_type();
    // Opaque runtime context, fixed-width argument descriptors, and one result descriptor.
    signature.params.extend([
        AbiParam::new(pointer_type),
        AbiParam::new(pointer_type),
        AbiParam::new(pointer_type),
    ]);
    signature.returns.push(AbiParam::new(types::I32));
    signature
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
    load_abi_value(builder, arguments, ty, base)
}

fn load_abi_value(
    builder: &mut FunctionBuilder<'_>,
    pointer: cranelift_codegen::ir::Value,
    ty: Type,
    base: i32,
) -> LoweredValue {
    if scalar_type(ty) {
        return LoweredValue::Scalar(builder.ins().load(
            clif_type(ty),
            MemFlagsData::new(),
            pointer,
            base,
        ));
    }
    debug_assert!(matches!(ty, Type::Image | Type::ImageView));
    LoweredValue::Image(std::array::from_fn(|word| {
        builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            pointer,
            base + i32::try_from(word * 8).unwrap(),
        )
    }))
}

fn store_result(
    builder: &mut FunctionBuilder<'_>,
    result: cranelift_codegen::ir::Value,
    value: LoweredValue,
) {
    store_abi_value(builder, result, 0, value);
}

fn store_abi_value(
    builder: &mut FunctionBuilder<'_>,
    pointer: cranelift_codegen::ir::Value,
    base: i32,
    value: LoweredValue,
) {
    match value {
        LoweredValue::Scalar(value) => {
            builder
                .ins()
                .store(MemFlagsData::new(), value, pointer, base);
        }
        LoweredValue::Image(words) => {
            for (word, value) in words.into_iter().enumerate() {
                builder.ins().store(
                    MemFlagsData::new(),
                    value,
                    pointer,
                    base + i32::try_from(word * 8).unwrap(),
                );
            }
        }
    }
}

fn clear_abi_value(
    builder: &mut FunctionBuilder<'_>,
    pointer: cranelift_codegen::ir::Value,
    base: i32,
) {
    let zero = builder.ins().iconst(types::I64, 0);
    for word in 0..crate::abi::ABI_VALUE_WORDS {
        builder.ins().store(
            MemFlagsData::new(),
            zero,
            pointer,
            base + i32::try_from(word * 8).unwrap(),
        );
    }
}

fn emit_transform_call(
    builder: &mut FunctionBuilder<'_>,
    runtime_context: cranelift_codegen::ir::Value,
    callee: cranelift_codegen::ir::FuncRef,
    arguments: &[ValueId],
    result_type: Type,
    values: &[Option<LoweredValue>],
) -> LoweredValue {
    let pointer_type = builder.func.dfg.value_type(runtime_context);
    let argument_bytes = arguments.len().max(1) * ABI_VALUE_BYTES;
    let argument_slot = builder.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        u32::try_from(argument_bytes).expect("native call arguments fit a stack slot"),
        3,
    ));
    let result_slot = builder.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        u32::try_from(ABI_VALUE_BYTES).unwrap(),
        3,
    ));
    let argument_pointer = builder.ins().stack_addr(pointer_type, argument_slot, 0);
    let result_pointer = builder.ins().stack_addr(pointer_type, result_slot, 0);
    for (index, argument) in arguments.iter().enumerate() {
        let offset = i32::try_from(index * ABI_VALUE_BYTES).expect("native call offset fits i32");
        clear_abi_value(builder, argument_pointer, offset);
        store_abi_value(
            builder,
            argument_pointer,
            offset,
            required_value(values, *argument),
        );
    }
    clear_abi_value(builder, result_pointer, 0);
    let call = builder
        .ins()
        .call(callee, &[runtime_context, argument_pointer, result_pointer]);
    let status = builder.inst_results(call)[0];
    let success = builder.create_block();
    let failure = builder.create_block();
    let succeeded = builder.ins().icmp_imm_u(IntCC::Equal, status, 0);
    builder.ins().brif(succeeded, success, &[], failure, &[]);
    builder.switch_to_block(failure);
    builder.ins().return_(&[status]);
    builder.switch_to_block(success);
    load_abi_value(builder, result_pointer, result_type, 0)
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

#[allow(clippy::too_many_arguments)]
fn emit_image_byte_map(
    builder: &mut FunctionBuilder<'_>,
    runtime_context: cranelift_codegen::ir::Value,
    image: [cranelift_codegen::ir::Value; 7],
    element: ValueId,
    instructions: &[ValueId],
    result: ValueId,
    transform: &Transform,
    function_refs: &[cranelift_codegen::ir::FuncRef],
    values: &mut [Option<LoweredValue>],
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
    let byte = builder
        .ins()
        .load(types::I8, MemFlagsData::new(), address, 0);
    values[element.0 as usize] = Some(LoweredValue::Scalar(byte));
    for instruction in instructions {
        let value = transform.value(*instruction);
        let lowered = match &value.kind {
            ValueKind::Constant(constant) => {
                LoweredValue::Scalar(lower_constant(builder, constant, value.ty))
            }
            ValueKind::Binary { op, left, right } => LoweredValue::Scalar(lower_binary(
                builder,
                *op,
                required_scalar(values, *left),
                required_scalar(values, *right),
                transform.value(*left).ty,
            )),
            ValueKind::Call {
                transform: callee,
                arguments,
            } => emit_transform_call(
                builder,
                runtime_context,
                function_refs[callee.0 as usize],
                arguments,
                value.ty,
                values,
            ),
            _ => unreachable!("typed image byte maps contain scalar instructions"),
        };
        values[instruction.0 as usize] = Some(lowered);
    }
    builder.ins().store(
        MemFlagsData::new(),
        required_scalar(values, result),
        address,
        0,
    );
    let next = builder.ins().iadd_imm_u(index, 1);
    builder.ins().jump(header, &[next.into()]);

    builder.switch_to_block(done);
}

fn emit_image_rgba8_scale(
    builder: &mut FunctionBuilder<'_>,
    image: [cranelift_codegen::ir::Value; 7],
    channels: &[(usize, cranelift_codegen::ir::Value)],
) {
    let valid_format = builder.create_block();
    let invalid_format = builder.create_block();
    let expected_format = builder
        .ins()
        .iconst(types::I64, i64::from(ABI_IMAGE_FORMAT_RGBA8));
    let format_matches =
        builder
            .ins()
            .icmp(IntCC::Equal, image[ABI_IMAGE_FORMAT_WORD], expected_format);
    builder
        .ins()
        .brif(format_matches, valid_format, &[], invalid_format, &[]);
    builder.switch_to_block(invalid_format);
    let status = builder
        .ins()
        .iconst(types::I32, i64::from(ABI_STATUS_IMAGE_FORMAT));
    builder.ins().return_(&[status]);
    builder.switch_to_block(valid_format);

    let row_header = builder.create_block();
    let pixel_header = builder.create_block();
    let pixel_body = builder.create_block();
    let next_row = builder.create_block();
    let done = builder.create_block();
    builder.append_block_param(row_header, types::I64);
    builder.append_block_param(pixel_header, types::I64);
    builder.append_block_param(pixel_header, types::I64);

    let zero = builder.ins().iconst(types::I64, 0);
    builder.ins().jump(row_header, &[zero.into()]);

    builder.switch_to_block(row_header);
    let row = builder.block_params(row_header)[0];
    let rows_finished = builder.ins().icmp(
        IntCC::UnsignedGreaterThanOrEqual,
        row,
        image[ABI_IMAGE_HEIGHT_WORD],
    );
    builder.ins().brif(
        rows_finished,
        done,
        &[],
        pixel_header,
        &[row.into(), zero.into()],
    );

    builder.switch_to_block(pixel_header);
    let row = builder.block_params(pixel_header)[0];
    let column = builder.block_params(pixel_header)[1];
    let pixels_finished = builder.ins().icmp(
        IntCC::UnsignedGreaterThanOrEqual,
        column,
        image[ABI_IMAGE_WIDTH_WORD],
    );
    builder
        .ins()
        .brif(pixels_finished, next_row, &[], pixel_body, &[]);

    builder.switch_to_block(pixel_body);
    let row_offset = builder.ins().imul(row, image[ABI_IMAGE_STRIDE_WORD]);
    let pixel_offset = builder.ins().imul_imm_u(column, 4);
    let offset = builder.ins().iadd(row_offset, pixel_offset);
    let pixel = builder.ins().iadd(image[ABI_POINTER_WORD], offset);
    for (channel, factor) in channels {
        let address = builder
            .ins()
            .iadd_imm_u(pixel, i64::try_from(*channel).unwrap());
        let byte = builder
            .ins()
            .load(types::I8, MemFlagsData::new(), address, 0);
        let byte = builder.ins().uextend(types::I32, byte);
        let byte = builder.ins().fcvt_from_uint(types::F32, byte);
        let scaled = builder.ins().fmul(byte, *factor);
        let converted = builder.ins().fcvt_to_uint_sat(types::I32, scaled);
        let converted = builder.ins().ireduce(types::I8, converted);
        let float_zero = builder.ins().f32const(Ieee32::with_bits(0.0f32.to_bits()));
        let positive = builder.ins().fcmp(FloatCC::GreaterThan, scaled, float_zero);
        let maximum_float = builder
            .ins()
            .f32const(Ieee32::with_bits(255.0f32.to_bits()));
        let saturated = builder
            .ins()
            .fcmp(FloatCC::GreaterThanOrEqual, scaled, maximum_float);
        let maximum = builder.ins().iconst(types::I8, 255);
        let zero = builder.ins().iconst(types::I8, 0);
        let upper_bounded = builder.ins().select(saturated, maximum, converted);
        let result = builder.ins().select(positive, upper_bounded, zero);
        builder.ins().store(MemFlagsData::new(), result, address, 0);
    }
    let next_column = builder.ins().iadd_imm_u(column, 1);
    builder
        .ins()
        .jump(pixel_header, &[row.into(), next_column.into()]);

    builder.switch_to_block(next_row);
    let next_row = builder.ins().iadd_imm_u(row, 1);
    builder.ins().jump(row_header, &[next_row.into()]);

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

fn validate_host() -> Result<(), Vec<Diagnostic>> {
    if !cfg!(target_endian = "little") {
        return Err(vec![backend_error(
            "the initial native value ABI supports little-endian hosts only",
        )]);
    }
    if !cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
        return Err(vec![backend_error(
            "the initial Cranelift object backend supports x86-64 and AArch64 hosts",
        )]);
    }
    Ok(())
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
