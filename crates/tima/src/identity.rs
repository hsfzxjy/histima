use std::error::Error;
use std::fmt;

use crate::ast::BinaryOp;
use crate::diagnostic::Diagnostic;
use crate::ir::{
    Capability, Constant, RuntimeCall, Terminator, TransformId, Type, TypedModule, ValueKind,
};
use crate::runtime::{OuterValue, ValueData};

/// Version of Tima's canonical semantic encoding. Incrementing this does not
/// change the native ABI version; it deliberately invalidates semantic IDs.
pub const SEMANTIC_ID_VERSION: u32 = 1;
const TRANSFORM_ID_VERSION: u32 = 2;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Digest([u8; 32]);

impl Digest {
    fn from_hasher(hasher: CanonicalHasher) -> Self {
        Self(hasher.finish())
    }

    fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityParseError {
    message: String,
}

impl fmt::Display for IdentityParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for IdentityParseError {}

fn parse_digest(text: &str) -> Result<Digest, IdentityParseError> {
    if text.len() != 64 {
        return Err(IdentityParseError {
            message: format!(
                "identity must contain exactly 64 lowercase hexadecimal characters, found {}",
                text.len()
            ),
        });
    }
    let mut bytes = [0_u8; 32];
    let (pairs, remainder) = text.as_bytes().as_chunks::<2>();
    debug_assert!(remainder.is_empty());
    for (index, pair) in pairs.iter().enumerate() {
        bytes[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Ok(Digest(bytes))
}

fn hex_nibble(byte: u8) -> Result<u8, IdentityParseError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(IdentityParseError {
            message: "identity must use canonical lowercase hexadecimal characters".to_owned(),
        }),
    }
}

fn format_digest(digest: Digest, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    for byte in digest.0 {
        write!(formatter, "{byte:02x}")?;
    }
    Ok(())
}

macro_rules! identity_type {
    ($name:ident) => {
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(Digest);

        impl $name {
            pub fn as_bytes(&self) -> &[u8; 32] {
                self.0.as_bytes()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                format_digest(self.0, formatter)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_tuple(stringify!($name))
                    .field(&self.to_string())
                    .finish()
            }
        }

        impl std::str::FromStr for $name {
            type Err = IdentityParseError;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                parse_digest(text).map(Self)
            }
        }
    };
}

identity_type!(TransformIdentity);
identity_type!(RecipeIdentity);
identity_type!(ContentIdentity);
identity_type!(ArtifactIdentity);
identity_type!(ArtifactBundleIdentity);
identity_type!(DependencyIdentity);
identity_type!(SourceIdentity);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SemanticValueIdentity {
    Content(ContentIdentity),
    Source(SourceIdentity),
    Recipe(RecipeIdentity),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IdentityDomain {
    Transform,
    Source,
    Recipe,
    Content,
}

impl IdentityDomain {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Transform => "Transform ID",
            Self::Source => "Source ID",
            Self::Recipe => "Recipe ID",
            Self::Content => "Content ID",
        }
    }
}

/// Supplies identities known to a host-local store for abbreviated identity
/// assertions. Implementations may return at most enough matches to establish
/// ambiguity; callers canonicalize duplicates before deciding.
pub trait IdentityPrefixResolver {
    fn matching_identities(
        &self,
        domain: IdentityDomain,
        prefix: &str,
    ) -> Result<Vec<String>, String>;
}

impl From<ContentIdentity> for SemanticValueIdentity {
    fn from(value: ContentIdentity) -> Self {
        Self::Content(value)
    }
}

impl From<SourceIdentity> for SemanticValueIdentity {
    fn from(value: SourceIdentity) -> Self {
        Self::Source(value)
    }
}

impl From<RecipeIdentity> for SemanticValueIdentity {
    fn from(value: RecipeIdentity) -> Self {
        Self::Recipe(value)
    }
}

impl fmt::Display for SemanticValueIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Content(identity) => write!(formatter, "content:{identity}"),
            Self::Source(identity) => write!(formatter, "source:{identity}"),
            Self::Recipe(identity) => write!(formatter, "recipe:{identity}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct TransformIdentities {
    values: Vec<TransformIdentity>,
}

impl TransformIdentities {
    pub fn get(&self, id: TransformId) -> TransformIdentity {
        self.values[id.0 as usize]
    }

    pub fn find(&self, module: &TypedModule, name: &str) -> Option<TransformIdentity> {
        module.find(name).map(|(id, _)| self.get(id))
    }

    pub fn iter(&self) -> impl Iterator<Item = TransformIdentity> + '_ {
        self.values.iter().copied()
    }

    pub fn find_id(&self, identity: TransformIdentity) -> Option<TransformId> {
        self.values
            .iter()
            .position(|candidate| *candidate == identity)
            .map(|index| TransformId(index as u32))
    }
}

