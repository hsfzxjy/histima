use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::CompiledProgram;
use crate::ast::{Argument, BinaryOp, ExprId, ExprKind, Item};
use crate::backend::native::{AbiImage, AbiImageView, AbiValue, NativeModule};
use crate::diagnostic::Diagnostic;
use crate::ir::{Constant, Terminator, TransformId, Type, ValueKind};
use crate::source::Span;

/// Opaque hook for the future lineage graph. Keeping it outside `ValueData`
/// ensures native payloads never acquire outer tracing metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LineageId(pub u32);

/// An immutable outer value. Composite payloads use immutable `Arc` storage;
/// there is no API that exposes mutable list, record, string, or asset data.
#[derive(Clone, Debug, PartialEq)]
pub struct OuterValue {
    pub data: ValueData,
    pub lineage: Option<LineageId>,
}

impl OuterValue {
    pub fn plain(data: ValueData) -> Self {
        Self {
            data,
            lineage: None,
        }
    }

    pub fn image(image: ImageValue) -> Self {
        Self::plain(ValueData::Image(Arc::new(image)))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ValueData {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f32),
    String(Arc<str>),
    List(Arc<[OuterValue]>),
    Record(Arc<BTreeMap<String, OuterValue>>),
    Asset(Arc<AssetValue>),
    Image(Arc<ImageValue>),
    Transform(TransformId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetValue {
    /// A locator is not a content identity. Asset observation/materialization
    /// will record content identity and source lineage in a later milestone.
    pub locator: Arc<str>,
}

#[derive(Debug, PartialEq, Eq)]
struct ImageStorage {
    bytes: Vec<u8>,
}

impl ImageStorage {
    fn new(mut bytes: Vec<u8>) -> Self {
        // Empty Vec pointers are shared dangling sentinels, which cannot serve
        // as invocation-local storage identities. Reserve one byte without
        // changing the logical image length so even empty images have a live,
        // unique allocation while crossing the native boundary.
        if bytes.is_empty() {
            bytes.reserve_exact(1);
        }
        Self { bytes }
    }
}

/// Immutable outer image descriptor backed by shareable byte storage.
///
/// `width` and `height` are logical dimensions, while `stride` is the backing
/// byte count per row. Pixel format is deliberately outside this first
/// ownership milestone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageValue {
    storage: Arc<ImageStorage>,
    width: usize,
    height: usize,
    stride: usize,
}

impl ImageValue {
    pub fn new(
        width: usize,
        height: usize,
        stride: usize,
        bytes: Vec<u8>,
    ) -> Result<Self, ImageLayoutError> {
        validate_image_layout(height, stride, bytes.len())?;
        Ok(Self {
            storage: Arc::new(ImageStorage::new(bytes)),
            width,
            height,
            stride,
        })
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn stride(&self) -> usize {
        self.stride
    }

    pub fn bytes(&self) -> &[u8] {
        &self.storage.bytes
    }

    pub fn shares_storage_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.storage, &other.storage)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageLayoutError {
    message: String,
}

impl fmt::Display for ImageLayoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ImageLayoutError {}

fn validate_image_layout(
    height: usize,
    stride: usize,
    byte_len: usize,
) -> Result<(), ImageLayoutError> {
    let expected = height.checked_mul(stride).ok_or_else(|| ImageLayoutError {
        message: "image byte length overflows usize".to_owned(),
    })?;
    if byte_len != expected {
        return Err(ImageLayoutError {
            message: format!(
                "image storage has {byte_len} bytes, but height {height} and stride {stride} require {expected}"
            ),
        });
    }
    Ok(())
}

#[derive(Clone, Debug, Default)]
pub struct Execution {
    pub bindings: BTreeMap<String, OuterValue>,
    pub last_value: Option<OuterValue>,
}

pub fn execute(program: &CompiledProgram) -> Result<Execution, Vec<Diagnostic>> {
    let engine = IrInterpreter {
        module: &program.transforms,
    };
    execute_with(program, &engine, BTreeMap::new())
}

/// Executes outer code while dispatching transform calls to a loaded native
/// artifact. Outer expressions remain interpreted; only checked inner
/// transforms cross this boundary.
pub fn execute_native(
    program: &CompiledProgram,
    native: &NativeModule,
) -> Result<Execution, Vec<Diagnostic>> {
    let engine = NativeEngine {
        module: &program.transforms,
        native,
    };
    execute_with(program, &engine, BTreeMap::new())
}

/// Runs a program with immutable values supplied by the Histima host runtime.
/// This is the initial integration point for materialized asset-native values;
/// it avoids inventing source-language image literal semantics.
pub fn execute_native_with_bindings(
    program: &CompiledProgram,
    native: &NativeModule,
    bindings: BTreeMap<String, OuterValue>,
) -> Result<Execution, Vec<Diagnostic>> {
    let engine = NativeEngine {
        module: &program.transforms,
        native,
    };
    execute_with(program, &engine, bindings)
}

/// Invokes one checked transform with owned outer arguments. This makes the
/// ownership transition directly usable by Histima and testable independently
/// of outer binding liveness.
pub fn invoke_native_transform(
    program: &CompiledProgram,
    native: &NativeModule,
    transform: TransformId,
    arguments: Vec<OuterValue>,
) -> Result<OuterValue, Diagnostic> {
    let definition = program.transforms.get(transform);
    if arguments.len() != definition.parameters.len() {
        return Err(Diagnostic::error(
            format!(
                "transform `{}` expects {} arguments, but {} were supplied",
                definition.name,
                definition.parameters.len(),
                arguments.len()
            ),
            definition.span,
        ));
    }
    let arguments = arguments
        .into_iter()
        .map(|value| (value, definition.span))
        .collect();
    NativeEngine {
        module: &program.transforms,
        native,
    }
    .invoke(transform, arguments)
}

fn execute_with(
    program: &CompiledProgram,
    engine: &dyn TransformEngine,
    bindings: BTreeMap<String, OuterValue>,
) -> Result<Execution, Vec<Diagnostic>> {
    Interpreter {
        program,
        engine,
        execution: Execution {
            bindings,
            last_value: None,
        },
    }
    .run()
    .map_err(|diagnostic| vec![diagnostic])
}

trait TransformEngine {
    fn invoke(
        &self,
        id: TransformId,
        arguments: Vec<(OuterValue, Span)>,
    ) -> Result<OuterValue, Diagnostic>;
}

struct Interpreter<'program, 'engine> {
    program: &'program CompiledProgram,
    engine: &'engine dyn TransformEngine,
    execution: Execution,
}

impl Interpreter<'_, '_> {
    fn run(mut self) -> Result<Execution, Diagnostic> {
        for item in &self.program.syntax.items {
            match item {
                Item::Transform(_) => {}
                Item::Binding(binding) => {
                    if self.execution.bindings.contains_key(&binding.name) {
                        return Err(Diagnostic::error(
                            format!(
                                "outer binding `{}` is already defined in this initial subset",
                                binding.name
                            ),
                            binding.name_span,
                        )
                        .with_note("binding rebinding semantics have not been chosen yet"));
                    }
                    let value = self.expression(binding.value)?;
                    self.execution
                        .bindings
                        .insert(binding.name.clone(), value.clone());
                    self.execution.last_value = Some(value);
                }
                Item::Expression(expression) => {
                    self.execution.last_value = Some(self.expression(*expression)?);
                }
            }
        }
        Ok(self.execution)
    }

    fn expression(&mut self, id: ExprId) -> Result<OuterValue, Diagnostic> {
        let expression = self.program.syntax.expr(id);
        let value = match &expression.kind {
            ExprKind::Null => OuterValue::plain(ValueData::Null),
            ExprKind::Bool(value) => OuterValue::plain(ValueData::Bool(*value)),
            ExprKind::Integer(value) => OuterValue::plain(ValueData::Integer(*value)),
            ExprKind::Float(value) => OuterValue::plain(ValueData::Float(*value as f32)),
            ExprKind::String(value) => {
                OuterValue::plain(ValueData::String(Arc::from(value.as_str())))
            }
            ExprKind::Name(name) => self.resolve_name(name, expression.span)?,
            ExprKind::List(values) => {
                let values = values
                    .iter()
                    .map(|value| self.expression(*value))
                    .collect::<Result<Vec<_>, _>>()?;
                OuterValue::plain(ValueData::List(Arc::from(values)))
            }
            ExprKind::Record(fields) => {
                let mut values = BTreeMap::new();
                for field in fields {
                    if values.contains_key(&field.name) {
                        return Err(Diagnostic::error(
                            format!("record field `{}` is specified more than once", field.name),
                            field.name_span,
                        ));
                    }
                    values.insert(field.name.clone(), self.expression(field.value)?);
                }
                OuterValue::plain(ValueData::Record(Arc::new(values)))
            }
            ExprKind::Binary { op, left, right } => {
                let left = self.expression(*left)?;
                let right = self.expression(*right)?;
                outer_binary(*op, left, right, expression.span)?
            }
            ExprKind::Call { callee, arguments } => {
                self.call(*callee, arguments, None, expression.span)?
            }
            ExprKind::Pipeline { input, stage } => {
                let input = self.expression(*input)?;
                match &self.program.syntax.expr(*stage).kind {
                    ExprKind::Call { callee, arguments } => {
                        self.call(*callee, arguments, Some(input), expression.span)?
                    }
                    ExprKind::Name(_) => self.call(*stage, &[], Some(input), expression.span)?,
                    _ => {
                        return Err(Diagnostic::error(
                            "pipeline stage must be a transform name or call",
                            self.program.syntax.expr(*stage).span,
                        ));
                    }
                }
            }
        };
        Ok(value)
    }

    fn resolve_name(&self, name: &str, span: Span) -> Result<OuterValue, Diagnostic> {
        if let Some(value) = self.execution.bindings.get(name) {
            return Ok(value.clone());
        }
        if let Some((id, _)) = self.program.transforms.find(name) {
            return Ok(OuterValue::plain(ValueData::Transform(id)));
        }
        Err(Diagnostic::error(
            format!("unknown outer name `{name}`"),
            span,
        ))
    }

    fn call(
        &mut self,
        callee: ExprId,
        arguments: &[Argument],
        pipeline_input: Option<OuterValue>,
        span: Span,
    ) -> Result<OuterValue, Diagnostic> {
        let callee_expression = self.program.syntax.expr(callee);
        let ExprKind::Name(name) = &callee_expression.kind else {
            return Err(Diagnostic::error(
                "outer calls require a directly named builtin or transform",
                callee_expression.span,
            ));
        };
        let mut evaluated = Vec::new();
        if let Some(input) = pipeline_input {
            evaluated.push((None, input, callee_expression.span));
        }
        for argument in arguments {
            evaluated.push((
                argument.name.as_ref().map(|(name, _)| name.clone()),
                self.expression(argument.value)?,
                argument.span,
            ));
        }
        if name == "asset" {
            return self.asset(evaluated, span);
        }
        let Some((id, transform)) = self.program.transforms.find(name) else {
            return Err(Diagnostic::error(
                format!("unknown outer callable `{name}`"),
                callee_expression.span,
            ));
        };
        let mut ordered: Vec<Option<(OuterValue, Span)>> = vec![None; transform.parameters.len()];
        let mut next_positional = 0;
        for (name, value, argument_span) in evaluated {
            let index = if let Some(name) = name {
                transform
                    .parameters
                    .iter()
                    .position(|parameter| parameter.name == name)
                    .ok_or_else(|| {
                        Diagnostic::error(
                            format!("transform `{}` has no parameter `{name}`", transform.name),
                            argument_span,
                        )
                    })?
            } else {
                while next_positional < ordered.len() && ordered[next_positional].is_some() {
                    next_positional += 1;
                }
                if next_positional == ordered.len() {
                    return Err(Diagnostic::error(
                        format!("too many arguments for transform `{}`", transform.name),
                        argument_span,
                    ));
                }
                let index = next_positional;
                next_positional += 1;
                index
            };
            if ordered[index].is_some() {
                return Err(Diagnostic::error(
                    format!(
                        "parameter `{}` is supplied more than once",
                        transform.parameters[index].name
                    ),
                    argument_span,
                ));
            }
            ordered[index] = Some((value, argument_span));
        }
        let mut values = Vec::new();
        for (index, value) in ordered.into_iter().enumerate() {
            let Some(value) = value else {
                return Err(Diagnostic::error(
                    format!(
                        "missing argument `{}` for transform `{}`",
                        transform.parameters[index].name, transform.name
                    ),
                    span,
                ));
            };
            values.push(value);
        }
        self.engine.invoke(id, values)
    }

    fn asset(
        &self,
        arguments: Vec<(Option<String>, OuterValue, Span)>,
        span: Span,
    ) -> Result<OuterValue, Diagnostic> {
        if arguments.len() != 1
            || arguments[0]
                .0
                .as_deref()
                .is_some_and(|name| name != "path" && name != "locator")
        {
            return Err(Diagnostic::error(
                "asset expects exactly one locator string",
                span,
            ));
        }
        let ValueData::String(locator) = &arguments[0].1.data else {
            return Err(Diagnostic::error(
                "asset locator must be a string",
                arguments[0].2,
            ));
        };
        Ok(OuterValue::plain(ValueData::Asset(Arc::new(AssetValue {
            locator: locator.clone(),
        }))))
    }
}

fn outer_binary(
    op: BinaryOp,
    left: OuterValue,
    right: OuterValue,
    span: Span,
) -> Result<OuterValue, Diagnostic> {
    let data = match (left.data, right.data) {
        (ValueData::Integer(left), ValueData::Integer(right)) => {
            ValueData::Integer(integer_binary(op, left, right, span)?)
        }
        (ValueData::Float(left), ValueData::Float(right)) => ValueData::Float(match op {
            BinaryOp::Add => left + right,
            BinaryOp::Subtract => left - right,
            BinaryOp::Multiply => left * right,
            BinaryOp::Divide => left / right,
        }),
        _ => {
            return Err(Diagnostic::error(
                "outer arithmetic requires two integers or two floats",
                span,
            ));
        }
    };
    Ok(OuterValue::plain(data))
}

fn integer_binary(op: BinaryOp, left: i64, right: i64, span: Span) -> Result<i64, Diagnostic> {
    let result = match op {
        BinaryOp::Add => left.checked_add(right),
        BinaryOp::Subtract => left.checked_sub(right),
        BinaryOp::Multiply => left.checked_mul(right),
        BinaryOp::Divide => left.checked_div(right),
    };
    result.ok_or_else(|| Diagnostic::error("integer arithmetic overflow or division by zero", span))
}

#[derive(Clone, Copy, Debug)]
enum NativeScalar {
    Bool(bool),
    I64(i64),
    F32(f32),
}

struct IrInterpreter<'a> {
    module: &'a crate::ir::TypedModule,
}

impl IrInterpreter<'_> {
    fn invoke_at_depth(
        &self,
        id: TransformId,
        arguments: &[(OuterValue, Span)],
        depth: usize,
    ) -> Result<OuterValue, Diagnostic> {
        let transform = self.module.get(id);
        if depth >= 256 {
            return Err(Diagnostic::error(
                "transform call depth exceeded the interpreter limit",
                transform.span,
            ));
        }
        let mut values = vec![None; transform.values.len()];
        for (parameter, (argument, span)) in transform.parameters.iter().zip(arguments) {
            values[parameter.value.0 as usize] = Some(lower_scalar(argument, parameter.ty, *span)?);
        }
        let block = &transform.blocks[transform.entry.0 as usize];
        for id in &block.instructions {
            let value = transform.value(*id);
            let evaluated = match &value.kind {
                ValueKind::Parameter { .. } => unreachable!(),
                ValueKind::Constant(constant) => match constant {
                    Constant::Bool(value) => NativeScalar::Bool(*value),
                    Constant::I64(value) => NativeScalar::I64(*value),
                    Constant::F32(value) => NativeScalar::F32(*value),
                },
                ValueKind::Binary { op, left, right } => native_binary(
                    *op,
                    values[left.0 as usize].unwrap(),
                    values[right.0 as usize].unwrap(),
                    value.span,
                )?,
                ValueKind::Call {
                    transform: callee,
                    arguments,
                } => {
                    let callee_transform = self.module.get(*callee);
                    let call_arguments = arguments
                        .iter()
                        .zip(&callee_transform.parameters)
                        .map(|(argument, _)| {
                            let scalar = values[argument.0 as usize].unwrap();
                            (freeze_scalar(scalar), value.span)
                        })
                        .collect::<Vec<_>>();
                    let result = self.invoke_at_depth(*callee, &call_arguments, depth + 1)?;
                    lower_scalar(&result, callee_transform.return_type, value.span)?
                }
            };
            values[id.0 as usize] = Some(evaluated);
        }
        match block.terminator {
            Terminator::Return(value) => Ok(freeze_scalar(values[value.0 as usize].unwrap())),
        }
    }
}

impl TransformEngine for IrInterpreter<'_> {
    fn invoke(
        &self,
        id: TransformId,
        arguments: Vec<(OuterValue, Span)>,
    ) -> Result<OuterValue, Diagnostic> {
        self.invoke_at_depth(id, &arguments, 0)
    }
}

