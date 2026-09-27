use crate::ast::*;
use crate::diagnostic::Diagnostic;
use crate::lexer::{self, Token, TokenKind};
use crate::source::{SourceFile, Span};

pub fn parse(source: &SourceFile) -> Result<Program, Vec<Diagnostic>> {
    let tokens = lexer::lex(source)?;
    Parser {
        tokens,
        position: 0,
        expressions: Vec::new(),
    }
    .program()
    .map_err(|diagnostic| vec![diagnostic])
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
    expressions: Vec<Expr>,
}

impl Parser {
    fn program(mut self) -> Result<Program, Diagnostic> {
        let mut items = Vec::new();
        self.separators();
        while !self.at(|kind| matches!(kind, TokenKind::Eof)) {
            let item = if self.at(|kind| matches!(kind, TokenKind::Transform)) {
                Item::Transform(self.transform()?)
            } else if self.at_binding() {
                Item::Binding(self.binding()?)
            } else {
                Item::Expression(self.expression()?)
            };
            items.push(item);
            if !self.at(|kind| {
                matches!(
                    kind,
                    TokenKind::Newline | TokenKind::Semicolon | TokenKind::Eof
                )
            }) {
                return Err(self.expected("a newline or `;` after the top-level item"));
            }
            self.separators();
        }
        Ok(Program {
            items,
            expressions: self.expressions,
        })
    }

    fn at_binding(&self) -> bool {
        matches!(self.current().kind, TokenKind::Identifier(_))
            && self
                .tokens
                .get(self.position + 1)
                .is_some_and(|token| matches!(token.kind, TokenKind::Equal))
    }

    fn binding(&mut self) -> Result<Binding, Diagnostic> {
        let (name, name_span) = self.identifier("a binding name")?;
        self.expect(|kind| matches!(kind, TokenKind::Equal), "`=`")?;
        self.inline_newlines();
        let value = self.expression()?;
        let span = name_span.join(self.expr(value).span);
        Ok(Binding {
            name,
            name_span,
            value,
            span,
        })
    }

    fn transform(&mut self) -> Result<TransformDecl, Diagnostic> {
        let start = self.bump().span;
        let (name, name_span) = self.identifier("a transform name")?;
        self.expect(|kind| matches!(kind, TokenKind::LeftParen), "`(`")?;
        self.inline_newlines();
        let mut parameters = Vec::new();
        if !self.at(|kind| matches!(kind, TokenKind::RightParen)) {
            loop {
                let (parameter_name, parameter_name_span) = self.identifier("a parameter name")?;
                self.expect(|kind| matches!(kind, TokenKind::Colon), "`:`")?;
                let ty = self.type_ref()?;
                parameters.push(Parameter {
                    name: parameter_name,
                    name_span: parameter_name_span,
                    span: parameter_name_span.join(ty.span),
                    ty,
                });
                self.inline_newlines();
                if !self.eat(|kind| matches!(kind, TokenKind::Comma)) {
                    break;
                }
                self.inline_newlines();
            }
        }
        self.expect(|kind| matches!(kind, TokenKind::RightParen), "`)`")?;
        self.expect(|kind| matches!(kind, TokenKind::Arrow), "`->`")?;
        let return_type = self.type_ref()?;
        self.expect(|kind| matches!(kind, TokenKind::LeftBrace), "`{`")?;
        let (body, end) = self.inner_block()?;
        Ok(TransformDecl {
            name,
            name_span,
            parameters,
            return_type,
            body,
            span: start.join(end),
        })
    }

    fn inner_block(&mut self) -> Result<(Vec<InnerStmt>, Span), Diagnostic> {
        self.separators();
        let mut body = Vec::new();
        while !self.at(|kind| matches!(kind, TokenKind::RightBrace | TokenKind::Eof)) {
            body.push(self.inner_statement()?);
            if !self.at(|kind| {
                matches!(
                    kind,
                    TokenKind::Newline | TokenKind::Semicolon | TokenKind::RightBrace
                )
            }) {
                return Err(self.expected("a newline, `;`, or `}` after the statement"));
            }
            self.separators();
        }
        let end = self
            .expect(|kind| matches!(kind, TokenKind::RightBrace), "`}`")?
            .span;
        Ok((body, end))
    }

