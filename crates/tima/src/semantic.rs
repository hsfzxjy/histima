use std::collections::{BTreeMap, BTreeSet};

use crate::ast::{self, ExprId, ExprKind, InnerStmt, Item};
use crate::diagnostic::Diagnostic;
use crate::ir::{
    BasicBlock, BlockId, Constant, Parameter, Rgba8Channel, RuntimeCall, Terminator, Transform,
    TransformId, Type, TypedModule, Value, ValueId, ValueKind,
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
            if matches!(
                declaration.name.as_str(),
                "environment_i64" | "image_zero" | "image_fill"
            ) {
                self.diagnostics.push(
                    Diagnostic::error(
                        format!(
                            "`{}` is reserved for an inner runtime operation",
                            declaration.name
                        ),
                        declaration.name_span,
                    )
                    .with_note("inner builtins cannot be redefined as transforms"),
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
            "u8" => Type::U8,
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
                        "the initial native-safe types are bool, u8, i64, f32, Image, and ImageView",
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
    moved: BTreeSet<ValueId>,
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
            moved: BTreeSet::new(),
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
                InnerStmt::For {
                    binding,
                    binding_span,
                    iterable,
                    body,
                    span,
                } => self.image_byte_loop(binding, *binding_span, *iterable, body, *span),
                InnerStmt::Assignment { span, .. } => self.diagnostics.push(
                    Diagnostic::error(
                        "inner field assignment is only available inside an image pixel loop",
                        *span,
                    )
                    .with_note("example: for p in img.pixels { p.r *= factor }"),
                ),
            }
        }
        terminated
    }

    fn image_byte_loop(
        &mut self,
        binding: &str,
        binding_span: crate::source::Span,
        iterable: ExprId,
        body: &[InnerStmt],
        span: crate::source::Span,
    ) {
        if let Some((_, previous_span)) = self.environment.get(binding) {
            self.diagnostics.push(
                Diagnostic::error(
                    format!("inner loop binding `{binding}` would shadow an existing value"),
                    binding_span,
                )
                .with_label(*previous_span, "existing value is here")
                .with_note("inner bindings cannot shadow parameters or earlier locals"),
            );
            return;
        }

        let iterable_expression = self.program.expr(iterable);
        let ExprKind::Member {
            receiver,
            name,
            name_span,
        } = &iterable_expression.kind
        else {
            self.diagnostics.push(
                Diagnostic::error(
                    "initial inner `for` requires an owned image `.bytes` or `.pixels` iterator",
                    iterable_expression.span,
                )
                .with_note(
                    "examples: for byte in img.bytes { byte = value }; for p in img.pixels { p.r *= factor }",
                ),
            );
            return;
        };
        if name == "pixels" {
            self.image_pixel_loop(binding, binding_span, *receiver, body, span);
            return;
        }
        if name != "bytes" {
            self.diagnostics.push(
                Diagnostic::error(
                    format!("Image has no iterable member `{name}` in the initial language"),
                    *name_span,
                )
                .with_note("supported inner iterators are owned `Image.bytes` and `Image.pixels`"),
            );
            return;
        }
        let ExprKind::Name(image_name) = &self.program.expr(*receiver).kind else {
            self.diagnostics.push(
                Diagnostic::error(
                    "image byte iteration requires a directly named owned Image",
                    self.program.expr(*receiver).span,
                )
                .with_note("bind the owned image to a local before iterating it"),
            );
            return;
        };
        let Some(image) = self.expression(*receiver) else {
            return;
        };
        let actual_image = self.values[image.0 as usize].ty;
        if actual_image != Type::Image {
            self.diagnostics.push(
                Diagnostic::error(
                    format!(
                        "image byte iteration requires owned Image, not {}",
                        actual_image.name()
                    ),
                    iterable_expression.span,
                )
                .with_note("ImageView is read-only and may alias"),
            );
            return;
        }

        let [InnerStmt::Binding(assignment)] = body else {
            self.diagnostics.push(
                Diagnostic::error(
                    "initial image byte loop body must contain exactly one byte assignment",
                    span,
                )
                .with_note(format!(
                    "expected `{binding} = value`; general loop bodies are not implemented yet"
                )),
            );
            return;
        };
        if assignment.name != binding {
            self.diagnostics.push(
                Diagnostic::error(
                    format!(
                        "image byte loop must assign its `{binding}` binding, not `{}`",
                        assignment.name
                    ),
                    assignment.name_span,
                )
                .with_note("ordinary inner locals remain immutable"),
            );
            return;
        }
        let image_name_span = self.environment[image_name].1;
        if !expression_mentions_name(self.program, assignment.value, binding) {
            let Some(value) = self.expression(assignment.value) else {
                return;
            };
            if !self.require_byte_loop_result(value, assignment.value) {
                return;
            }
            self.moved.insert(image);
            let filled = self.alloc(
                Type::Image,
                ValueKind::ImageFill { image, value },
                span,
                true,
            );
            self.environment
                .insert(image_name.clone(), (filled, image_name_span));
            return;
        }

        let moved_before = self.moved.clone();
        self.moved.insert(image);
        let element = self.alloc(Type::U8, ValueKind::ImageByteElement, binding_span, false);
        self.environment
            .insert(binding.to_owned(), (element, binding_span));
        let instruction_start = self.blocks[self.current_block.0 as usize]
            .instructions
            .len();
        let value = self.expression(assignment.value);
        let instructions = self.blocks[self.current_block.0 as usize]
            .instructions
            .split_off(instruction_start);
        self.environment.remove(binding);
        let Some(value) = value else {
            return;
        };
        if !self.require_byte_loop_result(value, assignment.value) {
            return;
        }
        if self
            .moved
            .iter()
            .any(|moved| *moved != image && !moved_before.contains(moved))
        {
            self.diagnostics.push(
                Diagnostic::error(
                    "image byte loop expression cannot consume another owned value",
                    self.program.expr(assignment.value).span,
                )
                .with_note("loop-carried ownership for additional values is not implemented yet"),
            );
            return;
        }
        let mapped = self.alloc(
            Type::Image,
            ValueKind::ImageByteMap {
                image,
                element,
                instructions,
                result: value,
            },
            span,
            true,
        );
        self.environment
            .insert(image_name.clone(), (mapped, image_name_span));
    }

    fn image_pixel_loop(
        &mut self,
        binding: &str,
        _binding_span: crate::source::Span,
        receiver: ExprId,
        body: &[InnerStmt],
        span: crate::source::Span,
    ) {
        let ExprKind::Name(image_name) = &self.program.expr(receiver).kind else {
            self.diagnostics.push(
                Diagnostic::error(
                    "image pixel iteration requires a directly named owned Image",
                    self.program.expr(receiver).span,
                )
                .with_note("bind the owned image to a local before iterating it"),
            );
            return;
        };
        let Some(image) = self.expression(receiver) else {
            return;
        };
        let actual_image = self.values[image.0 as usize].ty;
        if actual_image != Type::Image {
            self.diagnostics.push(
                Diagnostic::error(
                    format!(
                        "image pixel iteration requires owned Image, not {}",
                        actual_image.name()
                    ),
                    self.program.expr(receiver).span,
                )
                .with_note("ImageView is read-only and may alias"),
            );
            return;
        }
        if body.is_empty() {
            self.diagnostics.push(
                Diagnostic::error("image pixel loop body cannot be empty", span)
                    .with_note("scale at least one channel, for example `p.r *= factor`"),
            );
            return;
        }

        let moved_before = self.moved.clone();
        let mut seen = BTreeSet::new();
        let mut channels = Vec::new();
        for statement in body {
            let InnerStmt::Assignment {
                target,
                op,
                value,
                span: assignment_span,
            } = statement
            else {
                self.diagnostics.push(
                    Diagnostic::error(
                        "initial image pixel loop body accepts only channel scale assignments",
                        inner_statement_span(statement),
                    )
                    .with_note("supported form: `p.r *= factor`"),
                );
                return;
            };
            if *op != ast::AssignmentOp::Multiply {
                self.diagnostics.push(
                    Diagnostic::error(
                        "initial image pixel assignment requires `*=`, not `=`",
                        *assignment_span,
                    )
                    .with_note("general channel replacement and conversion semantics are deferred"),
                );
                return;
            }
            let ExprKind::Member {
                receiver: pixel,
                name,
                name_span,
            } = &self.program.expr(*target).kind
            else {
                unreachable!("parser admits only member assignment targets")
            };
            if !matches!(&self.program.expr(*pixel).kind, ExprKind::Name(name) if name == binding) {
                self.diagnostics.push(Diagnostic::error(
                    format!("pixel loop assignment must target `{binding}.<channel>`"),
                    self.program.expr(*target).span,
                ));
                return;
            }
            let channel = match name.as_str() {
                "r" => Rgba8Channel::Red,
                "g" => Rgba8Channel::Green,
                "b" => Rgba8Channel::Blue,
                "a" => Rgba8Channel::Alpha,
                _ => {
                    self.diagnostics.push(
                        Diagnostic::error(
                            format!("RGBA8 pixel has no channel `{name}`"),
                            *name_span,
                        )
                        .with_note("available channels are r, g, b, and a"),
                    );
                    return;
                }
            };
            if !seen.insert(channel) {
                self.diagnostics.push(
                    Diagnostic::error(
                        format!("RGBA8 channel `{name}` is scaled more than once in this loop"),
                        *name_span,
                    )
                    .with_note("the initial pixel loop permits one scale per channel"),
                );
                return;
            }
            if expression_mentions_name(self.program, *value, binding) {
                self.diagnostics.push(
                    Diagnostic::error(
                        "pixel scale factor cannot read the pixel binding yet",
                        self.program.expr(*value).span,
                    )
                    .with_note("this slice supports only `p.<channel> *= f32`"),
                );
                return;
            }
            let Some(factor) = self.expression(*value) else {
                return;
            };
            let actual = self.values[factor.0 as usize].ty;
            if actual != Type::F32 {
                self.diagnostics.push(
                    Diagnostic::error(
                        format!(
                            "RGBA8 channel scale factor must be f32, not {}",
                            actual.name()
                        ),
                        self.program.expr(*value).span,
                    )
                    .with_note("pass an f32 parameter or expression as the scale factor"),
                );
                return;
            }
            channels.push((channel, factor));
        }
        if self.moved.iter().any(|moved| !moved_before.contains(moved)) {
            self.diagnostics.push(
                Diagnostic::error(
                    "image pixel scale factor cannot consume an owned value",
                    span,
                )
                .with_note("pixel-loop factors are evaluated once before iteration"),
            );
            return;
        }

        self.moved.insert(image);
        let scaled = self.alloc(
            Type::Image,
            ValueKind::ImageRgba8Scale { image, channels },
            span,
            true,
        );
        let image_name_span = self.environment[image_name].1;
        self.environment
            .insert(image_name.clone(), (scaled, image_name_span));
    }

    fn require_byte_loop_result(&mut self, value: ValueId, syntax: ExprId) -> bool {
        let actual_value = self.values[value.0 as usize].ty;
        if actual_value == Type::U8 {
            return true;
        }
        self.diagnostics.push(
            Diagnostic::error(
                format!(
                    "image byte assignment requires u8, not {}",
                    actual_value.name()
                ),
                self.program.expr(syntax).span,
            )
            .with_note("the loop assignment must produce one byte for each input byte"),
        );
        false
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
        let outer_moved = self.moved.clone();
        self.current_block = then_block;
        self.environment = outer_environment.clone();
        self.moved = outer_moved.clone();
        let then_returns = self.lower_statements(then_body);
        let then_end = self.current_block;
        let then_moved = self.moved.clone();

        self.current_block = else_block;
        self.environment = outer_environment.clone();
        self.moved = outer_moved;
        let else_returns = self.lower_statements(else_body);
        let else_end = self.current_block;
        let else_moved = self.moved.clone();
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
        self.moved = match (then_returns, else_returns) {
            (false, false) => then_moved.union(&else_moved).copied().collect(),
            (false, true) => then_moved,
            (true, false) => else_moved,
            (true, true) => unreachable!(),
        };
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
                Some((value, _)) if self.moved.contains(&value) => {
                    self.diagnostics.push(
                        Diagnostic::error(
                            format!("owned inner value `{name}` has already been moved"),
                            expression.span,
                        )
                        .with_note(
                            "owned image operations consume their Image input and return new ownership",
                        ),
                    );
                    None
                }
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
                let result_type = if op.is_arithmetic() {
                    if left_type == right_type && left_type.is_numeric() {
                        left_type
                    } else {
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
                } else if op.is_equality() {
                    if left_type == right_type
                        && matches!(left_type, Type::Bool | Type::U8 | Type::I64 | Type::F32)
                    {
                        Type::Bool
                    } else {
                        self.diagnostics.push(
                            Diagnostic::error(
                                "inner equality requires operands of the same scalar type",
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
                } else if left_type == right_type
                    && (left_type.is_numeric() || left_type == Type::U8)
                {
                    Type::Bool
                } else {
                    self.diagnostics.push(
                        Diagnostic::error(
                            "inner ordering requires operands of the same numeric type",
                            expression.span,
                        )
                        .with_note(format!(
                            "left is {}, right is {}",
                            left_type.name(),
                            right_type.name()
                        )),
                    );
                    return None;
                };
                Some(self.alloc(
                    result_type,
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
                let Some((name, qualified)) = inner_callable_name(self.program, *callee) else {
                    self.diagnostics.push(Diagnostic::error(
                        "inner calls require a directly named transform",
                        self.program.expr(*callee).span,
                    ));
                    return None;
                };
                if name == "environment_i64" {
                    if qualified {
                        self.diagnostics.push(Diagnostic::error(
                            "inner runtime operations cannot use semantic identity qualifiers",
                            self.program.expr(*callee).span,
                        ));
                        return None;
                    }
                    return self.environment_i64(arguments, expression.span);
                }
                if name == "image_zero" {
                    if qualified {
                        self.diagnostics.push(Diagnostic::error(
                            "inner runtime operations cannot use semantic identity qualifiers",
                            self.program.expr(*callee).span,
                        ));
                        return None;
                    }
                    return self.image_zero(arguments, expression.span);
                }
                if name == "image_fill" {
                    if qualified {
                        self.diagnostics.push(Diagnostic::error(
                            "inner runtime operations cannot use semantic identity qualifiers",
                            self.program.expr(*callee).span,
                        ));
                        return None;
                    }
                    return self.image_fill(arguments, expression.span);
                }
                let Some(signature) = self.signatures.get(name) else {
                    self.diagnostics.push(Diagnostic::error(
                        format!("unknown inner transform `{name}`"),
                        self.program.expr(*callee).span,
                    ));
                    return None;
                };
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
                    if *expected == Type::Image {
                        self.moved.insert(value);
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
            | ExprKind::Member { .. }
            | ExprKind::IdentityQualified { .. }
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

    fn image_zero(
        &mut self,
        arguments: &[ast::Argument],
        span: crate::source::Span,
    ) -> Option<ValueId> {
        if arguments.len() != 1 || arguments[0].name.is_some() {
            self.diagnostics.push(
                Diagnostic::error("image_zero expects one positional Image argument", span)
                    .with_note("example: cleared = image_zero(img)"),
            );
            return None;
        }
        let image = self.expression(arguments[0].value)?;
        let actual = self.values[image.0 as usize].ty;
        if actual != Type::Image {
            self.diagnostics.push(
                Diagnostic::error(
                    format!("image_zero requires owned Image, not {}", actual.name()),
                    arguments[0].span,
                )
                .with_note("read-only ImageView values cannot be mutated"),
            );
            return None;
        }
        self.moved.insert(image);
        Some(self.alloc(Type::Image, ValueKind::ImageZero { image }, span, true))
    }

    fn image_fill(
        &mut self,
        arguments: &[ast::Argument],
        span: crate::source::Span,
    ) -> Option<ValueId> {
        if arguments.len() != 2 || arguments.iter().any(|argument| argument.name.is_some()) {
            self.diagnostics.push(
                Diagnostic::error("image_fill expects positional Image and u8 arguments", span)
                    .with_note("example: filled = image_fill(img, value)"),
            );
            return None;
        }
        let image = self.expression(arguments[0].value)?;
        let actual_image = self.values[image.0 as usize].ty;
        if actual_image != Type::Image {
            self.diagnostics.push(
                Diagnostic::error(
                    format!(
                        "image_fill requires owned Image, not {}",
                        actual_image.name()
                    ),
                    arguments[0].span,
                )
                .with_note("read-only ImageView values cannot be mutated"),
            );
            return None;
        }
        let value = self.expression(arguments[1].value)?;
        let actual_value = self.values[value.0 as usize].ty;
        if actual_value != Type::U8 {
            self.diagnostics.push(
                Diagnostic::error(
                    format!(
                        "image_fill requires u8 fill value, not {}",
                        actual_value.name()
                    ),
                    arguments[1].span,
                )
                .with_note("integer literals are i64; pass a u8 transform parameter for now"),
            );
            return None;
        }
        self.moved.insert(image);
        Some(self.alloc(
            Type::Image,
            ValueKind::ImageFill { image, value },
            span,
            true,
        ))
    }
}

fn inner_statement_span(statement: &InnerStmt) -> crate::source::Span {
    match statement {
        InnerStmt::Binding(binding) => binding.span,
        InnerStmt::Return { span, .. }
        | InnerStmt::If { span, .. }
        | InnerStmt::For { span, .. }
        | InnerStmt::Assignment { span, .. } => *span,
    }
}

fn inner_callable_name(program: &ast::Program, expression: ExprId) -> Option<(&str, bool)> {
    match &program.expr(expression).kind {
        ExprKind::Name(name) => Some((name, false)),
        ExprKind::IdentityQualified { callable, .. } => {
            let ExprKind::Name(name) = &program.expr(*callable).kind else {
                return None;
            };
            Some((name, true))
        }
        _ => None,
    }
}

fn expression_mentions_name(program: &ast::Program, expression: ExprId, name: &str) -> bool {
    match &program.expr(expression).kind {
        ExprKind::Name(candidate) => candidate == name,
        ExprKind::List(values) => values
            .iter()
            .any(|value| expression_mentions_name(program, *value, name)),
        ExprKind::Record(fields) => fields
            .iter()
            .any(|field| expression_mentions_name(program, field.value, name)),
        ExprKind::Call { arguments, .. } => arguments
            .iter()
            .any(|argument| expression_mentions_name(program, argument.value, name)),
        ExprKind::Member { receiver, .. } => expression_mentions_name(program, *receiver, name),
        ExprKind::IdentityQualified { callable, .. } => {
            expression_mentions_name(program, *callable, name)
        }
        ExprKind::Binary { left, right, .. } => {
            expression_mentions_name(program, *left, name)
                || expression_mentions_name(program, *right, name)
        }
        ExprKind::Pipeline { input, stage } => {
            expression_mentions_name(program, *input, name)
                || expression_mentions_name(program, *stage, name)
        }
        ExprKind::Null
        | ExprKind::Bool(_)
        | ExprKind::Integer(_)
        | ExprKind::Float(_)
        | ExprKind::String(_) => false,
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
    fn checks_scalar_comparisons_and_lowers_bool_results() {
        let compiled = compile(
            "test.tima",
            "transform less(left: f32, right: f32) -> bool { return left < right }\n\
             transform same(left: bool, right: bool) -> bool { return left == right }\n",
        )
        .unwrap();
        let less = &compiled.transforms.transforms[0];
        assert_eq!(less.return_type, ir::Type::Bool);
        assert_eq!(less.values[2].ty, ir::Type::Bool);
        assert!(matches!(
            less.values[2].kind,
            ir::ValueKind::Binary {
                op: crate::ast::BinaryOp::Less,
                ..
            }
        ));

        let diagnostics = compile(
            "bad.tima",
            "transform bad(left: bool, right: bool) -> bool { return left < right }\n",
        )
        .unwrap_err();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("inner ordering"))
        );

        let diagnostics = compile(
            "bad_image.tima",
            "transform bad(left: ImageView, right: ImageView) -> bool { return left == right }\n",
        )
        .unwrap_err();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("inner equality"))
        );
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
    fn moves_owned_images_through_inner_calls_and_rejects_aliases() {
        let compiled = compile(
            "safe.tima",
            "transform choose(a: Image, b: Image) -> Image { return a }\n\
             transform safe(x: Image, y: Image) -> Image {\n\
                 chosen = choose(x, y)\n\
                 return chosen\n\
             }\n",
        )
        .unwrap();
        assert!(matches!(
            compiled.transforms.transforms[1].values[2].kind,
            ir::ValueKind::Call {
                transform: ir::TransformId(0),
                ref arguments,
            } if arguments == &[ir::ValueId(0), ir::ValueId(1)]
        ));

        let diagnostics = compile(
            "test.tima",
            "transform choose(a: Image, b: Image) -> Image { return a }\n\
             transform unsafe_alias(x: Image) -> Image { return choose(x, x) }\n",
        )
        .unwrap_err();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("already been moved"))
        );

        let diagnostics = compile(
            "moved.tima",
            "transform consume(img: Image) -> Image { return img }\n\
             transform bad(img: Image) -> Image {\n\
                 result = consume(img)\n\
                 return img\n\
             }\n",
        )
        .unwrap_err();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("already been moved"))
        );
    }

    #[test]
    fn lowers_consuming_owned_image_zero_and_rejects_use_after_move() {
        let compiled = compile(
            "test.tima",
            "transform clear(img: Image) -> Image {\n\
                 cleared = image_zero(img)\n\
                 return cleared\n\
             }\n",
        )
        .unwrap();
        assert!(matches!(
            compiled.transforms.transforms[0].values[1].kind,
            ir::ValueKind::ImageZero {
                image: ir::ValueId(0)
            }
        ));

        let diagnostics = compile(
            "moved.tima",
            "transform bad(img: Image) -> Image {\n\
                 cleared = image_zero(img)\n\
                 return img\n\
             }\n",
        )
        .unwrap_err();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("already been moved"))
        );

        let diagnostics = compile(
            "view.tima",
            "transform bad(img: ImageView) -> ImageView { return image_zero(img) }\n",
        )
        .unwrap_err();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("requires owned Image"))
        );
    }

    #[test]
    fn lowers_u8_image_fill_and_rejects_invalid_ownership_or_value_types() {
        let compiled = compile(
            "fill.tima",
            "transform fill(img: Image, value: u8) -> Image {\n\
                 filled = image_fill(img, value)\n\
                 return filled\n\
             }\n",
        )
        .unwrap();
        assert_eq!(
            compiled.transforms.transforms[0].parameters[1].ty,
            ir::Type::U8
        );
        assert!(matches!(
            compiled.transforms.transforms[0].values[2].kind,
            ir::ValueKind::ImageFill {
                image: ir::ValueId(0),
                value: ir::ValueId(1),
            }
        ));

        let diagnostics = compile(
            "moved.tima",
            "transform bad(img: Image, value: u8) -> Image {\n\
                 filled = image_fill(img, value)\n\
                 return img\n\
             }\n",
        )
        .unwrap_err();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("already been moved"))
        );

        let diagnostics = compile(
            "view.tima",
            "transform bad(img: ImageView, value: u8) -> ImageView { return image_fill(img, value) }\n",
        )
        .unwrap_err();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("requires owned Image"))
        );

        let diagnostics = compile(
            "value.tima",
            "transform bad(img: Image, value: i64) -> Image { return image_fill(img, value) }\n",
        )
        .unwrap_err();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("requires u8 fill value"))
        );
    }

    #[test]
    fn normalizes_initial_image_byte_loop_to_owned_fill_ir() {
        let compiled = compile(
            "loop.tima",
            "transform fill(img: Image, value: u8) -> Image {\n\
                 for byte in img.bytes { byte = value }\n\
                 return img\n\
             }\n",
        )
        .unwrap();
        let transform = &compiled.transforms.transforms[0];
        assert!(matches!(
            transform.values[2].kind,
            ir::ValueKind::ImageFill {
                image: ir::ValueId(0),
                value: ir::ValueId(1),
            }
        ));
        assert!(matches!(
            transform.blocks[0].terminator,
            ir::Terminator::Return(ir::ValueId(2))
        ));

        let diagnostics = compile(
            "view.tima",
            "transform bad(img: ImageView, value: u8) -> ImageView {\n\
                 for byte in img.bytes { byte = value }\n\
                 return img\n\
             }\n",
        )
        .unwrap_err();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("requires owned Image"))
        );

        let diagnostics = compile(
            "body.tima",
            "transform bad(img: Image, value: u8) -> Image {\n\
                 for byte in img.bytes { other = value }\n\
                 return img\n\
             }\n",
        )
        .unwrap_err();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("must assign its `byte`"))
        );

        let diagnostics = compile(
            "alias.tima",
            "transform bad(img: Image, value: u8) -> Image {\n\
                 alias = img\n\
                 for byte in img.bytes { byte = value }\n\
                 return alias\n\
             }\n",
        )
        .unwrap_err();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("already been moved"))
        );
    }

    #[test]
    fn lowers_byte_dependent_image_loop_to_structured_map_ir() {
        let compiled = compile(
            "map.tima",
            "transform choose(current: u8, target: u8, replacement: u8) -> u8 {\n\
                 if current == target { return replacement } else { return current }\n\
             }\n\
             transform replace(img: Image, target: u8, replacement: u8) -> Image {\n\
                 for byte in img.bytes { byte = choose(byte, target, replacement) }\n\
                 return img\n\
             }\n",
        )
        .unwrap();
        let transform = &compiled.transforms.transforms[1];
        assert_eq!(transform.blocks[0].instructions, [ir::ValueId(5)]);
        assert!(matches!(
            &transform.values[3].kind,
            ir::ValueKind::ImageByteElement
        ));
        assert!(matches!(
            &transform.values[5].kind,
            ir::ValueKind::ImageByteMap {
                image: ir::ValueId(0),
                element: ir::ValueId(3),
                instructions,
                result: ir::ValueId(4),
            } if instructions == &[ir::ValueId(4)]
        ));
        assert!(matches!(
            transform.blocks[0].terminator,
            ir::Terminator::Return(ir::ValueId(5))
        ));

        let diagnostics = compile(
            "consume.tima",
            "transform steal(img: Image, value: u8) -> u8 { return value }\n\
             transform bad(img: Image, other: Image, value: u8) -> Image {\n\
                 for byte in img.bytes { byte = steal(other, byte) }\n\
                 return img\n\
             }\n",
        )
        .unwrap_err();
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("cannot consume another owned value")
        }));
    }

    #[test]
    fn lowers_constrained_rgba8_pixel_scaling_and_rejects_broader_mutation() {
        let compiled = compile(
            "darken.tima",
            "transform darken(img: Image, factor: f32) -> Image {\n\
                 for p in img.pixels {\n\
                     p.r *= factor\n\
                     p.g *= factor\n\
                     p.b *= factor\n\
                 }\n\
                 return img\n\
             }\n",
        )
        .unwrap();
        let transform = &compiled.transforms.transforms[0];
        assert!(matches!(
            &transform.values[2].kind,
            ir::ValueKind::ImageRgba8Scale { image, channels }
                if *image == ir::ValueId(0)
                    && channels == &[
                        (ir::Rgba8Channel::Red, ir::ValueId(1)),
                        (ir::Rgba8Channel::Green, ir::ValueId(1)),
                        (ir::Rgba8Channel::Blue, ir::ValueId(1)),
                    ]
        ));

        let diagnostics = compile(
            "replace.tima",
            "transform replace(img: Image, factor: f32) -> Image {\n\
                 for p in img.pixels { p.r = factor }\n\
                 return img\n\
             }\n",
        )
        .unwrap_err();
        assert!(diagnostics[0].message.contains("requires `*=`"));

        let diagnostics = compile(
            "view.tima",
            "transform bad(img: ImageView, factor: f32) -> ImageView {\n\
                 for p in img.pixels { p.r *= factor }\n\
                 return img\n\
             }\n",
        )
        .unwrap_err();
        assert!(diagnostics[0].message.contains("requires owned Image"));
    }

    #[test]
    fn permits_u8_comparisons_but_not_arithmetic() {
        compile(
            "compare.tima",
            "transform before(left: u8, right: u8) -> bool { return left < right }\n\
             transform same(left: u8, right: u8) -> bool { return left == right }\n",
        )
        .unwrap();

        let diagnostics = compile(
            "arithmetic.tima",
            "transform add(left: u8, right: u8) -> u8 { return left + right }\n",
        )
        .unwrap_err();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("inner arithmetic"))
        );
    }

    #[test]
    fn owned_image_moves_are_checked_across_branch_continuations() {
        let diagnostics = compile(
            "join.tima",
            "transform bad(img: Image, flag: bool) -> Image {\n\
                 if flag { cleared = image_zero(img) } else {}\n\
                 return img\n\
             }\n",
        )
        .unwrap_err();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("already been moved"))
        );

        compile(
            "returns.tima",
            "transform clear_if(img: Image, flag: bool) -> Image {\n\
                 if flag { return image_zero(img) } else { return img }\n\
             }\n",
        )
        .unwrap();
    }
}