struct NativeEngine<'a> {
    module: &'a crate::ir::TypedModule,
    native: &'a NativeModule,
}

impl TransformEngine for NativeEngine<'_> {
    fn invoke(
        &self,
        id: TransformId,
        arguments: Vec<(OuterValue, Span)>,
    ) -> Result<OuterValue, Diagnostic> {
        self.validate_path(id, &mut BTreeSet::new(), &mut BTreeSet::new())?;
        let transform = self.module.get(id);
        let mut lowered = Vec::with_capacity(arguments.len());
        for (parameter, (argument, span)) in transform.parameters.iter().zip(arguments) {
            lowered.push(lower_native_argument(argument, parameter.ty, span)?);
        }
        let abi_arguments = lowered
            .iter()
            .map(|argument| argument.abi)
            .collect::<Vec<_>>();
        let result = self
            .native
            .invoke(id, &abi_arguments)
            .map_err(|error| Diagnostic::error(error.to_string(), transform.span))?;
        freeze_native_result(result, transform.return_type, &mut lowered, transform.span)
    }
}

impl NativeEngine<'_> {
    fn validate_path(
        &self,
        id: TransformId,
        visiting: &mut BTreeSet<u32>,
        visited: &mut BTreeSet<u32>,
    ) -> Result<(), Diagnostic> {
        if visited.contains(&id.0) {
            return Ok(());
        }
        let transform = self.module.get(id);
        if !visiting.insert(id.0) {
            return Err(Diagnostic::error(
                "recursive transform calls are not executable in the initial native runtime",
                transform.span,
            )
            .with_note("the reference interpreter retains a bounded recursion guard"));
        }
        for value in &transform.values {
            match &value.kind {
                ValueKind::Binary { .. } if value.ty == Type::I64 => {
                    return Err(
                        Diagnostic::error(
                            "i64 arithmetic is not executable in the initial native runtime",
                            value.span,
                        )
                        .with_note(
                            "Tima integer overflow and division-error semantics must be chosen before mapping them to C",
                        ),
                    );
                }
                ValueKind::Call {
                    transform: callee, ..
                } => self.validate_path(*callee, visiting, visited)?,
                _ => {}
            }
        }
        visiting.remove(&id.0);
        visited.insert(id.0);
        Ok(())
    }
}

