use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::identity::{
    ContentIdentity, DependencyIdentity, IdentityError, RecipeIdentity, SemanticValueIdentity,
    SourceIdentity, TransformIdentity, content_identity, dependency_identity, recipe_identity,
    source_identity,
};
use crate::runtime::{OuterValue, ValueData};

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

impl LineageArgument {
    pub fn record(name: impl Into<Arc<str>>, value: &OuterValue) -> Result<Self, IdentityError> {
        let semantic_identity = match value.lineage.as_ref().and_then(Lineage::semantic_identity) {
            Some(identity) => identity,
            None => content_identity(value)?.into(),
        };
        let recorded = match &value.data {
            ValueData::Null => RecordedValue::Null,
            ValueData::Bool(value) => RecordedValue::Bool(*value),
            ValueData::Integer(value) => RecordedValue::Integer(*value),
            ValueData::Float(value) => RecordedValue::Float(*value),
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
            ValueData::Image(_) => RecordedValue::Materialized {
                kind: "image",
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
    use crate::runtime::{ImageValue, ValueData};

    #[test]
    fn trace_renders_shared_sources_before_their_invocation() {
        let input = OuterValue::image(ImageValue::new(1, 1, 1, vec![7]).unwrap());
        let content = content_identity(&input).unwrap();
        let source = Lineage::observed_source("cat.raw", content);
        let input = input.with_lineage(source);
        let argument = LineageArgument::record("value", &input).unwrap();
        let compiled = crate::compile(
            "test.tima",
            "transform keep(value: ImageView) -> ImageView { return value }\n",
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
}
