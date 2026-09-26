use std::collections::BTreeMap;
use std::sync::Arc;

use crate::CompiledProgram;
use crate::ast::{Argument, BinaryOp, ExprId, ExprKind, Item};
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
    Transform(TransformId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetValue {
    /// A locator is not a content identity. Asset observation/materialization
    /// will record content identity and source lineage in a later milestone.
    pub locator: Arc<str>,
}

#[derive(Clone, Debug, Default)]
pub struct Execution {
    pub bindings: BTreeMap<String, OuterValue>,
    pub last_value: Option<OuterValue>,
}

pub fn execute(program: &CompiledProgram) -> Result<Execution, Vec<Diagnostic>> {
    Interpreter {
        program,
        execution: Execution::default(),
    }
    .run()
    .map_err(|diagnostic| vec![diagnostic])
}

struct Interpreter<'a> {
    program: &'a CompiledProgram,
    execution: Execution,
}

impl<'a> Interpreter<'a> {
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
        IrInterpreter {
            module: &self.program.transforms,
        }
        .invoke(id, &values, 0)
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
    fn invoke(
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
                    let result = self.invoke(*callee, &call_arguments, depth + 1)?;
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
                    "{} cannot cross into the interpreted transform boundary yet",
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
    use std::sync::Arc;

    use crate::runtime::{ValueData, execute};

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
}
