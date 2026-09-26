use std::collections::BTreeMap;

use crate::ast::{self, ExprId, ExprKind, InnerStmt, Item};
use crate::diagnostic::Diagnostic;
use crate::ir::{
    BasicBlock, BlockId, Constant, Parameter, RuntimeCall, Terminator, Transform, TransformId,
    Type, TypedModule, Value, ValueId, ValueKind,
};

pub fn check(program: &ast::Program) -> Result<TypedModule, Vec<Diagnostic>> {
    Checker::new(program).run()
}

#[derive(Clone)]
struct Signature {
    id: TransformId,
    parameters: Vec<(String, Type)>,
    return_type: Type,
}

struct Checker<'a> {
    program: &'a ast::Program,
    signatures: BTreeMap<String, Signature>,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Checker<'a> {
    fn new(program: &'a ast::Program) -> Self {
        Self {
            program,
            signatures: BTreeMap::new(),
            diagnostics: Vec::new(),
        }
    }

    fn run(mut self) -> Result<TypedModule, Vec<Diagnostic>> {
        self.collect_signatures();
        if !self.diagnostics.is_empty() {
            return Err(self.diagnostics);
        }

        let mut transforms = Vec::new();
        for item in &self.program.items {
            if let Item::Transform(declaration) = item {
                match Lowerer::new(self.program, &self.signatures, declaration).lower() {
                    Ok(transform) => transforms.push(transform),
                    Err(mut diagnostics) => self.diagnostics.append(&mut diagnostics),
                }
            }
        }
        if self.diagnostics.is_empty() {
            Ok(TypedModule { transforms })
        } else {
            Err(self.diagnostics)
        }
    }

    fn collect_signatures(&mut self) {
        for item in &self.program.items {
            let Item::Transform(declaration) = item else {
                continue;
            };
            if declaration.name == "environment_i64" {
                self.diagnostics.push(
                    Diagnostic::error(
                        "`environment_i64` is reserved for the runtime capability call",
                        declaration.name_span,
                    )
                    .with_note("runtime capability builtins cannot be redefined as transforms"),
                );
                continue;
            }
            if let Some(previous) = self.signatures.get(&declaration.name) {
                let earlier = self
                    .program
                    .items
                    .iter()
                    .filter_map(|item| match item {
                        Item::Transform(item) if item.name == declaration.name => {
                            Some(item.name_span)
                        }
                        _ => None,
                    })
                    .next()
                    .unwrap();
                self.diagnostics.push(
                    Diagnostic::error(
                        format!(
                            "transform `{}` is declared more than once",
                            declaration.name
                        ),
                        declaration.name_span,
                    )
                    .with_label(earlier, "first declaration is here")
                    .with_note(format!("existing transform index is {}", previous.id.0)),
                );
                continue;
            }
            let Some(return_type) = self.resolve_type(&declaration.return_type) else {
                continue;
            };
            let mut parameters = Vec::new();
            let mut names = BTreeMap::new();
            let mut valid = true;
            for parameter in &declaration.parameters {
                if let Some(previous_span) =
                    names.insert(parameter.name.clone(), parameter.name_span)
                {
                    self.diagnostics.push(
                        Diagnostic::error(
                            format!("parameter `{}` is declared more than once", parameter.name),
                            parameter.name_span,
                        )
                        .with_label(previous_span, "first parameter is here"),
                    );
                    valid = false;
                }
                if let Some(ty) = self.resolve_type(&parameter.ty) {
                    parameters.push((parameter.name.clone(), ty));
                } else {
                    valid = false;
                }
            }
            if valid {
                let id = TransformId(self.signatures.len() as u32);
                self.signatures.insert(
                    declaration.name.clone(),
                    Signature {
                        id,
                        parameters,
                        return_type,
                    },
                );
            }
        }
    }

    fn resolve_type(&mut self, reference: &ast::TypeRef) -> Option<Type> {
        let ty = match reference.name.as_str() {
            "bool" => Type::Bool,
            "i64" => Type::I64,
            "f32" => Type::F32,
            "Image" => Type::Image,
            "ImageView" => Type::ImageView,
            _ => {
                self.diagnostics.push(
                    Diagnostic::error(
                        format!("unknown native transform type `{}`", reference.name),
                        reference.span,
                    )
                    .with_note(
                        "the initial native-safe types are bool, i64, f32, Image, and ImageView",
                    ),
                );
                return None;
            }
        };
        Some(ty)
    }
}

