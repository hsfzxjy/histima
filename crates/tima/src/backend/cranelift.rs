use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::immediates::Ieee32;
use cranelift_codegen::ir::{
    AbiParam, Function, GlobalValue, InstBuilder, MemFlagsData, Signature, StackSlotData,
    StackSlotKind, UserFuncName, types,
};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module, default_libcall_names};
use cranelift_object::{ObjectBuilder, ObjectModule};

use crate::abi::{
    ABI_LENGTH_WORD, ABI_POINTER_WORD, ABI_RUNTIME_USER_DATA_OFFSET, ABI_RUNTIME_WORLD_CALL_OFFSET,
    ABI_VALUE_BYTES, ABI_WORLD_ENVIRONMENT_READ, ABI_WORLD_FILE_READ, ABI_WORLD_HTTP_GET,
    TIMA_ABI_VERSION, abi_callsite,
};
use crate::ast::BinaryOp;
use crate::backend::{ArtifactBackend, BackendArtifact};
use crate::diagnostic::Diagnostic;
use crate::ir::{
    Constant, RuntimeCall, Terminator, Transform, TransformId, Type, TypedModule, ValueId,
    ValueKind,
};

pub const CRANELIFT_BACKEND_VERSION: &str = "8";
pub const CRANELIFT_OPTIMIZATION: &str = "speed";

/// Ahead-of-time native object generation from backend-neutral Tima IR.
///
/// The initial slice accepts scalars and owned/view strings and bytes. Generic
/// shaped buffers currently remain on the typed-IR interpreter path.
#[derive(Clone, Copy, Debug, Default)]
pub struct CraneliftBackend;

/// Backend-only dispatch metadata for one source transform. This report is not
/// part of Tima semantics, Hash IR, or Transform identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CraneliftTransformPlan {
    pub transform: TransformId,
    pub name: String,
    pub native_compatible: bool,
    pub fallback_reasons: Vec<String>,
}

impl CraneliftBackend {
    pub fn supported_transforms(module: &TypedModule) -> Vec<bool> {
        supported_transforms(module)
    }

    pub fn transform_plan(module: &TypedModule) -> Vec<CraneliftTransformPlan> {
        transform_plan(module)
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
        let (static_strings, static_size) = define_static_strings(&mut object, module)?;
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
            let function = lower_transform(
                transform,
                index as u32,
                &mut object,
                &function_ids,
                &static_strings[index],
            )?;
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
            static_size,
        })
    }
}

#[derive(Clone, Copy)]
struct StaticString {
    data: Option<DataId>,
    length: u64,
}

#[derive(Clone, Copy)]
struct LoweredStaticString {
    data: Option<GlobalValue>,
    length: u64,
}

type StaticStringTable = Vec<Vec<Option<StaticString>>>;

