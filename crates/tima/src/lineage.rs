use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::ast::Item;
use crate::fraction::Fraction;
use crate::identity::{
    ContentIdentity, DependencyIdentity, IdentityError, RecipeIdentity, SemanticValueIdentity,
    SourceIdentity, TransformIdentity, content_identity, dependency_identity, recipe_identity,
    source_identity,
};
use crate::runtime::{OuterValue, ValueData};
use crate::source::SourceFile;

/// One immutable node in a semantic derivation DAG.
///
/// Nodes deliberately contain no execution timestamps, compiler artifacts, or
/// profiling state. Cloning a lineage only clones an `Arc`, so outer values can
/// share complete derivations cheaply.
#[derive(Clone, Debug, PartialEq)]
pub struct Lineage(Arc<LineageNode>);

#[derive(Clone, Debug, PartialEq)]
pub enum LineageNode {
    Source(SourceLineage),
    Invocation(InvocationLineage),
    ExternalObservation(ExternalObservationLineage),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceLineage {
    pub locator: Arc<str>,
    pub observed_content: Option<ContentIdentity>,
    pub source_id: Option<SourceIdentity>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InvocationLineage {
    pub transform_name: Arc<str>,
    pub transform_id: TransformIdentity,
    pub recipe_id: RecipeIdentity,
    pub arguments: Arc<[LineageArgument]>,
    pub observations: Arc<[Lineage]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalObservationLineage {
    pub capability: Arc<str>,
    pub key: Arc<[u8]>,
    pub observed_content: ContentIdentity,
    pub dependency_id: DependencyIdentity,
}

/// A replay-oriented argument snapshot. Large values are represented by their
/// content identity rather than retained storage so recording an invocation
/// never changes owned-value detach behavior at the native boundary.
#[derive(Clone, Debug, PartialEq)]
pub struct LineageArgument {
    pub name: Arc<str>,
    pub semantic_identity: SemanticValueIdentity,
    pub value: RecordedValue,
    pub lineage: Option<Lineage>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RecordedValue {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f32),
    Fraction(Fraction),
    String(Arc<String>),
    Materialized {
        kind: &'static str,
        content_id: ContentIdentity,
    },
    Source {
        locator: Arc<str>,
        source_id: SourceIdentity,
    },
}

impl Lineage {
    pub fn source(locator: impl Into<Arc<str>>, observed: Option<ContentIdentity>) -> Self {
        let locator = locator.into();
        let source_id = observed.map(|content| source_identity(&locator, content));
        Self(Arc::new(LineageNode::Source(SourceLineage {
            locator,
            observed_content: observed,
            source_id,
        })))
    }

    pub fn observed_source(
        locator: impl Into<Arc<str>>,
        observed_content: ContentIdentity,
    ) -> Self {
        Self::source(locator, Some(observed_content))
    }

    pub fn external_observation(
        capability: impl Into<Arc<str>>,
        key: impl Into<Arc<[u8]>>,
        observed_content: ContentIdentity,
    ) -> Self {
        let capability = capability.into();
        let key = key.into();
        let dependency_id = dependency_identity(&capability, &key, observed_content);
        Self(Arc::new(LineageNode::ExternalObservation(
            ExternalObservationLineage {
                capability,
                key,
                observed_content,
                dependency_id,
            },
        )))
    }

    pub fn invocation(
        transform_name: impl Into<Arc<str>>,
        transform_id: TransformIdentity,
        arguments: Vec<LineageArgument>,
        mut observations: Vec<Lineage>,
    ) -> Result<Self, LineageError> {
        for observation in &observations {
            if !matches!(observation.node(), LineageNode::ExternalObservation(_)) {
                return Err(LineageError::new(
                    "invocation observations must be external-observation lineage nodes",
                ));
            }
        }
        observations.sort_by_key(|observation| match observation.node() {
            LineageNode::ExternalObservation(value) => value.dependency_id,
            _ => unreachable!("observation kinds were validated above"),
        });
        observations.dedup_by_key(|observation| match observation.node() {
            LineageNode::ExternalObservation(value) => value.dependency_id,
            _ => unreachable!("observation kinds were validated above"),
        });
        let dependencies = observations
            .iter()
            .map(|observation| match observation.node() {
                LineageNode::ExternalObservation(value) => value.dependency_id,
                _ => unreachable!("observation kinds were validated above"),
            })
            .collect::<Vec<_>>();
        let semantic_arguments = arguments
            .iter()
            .map(|argument| argument.semantic_identity)
            .collect::<Vec<_>>();
        let recipe_id = recipe_identity(transform_id, &semantic_arguments, &dependencies);
        Ok(Self(Arc::new(LineageNode::Invocation(InvocationLineage {
            transform_name: transform_name.into(),
            transform_id,
            recipe_id,
            arguments: Arc::from(arguments),
            observations: Arc::from(observations),
        }))))
    }

    pub fn node(&self) -> &LineageNode {
        &self.0
    }

    pub fn semantic_identity(&self) -> Option<SemanticValueIdentity> {
        match self.node() {
            LineageNode::Source(source) => source.source_id.map(Into::into),
            LineageNode::Invocation(invocation) => Some(invocation.recipe_id.into()),
            LineageNode::ExternalObservation(_) => None,
        }
    }

    pub fn recipe_id(&self) -> Option<RecipeIdentity> {
        match self.node() {
            LineageNode::Invocation(invocation) => Some(invocation.recipe_id),
            LineageNode::Source(_) | LineageNode::ExternalObservation(_) => None,
        }
    }

    /// Produces a deterministic, human-readable derivation listing. Ancestors
    /// appear before consumers and shared ancestors are printed once.
    pub fn render(&self) -> String {
        let mut renderer = LineageRenderer::default();
        let root = renderer.visit(self);
        renderer.lines.push(format!("root = #{root}"));
        renderer.lines.join("\n")
    }

    /// Reconstructs this invocation lineage as one outer-language expression.
    ///
    /// The recorded derivation is pinned by full Transform and Recipe IDs. If
    /// `starting_input` is supplied, it replaces the deepest value on the
    /// first-argument chain and the old Recipe assertion is omitted because
    /// the substitution intentionally describes a new derivation.
    pub fn outer_expression(&self, starting_input: Option<&str>) -> Result<String, LineageError> {
        let starting_input = starting_input.map(validate_outer_expression).transpose()?;
        let LineageNode::Invocation(invocation) = self.node() else {
            return Err(LineageError::new(
                "only transform invocation lineage can be rendered as a recipe expression",
            ));
        };
        let mut renderer = OuterExpressionRenderer {
            starting_input: starting_input.as_deref(),
            substituted: false,
        };
        let rendered = renderer.invocation(invocation, true)?;
        if starting_input.is_some() && !renderer.substituted {
            return Err(LineageError::new(
                "the recorded invocation has no primary input to replace",
            ));
        }
        let expression = if starting_input.is_some() {
            rendered.text
        } else {
            format!("({})#{}", rendered.text, invocation.recipe_id)
        };
        validate_outer_expression(&expression)?;
        Ok(expression)
    }

    fn render_key(&self) -> LineageKey {
        match self.node() {
            LineageNode::Source(source) => source.source_id.map_or_else(
                || LineageKey::UnobservedSource(source.locator.clone()),
                LineageKey::Source,
            ),
            LineageNode::Invocation(invocation) => LineageKey::Invocation(invocation.recipe_id),
            LineageNode::ExternalObservation(observation) => {
                LineageKey::ExternalObservation(observation.dependency_id)
            }
        }
    }
}

fn validate_outer_expression(expression: &str) -> Result<String, LineageError> {
    if expression
        .chars()
        .any(|character| matches!(character, '\r' | '\n'))
    {
        return Err(LineageError::new(
            "a recipe starting input must be a one-line Tima expression",
        ));
    }
    let source = SourceFile::new("<recipe-expression>", expression);
    let program = crate::parser::parse(&source).map_err(|diagnostics| {
        let message = diagnostics
            .first()
            .map_or("invalid Tima syntax", |diagnostic| {
                diagnostic.message.as_str()
            });
        LineageError::new(format!("invalid Tima outer expression: {message}"))
    })?;
    if program.items.len() != 1 || !matches!(program.items.first(), Some(Item::Expression(_))) {
        return Err(LineageError::new(
            "recipe input must contain exactly one Tima outer expression",
        ));
    }
    Ok(expression.to_owned())
}

struct OuterExpressionRenderer<'a> {
    starting_input: Option<&'a str>,
    substituted: bool,
}

struct RenderedExpression {
    text: String,
    is_pipeline: bool,
}

impl OuterExpressionRenderer<'_> {
    fn invocation(
        &mut self,
        invocation: &InvocationLineage,
        primary: bool,
    ) -> Result<RenderedExpression, LineageError> {
        if primary && invocation.arguments.is_empty() && self.starting_input.is_some() {
            return self.substitute();
        }
        let callable = format!("{}#{}", invocation.transform_name, invocation.transform_id);
        let Some((first, rest)) = invocation.arguments.split_first() else {
            return Ok(self.pin_non_primary(
                invocation,
                primary,
                RenderedExpression {
                    text: format!("{callable}()"),
                    is_pipeline: false,
                },
            ));
        };
        let input = self.argument(first, primary)?;
        let stage = if rest.is_empty() {
            callable
        } else {
            let arguments = rest
                .iter()
                .map(|argument| {
                    let value = self.argument(argument, false)?;
                    Ok(if value.is_pipeline {
                        format!("({})", value.text)
                    } else {
                        value.text
                    })
                })
                .collect::<Result<Vec<_>, LineageError>>()?
                .join(", ");
            format!("{callable}({arguments})")
        };
        Ok(self.pin_non_primary(
            invocation,
            primary,
            RenderedExpression {
                text: format!("{} | {stage}", input.text),
                is_pipeline: true,
            },
        ))
    }

    fn argument(
        &mut self,
        argument: &LineageArgument,
        primary: bool,
    ) -> Result<RenderedExpression, LineageError> {
        if primary && self.starting_input.is_some() {
            if let Some(lineage) = &argument.lineage
                && let LineageNode::Invocation(invocation) = lineage.node()
            {
                return self.invocation(invocation, true);
            }
            return self.substitute();
        }
        if let Some(lineage) = &argument.lineage {
            return match lineage.node() {
                LineageNode::Invocation(invocation) => self.invocation(invocation, primary),
                LineageNode::Source(source) => render_source_expression(source),
                LineageNode::ExternalObservation(_) => Err(LineageError::new(
                    "external observations cannot be used as transform arguments",
                )),
            };
        }
        render_recorded_expression(&argument.value)
    }

    fn substitute(&mut self) -> Result<RenderedExpression, LineageError> {
        if self.substituted {
            return Err(LineageError::new(
                "one starting input cannot replace more than one lineage root",
            ));
        }
        let input = self
            .starting_input
            .ok_or_else(|| LineageError::new("no starting input was supplied"))?;
        self.substituted = true;
        Ok(RenderedExpression {
            text: format!("({input})"),
            is_pipeline: false,
        })
    }

    fn pin_non_primary(
        &self,
        invocation: &InvocationLineage,
        primary: bool,
        rendered: RenderedExpression,
    ) -> RenderedExpression {
        if primary {
            rendered
        } else {
            RenderedExpression {
                text: format!("({})#{}", rendered.text, invocation.recipe_id),
                is_pipeline: false,
            }
        }
    }
}

fn render_source_expression(source: &SourceLineage) -> Result<RenderedExpression, LineageError> {
    let source_id = source.source_id.ok_or_else(|| {
        LineageError::new(format!(
            "source {:?} was never observed and has no Source ID",
            source.locator
        ))
    })?;
    Ok(RenderedExpression {
        text: format!(
            "read(asset({}))#{source_id}",
            string_literal(&source.locator)?
        ),
        is_pipeline: false,
    })
}

fn render_recorded_expression(value: &RecordedValue) -> Result<RenderedExpression, LineageError> {
    let text = match value {
        RecordedValue::Null => "null".to_owned(),
        RecordedValue::Bool(value) => value.to_string(),
        RecordedValue::Integer(value) if *value >= 0 => value.to_string(),
        RecordedValue::Integer(value) => {
            return Err(LineageError::new(format!(
                "recorded negative integer {value} is not expressible without unary negation"
            )));
        }
        RecordedValue::Float(value) => float_literal(*value),
        RecordedValue::Fraction(value) => fraction_literal(*value),
        RecordedValue::String(value) => string_literal(value)?,
        RecordedValue::Source { locator, source_id } => {
            format!("read(asset({}))#{source_id}", string_literal(locator)?)
        }
        RecordedValue::Materialized { kind, content_id } => {
            return Err(LineageError::new(format!(
                "recorded {kind} argument {content_id} has no reconstructable lineage; supply --input when it is the primary input"
            )));
        }
    };
    Ok(RenderedExpression {
        text,
        is_pipeline: false,
    })
}

fn signed_integer_expression(value: i64) -> String {
    if value >= 0 {
        return value.to_string();
    }
    if value == i64::MIN {
        return format!("0 - {} - 1", i64::MAX);
    }
    format!("0 - {}", value.unsigned_abs())
}

fn fraction_literal(value: Fraction) -> String {
    if value.numerator() >= 0 {
        return format!("{}/{}", value.numerator(), value.denominator());
    }
    format!(
        "fraction({}, {})",
        signed_integer_expression(value.numerator()),
        value.denominator()
    )
}

fn string_literal(value: &str) -> Result<String, LineageError> {
    let mut literal = String::with_capacity(value.len() + 2);
    literal.push('"');
    for character in value.chars() {
        match character {
            '\n' => literal.push_str("\\n"),
            '\r' => literal.push_str("\\r"),
            '\t' => literal.push_str("\\t"),
            '"' => literal.push_str("\\\""),
            '\\' => literal.push_str("\\\\"),
            character if character.is_control() => {
                return Err(LineageError::new(format!(
                    "recorded string contains unsupported control character U+{:04X}",
                    character as u32
                )));
            }
            character => literal.push(character),
        }
    }
    literal.push('"');
    Ok(literal)
}

fn float_literal(value: f32) -> String {
    if value.is_finite() && !value.is_sign_negative() {
        for precision in 1..=149 {
            let candidate = format!("{value:.precision$}");
            if candidate
                .parse::<f64>()
                .is_ok_and(|parsed| (parsed as f32).to_bits() == value.to_bits())
            {
                return candidate;
            }
        }
    }
    format!("f32.from_bits({})", value.to_bits())
}

/// Selects the identity that represents an outer value in recipes and
/// postfix identity assertions. Derivation identity deliberately takes
/// precedence over equal materialized content.
pub fn semantic_value_identity(value: &OuterValue) -> Result<SemanticValueIdentity, IdentityError> {
    match value.lineage.as_ref().and_then(Lineage::semantic_identity) {
        Some(identity) => Ok(identity),
        None => Ok(content_identity(value)?.into()),
    }
}

impl LineageArgument {
    pub fn record(name: impl Into<Arc<str>>, value: &OuterValue) -> Result<Self, IdentityError> {
        let semantic_identity = semantic_value_identity(value)?;
        let recorded = match &value.data {
            ValueData::Null => RecordedValue::Null,
            ValueData::Bool(value) => RecordedValue::Bool(*value),
            ValueData::Integer(value) => RecordedValue::Integer(*value),
            ValueData::Float(value) => RecordedValue::Float(*value),
            ValueData::Fraction(value) => RecordedValue::Fraction(*value),
            ValueData::String(value) => RecordedValue::String(value.clone()),
            ValueData::Bytes(_) => RecordedValue::Materialized {
                kind: "bytes",
                content_id: content_identity(value)?,
            },
            ValueData::List(_) => RecordedValue::Materialized {
                kind: "list",
                content_id: content_identity(value)?,
            },
            ValueData::Record(_) => RecordedValue::Materialized {
                kind: "record",
                content_id: content_identity(value)?,
            },
            ValueData::Buffer(_) => RecordedValue::Materialized {
                kind: "buffer",
                content_id: content_identity(value)?,
            },
            ValueData::Asset(asset) => {
                let SemanticValueIdentity::Source(source_id) = semantic_identity else {
                    return Err(IdentityError::unavailable(
                        "asset lineage does not contain an observed source identity",
                    ));
                };
                RecordedValue::Source {
                    locator: asset.locator.clone(),
                    source_id,
                }
            }
            ValueData::Transform(_) | ValueData::Lineage(_) => {
                return Err(IdentityError::unavailable(
                    "callable transform and lineage values cannot be recorded as transform arguments",
                ));
            }
        };
        Ok(Self {
            name: name.into(),
            semantic_identity,
            value: recorded,
            lineage: value.lineage.clone(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineageError {
    message: String,
}

impl LineageError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for LineageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for LineageError {}

#[derive(Default)]
struct LineageRenderer {
    seen: BTreeMap<LineageKey, usize>,
    lines: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum LineageKey {
    Source(SourceIdentity),
    UnobservedSource(Arc<str>),
    Invocation(RecipeIdentity),
    ExternalObservation(DependencyIdentity),
}

impl LineageRenderer {
    fn visit(&mut self, lineage: &Lineage) -> usize {
        let key = lineage.render_key();
        if let Some(id) = self.seen.get(&key) {
            return *id;
        }
        match lineage.node() {
            LineageNode::Invocation(invocation) => {
                for argument in invocation.arguments.iter() {
                    if let Some(parent) = &argument.lineage {
                        self.visit(parent);
                    }
                }
                for observation in invocation.observations.iter() {
                    self.visit(observation);
                }
            }
            LineageNode::Source(_) | LineageNode::ExternalObservation(_) => {}
        }
        let id = self.seen.len();
        self.seen.insert(key, id);
        match lineage.node() {
            LineageNode::Source(source) => {
                let content = source
                    .observed_content
                    .map_or_else(|| "unobserved".to_owned(), |value| value.to_string());
                self.lines.push(format!(
                    "#{id} source {:?} content={content}",
                    source.locator
                ));
            }
            LineageNode::ExternalObservation(observation) => self.lines.push(format!(
                "#{id} observe {} {:?} content={}",
                observation.capability,
                String::from_utf8_lossy(&observation.key),
                observation.observed_content
            )),
            LineageNode::Invocation(invocation) => {
                self.lines.push(format!(
                    "#{id} invoke {} transform={} recipe={}",
                    invocation.transform_name, invocation.transform_id, invocation.recipe_id
                ));
                for argument in invocation.arguments.iter() {
                    let parent = argument
                        .lineage
                        .as_ref()
                        .and_then(|lineage| self.seen.get(&lineage.render_key()))
                        .map_or_else(String::new, |id| format!(" from=#{id}"));
                    self.lines.push(format!(
                        "    {} = {} identity={}{parent}",
                        argument.name, argument.value, argument.semantic_identity
                    ));
                }
            }
        }
        id
    }
}

impl fmt::Display for RecordedValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => formatter.write_str("null"),
            Self::Bool(value) => value.fmt(formatter),
            Self::Integer(value) => value.fmt(formatter),
            Self::Float(value) => value.fmt(formatter),
            Self::Fraction(value) => value.fmt(formatter),
            Self::String(value) => write!(formatter, "{value:?}"),
            Self::Materialized { kind, content_id } => {
                write!(formatter, "<{kind} content={content_id}>")
            }
            Self::Source { locator, source_id } => {
                write!(formatter, "asset({locator:?}, source={source_id})")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{BufferValue, ValueData};

    fn example_recipe_lineage(locator: &str) -> Lineage {
        let compiled = crate::compile(
            "test.tima",
            "transform decode(value: BytesView) -> BytesView { return value }\n\
             transform encode(value: BytesView, quality: i64) -> BytesView { return value }\n",
        )
        .unwrap();
        let source = Lineage::observed_source(
            locator,
            crate::identity::byte_content_identity(b"source bytes"),
        );
        let source_value = OuterValue::plain(ValueData::Bytes(Arc::new(b"source bytes".to_vec())))
            .with_lineage(source);
        let decoded = Lineage::invocation(
            "decode",
            compiled.identities.get(crate::ir::TransformId(0)),
            vec![LineageArgument::record("value", &source_value).unwrap()],
            vec![],
        )
        .unwrap();
        let decoded_value = OuterValue::plain(ValueData::Bytes(Arc::new(b"decoded".to_vec())))
            .with_lineage(decoded);
        Lineage::invocation(
            "encode",
            compiled.identities.get(crate::ir::TransformId(1)),
            vec![
                LineageArgument::record("value", &decoded_value).unwrap(),
                LineageArgument::record("quality", &OuterValue::plain(ValueData::Integer(85)))
                    .unwrap(),
            ],
            vec![],
        )
        .unwrap()
    }

    #[test]
    fn trace_renders_shared_sources_before_their_invocation() {
        let input = OuterValue::buffer(BufferValue::new(vec![1], 1, vec![7]).unwrap());
        let content = content_identity(&input).unwrap();
        let source = Lineage::observed_source("cat.raw", content);
        let input = input.with_lineage(source);
        let argument = LineageArgument::record("value", &input).unwrap();
        let compiled = crate::compile(
            "test.tima",
            "transform keep(value: BufferView) -> BufferView { return value }\n",
        )
        .unwrap();
        let invocation = Lineage::invocation(
            "keep",
            compiled.identities.get(crate::ir::TransformId(0)),
            vec![argument],
            vec![],
        )
        .unwrap();
        let rendered = invocation.render();
        assert!(rendered.contains("#0 source \"cat.raw\""));
        assert!(rendered.contains("#1 invoke keep"));
        assert!(rendered.contains("from=#0"));
        assert!(rendered.ends_with("root = #1"));
    }

    #[test]
    fn external_observations_are_deduplicated_in_recipes() {
        let content = crate::identity::byte_content_identity(b"font");
        let observation =
            Lineage::external_observation("filesystem", b"font.ttf".as_slice(), content);
        let compiled = crate::compile(
            "test.tima",
            "transform keep(value: i64) -> i64 { return value }\n",
        )
        .unwrap();
        let argument =
            LineageArgument::record("value", &OuterValue::plain(ValueData::Integer(4))).unwrap();
        let first = Lineage::invocation(
            "keep",
            compiled.identities.get(crate::ir::TransformId(0)),
            vec![argument.clone()],
            vec![observation.clone(), observation.clone()],
        )
        .unwrap();
        let second = Lineage::invocation(
            "keep",
            compiled.identities.get(crate::ir::TransformId(0)),
            vec![argument],
            vec![observation],
        )
        .unwrap();
        let LineageNode::Invocation(first) = first.node() else {
            unreachable!()
        };
        let LineageNode::Invocation(second) = second.node() else {
            unreachable!()
        };
        assert_eq!(first.recipe_id, second.recipe_id);
        assert_eq!(first.observations.len(), 1);
    }

    #[test]
    fn recipe_expression_reconstructs_and_pins_a_linear_outer_pipeline() {
        let lineage = example_recipe_lineage("assets/cat \"one\".bin");
        let expression = lineage.outer_expression(None).unwrap();
        let LineageNode::Invocation(root) = lineage.node() else {
            unreachable!()
        };
        let LineageNode::Invocation(decoded) = root.arguments[0]
            .lineage
            .as_ref()
            .expect("encoded value has decode lineage")
            .node()
        else {
            unreachable!()
        };
        let LineageNode::Source(source) = decoded.arguments[0]
            .lineage
            .as_ref()
            .expect("decoded value has source lineage")
            .node()
        else {
            unreachable!()
        };

        assert_eq!(
            expression,
            format!(
                "(read(asset(\"assets/cat \\\"one\\\".bin\"))#{} | decode#{} | encode#{}(85))#{}",
                source.source_id.unwrap(),
                decoded.transform_id,
                root.transform_id,
                root.recipe_id
            )
        );
    }

    #[test]
    fn recipe_expression_can_replace_the_primary_starting_input() {
        let lineage = example_recipe_lineage("cat.bin");
        let expression = lineage
            .outer_expression(Some("asset(\"dog.bin\") | read"))
            .unwrap();
        let LineageNode::Invocation(root) = lineage.node() else {
            unreachable!()
        };
        let LineageNode::Invocation(decoded) = root.arguments[0]
            .lineage
            .as_ref()
            .expect("encoded value has decode lineage")
            .node()
        else {
            unreachable!()
        };

        assert_eq!(
            expression,
            format!(
                "(asset(\"dog.bin\") | read) | decode#{} | encode#{}(85)",
                decoded.transform_id, root.transform_id
            )
        );
        assert!(!expression.contains(&root.recipe_id.to_string()));
        assert!(!expression.contains("cat.bin"));
    }

    #[test]
    fn recipe_expression_requires_one_outer_expression_as_replacement() {
        let lineage = example_recipe_lineage("cat.bin");
        let error = lineage.outer_expression(Some("value = 1")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("exactly one Tima outer expression")
        );

        let error = lineage.outer_expression(Some("first\nsecond")).unwrap_err();
        assert!(error.to_string().contains("one-line"));
    }

    #[test]
    fn recipe_expression_replaces_only_the_primary_branch() {
        let primary = example_recipe_lineage("primary.bin");
        let secondary = example_recipe_lineage("secondary.bin");
        let secondary_recipe = secondary.recipe_id().unwrap();
        let primary_value =
            OuterValue::plain(ValueData::Bytes(Arc::new(vec![1]))).with_lineage(primary);
        let secondary_value =
            OuterValue::plain(ValueData::Bytes(Arc::new(vec![2]))).with_lineage(secondary);
        let compiled = crate::compile(
            "combine.tima",
            "transform combine(left: BytesView, right: BytesView) -> BytesView { return left }\n",
        )
        .unwrap();
        let combined = Lineage::invocation(
            "combine",
            compiled.identities.get(crate::ir::TransformId(0)),
            vec![
                LineageArgument::record("left", &primary_value).unwrap(),
                LineageArgument::record("right", &secondary_value).unwrap(),
            ],
            vec![],
        )
        .unwrap();

        let expression = combined.outer_expression(Some("replacement")).unwrap();
        assert!(expression.starts_with("(replacement) | "));
        assert!(!expression.contains("primary.bin"));
        assert!(expression.contains("secondary.bin"));
        assert!(expression.contains(&secondary_recipe.to_string()));
        assert!(expression.contains("combine#"));
    }

    #[test]
    fn recipe_expression_requires_lineage_for_materialized_arguments() {
        let compiled = crate::compile(
            "keep.tima",
            "transform keep(value: BytesView) -> BytesView { return value }\n",
        )
        .unwrap();
        let bytes = OuterValue::plain(ValueData::Bytes(Arc::new(vec![1, 2, 3])));
        let lineage = Lineage::invocation(
            "keep",
            compiled.identities.get(crate::ir::TransformId(0)),
            vec![LineageArgument::record("value", &bytes).unwrap()],
            vec![],
        )
        .unwrap();

        let error = lineage.outer_expression(None).unwrap_err();
        assert!(error.to_string().contains("no reconstructable lineage"));
        assert_eq!(
            lineage.outer_expression(Some("input")).unwrap(),
            format!(
                "(input) | keep#{}",
                compiled.identities.get(crate::ir::TransformId(0))
            )
        );
    }

    #[test]
    fn recipe_expression_float_values_round_trip_through_tima_bits() {
        let mut patterns = vec![
            0,
            0x8000_0000,
            1,
            0x8000_0001,
            f32::MIN_POSITIVE.to_bits(),
            (-f32::MIN_POSITIVE).to_bits(),
            f32::MAX.to_bits(),
            (-f32::MAX).to_bits(),
            f32::INFINITY.to_bits(),
            f32::NEG_INFINITY.to_bits(),
            f32::NAN.to_bits(),
            0x7f80_0001,
            0xffc0_1234,
        ];
        for exponent in 0_u32..=255 {
            for mantissa in [0, 1, 0x003f_ffff, 0x007f_fffe, 0x007f_ffff] {
                let magnitude = (exponent << 23) | mantissa;
                patterns.push(magnitude);
                patterns.push(magnitude | 0x8000_0000);
            }
        }
        let mut random = 0x6d2b_79f5_u32;
        for _ in 0..2048 {
            random = random.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            patterns.push(random);
        }
        patterns.sort_unstable();
        patterns.dedup();

        let expressions = patterns
            .iter()
            .map(|bits| float_literal(f32::from_bits(*bits)))
            .collect::<Vec<_>>();
        assert_eq!(float_literal(0.8), "0.8");
        assert_eq!(float_literal(-0.0), "f32.from_bits(2147483648)");
        let compiled = crate::compile(
            "float-roundtrip.tima",
            format!("out = [{}]\n", expressions.join(", ")),
        )
        .unwrap();
        let execution = crate::runtime::execute(&compiled).unwrap();
        let ValueData::List(values) = &execution.bindings["out"].data else {
            panic!("expected generated float list")
        };
        assert_eq!(values.len(), patterns.len());
        for (value, expected) in values.iter().zip(patterns) {
            let ValueData::Float(value) = &value.data else {
                panic!("expected generated f32")
            };
            assert_eq!(value.to_bits(), expected);
        }
    }

    #[test]
    fn recipe_expression_fraction_values_round_trip_canonically() {
        for expected in [
            Fraction::new(2, 4).unwrap(),
            Fraction::new(-5, 7).unwrap(),
            Fraction::new(i64::MIN, i64::MAX).unwrap(),
        ] {
            let rendered = render_recorded_expression(&RecordedValue::Fraction(expected)).unwrap();
            let compiled = crate::compile(
                "fraction-roundtrip.tima",
                format!("out = {}\n", rendered.text),
            )
            .unwrap();
            let execution = crate::runtime::execute(&compiled).unwrap();
            assert_eq!(
                execution.bindings["out"].data,
                ValueData::Fraction(expected)
            );
        }
    }
}