struct LoweredArgument {
    abi: AbiValue,
    keep_alive: BoundaryStorage,
}

enum BoundaryStorage {
    Scalar,
    Owned(Option<OwnedImage>),
    View(Arc<ImageValue>),
}

struct OwnedImage {
    storage: ImageStorage,
}

fn lower_native_argument(
    value: OuterValue,
    expected: Type,
    span: Span,
) -> Result<LoweredArgument, Diagnostic> {
    match (expected, value.data) {
        (Type::Bool, ValueData::Bool(value)) => Ok(LoweredArgument {
            abi: AbiValue {
                boolean: u8::from(value),
            },
            keep_alive: BoundaryStorage::Scalar,
        }),
        (Type::I64, ValueData::Integer(value)) => Ok(LoweredArgument {
            abi: AbiValue { i64_value: value },
            keep_alive: BoundaryStorage::Scalar,
        }),
        (Type::F32, ValueData::Float(value)) => Ok(LoweredArgument {
            abi: AbiValue { f32_value: value },
            keep_alive: BoundaryStorage::Scalar,
        }),
        (Type::Image, ValueData::Image(image)) => {
            let image = Arc::try_unwrap(image).unwrap_or_else(|shared| (*shared).clone());
            let ImageValue {
                storage,
                width,
                height,
                stride,
            } = image;
            let mut storage = Arc::try_unwrap(storage)
                .unwrap_or_else(|shared| ImageStorage::new(shared.bytes.clone()));
            let abi = AbiValue {
                image: AbiImage {
                    data: storage.bytes.as_mut_ptr(),
                    width,
                    height,
                    stride,
                },
            };
            Ok(LoweredArgument {
                abi,
                keep_alive: BoundaryStorage::Owned(Some(OwnedImage { storage })),
            })
        }
        (Type::ImageView, ValueData::Image(image)) => Ok(LoweredArgument {
            abi: AbiValue {
                image_view: AbiImageView {
                    data: image.storage.bytes.as_ptr(),
                    width: image.width,
                    height: image.height,
                    stride: image.stride,
                },
            },
            keep_alive: BoundaryStorage::View(image),
        }),
        (expected, _) => Err(Diagnostic::error(
            format!(
                "outer value cannot cross into native parameter type {}",
                expected.name()
            ),
            span,
        )),
    }
}