pub fn transform_identities(module: &TypedModule) -> Result<TransformIdentities, Vec<Diagnostic>> {
    let mut resolver = TransformIdentityResolver {
        module,
        values: vec![None; module.transforms.len()],
        visiting: vec![false; module.transforms.len()],
    };
    for index in 0..module.transforms.len() {
        resolver.resolve(TransformId(index as u32))?;
    }
    Ok(TransformIdentities {
        values: resolver
            .values
            .into_iter()
            .map(|value| value.expect("all transform identities resolved"))
            .collect(),
    })
}

/// Semantic identity for a registered transform whose implementation is
/// versioned independently from user-authored typed IR.
pub fn registered_transform_identity(name: &str, semantic_version: u32) -> TransformIdentity {
    let mut hasher = CanonicalHasher::new(b"tima.registered-transform");
    hasher.u32(SEMANTIC_ID_VERSION);
    hasher.bytes(name.as_bytes());
    hasher.u32(semantic_version);
    TransformIdentity(Digest::from_hasher(hasher))
}

/// Semantic identity for an explicitly registered Wasm transform contract.
///
/// Exact module bytes are deliberately excluded: replacing an artifact with a
/// semantically equivalent implementation preserves this identity. Parameter
/// names are included because they are part of outer named-call semantics.
pub fn registered_wasm_transform_identity(
    name: &str,
    semantic_version: u32,
    abi_version: u32,
    parameters: &[(&str, u8)],
    result: u8,
) -> TransformIdentity {
    let mut hasher = CanonicalHasher::new(b"tima.registered-wasm-transform");
    hasher.u32(SEMANTIC_ID_VERSION);
    hasher.bytes(name.as_bytes());
    hasher.u32(semantic_version);
    hasher.u32(abi_version);
    hasher.u64(parameters.len() as u64);
    for (parameter_name, parameter_type) in parameters {
        hasher.bytes(parameter_name.as_bytes());
        hasher.u8(*parameter_type);
    }
    hasher.u8(result);
    TransformIdentity(Digest::from_hasher(hasher))
}

struct TransformIdentityResolver<'a> {
    module: &'a TypedModule,
    values: Vec<Option<TransformIdentity>>,
    visiting: Vec<bool>,
}

impl TransformIdentityResolver<'_> {
    fn resolve(&mut self, id: TransformId) -> Result<TransformIdentity, Vec<Diagnostic>> {
        if let Some(identity) = self.values[id.0 as usize] {
            return Ok(identity);
        }
        if self.visiting[id.0 as usize] {
            let transform = self.module.get(id);
            return Err(vec![Diagnostic::error(
                "recursive transform definitions are not supported by the initial semantic identity pass",
                transform.span,
            )
            .with_note("recursive identity requires canonical strongly connected definition groups")]);
        }
        self.visiting[id.0 as usize] = true;
        let transform = self.module.get(id);
        let mut referenced = Vec::with_capacity(transform.values.len());
        for value in &transform.values {
            let identity = match &value.kind {
                ValueKind::Call {
                    transform: callee, ..
                } => Some(self.resolve(*callee)?),
                _ => None,
            };
            referenced.push(identity);
        }

        let mut hasher = CanonicalHasher::new(b"tima.transform");
        hasher.u32(TRANSFORM_ID_VERSION);
        hasher.u32(transform.capabilities.len() as u32);
        for capability in &transform.capabilities {
            hasher.u8(match capability {
                Capability::EnvironmentRead => 1,
                Capability::FileRead => 2,
                Capability::HttpGet => 3,
            });
        }
        hasher.u32(transform.parameters.len() as u32);
        for parameter in &transform.parameters {
            encode_type(&mut hasher, parameter.ty);
        }
        encode_type(&mut hasher, transform.return_type);
        hasher.u32(transform.values.len() as u32);
        for (value, referenced_identity) in transform.values.iter().zip(referenced) {
            encode_type(&mut hasher, value.ty);
            match &value.kind {
                ValueKind::Parameter { index } => {
                    hasher.u8(0);
                    hasher.u32(*index);
                }
                ValueKind::Constant(constant) => {
                    hasher.u8(1);
                    encode_constant(&mut hasher, constant);
                }
                ValueKind::Binary { op, left, right } => {
                    hasher.u8(2);
                    encode_binary_op(&mut hasher, *op);
                    hasher.u32(left.0);
                    hasher.u32(right.0);
                }
                ValueKind::Call { arguments, .. } => {
                    hasher.u8(3);
                    hasher.raw(
                        referenced_identity
                            .expect("call values have referenced identity")
                            .as_bytes(),
                    );
                    hasher.u32(arguments.len() as u32);
                    for argument in arguments {
                        hasher.u32(argument.0);
                    }
                }
                ValueKind::ImageZero { image } => {
                    hasher.u8(5);
                    hasher.u32(image.0);
                }
                ValueKind::ImageFill { image, value } => {
                    hasher.u8(6);
                    hasher.u32(image.0);
                    hasher.u32(value.0);
                }
                ValueKind::ImageByteElement => hasher.u8(7),
                ValueKind::ImageByteMap {
                    image,
                    element,
                    instructions,
                    result,
                } => {
                    hasher.u8(8);
                    hasher.u32(image.0);
                    hasher.u32(element.0);
                    hasher.u32(instructions.len() as u32);
                    for instruction in instructions {
                        hasher.u32(instruction.0);
                    }
                    hasher.u32(result.0);
                }
                ValueKind::ImageRgba8Scale { image, channels } => {
                    hasher.u8(9);
                    hasher.u32(image.0);
                    hasher.u32(channels.len() as u32);
                    for (channel, factor) in channels {
                        hasher.u8(channel.offset() as u8);
                        hasher.u32(factor.0);
                    }
                }
                ValueKind::RuntimeCall(RuntimeCall::EnvironmentI64 { name }) => {
                    hasher.u8(4);
                    hasher.u8(0);
                    hasher.bytes(name.as_bytes());
                }
                ValueKind::RuntimeCall(RuntimeCall::EnvironmentRead { name }) => {
                    hasher.u8(4);
                    hasher.u8(1);
                    hasher.u32(name.0);
                }
                ValueKind::RuntimeCall(RuntimeCall::FileRead { path }) => {
                    hasher.u8(4);
                    hasher.u8(2);
                    hasher.u32(path.0);
                }
                ValueKind::RuntimeCall(RuntimeCall::HttpGet { url }) => {
                    hasher.u8(4);
                    hasher.u8(3);
                    hasher.u32(url.0);
                }
            }
        }
        hasher.u32(transform.blocks.len() as u32);
        hasher.u32(transform.entry.0);
        for block in &transform.blocks {
            hasher.u32(block.instructions.len() as u32);
            for instruction in &block.instructions {
                hasher.u32(instruction.0);
            }
            match block.terminator {
                Terminator::Return(value) => {
                    hasher.u8(0);
                    hasher.u32(value.0);
                }
                Terminator::Jump(target) => {
                    hasher.u8(2);
                    hasher.u32(target.0);
                }
                Terminator::Branch {
                    condition,
                    then_block,
                    else_block,
                } => {
                    hasher.u8(1);
                    hasher.u32(condition.0);
                    hasher.u32(then_block.0);
                    hasher.u32(else_block.0);
                }
            }
        }
        let identity = TransformIdentity(Digest::from_hasher(hasher));
        self.visiting[id.0 as usize] = false;
        self.values[id.0 as usize] = Some(identity);
        Ok(identity)
    }
}