struct Lowerer<'a> {
    program: &'a ast::Program,
    signatures: &'a BTreeMap<String, Signature>,
    declaration: &'a ast::TransformDecl,
    environment: BTreeMap<String, ValueId>,
    values: Vec<Value>,
    instructions: Vec<ValueId>,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Lowerer<'a> {
    fn new(
        program: &'a ast::Program,
        signatures: &'a BTreeMap<String, Signature>,
        declaration: &'a ast::TransformDecl,
    ) -> Self {
        Self {
            program,
            signatures,
            declaration,
            environment: BTreeMap::new(),
            values: Vec::new(),
            instructions: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    fn lower(mut self) -> Result<Transform, Vec<Diagnostic>> {
        let signature = &self.signatures[&self.declaration.name];
        let mut parameters = Vec::new();
        for (index, (syntax, (_, ty))) in self
            .declaration
            .parameters
            .iter()
            .zip(&signature.parameters)
            .enumerate()
        {
            let value = self.alloc(
                *ty,
                ValueKind::Parameter {
                    index: index as u32,
                },
                syntax.span,
                false,
            );
            self.environment.insert(syntax.name.clone(), value);
            parameters.push(Parameter {
                name: syntax.name.clone(),
                ty: *ty,
                value,
                span: syntax.span,
            });
        }

        let return_value = match self.declaration.body.as_slice() {
            [InnerStmt::Return { value, .. }] => self.expression(*value),
            [] => {
                self.diagnostics.push(Diagnostic::error(
                    "transform body must return a value",
                    self.declaration.span,
                ));
                None
            }
            [_, second, ..] => {
                let span = match second {
                    InnerStmt::Return { span, .. } => *span,
                };
                self.diagnostics.push(
                    Diagnostic::error(
                        "the initial transform subset accepts exactly one return statement",
                        span,
                    )
                    .with_note(
                        "control flow and local statements will be added on top of the typed CFG",
                    ),
                );
                None
            }
        };

        if let Some(return_value) = return_value {
            let actual = self.values[return_value.0 as usize].ty;
            if actual != signature.return_type {
                self.diagnostics.push(Diagnostic::error(
                    format!(
                        "transform `{}` returns {}, but its declared result is {}",
                        self.declaration.name,
                        actual.name(),
                        signature.return_type.name()
                    ),
                    self.values[return_value.0 as usize].span,
                ));
            }
        }

        if !self.diagnostics.is_empty() {
            return Err(self.diagnostics);
        }
        let return_value = return_value.expect("no diagnostics implies return value");
        Ok(Transform {
            name: self.declaration.name.clone(),
            parameters,
            return_type: signature.return_type,
            values: self.values,
            blocks: vec![BasicBlock {
                instructions: self.instructions,
                terminator: Terminator::Return(return_value),
            }],
            entry: BlockId(0),
            span: self.declaration.span,
        })
    }

    fn expression(&mut self, id: ExprId) -> Option<ValueId> {
        let expression = self.program.expr(id);
        match &expression.kind {
            ExprKind::Bool(value) => Some(self.alloc(
                Type::Bool,
                ValueKind::Constant(Constant::Bool(*value)),
                expression.span,
                true,
            )),
            ExprKind::Integer(value) => Some(self.alloc(
                Type::I64,
                ValueKind::Constant(Constant::I64(*value)),
                expression.span,
                true,
            )),
            ExprKind::Float(value) => Some(self.alloc(
                Type::F32,
                ValueKind::Constant(Constant::F32(*value as f32)),
                expression.span,
                true,
            )),
            ExprKind::Name(name) => match self.environment.get(name).copied() {
                Some(value) => Some(value),
                None => {
                    self.diagnostics.push(Diagnostic::error(
                        format!("unknown inner value `{name}`"),
                        expression.span,
                    ));
                    None
                }
            },
            ExprKind::Binary { op, left, right } => {
                let left = self.expression(*left)?;
                let right = self.expression(*right)?;
                let left_type = self.values[left.0 as usize].ty;
                let right_type = self.values[right.0 as usize].ty;
                if left_type != right_type || !left_type.is_numeric() {
                    self.diagnostics.push(
                        Diagnostic::error(
                            "inner arithmetic requires operands of the same numeric type",
                            expression.span,
                        )
                        .with_note(format!(
                            "left is {}, right is {}",
                            left_type.name(),
                            right_type.name()
                        )),
                    );
                    return None;
                }
                Some(self.alloc(
                    left_type,
                    ValueKind::Binary {
                        op: *op,
                        left,
                        right,
                    },
                    expression.span,
                    true,
                ))
            }
            ExprKind::Call { callee, arguments } => {
                let ExprKind::Name(name) = &self.program.expr(*callee).kind else {
                    self.diagnostics.push(Diagnostic::error(
                        "inner calls require a directly named transform",
                        self.program.expr(*callee).span,
                    ));
                    return None;
                };
                if name == "environment_i64" {
                    return self.environment_i64(arguments, expression.span);
                }
                let Some(signature) = self.signatures.get(name) else {
                    self.diagnostics.push(Diagnostic::error(
                        format!("unknown inner transform `{name}`"),
                        self.program.expr(*callee).span,
                    ));
                    return None;
                };
                if signature
                    .parameters
                    .iter()
                    .any(|(_, ty)| *ty == Type::Image)
                {
                    self.diagnostics.push(
                        Diagnostic::error(
                            "owned Image arguments are not yet supported in inner-to-inner calls",
                            expression.span,
                        )
                        .with_note(
                            "move/detach lowering is required to preserve non-aliasing across nested native calls",
                        ),
                    );
                    return None;
                }
                if arguments.iter().any(|argument| argument.name.is_some()) {
                    self.diagnostics.push(Diagnostic::error(
                        "named arguments are not yet supported inside transforms",
                        expression.span,
                    ));
                    return None;
                }
                if arguments.len() != signature.parameters.len() {
                    self.diagnostics.push(Diagnostic::error(
                        format!(
                            "transform `{name}` expects {} arguments, but {} were supplied",
                            signature.parameters.len(),
                            arguments.len()
                        ),
                        expression.span,
                    ));
                    return None;
                }
                let mut lowered = Vec::new();
                for (argument, (_, expected)) in arguments.iter().zip(&signature.parameters) {
                    let value = self.expression(argument.value)?;
                    let actual = self.values[value.0 as usize].ty;
                    if actual != *expected {
                        self.diagnostics.push(Diagnostic::error(
                            format!(
                                "argument has type {}, but {} is required",
                                actual.name(),
                                expected.name()
                            ),
                            argument.span,
                        ));
                        return None;
                    }
                    lowered.push(value);
                }
                Some(self.alloc(
                    signature.return_type,
                    ValueKind::Call {
                        transform: signature.id,
                        arguments: lowered,
                    },
                    expression.span,
                    true,
                ))
            }
            ExprKind::Null
            | ExprKind::String(_)
            | ExprKind::List(_)
            | ExprKind::Record(_)
            | ExprKind::Pipeline { .. } => {
                self.diagnostics.push(
                    Diagnostic::error(
                        "outer-only value or syntax is not available inside a native transform",
                        expression.span,
                    )
                    .with_note("only native-safe typed values may cross the transform boundary"),
                );
                None
            }
        }
    }

    fn alloc(
        &mut self,
        ty: Type,
        kind: ValueKind,
        span: crate::source::Span,
        instruction: bool,
    ) -> ValueId {
        let id = ValueId(self.values.len() as u32);
        self.values.push(Value { ty, kind, span });
        if instruction {
            self.instructions.push(id);
        }
        id
    }

    fn environment_i64(
        &mut self,
        arguments: &[ast::Argument],
        span: crate::source::Span,
    ) -> Option<ValueId> {
        if arguments.len() != 1 || arguments[0].name.is_some() {
            self.diagnostics.push(
                Diagnostic::error(
                    "environment_i64 expects one positional string-literal name",
                    span,
                )
                .with_note("example: environment_i64(\"HISTIMA_SCALE\")"),
            );
            return None;
        }
        let argument = &arguments[0];
        let ExprKind::String(name) = &self.program.expr(argument.value).kind else {
            self.diagnostics.push(
                Diagnostic::error(
                    "environment_i64 requires a string-literal name",
                    argument.span,
                )
                .with_note("dynamic capability keys are intentionally not part of this slice"),
            );
            return None;
        };
        if name.is_empty() {
            self.diagnostics.push(Diagnostic::error(
                "environment_i64 requires a non-empty environment name",
                argument.span,
            ));
            return None;
        }
        Some(self.alloc(
            Type::I64,
            ValueKind::RuntimeCall(RuntimeCall::EnvironmentI64 { name: name.clone() }),
            span,
            true,
        ))
    }
}

#[cfg(test)]
mod tests {
    use crate::{compile, ir};

    #[test]
    fn lowers_checked_transform_to_backend_neutral_ir() {
        let compiled = compile(
            "test.tima",
            "transform scale(x: f32, factor: f32) -> f32 { return x * factor }\n",
        )
        .unwrap();
        let transform = &compiled.transforms.transforms[0];
        assert_eq!(transform.return_type, ir::Type::F32);
        assert!(matches!(
            transform.values[2].kind,
            ir::ValueKind::Binary { .. }
        ));
    }

    #[test]
    fn rejects_dynamic_values_in_inner_code() {
        let diagnostics =
            compile("test.tima", "transform bad(x: f32) -> f32 { return [x] }\n").unwrap_err();
        assert!(diagnostics[0].message.contains("outer-only"));
    }

    #[test]
    fn lowers_literal_environment_reads_to_typed_runtime_calls() {
        let compiled = compile(
            "test.tima",
            "transform configured() -> i64 { return environment_i64(\"MODE\") }\n",
        )
        .unwrap();
        assert!(matches!(
            &compiled.transforms.transforms[0].values[0].kind,
            ir::ValueKind::RuntimeCall(ir::RuntimeCall::EnvironmentI64 { name }) if name == "MODE"
        ));
    }

    #[test]
    fn rejects_dynamic_environment_keys() {
        let diagnostics = compile(
            "test.tima",
            "transform configured(key: i64) -> i64 { return environment_i64(key) }\n",
        )
        .unwrap_err();
        assert!(diagnostics[0].message.contains("string-literal name"));
    }

    #[test]
    fn rejects_owned_image_inner_calls_until_move_lowering_exists() {
        let diagnostics = compile(
            "test.tima",
            "transform choose(a: Image, b: Image) -> Image { return a }\n\
             transform unsafe_alias(x: Image) -> Image { return choose(x, x) }\n",
        )
        .unwrap_err();
        assert!(diagnostics[0].message.contains("owned Image arguments"));
    }
}