fn define_static_strings(
    object: &mut ObjectModule,
    module: &TypedModule,
) -> Result<(StaticStringTable, u64), Vec<Diagnostic>> {
    let mut total_size = 0u64;
    let mut strings = Vec::with_capacity(module.transforms.len());
    for (transform_index, transform) in module.transforms.iter().enumerate() {
        let mut values = vec![None; transform.values.len()];
        for (value_index, value) in transform.values.iter().enumerate() {
            let ValueKind::Constant(Constant::String(text)) = &value.kind else {
                continue;
            };
            let length = u64::try_from(text.len()).map_err(|_| {
                vec![Diagnostic::error(
                    "string literal is too large for the native ABI",
                    value.span,
                )]
            })?;
            total_size = total_size.checked_add(length).ok_or_else(|| {
                vec![Diagnostic::error(
                    "native static data size overflowed u64",
                    value.span,
                )]
            })?;
            let data = if text.is_empty() {
                None
            } else {
                let name = format!("tima_string_{transform_index}_{value_index}");
                let data = object
                    .declare_data(&name, Linkage::Local, false, false)
                    .map_err(|error| {
                        vec![Diagnostic::error(
                            format!("could not declare native string literal: {error}"),
                            value.span,
                        )]
                    })?;
                let mut description = DataDescription::new();
                description.define(text.as_bytes().into());
                object.define_data(data, &description).map_err(|error| {
                    vec![Diagnostic::error(
                        format!("could not define native string literal: {error}"),
                        value.span,
                    )]
                })?;
                Some(data)
            };
            values[value_index] = Some(StaticString { data, length });
        }
        strings.push(values);
    }
    Ok((strings, total_size))
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

fn transform_plan(module: &TypedModule) -> Vec<CraneliftTransformPlan> {
    let supported = supported_transforms(module);
    module
        .transforms
        .iter()
        .enumerate()
        .map(|(index, transform)| {
            let mut fallback_reasons = Vec::new();
            if !supported[index] {
                for diagnostic in validate_transform(transform) {
                    push_unique(&mut fallback_reasons, diagnostic.message);
                }
                if fallback_reasons.is_empty() {
                    for value in &transform.values {
                        let ValueKind::Call {
                            transform: callee, ..
                        } = value.kind
                        else {
                            continue;
                        };
                        if !supported[callee.0 as usize] {
                            push_unique(
                                &mut fallback_reasons,
                                format!(
                                    "calls `{}` which is not native-compatible",
                                    module.get(callee).name
                                ),
                            );
                        }
                    }
                }
                if fallback_reasons.is_empty() {
                    fallback_reasons.push(
                        "recursive source-transform calls are outside the current Cranelift AOT subset"
                            .to_owned(),
                    );
                }
            }
            CraneliftTransformPlan {
                transform: TransformId(index as u32),
                name: transform.name.clone(),
                native_compatible: supported[index],
                fallback_reasons,
            }
        })
        .collect()
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
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
            .with_note(
                "supported boundary types are bool, u8, i64, f32, String, StringView, Bytes, and BytesView",
            ),
        );
        return diagnostics;
    }
    for value in &transform.values {
        match &value.kind {
            ValueKind::Parameter { .. } | ValueKind::Constant(_) => {}
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
            ValueKind::U8Scale { .. } => diagnostics.push(Diagnostic::error(
                "u8.scale is outside the current Cranelift AOT subset",
                value.span,
            )),
            ValueKind::Call { .. } => {}
            ValueKind::BufferZero { .. }
            | ValueKind::BufferFill { .. }
            | ValueKind::BufferByteElement
            | ValueKind::BufferByteIndex
            | ValueKind::BufferByteMap { .. } => diagnostics.push(Diagnostic::error(
                "generic Buffer operations are outside the current Cranelift AOT subset",
                value.span,
            )),
            ValueKind::RuntimeCall(
                RuntimeCall::EnvironmentRead { .. }
                | RuntimeCall::FileRead { .. }
                | RuntimeCall::HttpGet { .. },
            ) => {}
            ValueKind::RuntimeCall(RuntimeCall::EnvironmentI64 { .. }) => {
                diagnostics.push(Diagnostic::error(
                    "operation is outside the initial Cranelift AOT subset",
                    value.span,
                ))
            }
        }
    }
    diagnostics
}