fn freeze_native_result(
    result: AbiValue,
    ty: Type,
    arguments: &mut [LoweredArgument],
    span: Span,
) -> Result<OuterValue, Diagnostic> {
    // SAFETY: the generated adapter writes the union field selected by the
    // statically checked transform return type.
    unsafe {
        match ty {
            Type::Bool => Ok(freeze_scalar(NativeScalar::Bool(result.boolean != 0))),
            Type::I64 => Ok(freeze_scalar(NativeScalar::I64(result.i64_value))),
            Type::F32 => Ok(freeze_scalar(NativeScalar::F32(result.f32_value))),
            Type::Image => freeze_owned_image(result.image, arguments, span),
            Type::ImageView => freeze_image_view(result.image_view, arguments, span),
        }
    }
}

fn freeze_owned_image(
    returned: AbiImage,
    arguments: &mut [LoweredArgument],
    span: Span,
) -> Result<OuterValue, Diagnostic> {
    for argument in arguments {
        let BoundaryStorage::Owned(owned) = &mut argument.keep_alive else {
            continue;
        };
        let Some(candidate) = owned.as_ref() else {
            continue;
        };
        if candidate.storage.bytes.as_ptr() != returned.data.cast_const() {
            continue;
        }
        validate_returned_layout(
            returned.width,
            returned.height,
            returned.stride,
            candidate.storage.bytes.len(),
            span,
        )?;
        let owned = owned.take().expect("matched owned image remains available");
        return Ok(OuterValue::image(ImageValue {
            storage: Arc::new(owned.storage),
            width: returned.width,
            height: returned.height,
            stride: returned.stride,
        }));
    }
    Err(Diagnostic::error(
        "returned owned image does not reference storage acquired by this invocation",
        span,
    )
    .with_note("native allocation will require an explicit runtime allocator capability"))
}