    fn inner_statement(&mut self) -> Result<InnerStmt, Diagnostic> {
        if self.eat(|kind| matches!(kind, TokenKind::Return)) {
            let return_start = self.previous().span;
            let value = self.expression()?;
            let span = return_start.join(self.expr(value).span);
            return Ok(InnerStmt::Return { value, span });
        }
        if self.eat(|kind| matches!(kind, TokenKind::If)) {
            let start = self.previous().span;
            let condition = self.expression()?;
            self.expect(|kind| matches!(kind, TokenKind::LeftBrace), "`{`")?;
            let (then_body, _) = self.inner_block()?;
            self.inline_newlines();
            self.expect(|kind| matches!(kind, TokenKind::Else), "`else`")?;
            self.expect(|kind| matches!(kind, TokenKind::LeftBrace), "`{`")?;
            let (else_body, end) = self.inner_block()?;
            return Ok(InnerStmt::If {
                condition,
                then_body,
                else_body,
                span: start.join(end),
            });
        }
        if self.eat(|kind| matches!(kind, TokenKind::For)) {
            let start = self.previous().span;
            let (binding, binding_span) = self.identifier("a loop binding")?;
            self.expect(|kind| matches!(kind, TokenKind::In), "`in`")?;
            let iterable = self.expression()?;
            self.expect(|kind| matches!(kind, TokenKind::LeftBrace), "`{`")?;
            let (body, end) = self.inner_block()?;
            return Ok(InnerStmt::For {
                binding,
                binding_span,
                iterable,
                body,
                span: start.join(end),
            });
        }
        if self.at_member_assignment() {
            let target = self.postfix()?;
            let op = if self.eat(|kind| matches!(kind, TokenKind::StarEqual)) {
                AssignmentOp::Multiply
            } else {
                self.expect(|kind| matches!(kind, TokenKind::Equal), "`=` or `*=`")?;
                AssignmentOp::Assign
            };
            let value = self.expression()?;
            let span = self.expr(target).span.join(self.expr(value).span);
            return Ok(InnerStmt::Assignment {
                target,
                op,
                value,
                span,
            });
        }
        if self.at_binding() {
            return self.binding().map(InnerStmt::Binding);
        }
        Err(self
            .expected("a local binding, assignment, `if`, `for`, or `return` inside the transform"))
    }

    fn at_member_assignment(&self) -> bool {
        matches!(self.current().kind, TokenKind::Identifier(_))
            && self
                .tokens
                .get(self.position + 1)
                .is_some_and(|token| matches!(token.kind, TokenKind::Dot))
            && self
                .tokens
                .get(self.position + 2)
                .is_some_and(|token| matches!(token.kind, TokenKind::Identifier(_)))
            && self
                .tokens
                .get(self.position + 3)
                .is_some_and(|token| matches!(token.kind, TokenKind::Equal | TokenKind::StarEqual))
    }

    fn type_ref(&mut self) -> Result<TypeRef, Diagnostic> {
        let (name, span) = self.identifier("a type name")?;
        Ok(TypeRef { name, span })
    }

    fn expression(&mut self) -> Result<ExprId, Diagnostic> {
        self.pipeline()
    }

    fn pipeline(&mut self) -> Result<ExprId, Diagnostic> {
        let mut input = self.comparison()?;
        loop {
            let checkpoint = self.position;
            self.inline_newlines();
            if !self.eat(|kind| matches!(kind, TokenKind::Pipe)) {
                self.position = checkpoint;
                break;
            }
            self.inline_newlines();
            let stage = self.comparison()?;
            let span = self.expr(input).span.join(self.expr(stage).span);
            input = self.alloc(ExprKind::Pipeline { input, stage }, span);
        }
        Ok(input)
    }

    fn comparison(&mut self) -> Result<ExprId, Diagnostic> {
        let left = self.additive()?;
        let op = if self.eat(|kind| matches!(kind, TokenKind::EqualEqual)) {
            BinaryOp::Equal
        } else if self.eat(|kind| matches!(kind, TokenKind::BangEqual)) {
            BinaryOp::NotEqual
        } else if self.eat(|kind| matches!(kind, TokenKind::Less)) {
            BinaryOp::Less
        } else if self.eat(|kind| matches!(kind, TokenKind::LessEqual)) {
            BinaryOp::LessEqual
        } else if self.eat(|kind| matches!(kind, TokenKind::Greater)) {
            BinaryOp::Greater
        } else if self.eat(|kind| matches!(kind, TokenKind::GreaterEqual)) {
            BinaryOp::GreaterEqual
        } else {
            return Ok(left);
        };
        let right = self.additive()?;
        let span = self.expr(left).span.join(self.expr(right).span);
        Ok(self.alloc(ExprKind::Binary { op, left, right }, span))
    }

