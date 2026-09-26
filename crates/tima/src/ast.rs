use crate::source::Span;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ExprId(pub u32);

#[derive(Clone, Debug)]
pub struct Program {
    pub items: Vec<Item>,
    pub expressions: Vec<Expr>,
}

impl Program {
    pub fn expr(&self, id: ExprId) -> &Expr {
        &self.expressions[id.0 as usize]
    }
}

#[derive(Clone, Debug)]
pub enum Item {
    Binding(Binding),
    Transform(TransformDecl),
    Expression(ExprId),
}

#[derive(Clone, Debug)]
pub struct Binding {
    pub name: String,
    pub name_span: Span,
    pub value: ExprId,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct TransformDecl {
    pub name: String,
    pub name_span: Span,
    pub parameters: Vec<Parameter>,
    pub return_type: TypeRef,
    pub body: Vec<InnerStmt>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Parameter {
    pub name: String,
    pub name_span: Span,
    pub ty: TypeRef,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct TypeRef {
    pub name: String,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum InnerStmt {
    Binding(Binding),
    Return {
        value: ExprId,
        span: Span,
    },
    If {
        condition: ExprId,
        then_body: Vec<InnerStmt>,
        else_body: Vec<InnerStmt>,
        span: Span,
    },
}

#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(String),
    Name(String),
    List(Vec<ExprId>),
    Record(Vec<RecordField>),
    Call {
        callee: ExprId,
        arguments: Vec<Argument>,
    },
    Binary {
        op: BinaryOp,
        left: ExprId,
        right: ExprId,
    },
    Pipeline {
        input: ExprId,
        stage: ExprId,
    },
}

#[derive(Clone, Debug)]
pub struct RecordField {
    pub name: String,
    pub name_span: Span,
    pub value: ExprId,
}

#[derive(Clone, Debug)]
pub struct Argument {
    pub name: Option<(String, Span)>,
    pub value: ExprId,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
}

impl BinaryOp {
    pub fn is_arithmetic(self) -> bool {
        matches!(
            self,
            Self::Add | Self::Subtract | Self::Multiply | Self::Divide
        )
    }

    pub fn is_equality(self) -> bool {
        matches!(self, Self::Equal | Self::NotEqual)
    }
}