fn freeze_image_view(
    returned: AbiImageView,
    arguments: &mut [LoweredArgument],
    span: Span,
) -> Result<OuterValue, Diagnostic> {
    for argument in arguments {
        let BoundaryStorage::View(image) = &argument.keep_alive else {
            continue;
        };
        if image.storage.bytes.as_ptr() != returned.data {
            continue;
        }
        validate_returned_layout(
            returned.width,
            returned.height,
            returned.stride,
            image.storage.bytes.len(),
            span,
        )?;
        return Ok(OuterValue::image(ImageValue {
            storage: image.storage.clone(),
            width: returned.width,
            height: returned.height,
            stride: returned.stride,
        }));
    }
    Err(Diagnostic::error(
        "returned image view does not reference a live input view",
        span,
    ))
}

fn validate_returned_layout(
    _width: usize,
    height: usize,
    stride: usize,
    byte_len: usize,
    span: Span,
) -> Result<(), Diagnostic> {
    validate_image_layout(height, stride, byte_len).map_err(|error| {
        Diagnostic::error(
            format!("native transform returned an invalid image: {error}"),
            span,
        )
    })
}

fn lower_scalar(
    value: &OuterValue,
    expected: Type,
    span: Span,
) -> Result<NativeScalar, Diagnostic> {
    match (expected, &value.data) {
        (Type::Bool, ValueData::Bool(value)) => Ok(NativeScalar::Bool(*value)),
        (Type::I64, ValueData::Integer(value)) => Ok(NativeScalar::I64(*value)),
        (Type::F32, ValueData::Float(value)) => Ok(NativeScalar::F32(*value)),
        (Type::Image | Type::ImageView, _) => Err(
            Diagnostic::error(
                format!(
                    "{} cannot cross into the transform boundary yet",
                    expected.name()
                ),
                span,
            )
            .with_note("owned detach/view acquisition and freeze-on-return are reserved for the native boundary milestone"),
        ),
        _ => Err(Diagnostic::error(
            format!("outer value cannot cross into native parameter type {}", expected.name()),
            span,
        )),
    }
}