    fn additive(&mut self) -> Result<ExprId, Diagnostic> {
        let mut left = self.multiplicative()?;
        loop {
            let op = if self.eat(|kind| matches!(kind, TokenKind::Plus)) {
                BinaryOp::Add
            } else if self.eat(|kind| matches!(kind, TokenKind::Minus)) {
                BinaryOp::Subtract
            } else {
                break;
            };
            let right = self.multiplicative()?;
            let span = self.expr(left).span.join(self.expr(right).span);
            left = self.alloc(ExprKind::Binary { op, left, right }, span);
        }
        Ok(left)
    }

    fn multiplicative(&mut self) -> Result<ExprId, Diagnostic> {
        let mut left = self.postfix()?;
        loop {
            let op = if self.eat(|kind| matches!(kind, TokenKind::Star)) {
                BinaryOp::Multiply
            } else if self.eat(|kind| matches!(kind, TokenKind::Slash)) {
                BinaryOp::Divide
            } else {
                break;
            };
            let right = self.postfix()?;
            let span = self.expr(left).span.join(self.expr(right).span);
            left = self.alloc(ExprKind::Binary { op, left, right }, span);
        }
        Ok(left)
    }

    fn postfix(&mut self) -> Result<ExprId, Diagnostic> {
        let mut expression = self.primary()?;
        loop {
            if self.eat(|kind| matches!(kind, TokenKind::LeftParen)) {
                let mut arguments = Vec::new();
                self.inline_newlines();
                if !self.at(|kind| matches!(kind, TokenKind::RightParen)) {
                    loop {
                        let start = self.current().span;
                        let name = if matches!(self.current().kind, TokenKind::Identifier(_))
                            && self
                                .tokens
                                .get(self.position + 1)
                                .is_some_and(|token| matches!(token.kind, TokenKind::Equal))
                        {
                            let (name, span) = self.identifier("an argument name")?;
                            self.bump();
                            Some((name, span))
                        } else {
                            None
                        };
                        let value = self.expression()?;
                        arguments.push(Argument {
                            name,
                            value,
                            span: start.join(self.expr(value).span),
                        });
                        self.inline_newlines();
                        if !self.eat(|kind| matches!(kind, TokenKind::Comma)) {
                            break;
                        }
                        self.inline_newlines();
                    }
                }
                let end = self
                    .expect(|kind| matches!(kind, TokenKind::RightParen), "`)`")?
                    .span;
                let span = self.expr(expression).span.join(end);
                expression = self.alloc(
                    ExprKind::Call {
                        callee: expression,
                        arguments,
                    },
                    span,
                );
            } else if self.eat(|kind| matches!(kind, TokenKind::Dot)) {
                let (name, name_span) = self.identifier("a member name after `.`")?;
                let span = self.expr(expression).span.join(name_span);
                expression = self.alloc(
                    ExprKind::Member {
                        receiver: expression,
                        name,
                        name_span,
                    },
                    span,
                );
            } else {
                break;
            }
        }
        Ok(expression)
    }

    fn primary(&mut self) -> Result<ExprId, Diagnostic> {
        let token = self.bump();
        let span = token.span;
        let kind = match token.kind {
            TokenKind::Null => ExprKind::Null,
            TokenKind::True => ExprKind::Bool(true),
            TokenKind::False => ExprKind::Bool(false),
            TokenKind::Integer(value) => ExprKind::Integer(value),
            TokenKind::Float(value) => ExprKind::Float(value),
            TokenKind::String(value) => ExprKind::String(value),
            TokenKind::Identifier(name) => ExprKind::Name(name),
            TokenKind::LeftParen => {
                let value = self.expression()?;
                self.expect(|kind| matches!(kind, TokenKind::RightParen), "`)`")?;
                return Ok(value);
            }
            TokenKind::LeftBracket => return self.list(span),
            TokenKind::LeftBrace => return self.record(span),
            _ => return Err(Diagnostic::error("expected an expression", span)),
        };
        Ok(self.alloc(kind, span))
    }