pub fn content_identity(value: &OuterValue) -> Result<ContentIdentity, IdentityError> {
    let mut hasher = CanonicalHasher::new(b"tima.content");
    hasher.u32(SEMANTIC_ID_VERSION);
    encode_outer_value(&mut hasher, value)?;
    Ok(ContentIdentity(Digest::from_hasher(hasher)))
}

fn encode_outer_value(
    hasher: &mut CanonicalHasher,
    value: &OuterValue,
) -> Result<(), IdentityError> {
    match &value.data {
        ValueData::Null => hasher.u8(0),
        ValueData::Bool(value) => {
            hasher.u8(1);
            hasher.u8(u8::from(*value));
        }
        ValueData::Integer(value) => {
            hasher.u8(2);
            hasher.i64(*value);
        }
        ValueData::Float(value) => {
            hasher.u8(3);
            hasher.u32(value.to_bits());
        }
        ValueData::Fraction(value) => {
            hasher.u8(9);
            hasher.i64(value.numerator());
            hasher.u64(value.denominator());
        }
        ValueData::String(value) => {
            hasher.u8(4);
            hasher.bytes(value.as_bytes());
        }
        ValueData::Bytes(value) => {
            hasher.u8(8);
            hasher.bytes(value);
        }
        ValueData::List(values) => {
            hasher.u8(5);
            hasher.u64(values.len() as u64);
            for value in values.iter() {
                hasher.raw(content_identity(value)?.as_bytes());
            }
        }
        ValueData::Record(values) => {
            hasher.u8(6);
            hasher.u64(values.len() as u64);
            for (name, value) in values.iter() {
                hasher.bytes(name.as_bytes());
                hasher.raw(content_identity(value)?.as_bytes());
            }
        }
        ValueData::Image(image) => {
            hasher.u8(7);
            hasher.u32(image.format().abi_tag());
            hasher.u64(image.width() as u64);
            hasher.u64(image.height() as u64);
            hasher.u64(image.stride() as u64);
            image.with_bytes(|bytes| hasher.bytes(bytes));
        }
        ValueData::Asset(asset) => {
            return Err(IdentityError::unavailable(format!(
                "asset `{}` has a locator but no observed content identity",
                asset.locator
            )));
        }
        ValueData::Transform(_) | ValueData::Lineage(_) => {
            return Err(IdentityError::unavailable(
                "callable transform and lineage values do not have content identity",
            ));
        }
    }
    Ok(())
}