fn lower_transform(
    transform: &Transform,
    index: u32,
    module: &mut ObjectModule,
    function_ids: &[FuncId],
    static_strings: &[Option<StaticString>],
) -> Result<Function, Vec<Diagnostic>> {
    let signature = native_signature(module);
    let world_call_signature = world_call_signature(module);
    let frontend_config = module.target_config();
    let mut function = Function::with_name_signature(UserFuncName::user(0, index), signature);
    let world_call_signature = function.import_signature(world_call_signature);
    let function_refs = function_ids
        .iter()
        .map(|callee| module.declare_func_in_func(*callee, &mut function))
        .collect::<Vec<_>>();
    let static_strings = static_strings
        .iter()
        .map(|value| {
            value.map(|value| LoweredStaticString {
                data: value
                    .data
                    .map(|data| module.declare_data_in_func(data, &mut function)),
                length: value.length,
            })
        })
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
                    ValueKind::Constant(Constant::String(_)) => lower_static_string(
                        &mut builder,
                        static_strings[id.0 as usize]
                            .expect("string constants have declared object data"),
                    ),
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
                    ValueKind::U8Scale { .. } => {
                        unreachable!("validation rejects u8.scale")
                    }
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
                    ValueKind::BufferZero { .. }
                    | ValueKind::BufferFill { .. }
                    | ValueKind::BufferByteElement
                    | ValueKind::BufferByteIndex
                    | ValueKind::BufferByteMap { .. } => {
                        unreachable!("validation rejects generic Buffer operations")
                    }
                    ValueKind::RuntimeCall(call) => emit_world_call(
                        &mut builder,
                        runtime_context,
                        world_call_signature,
                        abi_callsite(index, id.0),
                        call,
                        value.ty,
                        &values,
                    ),
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

fn world_call_signature(module: &ObjectModule) -> Signature {
    let mut signature = module.make_signature();
    let pointer_type = module.target_config().pointer_type();
    signature.params.extend([
        AbiParam::new(pointer_type),
        AbiParam::new(types::I64),
        AbiParam::new(types::I32),
        AbiParam::new(pointer_type),
        AbiParam::new(types::I64),
        AbiParam::new(pointer_type),
    ]);
    signature.returns.push(AbiParam::new(types::I32));
    signature
}

#[derive(Clone, Copy)]
enum LoweredValue {
    Scalar(cranelift_codegen::ir::Value),
    Buffer([cranelift_codegen::ir::Value; 3]),
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
    let load_words = |builder: &mut FunctionBuilder<'_>, count: usize| {
        (0..count)
            .map(|word| {
                builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    pointer,
                    base + i32::try_from(word * 8).unwrap(),
                )
            })
            .collect::<Vec<_>>()
    };
    debug_assert!(matches!(
        ty,
        Type::String | Type::StringView | Type::Bytes | Type::BytesView
    ));
    let words = load_words(builder, 3);
    LoweredValue::Buffer(words.try_into().unwrap())
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
        LoweredValue::Buffer(words) => {
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

fn emit_world_call(
    builder: &mut FunctionBuilder<'_>,
    runtime_context: cranelift_codegen::ir::Value,
    signature: cranelift_codegen::ir::SigRef,
    callsite: u64,
    call: &RuntimeCall,
    result_type: Type,
    values: &[Option<LoweredValue>],
) -> LoweredValue {
    let (operation, key) = match call {
        RuntimeCall::EnvironmentRead { name } => (ABI_WORLD_ENVIRONMENT_READ, *name),
        RuntimeCall::FileRead { path } => (ABI_WORLD_FILE_READ, *path),
        RuntimeCall::HttpGet { url } => (ABI_WORLD_HTTP_GET, *url),
        RuntimeCall::EnvironmentI64 { .. } => {
            unreachable!("validation rejects legacy environment_i64 calls")
        }
    };
    let key = required_buffer(values, key);
    let pointer_type = builder.func.dfg.value_type(runtime_context);
    let user_data = builder.ins().load(
        pointer_type,
        MemFlagsData::new(),
        runtime_context,
        ABI_RUNTIME_USER_DATA_OFFSET,
    );
    let callback = builder.ins().load(
        pointer_type,
        MemFlagsData::new(),
        runtime_context,
        ABI_RUNTIME_WORLD_CALL_OFFSET,
    );
    let result_slot = builder.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        u32::try_from(ABI_VALUE_BYTES).unwrap(),
        3,
    ));
    let result_pointer = builder.ins().stack_addr(pointer_type, result_slot, 0);
    clear_abi_value(builder, result_pointer, 0);
    let callsite = builder.ins().iconst(types::I64, callsite as i64);
    let operation = builder.ins().iconst(types::I32, i64::from(operation));
    let call = builder.ins().call_indirect(
        signature,
        callback,
        &[
            user_data,
            callsite,
            operation,
            key[ABI_POINTER_WORD],
            key[ABI_LENGTH_WORD],
            result_pointer,
        ],
    );
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
        Constant::String(_) => unreachable!("string constants use object data lowering"),
    }
}