    fn list(&mut self, start: Span) -> Result<ExprId, Diagnostic> {
        let mut values = Vec::new();
        self.inline_newlines();
        if !self.at(|kind| matches!(kind, TokenKind::RightBracket)) {
            loop {
                values.push(self.expression()?);
                self.inline_newlines();
                if !self.eat(|kind| matches!(kind, TokenKind::Comma)) {
                    break;
                }
                self.inline_newlines();
            }
        }
        let end = self
            .expect(|kind| matches!(kind, TokenKind::RightBracket), "`]`")?
            .span;
        Ok(self.alloc(ExprKind::List(values), start.join(end)))
    }

    fn record(&mut self, start: Span) -> Result<ExprId, Diagnostic> {
        let mut fields = Vec::new();
        self.inline_newlines();
        if !self.at(|kind| matches!(kind, TokenKind::RightBrace)) {
            loop {
                let (name, name_span) = self.identifier("a record field name")?;
                self.expect(|kind| matches!(kind, TokenKind::Colon), "`:`")?;
                let value = self.expression()?;
                fields.push(RecordField {
                    name,
                    name_span,
                    value,
                });
                self.inline_newlines();
                if !self.eat(|kind| matches!(kind, TokenKind::Comma)) {
                    break;
                }
                self.inline_newlines();
            }
        }
        let end = self
            .expect(|kind| matches!(kind, TokenKind::RightBrace), "`}`")?
            .span;
        Ok(self.alloc(ExprKind::Record(fields), start.join(end)))
    }

    fn alloc(&mut self, kind: ExprKind, span: Span) -> ExprId {
        let id = ExprId(self.expressions.len() as u32);
        self.expressions.push(Expr { kind, span });
        id
    }

    fn expr(&self, id: ExprId) -> &Expr {
        &self.expressions[id.0 as usize]
    }

    fn current(&self) -> &Token {
        &self.tokens[self.position]
    }

    fn previous(&self) -> &Token {
        &self.tokens[self.position - 1]
    }

    fn bump(&mut self) -> Token {
        let token = self.current().clone();
        if !matches!(token.kind, TokenKind::Eof) {
            self.position += 1;
        }
        token
    }

    fn at(&self, predicate: impl FnOnce(&TokenKind) -> bool) -> bool {
        predicate(&self.current().kind)
    }

