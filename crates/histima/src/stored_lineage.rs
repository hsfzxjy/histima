use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension};
use tima::identity::{
    ContentIdentity, DependencyIdentity, RecipeIdentity, SemanticValueIdentity, SourceIdentity,
    TransformIdentity, content_identity,
};
use tima::lineage::{Lineage, LineageArgument, LineageNode, RecordedValue};
use tima::runtime::{OuterValue, ValueData};

use crate::error::{Error, Result};

const MAX_LINEAGE_DEPTH: usize = 256;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StoredRecipe {
    pub content_id: ContentIdentity,
    pub lineage: Lineage,
}

pub(crate) fn load(
    connection: &Connection,
    recipe: RecipeIdentity,
) -> Result<Option<StoredRecipe>> {
    let content_id = connection
        .query_row(
            "SELECT content_id FROM recipe_results WHERE recipe_id = ?1",
            [recipe.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(content_id) = content_id else {
        return Ok(None);
    };
    let content_id = parse_identity::<ContentIdentity>("Content", &content_id)?;
    let mut loader = Loader {
        connection,
        loaded: BTreeMap::new(),
        loading: BTreeSet::new(),
    };
    let lineage = loader.invocation(recipe, 0)?;
    Ok(Some(StoredRecipe {
        content_id,
        lineage,
    }))
}

struct Loader<'connection> {
    connection: &'connection Connection,
    loaded: BTreeMap<RecipeIdentity, Lineage>,
    loading: BTreeSet<RecipeIdentity>,
}

impl Loader<'_> {
    fn invocation(&mut self, recipe: RecipeIdentity, depth: usize) -> Result<Lineage> {
        if let Some(lineage) = self.loaded.get(&recipe) {
            return Ok(lineage.clone());
        }
        if depth >= MAX_LINEAGE_DEPTH {
            return Err(Error::catalog(format!(
                "stored lineage for recipe {recipe} exceeds depth limit {MAX_LINEAGE_DEPTH}"
            )));
        }
        if !self.loading.insert(recipe) {
            return Err(Error::catalog(format!(
                "stored lineage contains a cycle at recipe {recipe}"
            )));
        }
        let recipe_text = recipe.to_string();
        let invocation = self
            .connection
            .query_row(
                "SELECT transform_id, transform_name
                 FROM lineage_invocations WHERE recipe_id = ?1",
                [&recipe_text],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .ok_or_else(|| {
                Error::catalog(format!("recipe {recipe} has no invocation lineage row"))
            })?;
        let transform_id = parse_identity::<TransformIdentity>("Transform", &invocation.0)?;
        let arguments = self.arguments(recipe, depth)?;
        let observations = self.observations(recipe)?;
        let lineage = Lineage::invocation(invocation.1, transform_id, arguments, observations)
            .map_err(|error| Error::catalog(format!("cannot rebuild recipe {recipe}: {error}")))?;
        let rebuilt = lineage
            .recipe_id()
            .expect("invocation lineage always has a Recipe ID");
        if rebuilt != recipe {
            return Err(Error::catalog(format!(
                "stored lineage claims recipe {recipe}, but its semantic fields rebuild as {rebuilt}"
            )));
        }
        self.loading.remove(&recipe);
        self.loaded.insert(recipe, lineage.clone());
        Ok(lineage)
    }

    fn arguments(&mut self, recipe: RecipeIdentity, depth: usize) -> Result<Vec<LineageArgument>> {
        let recipe_text = recipe.to_string();
        let mut statement = self.connection.prepare(
            "SELECT argument_index, argument_name, semantic_kind, semantic_id,
                    recorded_kind, recorded_text, recorded_content_id, parent_kind, parent_id
             FROM lineage_arguments
             WHERE recipe_id = ?1
             ORDER BY argument_index",
        )?;
        let rows = statement.query_map([&recipe_text], |row| {
            Ok(ArgumentRow {
                index: row.get(0)?,
                name: row.get(1)?,
                semantic_kind: row.get(2)?,
                semantic_id: row.get(3)?,
                recorded_kind: row.get(4)?,
                recorded_text: row.get(5)?,
                recorded_content_id: row.get(6)?,
                parent_kind: row.get(7)?,
                parent_id: row.get(8)?,
            })
        })?;
        let rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let mut arguments = Vec::with_capacity(rows.len());
        for (expected_index, row) in rows.into_iter().enumerate() {
            let expected_index = i64::try_from(expected_index)
                .map_err(|_| Error::catalog("lineage argument index exceeds SQLite INTEGER"))?;
            if row.index != expected_index {
                return Err(Error::catalog(format!(
                    "recipe {recipe} argument indices are not contiguous at {expected_index}"
                )));
            }
            let semantic_identity = parse_semantic(&row.semantic_kind, &row.semantic_id)?;
            let parent =
                self.parent(row.parent_kind.as_deref(), row.parent_id.as_deref(), depth)?;
            let value = parse_recorded(
                &row.recorded_kind,
                row.recorded_text,
                row.recorded_content_id,
                semantic_identity,
            )?;
            validate_argument(&value, semantic_identity, parent.as_ref())?;
            arguments.push(LineageArgument {
                name: Arc::from(row.name),
                semantic_identity,
                value,
                lineage: parent,
            });
        }
        Ok(arguments)
    }

    fn parent(
        &mut self,
        kind: Option<&str>,
        identity: Option<&str>,
        depth: usize,
    ) -> Result<Option<Lineage>> {
        match (kind, identity) {
            (None, None) => Ok(None),
            (Some("source"), Some(identity)) => {
                let source = parse_identity::<SourceIdentity>("Source", identity)?;
                self.source(source).map(Some)
            }
            (Some("recipe"), Some(identity)) => {
                let recipe = parse_identity::<RecipeIdentity>("Recipe", identity)?;
                self.invocation(recipe, depth + 1).map(Some)
            }
            (Some(kind), Some(_)) => Err(Error::catalog(format!(
                "unknown stored lineage parent kind {kind:?}"
            ))),
            _ => Err(Error::catalog(
                "stored lineage parent kind and identity must both be present",
            )),
        }
    }

    fn source(&self, source: SourceIdentity) -> Result<Lineage> {
        let source_text = source.to_string();
        let row = self
            .connection
            .query_row(
                "SELECT locator, content_id FROM source_assets WHERE source_id = ?1",
                [&source_text],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .ok_or_else(|| Error::catalog(format!("stored source {source} is unavailable")))?;
        let content = parse_identity::<ContentIdentity>("Content", &row.1)?;
        let lineage = Lineage::observed_source(row.0, content);
        let LineageNode::Source(rebuilt) = lineage.node() else {
            unreachable!("observed_source constructs source lineage")
        };
        if rebuilt.source_id != Some(source) {
            return Err(Error::catalog(format!(
                "stored source {source} rebuilds as {}",
                rebuilt
                    .source_id
                    .expect("observed source always has a Source ID")
            )));
        }
        Ok(lineage)
    }

    fn observations(&self, recipe: RecipeIdentity) -> Result<Vec<Lineage>> {
        let recipe_text = recipe.to_string();
        let mut statement = self.connection.prepare(
            "SELECT link.observation_index, observation.dependency_id,
                    observation.capability, observation.observation_key,
                    observation.observed_content_id
             FROM invocation_observations AS link
             JOIN external_observations AS observation
               ON observation.dependency_id = link.dependency_id
             WHERE link.recipe_id = ?1
             ORDER BY link.observation_index",
        )?;
        let rows = statement.query_map([recipe_text], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        let mut observations = Vec::new();
        for (expected_index, row) in rows.enumerate() {
            let (index, dependency, capability, key, content) = row?;
            let expected_index = i64::try_from(expected_index)
                .map_err(|_| Error::catalog("observation index exceeds SQLite INTEGER"))?;
            if index != expected_index {
                return Err(Error::catalog(format!(
                    "recipe {recipe} observation indices are not contiguous at {expected_index}"
                )));
            }
            let dependency = parse_identity::<DependencyIdentity>("Dependency", &dependency)?;
            let content = parse_identity::<ContentIdentity>("Content", &content)?;
            let lineage = Lineage::external_observation(capability, key, content);
            let LineageNode::ExternalObservation(rebuilt) = lineage.node() else {
                unreachable!("external_observation constructs observation lineage")
            };
            if rebuilt.dependency_id != dependency {
                return Err(Error::catalog(format!(
                    "stored dependency {dependency} rebuilds as {}",
                    rebuilt.dependency_id
                )));
            }
            observations.push(lineage);
        }
        Ok(observations)
    }
}

struct ArgumentRow {
    index: i64,
    name: String,
    semantic_kind: String,
    semantic_id: String,
    recorded_kind: String,
    recorded_text: Option<String>,
    recorded_content_id: Option<String>,
    parent_kind: Option<String>,
    parent_id: Option<String>,
}

fn parse_semantic(kind: &str, identity: &str) -> Result<SemanticValueIdentity> {
    match kind {
        "content" => parse_identity::<ContentIdentity>("Content", identity).map(Into::into),
        "source" => parse_identity::<SourceIdentity>("Source", identity).map(Into::into),
        "recipe" => parse_identity::<RecipeIdentity>("Recipe", identity).map(Into::into),
        _ => Err(Error::catalog(format!(
            "unknown semantic identity kind {kind:?}"
        ))),
    }
}

fn parse_recorded(
    kind: &str,
    text: Option<String>,
    content: Option<String>,
    semantic: SemanticValueIdentity,
) -> Result<RecordedValue> {
    match kind {
        "null" => {
            require_absent(kind, &text, &content)?;
            Ok(RecordedValue::Null)
        }
        "bool" => {
            require_no_content(kind, &content)?;
            match text.as_deref() {
                Some("0") => Ok(RecordedValue::Bool(false)),
                Some("1") => Ok(RecordedValue::Bool(true)),
                _ => Err(Error::catalog("stored bool must be encoded as 0 or 1")),
            }
        }
        "i64" => {
            require_no_content(kind, &content)?;
            let text = text.ok_or_else(|| Error::catalog("stored i64 has no value"))?;
            text.parse::<i64>()
                .map(RecordedValue::Integer)
                .map_err(|error| Error::catalog(format!("invalid stored i64 {text:?}: {error}")))
        }
        "f32" => {
            require_no_content(kind, &content)?;
            let text = text.ok_or_else(|| Error::catalog("stored f32 has no value"))?;
            if text.len() != 8 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(Error::catalog(format!(
                    "stored f32 bits are not eight hexadecimal digits: {text:?}"
                )));
            }
            let bits = u32::from_str_radix(&text, 16).map_err(|error| {
                Error::catalog(format!("invalid stored f32 bits {text:?}: {error}"))
            })?;
            Ok(RecordedValue::Float(f32::from_bits(bits)))
        }
        "string" => {
            require_no_content(kind, &content)?;
            Ok(RecordedValue::String(Arc::new(text.ok_or_else(|| {
                Error::catalog("stored string has no value")
            })?)))
        }
        "bytes" | "list" | "record" | "image" => {
            if text.is_some() {
                return Err(Error::catalog(format!(
                    "stored materialized {kind} unexpectedly has inline text"
                )));
            }
            let content = content
                .ok_or_else(|| Error::catalog(format!("stored {kind} has no Content ID")))?;
            let content_id = parse_identity::<ContentIdentity>("Content", &content)?;
            let kind = match kind {
                "bytes" => "bytes",
                "list" => "list",
                "record" => "record",
                "image" => "image",
                _ => unreachable!(),
            };
            Ok(RecordedValue::Materialized { kind, content_id })
        }
        "source" => {
            require_no_content(kind, &content)?;
            let locator = text.ok_or_else(|| Error::catalog("stored source has no locator"))?;
            let SemanticValueIdentity::Source(source_id) = semantic else {
                return Err(Error::catalog(
                    "stored source value does not carry Source semantic identity",
                ));
            };
            Ok(RecordedValue::Source {
                locator: Arc::from(locator),
                source_id,
            })
        }
        _ => Err(Error::catalog(format!(
            "unknown recorded argument kind {kind:?}"
        ))),
    }
}

fn validate_argument(
    value: &RecordedValue,
    semantic: SemanticValueIdentity,
    parent: Option<&Lineage>,
) -> Result<()> {
    if let Some(parent) = parent {
        let parent_semantic = parent
            .semantic_identity()
            .ok_or_else(|| Error::catalog("stored argument parent has no semantic identity"))?;
        if semantic != parent_semantic {
            return Err(Error::catalog(
                "stored argument semantic identity disagrees with its parent lineage",
            ));
        }
    }

    if let RecordedValue::Source { locator, source_id } = value {
        if semantic != SemanticValueIdentity::Source(*source_id) {
            return Err(Error::catalog(
                "stored source value disagrees with its semantic identity",
            ));
        }
        let Some(parent) = parent else {
            return Err(Error::catalog("stored source value has no source lineage"));
        };
        let LineageNode::Source(source) = parent.node() else {
            return Err(Error::catalog(
                "stored source value parent is not source lineage",
            ));
        };
        if source.source_id != Some(*source_id) || source.locator.as_ref() != locator.as_ref() {
            return Err(Error::catalog(
                "stored source value disagrees with its source lineage",
            ));
        }
        return Ok(());
    }

    if parent.is_some() {
        return Ok(());
    }
    let content = match value {
        RecordedValue::Null => content_identity(&OuterValue::plain(ValueData::Null)),
        RecordedValue::Bool(value) => content_identity(&OuterValue::plain(ValueData::Bool(*value))),
        RecordedValue::Integer(value) => {
            content_identity(&OuterValue::plain(ValueData::Integer(*value)))
        }
        RecordedValue::Float(value) => {
            content_identity(&OuterValue::plain(ValueData::Float(*value)))
        }
        RecordedValue::String(value) => {
            content_identity(&OuterValue::plain(ValueData::String(value.clone())))
        }
        RecordedValue::Materialized { content_id, .. } => Ok(*content_id),
        RecordedValue::Source { .. } => unreachable!("source arguments return above"),
    }
    .map_err(|error| Error::catalog(format!("cannot validate stored argument: {error}")))?;
    if semantic != SemanticValueIdentity::Content(content) {
        return Err(Error::catalog(
            "stored argument value disagrees with its semantic identity",
        ));
    }
    Ok(())
}

fn require_absent(kind: &str, text: &Option<String>, content: &Option<String>) -> Result<()> {
    if text.is_some() || content.is_some() {
        return Err(Error::catalog(format!(
            "stored {kind} unexpectedly carries payload fields"
        )));
    }
    Ok(())
}

fn require_no_content(kind: &str, content: &Option<String>) -> Result<()> {
    if content.is_some() {
        return Err(Error::catalog(format!(
            "stored {kind} unexpectedly carries a Content ID"
        )));
    }
    Ok(())
}

fn parse_identity<T>(kind: &str, text: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    text.parse::<T>()
        .map_err(|error| Error::catalog(format!("invalid stored {kind} ID {text:?}: {error}")))
}
