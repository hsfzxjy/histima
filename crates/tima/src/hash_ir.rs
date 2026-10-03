//! Canonical semantic identity graph used only to derive Transform IDs.
//!
//! Hash IR is the boundary between Tima's language-defined semantic
//! normalization and any execution/optimization IR. It is not interpreter or
//! backend IR, and compiler optimizations occur after or independently of this
//! representation. Hash IR deliberately does not attempt general program
//! equivalence, algebraic simplification, CSE, inlining, or dead-code
//! elimination. Omitting nodes unreachable from the semantic root removes
//! arena garbage; it is not an optimization pass.
//!
//! The v1 wire grammar and structural tags are frozen. Semantic nodes carry
//! schema-qualified names and independent schema versions, so language
//! semantics evolve through schemas without changing the structural format.
//! Node sharing is itself semantic: lowering creates one node per semantic
//! computation/value and reuses it for multiple uses. Distinct computations
//! remain distinct nodes even when structurally identical. The language
//! lowering must produce that sharing deterministically before execution
//! optimization can influence it.
//!
//! Backend details, artifacts, source locations, and source-facing names do
//! not belong in this graph. A lowering is responsible for choosing schemas
//! and fields that capture exactly its source language's semantic distinctions.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use crate::ast::BinaryOp as AstBinaryOp;
use crate::identity::TransformIdentity;
use crate::ir::{
    Capability as IrCapability, Constant as IrConstant, RuntimeCall as IrRuntimeCall,
    Terminator as IrTerminator, Transform as IrTransform, Type as IrType, ValueKind as IrValueKind,
};

pub const FORMAT_VERSION: u32 = 1;

/// Index into a definition's node arena. Arena order is not semantic: the
/// encoder renumbers reachable nodes by deterministic traversal from `root`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u32);

/// One finite, rooted semantic graph. Sharing between reachable nodes is part
/// of the graph's meaning; only arena allocation order is canonicalized away.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Definition {
    pub root: NodeId,
    pub nodes: Vec<Node>,
}

/// A node's schema gives its fields meaning. Schema evolution is independent
/// from the Hash IR wire format: incompatible schema semantics use a new
/// schema version, not a new Hash IR version.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Schema {
    pub namespace: String,
    pub name: String,
    pub version: u32,
}

impl Schema {
    pub fn new(namespace: impl Into<String>, name: impl Into<String>, version: u32) -> Self {
        Self {
            namespace: namespace.into(),
            name: name.into(),
            version,
        }
    }
}

/// A schema-qualified record. Field order in memory is deliberately ignored;
/// canonical encoding sorts fields by their UTF-8 names. Field names must be
/// unique within a node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub schema: Schema,
    pub fields: Vec<Field>,
}