    fn eat(&mut self, predicate: impl FnOnce(&TokenKind) -> bool) -> bool {
        if self.at(predicate) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(
        &mut self,
        predicate: impl FnOnce(&TokenKind) -> bool,
        description: &str,
    ) -> Result<Token, Diagnostic> {
        if self.at(predicate) {
            Ok(self.bump())
        } else {
            Err(self.expected(description))
        }
    }

    fn expected(&self, description: &str) -> Diagnostic {
        Diagnostic::error(format!("expected {description}"), self.current().span)
    }

    fn identifier(&mut self, description: &str) -> Result<(String, Span), Diagnostic> {
        let token = self.bump();
        match token.kind {
            TokenKind::Identifier(name) => Ok((name, token.span)),
            _ => Err(Diagnostic::error(
                format!("expected {description}"),
                token.span,
            )),
        }
    }

    fn separators(&mut self) {
        while self.eat(|kind| matches!(kind, TokenKind::Newline | TokenKind::Semicolon)) {}
    }

    fn inline_newlines(&mut self) {
        while self.eat(|kind| matches!(kind, TokenKind::Newline)) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_expression_arena_between_outer_and_inner_code() {
        let source = SourceFile::new(
            "test.tima",
            "transform scale(x: f32, factor: f32) -> f32 {\n return x * factor\n}\nout = 4.0 | scale(0.5)\n",
        );
        let program = parse(&source).unwrap();
        assert_eq!(program.items.len(), 2);
        assert!(matches!(program.items[0], Item::Transform(_)));
        let Item::Binding(binding) = &program.items[1] else {
            panic!("expected binding")
        };
        assert!(matches!(
            program.expr(binding.value).kind,
            ExprKind::Pipeline { .. }
        ));
    }

    #[test]
    fn parses_immutable_composite_literals() {
        let source = SourceFile::new("test.tima", "x = {name: \"cat\", sizes: [1, 2]}\n");
        let program = parse(&source).unwrap();
        let Item::Binding(binding) = &program.items[0] else {
            panic!("expected binding")
        };
        assert!(matches!(
            program.expr(binding.value).kind,
            ExprKind::Record(_)
        ));
    }

    #[test]
    fn parses_inner_local_bindings_in_the_shared_statement_tree() {
        let source = SourceFile::new(
            "test.tima",
            "transform double(x: f32) -> f32 {\n doubled = x * 2.0\n return doubled\n}\n",
        );
        let program = parse(&source).unwrap();
        let Item::Transform(transform) = &program.items[0] else {
            panic!("expected transform")
        };
        assert!(matches!(transform.body[0], InnerStmt::Binding(_)));
        assert!(matches!(transform.body[1], InnerStmt::Return { .. }));
    }

    #[test]
    fn parses_owned_image_byte_iteration_in_the_shared_tree() {
        let source = SourceFile::new(
            "test.tima",
            "transform fill(img: Image, value: u8) -> Image {\n\
                 for byte in img.bytes {\n\
                     byte = value\n\
                 }\n\
                 return img\n\
             }\n",
        );
        let program = parse(&source).unwrap();
        let Item::Transform(transform) = &program.items[0] else {
            panic!("expected transform")
        };
        let InnerStmt::For {
            binding,
            iterable,
            body,
            ..
        } = &transform.body[0]
        else {
            panic!("expected for statement")
        };
        assert_eq!(binding, "byte");
        assert!(matches!(
            &program.expr(*iterable).kind,
            ExprKind::Member { receiver, name, .. }
                if name == "bytes"
                    && matches!(&program.expr(*receiver).kind, ExprKind::Name(name) if name == "img")
        ));
        assert!(matches!(
            &body[..],
            [InnerStmt::Binding(Binding { name, .. })] if name == "byte"
        ));
    }

    #[test]
    fn parses_rgba8_pixel_channel_scale_assignments() {
        let source = SourceFile::new(
            "test.tima",
            "transform darken(img: Image, factor: f32) -> Image {\n\
                 for p in img.pixels {\n\
                     p.r *= factor\n\
                     p.g *= factor\n\
                     p.b *= factor\n\
                 }\n\
                 return img\n\
             }\n",
        );
        let program = parse(&source).unwrap();
        let Item::Transform(transform) = &program.items[0] else {
            panic!("expected transform")
        };
        let InnerStmt::For { body, .. } = &transform.body[0] else {
            panic!("expected for statement")
        };
        assert_eq!(body.len(), 3);
        let InnerStmt::Assignment {
            target,
            op: AssignmentOp::Multiply,
            ..
        } = body[0]
        else {
            panic!("expected multiply assignment")
        };
        assert!(matches!(
            &program.expr(target).kind,
            ExprKind::Member { receiver, name, .. }
                if name == "r"
                    && matches!(&program.expr(*receiver).kind, ExprKind::Name(name) if name == "p")
        ));
    }

    #[test]
    fn parses_terminating_inner_conditionals_in_the_shared_statement_tree() {
        let source = SourceFile::new(
            "test.tima",
            "transform choose(flag: bool, left: f32, right: f32) -> f32 {\n\
                 if flag {\n\
                     selected = left\n\
                     return selected\n\
                 } else {\n\
                     return right\n\
                 }\n\
             }\n",
        );
        let program = parse(&source).unwrap();
        let Item::Transform(transform) = &program.items[0] else {
            panic!("expected transform")
        };
        let InnerStmt::If {
            then_body,
            else_body,
            ..
        } = &transform.body[0]
        else {
            panic!("expected conditional")
        };
        assert!(matches!(then_body[0], InnerStmt::Binding(_)));
        assert!(matches!(then_body[1], InnerStmt::Return { .. }));
        assert!(matches!(else_body[0], InnerStmt::Return { .. }));
    }

    #[test]
    fn parses_comparisons_below_arithmetic_precedence() {
        let source = SourceFile::new("test.tima", "result = 1 + 2 < 3 * 4\n");
        let program = parse(&source).unwrap();
        let Item::Binding(binding) = &program.items[0] else {
            panic!("expected binding")
        };
        let ExprKind::Binary {
            op: BinaryOp::Less,
            left,
            right,
        } = program.expr(binding.value).kind
        else {
            panic!("expected comparison")
        };
        assert!(matches!(
            program.expr(left).kind,
            ExprKind::Binary {
                op: BinaryOp::Add,
                ..
            }
        ));
        assert!(matches!(
            program.expr(right).kind,
            ExprKind::Binary {
                op: BinaryOp::Multiply,
                ..
            }
        ));
    }
}