fn freeze_scalar(value: NativeScalar) -> OuterValue {
    OuterValue::plain(match value {
        NativeScalar::Bool(value) => ValueData::Bool(value),
        NativeScalar::I64(value) => ValueData::Integer(value),
        NativeScalar::F32(value) => ValueData::Float(value),
    })
}

fn native_binary(
    op: BinaryOp,
    left: NativeScalar,
    right: NativeScalar,
    span: Span,
) -> Result<NativeScalar, Diagnostic> {
    match (left, right) {
        (NativeScalar::I64(left), NativeScalar::I64(right)) => {
            Ok(NativeScalar::I64(integer_binary(op, left, right, span)?))
        }
        (NativeScalar::F32(left), NativeScalar::F32(right)) => Ok(NativeScalar::F32(match op {
            BinaryOp::Add => left + right,
            BinaryOp::Subtract => left - right,
            BinaryOp::Multiply => left * right,
            BinaryOp::Divide => left / right,
        })),
        _ => unreachable!("typed IR guarantees matching numeric operands"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    use crate::backend::NativeBackend;
    use crate::backend::c::CBackend;
    use crate::backend::native::{ClangCompiler, NativeModule};
    use crate::ir::{TransformId, Type};
    use crate::runtime::{
        ImageValue, OuterValue, ValueData, execute, execute_native, execute_native_with_bindings,
        invoke_native_transform, lower_native_argument,
    };
    use crate::source::Span;

    #[test]
    fn runs_a_scalar_transform_through_a_pipeline() {
        let compiled = crate::compile(
            "test.tima",
            "transform scale(x: f32, factor: f32) -> f32 { return x * factor }\nout = 8.0 | scale(factor=0.25)\n",
        )
        .unwrap();
        let execution = execute(&compiled).unwrap();
        assert_eq!(execution.bindings["out"].data, ValueData::Float(2.0));
    }

    #[test]
    fn outer_composites_are_shared_but_not_mutable() {
        let compiled = crate::compile("test.tima", "x = [1, {name: \"cat\"}]\ny = x\n").unwrap();
        let execution = execute(&compiled).unwrap();
        let ValueData::List(left) = &execution.bindings["x"].data else {
            panic!("expected list")
        };
        let ValueData::List(right) = &execution.bindings["y"].data else {
            panic!("expected list")
        };
        assert!(Arc::ptr_eq(left, right));
    }

    #[test]
    fn asset_is_a_logical_locator_not_eager_io() {
        let compiled = crate::compile("test.tima", "img = asset(\"cat.png\")\n").unwrap();
        let execution = execute(&compiled).unwrap();
        let ValueData::Asset(asset) = &execution.bindings["img"].data else {
            panic!("expected asset")
        };
        assert_eq!(&*asset.locator, "cat.png");
    }

    #[test]
    fn compiles_loads_and_runs_nested_transforms_as_native_code() {
        let compiled = crate::compile(
            "test.tima",
            "transform double(x: f32) -> f32 { return x * 2.0 }\n\
             transform scale_twice(x: f32, factor: f32) -> f32 { return double(x * factor) }\n\
             transform keep_i64(x: i64) -> i64 { return x }\n\
             transform keep_bool(x: bool) -> bool { return x }\n\
             out = 8.0 | scale_twice(factor=0.25)\n\
             count = keep_i64(7)\n\
             flag = keep_bool(true)\n",
        )
        .unwrap();
        let generated = CBackend.emit(&compiled.transforms).unwrap();
        let build_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("build");
        let artifact = ClangCompiler::default()
            .compile(&generated, build_root)
            .unwrap();
        assert!(artifact.compiler_version.contains("clang"));
        assert!(!artifact.target.is_empty());
        assert_ne!(
            artifact.identity(compiled.identities.get(TransformId(0))),
            artifact.identity(compiled.identities.get(TransformId(1)))
        );
        let native = NativeModule::load(&artifact, &compiled.transforms).unwrap();
        let execution = execute_native(&compiled, &native).unwrap();
        assert_eq!(execution.bindings["out"].data, ValueData::Float(4.0));
        assert_eq!(execution.bindings["count"].data, ValueData::Integer(7));
        assert_eq!(execution.bindings["flag"].data, ValueData::Bool(true));
    }

    #[test]
    fn native_runtime_defers_unspecified_i64_arithmetic_semantics() {
        let compiled = crate::compile(
            "test.tima",
            "transform add(x: i64, y: i64) -> i64 { return x + y }\n\
             out = add(1, 2)\n",
        )
        .unwrap();
        let generated = CBackend.emit(&compiled.transforms).unwrap();
        let build_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("build");
        let artifact = ClangCompiler::default()
            .compile(&generated, build_root)
            .unwrap();
        let native = NativeModule::load(&artifact, &compiled.transforms).unwrap();
        let diagnostics = execute_native(&compiled, &native).unwrap_err();
        assert!(diagnostics[0].message.contains("i64 arithmetic"));
    }

    #[test]
    fn owned_image_arguments_detach_when_storage_is_shared() {
        let image = OuterValue::image(ImageValue::new(2, 2, 2, vec![1, 2, 3, 4]).unwrap());
        let original_pointer = match &image.data {
            ValueData::Image(image) => image.storage.bytes.as_ptr(),
            _ => unreachable!(),
        };
        let retained_outer_alias = image.clone();
        let first = lower_native_argument(image.clone(), Type::Image, Span::default()).unwrap();
        let second = lower_native_argument(image, Type::Image, Span::default()).unwrap();
        // SAFETY: each union was initialized with its `image` field.
        let first_pointer = unsafe { first.abi.image.data.cast_const() };
        // SAFETY: each union was initialized with its `image` field.
        let second_pointer = unsafe { second.abi.image.data.cast_const() };
        assert_ne!(first_pointer, original_pointer);
        assert_ne!(second_pointer, original_pointer);
        assert_ne!(first_pointer, second_pointer);
        drop(retained_outer_alias);
    }

    #[test]
    fn image_views_alias_shared_storage_without_copying() {
        let image = OuterValue::image(ImageValue::new(2, 2, 2, vec![1, 2, 3, 4]).unwrap());
        let original_pointer = match &image.data {
            ValueData::Image(image) => image.storage.bytes.as_ptr(),
            _ => unreachable!(),
        };
        let first = lower_native_argument(image.clone(), Type::ImageView, Span::default()).unwrap();
        let second = lower_native_argument(image, Type::ImageView, Span::default()).unwrap();
        // SAFETY: each union was initialized with its `image_view` field.
        assert_eq!(unsafe { first.abi.image_view.data }, original_pointer);
        // SAFETY: each union was initialized with its `image_view` field.
        assert_eq!(unsafe { second.abi.image_view.data }, original_pointer);
    }

    #[test]
    fn owned_image_detaches_from_a_simultaneous_view() {
        let image = OuterValue::image(ImageValue::new(2, 2, 2, vec![1, 2, 3, 4]).unwrap());
        let view = lower_native_argument(image.clone(), Type::ImageView, Span::default()).unwrap();
        let owned = lower_native_argument(image, Type::Image, Span::default()).unwrap();
        // SAFETY: the unions were initialized with the fields read here.
        let view_pointer = unsafe { view.abi.image_view.data };
        // SAFETY: the unions were initialized with the fields read here.
        let owned_pointer = unsafe { owned.abi.image.data.cast_const() };
        assert_ne!(owned_pointer, view_pointer);
    }

    #[test]
    fn native_image_boundary_detaches_owned_values_and_freezes_returns() {
        let compiled = crate::compile(
            "test.tima",
            "transform own(img: Image) -> Image { return img }\n\
             transform view(img: ImageView) -> ImageView { return img }\n\
             owned = img | own\n\
             viewed = img | view\n",
        )
        .unwrap();
        let generated = CBackend.emit(&compiled.transforms).unwrap();
        let build_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("build");
        let artifact = ClangCompiler::default()
            .compile(&generated, build_root)
            .unwrap();
        let native = NativeModule::load(&artifact, &compiled.transforms).unwrap();
        let input = OuterValue::image(ImageValue::new(2, 2, 2, vec![1, 2, 3, 4]).unwrap());
        let execution = execute_native_with_bindings(
            &compiled,
            &native,
            BTreeMap::from([("img".to_owned(), input)]),
        )
        .unwrap();
        let ValueData::Image(original) = &execution.bindings["img"].data else {
            panic!("expected input image")
        };
        let ValueData::Image(owned) = &execution.bindings["owned"].data else {
            panic!("expected owned result image")
        };
        let ValueData::Image(viewed) = &execution.bindings["viewed"].data else {
            panic!("expected viewed result image")
        };
        assert!(!original.shares_storage_with(owned));
        assert!(original.shares_storage_with(viewed));
        assert_eq!(original.bytes(), owned.bytes());
        assert_eq!(original.bytes(), viewed.bytes());
    }

    #[test]
    fn unique_owned_image_storage_transfers_without_copying() {
        let compiled = crate::compile(
            "test.tima",
            "transform own(img: Image) -> Image { return img }\n",
        )
        .unwrap();
        let generated = CBackend.emit(&compiled.transforms).unwrap();
        let build_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("build");
        let artifact = ClangCompiler::default()
            .compile(&generated, build_root)
            .unwrap();
        let native = NativeModule::load(&artifact, &compiled.transforms).unwrap();
        let image = ImageValue::new(2, 2, 2, vec![1, 2, 3, 4]).unwrap();
        let original_pointer = image.storage.bytes.as_ptr();
        let result = invoke_native_transform(
            &compiled,
            &native,
            crate::ir::TransformId(0),
            vec![OuterValue::image(image)],
        )
        .unwrap();
        let ValueData::Image(result) = result.data else {
            panic!("expected image result")
        };
        assert_eq!(result.storage.bytes.as_ptr(), original_pointer);
        assert_eq!(result.bytes(), &[1, 2, 3, 4]);
    }

    #[test]
    fn validates_outer_image_layouts() {
        let error = ImageValue::new(2, 2, 2, vec![0; 3]).unwrap_err();
        assert!(error.to_string().contains("require 4"));
    }
}