fn lower_static_string(
    builder: &mut FunctionBuilder<'_>,
    string: LoweredStaticString,
) -> LoweredValue {
    let pointer = match string.data {
        Some(data) => builder.ins().symbol_value(types::I64, data),
        None => builder.ins().iconst(types::I64, 0),
    };
    let length = builder.ins().iconst(types::I64, string.length as i64);
    let capacity = builder.ins().iconst(types::I64, 0);
    LoweredValue::Buffer([pointer, length, capacity])
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
    scalar_type(ty)
        || matches!(
            ty,
            Type::String | Type::StringView | Type::Bytes | Type::BytesView
        )
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

fn required_buffer(
    values: &[Option<LoweredValue>],
    id: ValueId,
) -> [cranelift_codegen::ir::Value; 3] {
    let LoweredValue::Buffer(value) = required_value(values, id) else {
        unreachable!("typed buffer operation has a buffer operand")
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

        let unsupported_world_call = crate::compile(
            "world.tima",
            "transform read() -> i64 uses env.read { return environment_i64(\"MODE\") }\n",
        )
        .unwrap();
        let diagnostics = CraneliftBackend
            .emit(&unsupported_world_call.transforms)
            .unwrap_err();
        assert!(
            diagnostics[0]
                .message
                .contains("outside the initial Cranelift AOT subset")
        );
    }

    #[test]
    fn reports_native_admission_and_transitive_fallback_reasons() {
        let compiled = crate::compile(
            "plan.tima",
            "transform scale(value: f32, factor: f32) -> f32 { return value * factor }\n\
             transform checked(left: i64, right: i64) -> i64 { return left + right }\n\
             transform wrapper(left: i64, right: i64) -> i64 {\n\
                 return checked(left, right)\n\
             }\n",
        )
        .unwrap();
        let plan = CraneliftBackend::transform_plan(&compiled.transforms);

        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0].name, "scale");
        assert!(plan[0].native_compatible);
        assert!(plan[0].fallback_reasons.is_empty());
        assert_eq!(plan[1].name, "checked");
        assert!(!plan[1].native_compatible);
        assert_eq!(
            plan[1].fallback_reasons,
            ["Cranelift AOT does not yet lower checked i64 arithmetic"]
        );
        assert_eq!(plan[2].name, "wrapper");
        assert!(!plan[2].native_compatible);
        assert_eq!(
            plan[2].fallback_reasons,
            ["calls `checked` which is not native-compatible"]
        );
        assert_eq!(
            CraneliftBackend::supported_transforms(&compiled.transforms),
            plan.iter()
                .map(|transform| transform.native_compatible)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn emits_deterministic_read_only_string_data() {
        let compiled = crate::compile(
            "strings.tima",
            "transform read() -> Bytes uses file.read { return file.read(\"asset.bin\") }\n\
             transform label() -> StringView { return \"ready\" }\n",
        )
        .unwrap();
        let first = CraneliftBackend.emit(&compiled.transforms).unwrap();
        let second = CraneliftBackend.emit(&compiled.transforms).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.static_size, 14);

        let object = object::File::parse(first.bytes.as_slice()).unwrap();
        let symbols = object
            .symbols()
            .filter_map(|symbol| symbol.name().ok())
            .collect::<Vec<_>>();
        assert!(symbols.contains(&"tima_string_0_0"));
        assert!(symbols.contains(&"tima_string_1_0"));
    }
}