/// Content identity for an untyped materialized byte object, such as observed
/// source asset bytes. Typed values use [`content_identity`] instead.
pub fn byte_content_identity(bytes: &[u8]) -> ContentIdentity {
    let mut hasher = CanonicalHasher::new(b"tima.content.bytes");
    hasher.u32(SEMANTIC_ID_VERSION);
    hasher.bytes(bytes);
    ContentIdentity(Digest::from_hasher(hasher))
}

/// Identity of one observed source reference. Locator and observed bytes are
/// kept distinct: changing either changes the source identity, while content
/// deduplication continues to use [`ContentIdentity`] alone.
pub fn source_identity(locator: &str, observed_content: ContentIdentity) -> SourceIdentity {
    let mut hasher = CanonicalHasher::new(b"tima.source");
    hasher.u32(SEMANTIC_ID_VERSION);
    hasher.bytes(locator.as_bytes());
    hasher.raw(observed_content.as_bytes());
    SourceIdentity(Digest::from_hasher(hasher))
}

pub fn dependency_identity(
    capability: &str,
    key: &[u8],
    observed_value: ContentIdentity,
) -> DependencyIdentity {
    let mut hasher = CanonicalHasher::new(b"tima.dependency");
    hasher.u32(SEMANTIC_ID_VERSION);
    hasher.bytes(capability.as_bytes());
    hasher.bytes(key);
    hasher.raw(observed_value.as_bytes());
    DependencyIdentity(Digest::from_hasher(hasher))
}

pub fn recipe_identity(
    transform: TransformIdentity,
    arguments: &[SemanticValueIdentity],
    dependencies: &[DependencyIdentity],
) -> RecipeIdentity {
    let mut hasher = CanonicalHasher::new(b"tima.recipe");
    hasher.u32(SEMANTIC_ID_VERSION);
    hasher.raw(transform.as_bytes());
    hasher.u64(arguments.len() as u64);
    for argument in arguments {
        match argument {
            SemanticValueIdentity::Content(identity) => {
                hasher.u8(0);
                hasher.raw(identity.as_bytes());
            }
            SemanticValueIdentity::Source(identity) => {
                hasher.u8(1);
                hasher.raw(identity.as_bytes());
            }
            SemanticValueIdentity::Recipe(identity) => {
                hasher.u8(2);
                hasher.raw(identity.as_bytes());
            }
        }
    }
    let mut dependencies = dependencies.to_vec();
    dependencies.sort_unstable();
    dependencies.dedup();
    hasher.u64(dependencies.len() as u64);
    for dependency in dependencies {
        hasher.raw(dependency.as_bytes());
    }
    RecipeIdentity(Digest::from_hasher(hasher))
}

#[derive(Clone, Debug)]
pub struct ArtifactConfiguration<'a> {
    pub backend: &'a str,
    pub backend_version: &'a str,
    pub compiler_version: &'a str,
    pub target: &'a str,
    pub cpu_features: &'a [&'a str],
    pub optimization: &'a str,
    pub abi_version: u32,
}

pub fn artifact_identity(
    transform: TransformIdentity,
    configuration: &ArtifactConfiguration<'_>,
) -> ArtifactIdentity {
    let mut hasher = CanonicalHasher::new(b"tima.artifact");
    hasher.u32(SEMANTIC_ID_VERSION);
    hasher.raw(transform.as_bytes());
    hasher.bytes(configuration.backend.as_bytes());
    hasher.bytes(configuration.backend_version.as_bytes());
    hasher.bytes(configuration.compiler_version.as_bytes());
    hasher.bytes(configuration.target.as_bytes());
    let mut features = configuration.cpu_features.to_vec();
    features.sort_unstable();
    features.dedup();
    hasher.u64(features.len() as u64);
    for feature in features {
        hasher.bytes(feature.as_bytes());
    }
    hasher.bytes(configuration.optimization.as_bytes());
    hasher.u32(configuration.abi_version);
    ArtifactIdentity(Digest::from_hasher(hasher))
}

/// Identifies one precompiled registered-Wasm module independently from the
/// semantic transform it implements. Module replacement therefore changes
/// Artifact ID even when the registry retains a semantically equivalent
/// Transform ID.
pub fn registered_wasm_artifact_identity(
    transform: TransformIdentity,
    module_bytes: &[u8],
    abi_version: u32,
) -> ArtifactIdentity {
    let module_content = byte_content_identity(module_bytes).to_string();
    artifact_identity(
        transform,
        &ArtifactConfiguration {
            backend: "registered-wasm",
            backend_version: "1",
            compiler_version: &module_content,
            target: "wasm32-unknown-unknown",
            cpu_features: &[],
            optimization: "precompiled",
            abi_version,
        },
    )
}