impl Node {
    pub fn new(schema: Schema, fields: Vec<Field>) -> Self {
        Self { schema, fields }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub value: Data,
}

impl Field {
    pub fn new(name: impl Into<String>, value: Data) -> Self {
        Self {
            name: name.into(),
            value,
        }
    }
}

/// Closed set of structural atoms used to describe open-ended semantic
/// schemas. Records are nodes; maps are sequences of entry nodes. Larger or
/// unusual numeric forms can use canonical bytes interpreted by their schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Data {
    Unit,
    Bool(bool),
    UInt(u64),
    SInt(i64),
    F32Bits(u32),
    F64Bits(u64),
    Bytes(Vec<u8>),
    Text(String),
    Node(NodeId),
    Digest([u8; 32]),
    Sequence(Vec<Data>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationError {
    message: String,
}

impl ValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ValidationError {}

impl Definition {
    /// Validates and encodes Hash IR v1. Unreachable arena nodes are ignored;
    /// they cannot affect a rooted semantic definition.
    pub fn try_canonical_bytes(&self) -> Result<Vec<u8>, ValidationError> {
        let (order, canonical_ids) = self.canonical_order()?;
        let mut encoder = Encoder::default();
        encoder.raw(b"TIMA-HASH-IR\0");
        encoder.u32(FORMAT_VERSION);
        encoder.sequence(&order, |encoder, id| {
            self.encode_node(*id, &canonical_ids, encoder)
        });
        encoder.u32(0); // deterministic traversal always assigns the root ID zero
        Ok(encoder.bytes)
    }

    /// Returns the canonical bytes for a trusted compiler-produced graph.
    /// Use `try_canonical_bytes` when accepting graphs from another producer.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        self.try_canonical_bytes()
            .expect("compiler-produced Hash IR is valid")
    }

    fn canonical_order(&self) -> Result<(Vec<NodeId>, Vec<Option<u32>>), ValidationError> {
        if self.root.0 as usize >= self.nodes.len() {
            return Err(ValidationError::new(format!(
                "Hash IR root node {} is outside the {}-node arena",
                self.root.0,
                self.nodes.len()
            )));
        }

        let mut order = Vec::new();
        let mut canonical_ids = vec![None; self.nodes.len()];
        self.discover(self.root, &mut order, &mut canonical_ids)?;
        Ok((order, canonical_ids))
    }

    fn discover(
        &self,
        id: NodeId,
        order: &mut Vec<NodeId>,
        canonical_ids: &mut [Option<u32>],
    ) -> Result<(), ValidationError> {
        let index = id.0 as usize;
        let Some(node) = self.nodes.get(index) else {
            return Err(ValidationError::new(format!(
                "Hash IR references node {} outside the {}-node arena",
                id.0,
                self.nodes.len()
            )));
        };
        if canonical_ids[index].is_some() {
            return Ok(());
        }

        canonical_ids[index] = Some(length(order.len()));
        order.push(id);
        validate_schema(&node.schema, id)?;
        let fields = sorted_fields(node, id)?;
        for field in fields {
            self.discover_data(&field.value, order, canonical_ids)?;
        }
        Ok(())
    }

    fn discover_data(
        &self,
        data: &Data,
        order: &mut Vec<NodeId>,
        canonical_ids: &mut [Option<u32>],
    ) -> Result<(), ValidationError> {
        match data {
            Data::Node(id) => self.discover(*id, order, canonical_ids),
            Data::Sequence(values) => {
                for value in values {
                    self.discover_data(value, order, canonical_ids)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn encode_node(&self, id: NodeId, canonical_ids: &[Option<u32>], encoder: &mut Encoder) {
        let node = &self.nodes[id.0 as usize];
        encoder.text(&node.schema.namespace);
        encoder.text(&node.schema.name);
        encoder.u32(node.schema.version);
        let fields = sorted_fields(node, id).expect("graph was validated during discovery");
        encoder.sequence(&fields, |encoder, field| {
            encoder.text(&field.name);
            encode_data(&field.value, canonical_ids, encoder);
        });
    }
}

fn validate_schema(schema: &Schema, id: NodeId) -> Result<(), ValidationError> {
    if schema.namespace.is_empty() || schema.name.is_empty() {
        return Err(ValidationError::new(format!(
            "Hash IR node {} has an empty schema namespace or name",
            id.0
        )));
    }
    Ok(())
}

fn sorted_fields(node: &Node, id: NodeId) -> Result<Vec<&Field>, ValidationError> {
    let mut names = BTreeSet::new();
    for field in &node.fields {
        if field.name.is_empty() {
            return Err(ValidationError::new(format!(
                "Hash IR node {} has an empty field name",
                id.0
            )));
        }
        if !names.insert(field.name.as_str()) {
            return Err(ValidationError::new(format!(
                "Hash IR node {} repeats field `{}`",
                id.0, field.name
            )));
        }
    }
    let mut fields: Vec<_> = node.fields.iter().collect();
    fields.sort_unstable_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
    Ok(fields)
}

fn encode_data(data: &Data, canonical_ids: &[Option<u32>], encoder: &mut Encoder) {
    match data {
        Data::Unit => encoder.u8(0),
        Data::Bool(value) => {
            encoder.u8(1);
            encoder.u8(u8::from(*value));
        }
        Data::UInt(value) => {
            encoder.u8(2);
            encoder.u64(*value);
        }
        Data::SInt(value) => {
            encoder.u8(3);
            encoder.i64(*value);
        }
        Data::F32Bits(value) => {
            encoder.u8(4);
            encoder.u32(*value);
        }
        Data::F64Bits(value) => {
            encoder.u8(5);
            encoder.u64(*value);
        }
        Data::Bytes(value) => {
            encoder.u8(6);
            encoder.data(value);
        }
        Data::Text(value) => {
            encoder.u8(7);
            encoder.text(value);
        }
        Data::Node(id) => {
            encoder.u8(8);
            encoder.u32(
                canonical_ids[id.0 as usize]
                    .expect("all referenced nodes were discovered before encoding"),
            );
        }
        Data::Digest(value) => {
            encoder.u8(9);
            encoder.raw(value);
        }
        Data::Sequence(values) => {
            encoder.u8(10);
            encoder.sequence(values, |encoder, value| {
                encode_data(value, canonical_ids, encoder)
            });
        }
    }
}

/// Lowers Tima's pre-optimization, language-normalized typed structure into
/// the open Hash IR graph. This must never consume backend-optimized IR.
/// `references` is parallel to the typed value arena and contains resolved
/// semantic Transform IDs for call nodes.
pub(crate) fn lower_tima(
    transform: &IrTransform,
    references: &[Option<TransformIdentity>],
) -> Definition {
    assert_eq!(transform.values.len(), references.len());
    let mut builder = Builder::default();
    let types = TypeNodes::new(&mut builder);

    let value_nodes: Vec<_> = transform.values.iter().map(|_| builder.reserve()).collect();
    let block_nodes: Vec<_> = transform.blocks.iter().map(|_| builder.reserve()).collect();

    for (index, (value, reference)) in transform.values.iter().zip(references).enumerate() {
        let type_node = types.get(value.ty);
        let node = lower_value(&value.kind, type_node, *reference, &value_nodes);
        builder.replace(value_nodes[index], node);
    }

    for (index, block) in transform.blocks.iter().enumerate() {
        let terminator = match block.terminator {
            IrTerminator::Return(value) => builder.node(
                tima("terminator.return"),
                vec![field("value", node_data(value_nodes[value.0 as usize]))],
            ),
            IrTerminator::Branch {
                condition,
                then_block,
                else_block,
            } => builder.node(
                tima("terminator.branch"),
                vec![
                    field("condition", node_data(value_nodes[condition.0 as usize])),
                    field("then", node_data(block_nodes[then_block.0 as usize])),
                    field("else", node_data(block_nodes[else_block.0 as usize])),
                ],
            ),
            IrTerminator::Jump(target) => builder.node(
                tima("terminator.jump"),
                vec![field("target", node_data(block_nodes[target.0 as usize]))],
            ),
        };
        builder.replace(
            block_nodes[index],
            Node::new(
                tima("cfg.block"),
                vec![
                    field("arguments", Data::Sequence(Vec::new())),
                    field(
                        "instructions",
                        nodes_data(
                            block
                                .instructions
                                .iter()
                                .map(|value| value_nodes[value.0 as usize]),
                        ),
                    ),
                    field("terminator", node_data(terminator)),
                ],
            ),
        );
    }

    let mut capabilities: Vec<_> = transform.capabilities.clone();
    capabilities.sort_unstable();
    capabilities.dedup();
    let capabilities: Vec<_> = capabilities
        .into_iter()
        .map(|capability| {
            let name = match capability {
                IrCapability::EnvironmentRead => "capability.env.read",
                IrCapability::FileRead => "capability.file.read",
                IrCapability::HttpGet => "capability.http.get",
            };
            let id = builder.node(tima(name), Vec::new());
            Data::Node(id)
        })
        .collect();

    let root = builder.node(
        tima("definition.transform"),
        vec![
            field("capabilities", Data::Sequence(capabilities)),
            field("entry", node_data(block_nodes[transform.entry.0 as usize])),
            field(
                "parameters",
                nodes_data(
                    transform
                        .parameters
                        .iter()
                        .map(|parameter| types.get(parameter.ty)),
                ),
            ),
            field("result", node_data(types.get(transform.return_type))),
        ],
    );
    builder.finish(root)
}

pub(crate) fn external(scheme: &str, name: &str, semantic_version: u32) -> Definition {
    external_definition(scheme, name, semantic_version, None, &[], None)
}

pub(crate) fn registered_wasm_external(
    name: &str,
    semantic_version: u32,
    abi_version: u32,
    parameters: &[(&str, u8)],
    result: u8,
) -> Definition {
    external_definition(
        "tima.registered-wasm-contract",
        name,
        semantic_version,
        Some(abi_version),
        parameters,
        Some(result),
    )
}

fn external_definition(
    scheme: &str,
    name: &str,
    semantic_version: u32,
    interface_version: Option<u32>,
    parameters: &[(&str, u8)],
    result: Option<u8>,
) -> Definition {
    let mut builder = Builder::default();
    let parameters: Vec<_> = parameters
        .iter()
        .map(|(name, type_code)| {
            let parameter = builder.node(
                histima("external.parameter"),
                vec![
                    field("name", Data::Text((*name).to_owned())),
                    field("type_code", Data::UInt(u64::from(*type_code))),
                ],
            );
            Data::Node(parameter)
        })
        .collect();
    let root = builder.node(
        histima("definition.external-transform"),
        vec![
            field(
                "interface_version",
                interface_version.map_or(Data::Unit, |value| Data::UInt(u64::from(value))),
            ),
            field("name", Data::Text(name.to_owned())),
            field("parameters", Data::Sequence(parameters)),
            field(
                "result_type",
                result.map_or(Data::Unit, |value| Data::UInt(u64::from(value))),
            ),
            field("scheme", Data::Text(scheme.to_owned())),
            field("semantic_version", Data::UInt(u64::from(semantic_version))),
        ],
    );
    builder.finish(root)
}

fn lower_value(
    kind: &IrValueKind,
    ty: NodeId,
    reference: Option<TransformIdentity>,
    values: &[NodeId],
) -> Node {
    let typed = |mut fields: Vec<Field>| {
        fields.push(field("type", node_data(ty)));
        fields
    };
    match kind {
        IrValueKind::Parameter { index } => Node::new(
            tima("value.parameter"),
            typed(vec![field("index", Data::UInt(u64::from(*index)))]),
        ),
        IrValueKind::Constant(constant) => match constant {
            IrConstant::Bool(value) => Node::new(
                tima("constant.bool"),
                typed(vec![field("value", Data::Bool(*value))]),
            ),
            IrConstant::I64(value) => Node::new(
                tima("constant.i64"),
                typed(vec![field("value", Data::SInt(*value))]),
            ),
            IrConstant::F32(value) => Node::new(
                tima("constant.f32"),
                typed(vec![field("bits", Data::F32Bits(value.to_bits()))]),
            ),
            IrConstant::String(value) => Node::new(
                tima("constant.string"),
                typed(vec![field("value", Data::Text(value.clone()))]),
            ),
        },
        IrValueKind::Binary { op, left, right } => Node::new(
            tima(binary_schema(*op)),
            typed(vec![
                field("left", node_data(values[left.0 as usize])),
                field("right", node_data(values[right.0 as usize])),
            ]),
        ),
        IrValueKind::U8Scale { value, factor } => Node::new(
            tima("numeric.u8-scale"),
            typed(vec![
                field("value", node_data(values[value.0 as usize])),
                field("factor", node_data(values[factor.0 as usize])),
            ]),
        ),
        IrValueKind::Call { arguments, .. } => Node::new(
            tima("operation.call"),
            typed(vec![
                field(
                    "arguments",
                    nodes_data(arguments.iter().map(|value| values[value.0 as usize])),
                ),
                field(
                    "transform",
                    // Calls cross the identity boundary by semantic digest,
                    // never by source name, declaration order, or IR index.
                    Data::Digest(
                        *reference
                            .expect("call values have a resolved Transform ID")
                            .as_bytes(),
                    ),
                ),
            ]),
        ),
        IrValueKind::RuntimeCall(call) => match call {
            IrRuntimeCall::EnvironmentI64 { name } => Node::new(
                tima("world.environment-i64"),
                typed(vec![field("name", Data::Text(name.clone()))]),
            ),
            IrRuntimeCall::EnvironmentRead { name } => Node::new(
                tima("world.environment-read"),
                typed(vec![field("name", node_data(values[name.0 as usize]))]),
            ),
            IrRuntimeCall::FileRead { path } => Node::new(
                tima("world.file-read"),
                typed(vec![field("path", node_data(values[path.0 as usize]))]),
            ),
            IrRuntimeCall::HttpGet { url } => Node::new(
                tima("world.http-get"),
                typed(vec![field("url", node_data(values[url.0 as usize]))]),
            ),
        },
        IrValueKind::BufferZero { buffer } => Node::new(
            tima("buffer.zero"),
            typed(vec![field("buffer", node_data(values[buffer.0 as usize]))]),
        ),
        IrValueKind::BufferFill { buffer, value } => Node::new(
            tima("buffer.fill"),
            typed(vec![
                field("buffer", node_data(values[buffer.0 as usize])),
                field("value", node_data(values[value.0 as usize])),
            ]),
        ),
        IrValueKind::BufferByteElement => Node::new(tima("buffer.byte-element"), typed(Vec::new())),
        IrValueKind::BufferByteIndex => Node::new(tima("buffer.byte-index"), typed(Vec::new())),
        IrValueKind::BufferByteMap {
            buffer,
            element,
            index: None,
            instructions,
            result,
        } => Node::new(
            tima("buffer.byte-map"),
            typed(vec![
                field("buffer", node_data(values[buffer.0 as usize])),
                field("element", node_data(values[element.0 as usize])),
                field(
                    "instructions",
                    nodes_data(instructions.iter().map(|value| values[value.0 as usize])),
                ),
                field("result", node_data(values[result.0 as usize])),
            ]),
        ),
        IrValueKind::BufferByteMap {
            buffer,
            element,
            index: Some(index),
            instructions,
            result,
        } => Node::new(
            tima("buffer.byte-map-indexed"),
            typed(vec![
                field("buffer", node_data(values[buffer.0 as usize])),
                field("element", node_data(values[element.0 as usize])),
                field("index", node_data(values[index.0 as usize])),
                field(
                    "instructions",
                    nodes_data(instructions.iter().map(|value| values[value.0 as usize])),
                ),
                field("result", node_data(values[result.0 as usize])),
            ]),
        ),
    }
}

fn binary_schema(op: AstBinaryOp) -> &'static str {
    match op {
        AstBinaryOp::Add => "binary.add",
        AstBinaryOp::Subtract => "binary.subtract",
        AstBinaryOp::Multiply => "binary.multiply",
        AstBinaryOp::Divide => "binary.divide",
        AstBinaryOp::Equal => "binary.equal",
        AstBinaryOp::NotEqual => "binary.not-equal",
        AstBinaryOp::Less => "binary.less",
        AstBinaryOp::LessEqual => "binary.less-equal",
        AstBinaryOp::Greater => "binary.greater",
        AstBinaryOp::GreaterEqual => "binary.greater-equal",
    }
}

struct TypeNodes {
    bool_: NodeId,
    u8_: NodeId,
    i64_: NodeId,
    f32_: NodeId,
    string: NodeId,
    string_view: NodeId,
    bytes: NodeId,
    bytes_view: NodeId,
    buffer: NodeId,
    buffer_view: NodeId,
}

impl TypeNodes {
    fn new(builder: &mut Builder) -> Self {
        Self {
            bool_: builder.node(tima("type.bool"), Vec::new()),
            u8_: builder.node(tima("type.u8"), Vec::new()),
            i64_: builder.node(tima("type.i64"), Vec::new()),
            f32_: builder.node(tima("type.f32"), Vec::new()),
            string: builder.node(tima("type.string"), Vec::new()),
            string_view: builder.node(tima("type.string-view"), Vec::new()),
            bytes: builder.node(tima("type.bytes"), Vec::new()),
            bytes_view: builder.node(tima("type.bytes-view"), Vec::new()),
            buffer: builder.node(tima("type.buffer"), Vec::new()),
            buffer_view: builder.node(tima("type.buffer-view"), Vec::new()),
        }
    }

    fn get(&self, ty: IrType) -> NodeId {
        match ty {
            IrType::Bool => self.bool_,
            IrType::U8 => self.u8_,
            IrType::I64 => self.i64_,
            IrType::F32 => self.f32_,
            IrType::String => self.string,
            IrType::StringView => self.string_view,
            IrType::Bytes => self.bytes,
            IrType::BytesView => self.bytes_view,
            IrType::Buffer => self.buffer,
            IrType::BufferView => self.buffer_view,
        }
    }
}

#[derive(Default)]
struct Builder {
    nodes: Vec<Node>,
}

impl Builder {
    fn reserve(&mut self) -> NodeId {
        self.node(Schema::new("tima.internal", "reserved", 0), Vec::new())
    }

    fn replace(&mut self, id: NodeId, node: Node) {
        self.nodes[id.0 as usize] = node;
    }

    fn node(&mut self, schema: Schema, fields: Vec<Field>) -> NodeId {
        let id = NodeId(length(self.nodes.len()));
        self.nodes.push(Node::new(schema, fields));
        id
    }

    fn finish(self, root: NodeId) -> Definition {
        Definition {
            root,
            nodes: self.nodes,
        }
    }
}

fn tima(name: &str) -> Schema {
    Schema::new("tima", name, 1)
}

fn histima(name: &str) -> Schema {
    Schema::new("histima", name, 1)
}

fn field(name: &str, value: Data) -> Field {
    Field::new(name, value)
}

fn node_data(id: NodeId) -> Data {
    Data::Node(id)
}

fn nodes_data(values: impl IntoIterator<Item = NodeId>) -> Data {
    Data::Sequence(values.into_iter().map(Data::Node).collect())
}

#[derive(Default)]
struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    fn raw(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    fn data(&mut self, value: &[u8]) {
        self.u32(length(value.len()));
        self.raw(value);
    }

    fn text(&mut self, value: &str) {
        self.data(value.as_bytes());
    }

    fn sequence<T>(&mut self, values: &[T], encode: impl Fn(&mut Self, &T)) {
        self.u32(length(values.len()));
        for value in values {
            encode(self, value);
        }
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.raw(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.raw(&value.to_le_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.raw(&value.to_le_bytes());
    }
}

fn length(value: usize) -> u32 {
    u32::try_from(value).expect("Hash IR collections fit in u32 arenas")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_graph_ignores_arena_and_field_order() {
        let first = Definition {
            root: NodeId(0),
            nodes: vec![
                Node::new(
                    Schema::new("example", "pair", 1),
                    vec![
                        Field::new("right", Data::Node(NodeId(2))),
                        Field::new("left", Data::Node(NodeId(1))),
                    ],
                ),
                Node::new(
                    Schema::new("example", "integer", 1),
                    vec![Field::new("value", Data::SInt(1))],
                ),
                Node::new(
                    Schema::new("example", "integer", 1),
                    vec![Field::new("value", Data::SInt(2))],
                ),
            ],
        };
        let second = Definition {
            root: NodeId(2),
            nodes: vec![
                Node::new(
                    Schema::new("example", "integer", 1),
                    vec![Field::new("value", Data::SInt(2))],
                ),
                Node::new(
                    Schema::new("example", "integer", 1),
                    vec![Field::new("value", Data::SInt(1))],
                ),
                Node::new(
                    Schema::new("example", "pair", 1),
                    vec![
                        Field::new("left", Data::Node(NodeId(1))),
                        Field::new("right", Data::Node(NodeId(0))),
                    ],
                ),
            ],
        };

        assert_eq!(first.canonical_bytes(), second.canonical_bytes());
    }

    #[test]
    fn canonical_graph_preserves_the_same_sharing_across_arena_layouts() {
        let root_first = Definition {
            root: NodeId(0),
            nodes: vec![
                Node::new(
                    Schema::new("example", "pair", 1),
                    vec![
                        Field::new("right", Data::Node(NodeId(1))),
                        Field::new("left", Data::Node(NodeId(1))),
                    ],
                ),
                Node::new(
                    Schema::new("example", "integer", 1),
                    vec![Field::new("value", Data::SInt(7))],
                ),
            ],
        };
        let child_first = Definition {
            root: NodeId(1),
            nodes: vec![
                Node::new(
                    Schema::new("example", "integer", 1),
                    vec![Field::new("value", Data::SInt(7))],
                ),
                Node::new(
                    Schema::new("example", "pair", 1),
                    vec![
                        Field::new("left", Data::Node(NodeId(0))),
                        Field::new("right", Data::Node(NodeId(0))),
                    ],
                ),
            ],
        };

        assert_eq!(root_first.canonical_bytes(), child_first.canonical_bytes());
    }

    #[test]
    fn node_sharing_is_an_intentional_normative_semantic_distinction() {
        let shared = Definition {
            root: NodeId(0),
            nodes: vec![
                Node::new(
                    Schema::new("example", "pair", 1),
                    vec![
                        Field::new("left", Data::Node(NodeId(1))),
                        Field::new("right", Data::Node(NodeId(1))),
                    ],
                ),
                Node::new(
                    Schema::new("example", "computation", 1),
                    vec![Field::new("value", Data::SInt(7))],
                ),
            ],
        };
        let recomputed = Definition {
            root: NodeId(0),
            nodes: vec![
                Node::new(
                    Schema::new("example", "pair", 1),
                    vec![
                        Field::new("left", Data::Node(NodeId(1))),
                        Field::new("right", Data::Node(NodeId(2))),
                    ],
                ),
                Node::new(
                    Schema::new("example", "computation", 1),
                    vec![Field::new("value", Data::SInt(7))],
                ),
                Node::new(
                    Schema::new("example", "computation", 1),
                    vec![Field::new("value", Data::SInt(7))],
                ),
            ],
        };

        // Hash IR does not perform structural hash-consing: one computation
        // used twice and two identical computations are different semantics.
        assert_ne!(shared.canonical_bytes(), recomputed.canonical_bytes());
    }

    #[test]
    fn graph_supports_cycles_and_ignores_unreachable_nodes() {
        let base = Definition {
            root: NodeId(0),
            nodes: vec![Node::new(
                Schema::new("example", "loop", 1),
                vec![Field::new("next", Data::Node(NodeId(0)))],
            )],
        };
        let with_unreachable = Definition {
            root: NodeId(1),
            nodes: vec![
                Node::new(Schema::new("ignored", "node", 99), Vec::new()),
                Node::new(
                    Schema::new("example", "loop", 1),
                    vec![Field::new("next", Data::Node(NodeId(1)))],
                ),
            ],
        };

        assert_eq!(base.canonical_bytes(), with_unreachable.canonical_bytes());
    }

    #[test]
    fn schemas_are_open_without_new_wire_tags() {
        let first = Definition {
            root: NodeId(0),
            nodes: vec![Node::new(
                Schema::new("future.language", "operation.quantum-fold", 7),
                vec![Field::new(
                    "payload",
                    Data::Sequence(vec![Data::UInt(3), Data::Bytes(vec![1, 2, 3])]),
                )],
            )],
        };
        let second = Definition {
            root: NodeId(0),
            nodes: vec![Node::new(
                Schema::new("future.language", "operation.quantum-fold", 8),
                vec![Field::new(
                    "payload",
                    Data::Sequence(vec![Data::UInt(3), Data::Bytes(vec![1, 2, 3])]),
                )],
            )],
        };

        assert_ne!(first.canonical_bytes(), second.canonical_bytes());
        assert!(first.canonical_bytes().starts_with(b"TIMA-HASH-IR\0"));
    }

    #[test]
    fn v1_encoding_vector_is_stable() {
        let definition = Definition {
            root: NodeId(0),
            nodes: vec![Node::new(Schema::new("a", "bc", 1), Vec::new())],
        };

        assert_eq!(
            definition
                .canonical_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            "54494d412d484153482d49520001000000010000000100000061020000006263010000000000000000000000"
        );
    }

    #[test]
    fn malformed_graph_is_rejected() {
        let duplicate = Definition {
            root: NodeId(0),
            nodes: vec![Node::new(
                Schema::new("example", "bad", 1),
                vec![
                    Field::new("same", Data::Unit),
                    Field::new("same", Data::Bool(true)),
                ],
            )],
        };
        let dangling = Definition {
            root: NodeId(0),
            nodes: vec![Node::new(
                Schema::new("example", "bad", 1),
                vec![Field::new("missing", Data::Node(NodeId(1)))],
            )],
        };

        assert!(duplicate.try_canonical_bytes().is_err());
        assert!(dangling.try_canonical_bytes().is_err());
    }
}
