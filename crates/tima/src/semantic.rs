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
    environment: BTreeMap<String, (ValueId, crate::source::Span)>,
    values: Vec<Value>,
    blocks: Vec<PendingBlock>,
    current_block: BlockId,
    diagnostics: Vec<Diagnostic>,
}

struct PendingBlock {
    instructions: Vec<ValueId>,
    terminator: Option<Terminator>,
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
            blocks: vec![PendingBlock {
                instructions: Vec::new(),
                terminator: None,
            }],
            current_block: BlockId(0),
            diagnostics: Vec::new(),
        }
    }

    fn lower(mut self) -> Result<Transform, Vec<Diagnostic>> {
        let signature = self.signatures[&self.declaration.name].clone();
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
            self.environment
                .insert(syntax.name.clone(), (value, syntax.name_span));
            parameters.push(Parameter {
                name: syntax.name.clone(),
                ty: *ty,
                value,
                span: syntax.span,
            });
        }

        let body = self.declaration.body.clone();
        if !self.lower_statements(&body) {
            self.diagnostics.push(
                Diagnostic::error("transform body must return a value", self.declaration.span)
                    .with_note("a transform body must end with `return` or a fully returning `if`"),
            );
        }

        if !self.diagnostics.is_empty() {
            return Err(self.diagnostics);
        }
        let blocks = self
            .blocks
            .into_iter()
            .map(|block| BasicBlock {
                instructions: block.instructions,
                terminator: block
                    .terminator
                    .expect("valid lowered blocks are terminated"),
            })
            .collect();
        Ok(Transform {
            name: self.declaration.name.clone(),
            parameters,
            return_type: signature.return_type,
            values: self.values,
            blocks,
            entry: BlockId(0),
            span: self.declaration.span,
        })
    }

    fn lower_statements(&mut self, statements: &[InnerStmt]) -> bool {
        let mut terminated = false;
        for statement in statements {
            if terminated {
                self.diagnostics.push(Diagnostic::error(
                    "inner statements cannot appear after `return` or a terminating `if`",
                    inner_statement_span(statement),
                ));
                continue;
            }
            match statement {
                InnerStmt::Binding(binding) => self.local_binding(binding),
                InnerStmt::Return { value, .. } => {
                    terminated = true;
                    if let Some(value) = self.expression(*value) {
                        self.check_return_type(value);
                        self.terminate(Terminator::Return(value));
                    }
                }
                InnerStmt::If {
                    condition,
                    then_body,
                    else_body,
                    ..
                } => {
                    terminated = self.conditional(*condition, then_body, else_body);
                }
            }
        }
        terminated
    }

    fn local_binding(&mut self, binding: &ast::Binding) {
        if let Some((_, previous_span)) = self.environment.get(&binding.name) {
            self.diagnostics.push(
                Diagnostic::error(
                    format!(
                        "inner value `{}` is already defined in this transform",
                        binding.name
                    ),
                    binding.name_span,
                )
                .with_label(*previous_span, "previous definition is here")
                .with_note("inner local bindings are immutable and cannot shadow"),
            );
            return;
        }
        if let Some(value) = self.expression(binding.value) {
            self.environment
                .insert(binding.name.clone(), (value, binding.name_span));
        }
    }

    fn conditional(
        &mut self,
        condition: ExprId,
        then_body: &[InnerStmt],
        else_body: &[InnerStmt],
    ) -> bool {
        let Some(condition) = self.expression(condition) else {
            return true;
        };
        let condition_type = self.values[condition.0 as usize].ty;
        if condition_type != Type::Bool {
            self.diagnostics.push(Diagnostic::error(
                format!(
                    "inner `if` condition must be bool, not {}",
                    condition_type.name()
                ),
                self.values[condition.0 as usize].span,
            ));
            return true;
        }

        let then_block = self.new_block();
        let else_block = self.new_block();
        self.terminate(Terminator::Branch {
            condition,
            then_block,
            else_block,
        });

        let outer_environment = self.environment.clone();
        self.current_block = then_block;
        self.environment = outer_environment.clone();
        let then_returns = self.lower_statements(then_body);
        let then_end = self.current_block;

        self.current_block = else_block;
        self.environment = outer_environment.clone();
        let else_returns = self.lower_statements(else_body);
        let else_end = self.current_block;
        self.environment = outer_environment;

        if then_returns && else_returns {
            return true;
        }

        let continuation = self.new_block();
        if !then_returns {
            self.current_block = then_end;
            self.terminate(Terminator::Jump(continuation));
        }
        if !else_returns {
            self.current_block = else_end;
            self.terminate(Terminator::Jump(continuation));
        }
        self.current_block = continuation;
        false
    }

    fn check_return_type(&mut self, value: ValueId) {
        let actual = self.values[value.0 as usize].ty;
        let expected = self.signatures[&self.declaration.name].return_type;
        if actual != expected {
            self.diagnostics.push(Diagnostic::error(
                format!(
                    "transform `{}` returns {}, but its declared result is {}",
                    self.declaration.name,
                    actual.name(),
                    expected.name()
                ),
                self.values[value.0 as usize].span,
            ));
        }
    }

    fn new_block(&mut self) -> BlockId {
        let id = BlockId(self.blocks.len() as u32);
        self.blocks.push(PendingBlock {
            instructions: Vec::new(),
            terminator: None,
        });
        id
    }

    fn terminate(&mut self, terminator: Terminator) {
        self.blocks[self.current_block.0 as usize].terminator = Some(terminator);
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
                Some((value, _)) => Some(value),
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
            self.blocks[self.current_block.0 as usize]
                .instructions
                .push(id);
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

fn inner_statement_span(statement: &InnerStmt) -> crate::source::Span {
    match statement {
        InnerStmt::Binding(binding) => binding.span,
        InnerStmt::Return { span, .. } | InnerStmt::If { span, .. } => *span,
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
    fn infers_and_lowers_immutable_inner_local_bindings() {
        let compiled = compile(
            "test.tima",
            "transform adjusted(x: f32) -> f32 {\n\
                 doubled = x * 2.0\n\
                 result = doubled + 1.0\n\
                 return result\n\
             }\n",
        )
        .unwrap();
        let transform = &compiled.transforms.transforms[0];
        assert_eq!(transform.blocks[0].instructions.len(), 4);
        assert!(matches!(
            transform.blocks[0].terminator,
            ir::Terminator::Return(ir::ValueId(4))
        ));
    }

    #[test]
    fn lowers_terminating_conditionals_to_backend_neutral_cfg() {
        let compiled = compile(
            "test.tima",
            "transform choose(flag: bool, left: f32, right: f32) -> f32 {\n\
                 if flag {\n\
                     selected = left\n\
                     return selected\n\
                 } else {\n\
                     selected = right\n\
                     return selected\n\
                 }\n\
             }\n",
        )
        .unwrap();
        let transform = &compiled.transforms.transforms[0];
        assert_eq!(transform.blocks.len(), 3);
        assert!(matches!(
            transform.blocks[0].terminator,
            ir::Terminator::Branch {
                condition: ir::ValueId(0),
                then_block: ir::BlockId(1),
                else_block: ir::BlockId(2),
            }
        ));
        assert!(matches!(
            transform.blocks[1].terminator,
            ir::Terminator::Return(ir::ValueId(1))
        ));
        assert!(matches!(
            transform.blocks[2].terminator,
            ir::Terminator::Return(ir::ValueId(2))
        ));
    }

    #[test]
    fn rejects_non_bool_conditions_and_missing_function_returns() {
        let non_bool = compile(
            "test.tima",
            "transform bad(value: f32) -> f32 {\n\
                 if value { return value } else { return value }\n\
             }\n",
        )
        .unwrap_err();
        assert!(
            non_bool
                .iter()
                .any(|diagnostic| diagnostic.message.contains("condition must be bool"))
        );

        let missing_return = compile(
            "test.tima",
            "transform bad(flag: bool, value: f32) -> f32 {\n\
                 if flag { selected = value } else { return value }\n\
             }\n",
        )
        .unwrap_err();
        assert!(
            missing_return
                .iter()
                .any(|diagnostic| diagnostic.message.contains("must return a value"))
        );
    }

    #[test]
    fn lowers_early_return_and_fallthrough_to_a_cfg_join() {
        let compiled = compile(
            "test.tima",
            "transform choose(flag: bool, left: f32, right: f32) -> f32 {\n\
                 if flag { return left } else {}\n\
                 return right\n\
             }\n",
        )
        .unwrap();
        let transform = &compiled.transforms.transforms[0];
        assert_eq!(transform.blocks.len(), 4);
        assert!(matches!(
            transform.blocks[0].terminator,
            ir::Terminator::Branch {
                then_block: ir::BlockId(1),
                else_block: ir::BlockId(2),
                ..
            }
        ));
        assert!(matches!(
            transform.blocks[1].terminator,
            ir::Terminator::Return(ir::ValueId(1))
        ));
        assert!(matches!(
            transform.blocks[2].terminator,
            ir::Terminator::Jump(ir::BlockId(3))
        ));
        assert!(matches!(
            transform.blocks[3].terminator,
            ir::Terminator::Return(ir::ValueId(2))
        ));
    }

    #[test]
    fn branch_local_bindings_do_not_escape_a_join() {
        let diagnostics = compile(
            "test.tima",
            "transform bad(flag: bool, left: f32, right: f32) -> f32 {\n\
                 if flag { selected = left } else { selected = right }\n\
                 return selected\n\
             }\n",
        )
        .unwrap_err();
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("unknown inner value `selected`")
        }));
    }

    #[test]
    fn rejects_inner_shadowing_and_statements_after_return() {
        let diagnostics = compile(
            "test.tima",
            "transform bad(x: f32) -> f32 {\n\
                 x = 2.0\n\
                 return x\n\
                 later = 3.0\n\
             }\n",
        )
        .unwrap_err();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("already defined"))
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("after `return`"))
        );
    }

    #[test]
    fn requires_a_return_after_inner_local_bindings() {
        let diagnostics = compile(
            "test.tima",
            "transform bad(x: f32) -> f32 {\n result = x * 2.0\n}\n",
        )
        .unwrap_err();
        assert!(diagnostics[0].message.contains("must return a value"));
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