/// Identity of one backend compilation unit containing an ordered set of
/// transform artifacts. Transform order is included because generated adapter
/// symbols are indexed by module order.
pub fn artifact_bundle_identity(artifacts: &[ArtifactIdentity]) -> ArtifactBundleIdentity {
    let mut hasher = CanonicalHasher::new(b"tima.artifact.bundle");
    hasher.u32(SEMANTIC_ID_VERSION);
    hasher.u64(artifacts.len() as u64);
    for artifact in artifacts {
        hasher.raw(artifact.as_bytes());
    }
    ArtifactBundleIdentity(Digest::from_hasher(hasher))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityError {
    message: String,
}

impl IdentityError {
    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for IdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for IdentityError {}

fn encode_type(hasher: &mut CanonicalHasher, ty: Type) {
    hasher.u8(match ty {
        Type::Bool => 0,
        Type::I64 => 1,
        Type::F32 => 2,
        Type::Image => 3,
        Type::ImageView => 4,
        Type::U8 => 5,
        Type::String => 6,
        Type::StringView => 7,
        Type::Bytes => 8,
        Type::BytesView => 9,
    });
}

fn encode_constant(hasher: &mut CanonicalHasher, constant: &Constant) {
    match constant {
        Constant::Bool(value) => {
            hasher.u8(0);
            hasher.u8(u8::from(*value));
        }
        Constant::I64(value) => {
            hasher.u8(1);
            hasher.i64(*value);
        }
        Constant::F32(value) => {
            hasher.u8(2);
            hasher.u32(value.to_bits());
        }
        Constant::String(value) => {
            hasher.u8(3);
            hasher.bytes(value.as_bytes());
        }
    }
}

fn encode_binary_op(hasher: &mut CanonicalHasher, op: BinaryOp) {
    hasher.u8(match op {
        BinaryOp::Add => 0,
        BinaryOp::Subtract => 1,
        BinaryOp::Multiply => 2,
        BinaryOp::Divide => 3,
        BinaryOp::Equal => 4,
        BinaryOp::NotEqual => 5,
        BinaryOp::Less => 6,
        BinaryOp::LessEqual => 7,
        BinaryOp::Greater => 8,
        BinaryOp::GreaterEqual => 9,
    });
}

struct CanonicalHasher {
    hash: Sha256,
}

impl CanonicalHasher {
    fn new(domain: &[u8]) -> Self {
        let mut value = Self {
            hash: Sha256::new(),
        };
        value.bytes(domain);
        value
    }

    fn finish(self) -> [u8; 32] {
        self.hash.finish()
    }

    fn raw(&mut self, value: &[u8]) {
        self.hash.update(value);
    }

    fn bytes(&mut self, value: &[u8]) {
        self.u64(value.len() as u64);
        self.raw(value);
    }

    fn u8(&mut self, value: u8) {
        self.raw(&[value]);
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

struct Sha256 {
    state: [u32; 8],
    block: [u8; 64],
    block_len: usize,
    byte_len: u64,
}

impl Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            block: [0; 64],
            block_len: 0,
            byte_len: 0,
        }
    }

    fn update(&mut self, mut input: &[u8]) {
        self.byte_len = self.byte_len.wrapping_add(input.len() as u64);
        if self.block_len != 0 {
            let take = (64 - self.block_len).min(input.len());
            self.block[self.block_len..self.block_len + take].copy_from_slice(&input[..take]);
            self.block_len += take;
            input = &input[take..];
            if self.block_len < 64 {
                return;
            }
            compress(&mut self.state, &self.block);
            self.block_len = 0;
        }
        while input.len() >= 64 {
            let (block, rest) = input.split_at(64);
            let block: &[u8; 64] = block.try_into().expect("64-byte SHA-256 block");
            compress(&mut self.state, block);
            input = rest;
        }
        self.block[..input.len()].copy_from_slice(input);
        self.block_len = input.len();
    }

    fn finish(mut self) -> [u8; 32] {
        let bit_len = self.byte_len.wrapping_mul(8);
        self.block[self.block_len] = 0x80;
        self.block_len += 1;
        if self.block_len > 56 {
            self.block[self.block_len..].fill(0);
            compress(&mut self.state, &self.block);
            self.block = [0; 64];
        } else {
            self.block[self.block_len..56].fill(0);
        }
        self.block[56..].copy_from_slice(&bit_len.to_be_bytes());
        compress(&mut self.state, &self.block);
        let mut output = [0; 32];
        for (index, word) in self.state.into_iter().enumerate() {
            output[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        output
    }
}

fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut words = [0u32; 64];
    for index in 0..16 {
        words[index] = u32::from_be_bytes(block[index * 4..index * 4 + 4].try_into().unwrap());
    }
    for index in 16..64 {
        let s0 = words[index - 15].rotate_right(7)
            ^ words[index - 15].rotate_right(18)
            ^ (words[index - 15] >> 3);
        let s1 = words[index - 2].rotate_right(17)
            ^ words[index - 2].rotate_right(19)
            ^ (words[index - 2] >> 10);
        words[index] = words[index - 16]
            .wrapping_add(s0)
            .wrapping_add(words[index - 7])
            .wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for index in 0..64 {
        let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let choose = (e & f) ^ ((!e) & g);
        let temporary1 = h
            .wrapping_add(sum1)
            .wrapping_add(choose)
            .wrapping_add(K[index])
            .wrapping_add(words[index]);
        let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let majority = (a & b) ^ (a & c) ^ (b & c);
        let temporary2 = sum0.wrapping_add(majority);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(temporary1);
        d = c;
        c = b;
        b = a;
        a = temporary1.wrapping_add(temporary2);
    }
    for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *slot = slot.wrapping_add(value);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::*;
    use crate::runtime::{ImageValue, OuterValue, ValueData};

    #[test]
    fn sha256_matches_published_vectors() {
        let mut empty = Sha256::new();
        empty.update(b"");
        assert_eq!(
            hex(empty.finish()),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let mut abc = Sha256::new();
        abc.update(b"a");
        abc.update(b"b");
        abc.update(b"c");
        assert_eq!(
            hex(abc.finish()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn identity_text_round_trips_in_canonical_lowercase_hex() {
        let identity = byte_content_identity(b"round trip");
        let text = identity.to_string();

        assert_eq!(text.parse::<ContentIdentity>().unwrap(), identity);
        assert!(text.to_uppercase().parse::<ContentIdentity>().is_err());
        assert!(text[..63].parse::<ContentIdentity>().is_err());
        assert!(
            format!("{}g", &text[..63])
                .parse::<ContentIdentity>()
                .is_err()
        );
    }

    #[test]
    fn registered_transform_identity_is_stable_and_versioned() {
        assert_eq!(
            registered_transform_identity("ppm.decode", 1),
            registered_transform_identity("ppm.decode", 1)
        );
        assert_ne!(
            registered_transform_identity("ppm.decode", 1),
            registered_transform_identity("ppm.decode", 2)
        );
        assert_ne!(
            registered_transform_identity("ppm.decode", 1),
            registered_transform_identity("ppm.encode", 1)
        );
    }

    #[test]
    fn registered_wasm_semantic_identity_includes_the_manifest_contract() {
        let base = registered_wasm_transform_identity(
            "fixture.encode",
            1,
            3,
            &[("image", 2), ("quality", 3)],
            1,
        );
        assert_eq!(
            base,
            registered_wasm_transform_identity(
                "fixture.encode",
                1,
                3,
                &[("image", 2), ("quality", 3)],
                1,
            )
        );
        assert_ne!(
            base,
            registered_wasm_transform_identity(
                "fixture.encode",
                1,
                3,
                &[("input", 2), ("quality", 3)],
                1,
            )
        );
        assert_ne!(
            base,
            registered_wasm_transform_identity("fixture.encode", 1, 3, &[("image", 2)], 1)
        );
        assert_ne!(
            base,
            registered_wasm_transform_identity(
                "fixture.encode",
                2,
                3,
                &[("image", 2), ("quality", 3)],
                1,
            )
        );
    }

    #[test]
    fn transform_identity_ignores_formatting_names_and_definition_order() {
        let first = crate::compile(
            "first.tima",
            "transform helper(value: f32) -> f32 { return value * 2.0 }\n\
             transform apply(input: f32) -> f32 { return helper(input) }\n",
        )
        .unwrap();
        let second = crate::compile(
            "second.tima",
            "// reordered and renamed\n\
             transform renamed(y: f32)->f32 {\n return doubled(y)\n }\n\
             transform doubled(x: f32) -> f32 { return x * 2.0 }\n",
        )
        .unwrap();
        assert_eq!(
            first.identities.find(&first.transforms, "helper"),
            second.identities.find(&second.transforms, "doubled")
        );
        assert_eq!(
            first.identities.find(&first.transforms, "apply"),
            second.identities.find(&second.transforms, "renamed")
        );
    }

    #[test]
    fn transform_identity_ignores_inner_local_names() {
        let first = crate::compile(
            "first.tima",
            "transform adjusted(x: f32) -> f32 {\n doubled = x * 2.0\n return doubled\n}\n",
        )
        .unwrap();
        let second = crate::compile(
            "second.tima",
            "transform renamed(value: f32) -> f32 {\n temporary = value * 2.0\n return temporary\n}\n",
        )
        .unwrap();
        assert_eq!(
            first.identities.get(TransformId(0)),
            second.identities.get(TransformId(0))
        );
    }

    #[test]
    fn transform_identity_canonicalizes_branch_locals_but_not_branch_meaning() {
        let first = crate::compile(
            "first.tima",
            "transform choose(flag: bool, a: f32, b: f32) -> f32 {\n\
                 if flag { selected = a; return selected } else {}\n\
                 return b\n\
             }\n",
        )
        .unwrap();
        let renamed = crate::compile(
            "renamed.tima",
            "transform renamed(test: bool, left: f32, right: f32) -> f32 {\n\
                 if test { temporary = left; return temporary } else {}\n\
                 return right\n\
             }\n",
        )
        .unwrap();
        let swapped = crate::compile(
            "swapped.tima",
            "transform choose(flag: bool, a: f32, b: f32) -> f32 {\n\
                 if flag { return b } else {}\n\
                 return a\n\
             }\n",
        )
        .unwrap();
        assert_eq!(
            first.identities.get(TransformId(0)),
            renamed.identities.get(TransformId(0))
        );
        assert_ne!(
            first.identities.get(TransformId(0)),
            swapped.identities.get(TransformId(0))
        );
    }

    #[test]
    fn semantic_change_changes_transform_identity() {
        let multiply = crate::compile(
            "one.tima",
            "transform value(x: f32) -> f32 { return x * 2.0 }\n",
        )
        .unwrap();
        let add = crate::compile(
            "two.tima",
            "transform value(x: f32) -> f32 { return x + 2.0 }\n",
        )
        .unwrap();
        assert_ne!(
            multiply.identities.get(TransformId(0)),
            add.identities.get(TransformId(0))
        );
    }

    #[test]
    fn comparison_operator_is_part_of_transform_identity() {
        let less = crate::compile(
            "less.tima",
            "transform compare(a: f32, b: f32) -> bool { return a < b }\n",
        )
        .unwrap();
        let less_equal = crate::compile(
            "less_equal.tima",
            "transform compare(a: f32, b: f32) -> bool { return a <= b }\n",
        )
        .unwrap();
        assert_ne!(
            less.identities.get(TransformId(0)),
            less_equal.identities.get(TransformId(0))
        );
    }

    #[test]
    fn owned_image_operation_is_part_of_transform_identity() {
        let keep = crate::compile(
            "keep.tima",
            "transform image(img: Image) -> Image { return img }\n",
        )
        .unwrap();
        let clear = crate::compile(
            "clear.tima",
            "transform image(img: Image) -> Image { return image_zero(img) }\n",
        )
        .unwrap();
        assert_ne!(
            keep.identities.get(TransformId(0)),
            clear.identities.get(TransformId(0))
        );

        let keep_with_value = crate::compile(
            "keep_value.tima",
            "transform image(img: Image, value: u8) -> Image { return img }\n",
        )
        .unwrap();
        let fill = crate::compile(
            "fill.tima",
            "transform image(img: Image, value: u8) -> Image { return image_fill(img, value) }\n",
        )
        .unwrap();
        assert_ne!(
            keep_with_value.identities.get(TransformId(0)),
            fill.identities.get(TransformId(0))
        );
    }

    #[test]
    fn normalized_image_byte_loop_has_fill_semantic_identity() {
        let builtin = crate::compile(
            "builtin.tima",
            "transform fill(img: Image, value: u8) -> Image { return image_fill(img, value) }\n",
        )
        .unwrap();
        let loop_surface = crate::compile(
            "loop.tima",
            "transform fill(img: Image, value: u8) -> Image {\n\
                 for byte in img.bytes { byte = value }\n\
                 return img\
             }\n",
        )
        .unwrap();
        assert_eq!(
            builtin.identities.get(TransformId(0)),
            loop_surface.identities.get(TransformId(0))
        );
    }

    #[test]
    fn image_byte_map_identity_ignores_loop_binding_name() {
        let first = crate::compile(
            "first.tima",
            "transform keep(img: Image) -> Image {\n\
                 for byte in img.bytes { byte = byte }\n\
                 return img\n\
             }\n",
        )
        .unwrap();
        let second = crate::compile(
            "second.tima",
            "transform keep(img: Image) -> Image {\n\
                 for element in img.bytes { element = element }\n\
                 return img\n\
             }\n",
        )
        .unwrap();
        assert_eq!(
            first.identities.get(TransformId(0)),
            second.identities.get(TransformId(0))
        );
    }

    #[test]
    fn rgba8_pixel_scale_identity_ignores_loop_binding_name() {
        let first = crate::compile(
            "first.tima",
            "transform darken(img: Image, factor: f32) -> Image {\n\
                 for p in img.pixels { p.r *= factor }\n\
                 return img\n\
             }\n",
        )
        .unwrap();
        let second = crate::compile(
            "second.tima",
            "transform darken(img: Image, factor: f32) -> Image {\n\
                 for pixel in img.pixels { pixel.r *= factor }\n\
                 return img\n\
             }\n",
        )
        .unwrap();
        assert_eq!(
            first.identities.get(TransformId(0)),
            second.identities.get(TransformId(0))
        );
    }

    #[test]
    fn capability_operation_and_key_are_part_of_transform_identity() {
        let first = crate::compile(
            "one.tima",
            "transform configured() -> i64 uses env.read { return environment_i64(\"MODE\") }\n",
        )
        .unwrap();
        let second = crate::compile(
            "two.tima",
            "transform configured() -> i64 uses env.read { return environment_i64(\"QUALITY\") }\n",
        )
        .unwrap();
        assert_ne!(
            first.identities.get(TransformId(0)),
            second.identities.get(TransformId(0))
        );
    }

    #[test]
    fn declared_capabilities_are_part_of_transform_identity() {
        let plain =
            crate::compile("plain.tima", "transform configured() -> i64 { return 1 }\n").unwrap();
        let declared = crate::compile(
            "declared.tima",
            "transform configured() -> i64 uses env.read { return 1 }\n",
        )
        .unwrap();
        assert_ne!(
            plain.identities.get(TransformId(0)),
            declared.identities.get(TransformId(0))
        );
    }

    #[test]
    fn content_identity_uses_materialized_value_not_storage_identity() {
        let first = OuterValue::image(ImageValue::new(2, 2, 2, vec![1, 2, 3, 4]).unwrap());
        let second = OuterValue::image(ImageValue::new(2, 2, 2, vec![1, 2, 3, 4]).unwrap());
        assert_eq!(
            content_identity(&first).unwrap(),
            content_identity(&second).unwrap()
        );

        let record = OuterValue::plain(ValueData::Record(Arc::new(BTreeMap::from([
            ("image".to_owned(), first),
            (
                "quality".to_owned(),
                OuterValue::plain(ValueData::Integer(85)),
            ),
        ]))));
        assert_ne!(
            content_identity(&record).unwrap(),
            content_identity(&second).unwrap()
        );

        let opaque = OuterValue::image(ImageValue::new(1, 1, 4, vec![1, 2, 3, 4]).unwrap());
        let rgba = OuterValue::image(ImageValue::new_rgba8(1, 1, 4, vec![1, 2, 3, 4]).unwrap());
        assert_ne!(
            content_identity(&opaque).unwrap(),
            content_identity(&rgba).unwrap()
        );
    }

    #[test]
    fn equivalent_fractions_have_one_content_identity() {
        let first = OuterValue::plain(ValueData::Fraction(
            crate::fraction::Fraction::new(1, 2).unwrap(),
        ));
        let second = OuterValue::plain(ValueData::Fraction(
            crate::fraction::Fraction::new(2, 4).unwrap(),
        ));
        assert_eq!(
            content_identity(&first).unwrap(),
            content_identity(&second).unwrap()
        );
        assert_ne!(
            content_identity(&first).unwrap(),
            content_identity(&OuterValue::plain(ValueData::Float(0.5))).unwrap()
        );
    }

    #[test]
    fn recipe_and_artifact_domains_remain_distinct() {
        let compiled =
            crate::compile("test.tima", "transform keep(x: f32) -> f32 { return x }\n").unwrap();
        let transform = compiled.identities.get(TransformId(0));
        let argument = content_identity(&OuterValue::plain(ValueData::Float(1.0))).unwrap();
        let recipe = recipe_identity(transform, &[argument.into()], &[]);
        let artifact = artifact_identity(
            transform,
            &ArtifactConfiguration {
                backend: "c",
                backend_version: "2",
                compiler_version: "clang 23",
                target: "x86_64-pc-windows-msvc",
                cpu_features: &[],
                optimization: "O2",
                abi_version: 2,
            },
        );
        assert_ne!(recipe.as_bytes(), artifact.as_bytes());
    }

    #[test]
    fn artifact_bundle_identity_includes_transform_order() {
        let compiled = crate::compile(
            "test.tima",
            "transform first(x: f32) -> f32 { return x }\n\
             transform second(x: f32) -> f32 { return x * 2.0 }\n",
        )
        .unwrap();
        let configuration = ArtifactConfiguration {
            backend: "c",
            backend_version: "2",
            compiler_version: "clang 23",
            target: "x86_64-pc-windows-msvc",
            cpu_features: &[],
            optimization: "O2",
            abi_version: 2,
        };
        let first = artifact_identity(compiled.identities.get(TransformId(0)), &configuration);
        let second = artifact_identity(compiled.identities.get(TransformId(1)), &configuration);
        assert_ne!(
            artifact_bundle_identity(&[first, second]),
            artifact_bundle_identity(&[second, first])
        );
    }

    #[test]
    fn dependency_observation_order_and_duplicates_do_not_change_recipe() {
        let compiled =
            crate::compile("test.tima", "transform keep(x: f32) -> f32 { return x }\n").unwrap();
        let transform = compiled.identities.get(TransformId(0));
        let argument = content_identity(&OuterValue::plain(ValueData::Float(1.0))).unwrap();
        let first = dependency_identity("filesystem", b"a.ttf", byte_content_identity(b"a"));
        let second = dependency_identity("environment", b"MODE", byte_content_identity(b"dark"));
        assert_eq!(
            recipe_identity(transform, &[argument.into()], &[first, second]),
            recipe_identity(transform, &[argument.into()], &[second, first, first])
        );
    }

    #[test]
    fn source_identity_keeps_locator_separate_from_content() {
        let content = byte_content_identity(b"same bytes");
        assert_ne!(
            source_identity("a.png", content),
            source_identity("b.png", content)
        );
        assert_eq!(content, byte_content_identity(b"same bytes"));
    }

    #[test]
    fn recursive_definitions_receive_a_clear_identity_diagnostic() {
        let diagnostics = crate::compile(
            "test.tima",
            "transform forever(x: f32) -> f32 { return forever(x) }\n",
        )
        .unwrap_err();
        assert!(
            diagnostics[0]
                .message
                .contains("recursive transform definitions")
        );
    }

    fn hex(bytes: [u8; 32]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}
