use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use crate::identity::{ContentIdentity, IdentityError, RecipeIdentity, content_identity};
use crate::runtime::{OuterValue, ValueData};

/// Runtime policy boundary for semantic transform-result caching.
///
/// Implementations may be in-memory or durable, but cache behavior must not
/// alter values or derivation lineage. The runtime attaches current lineage to
/// every returned value after lookup.
pub trait ResultCache {
    fn remember(&mut self, value: &OuterValue) -> Result<ContentIdentity, CacheError>;
    fn lookup(&mut self, recipe: RecipeIdentity) -> Result<Option<OuterValue>, CacheError>;
    fn store(
        &mut self,
        recipe: RecipeIdentity,
        value: &OuterValue,
    ) -> Result<ContentIdentity, CacheError>;
    fn materialized(&mut self, identity: ContentIdentity)
    -> Result<Option<OuterValue>, CacheError>;
}

/// Immutable in-memory content-addressed storage for materialized outer values.
/// Lineage is intentionally not stored: it is reconstructed from the recipe
/// that selected the content.
#[derive(Clone, Debug, Default)]
pub struct ContentStore {
    values: BTreeMap<ContentIdentity, ValueData>,
}

impl ContentStore {
    pub fn insert(&mut self, value: &OuterValue) -> Result<ContentIdentity, CacheError> {
        let identity = content_identity(value).map_err(CacheError::identity)?;
        self.insert_known(identity, value.data.clone());
        Ok(identity)
    }

    pub fn get(&self, identity: ContentIdentity) -> Option<OuterValue> {
        self.values.get(&identity).cloned().map(OuterValue::plain)
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    fn insert_known(&mut self, identity: ContentIdentity, value: ValueData) {
        self.values.entry(identity).or_insert(value);
    }
}

/// Recipe index layered over content-addressed storage. This is runtime policy,
/// not language semantics: removing the cache cannot change results or lineage.
#[derive(Clone, Debug, Default)]
pub struct TransformResultCache {
    recipes: BTreeMap<RecipeIdentity, ContentIdentity>,
    content: ContentStore,
    stats: CacheStats,
}

impl TransformResultCache {
    pub fn remember(&mut self, value: &OuterValue) -> Result<ContentIdentity, CacheError> {
        self.content.insert(value)
    }

    pub fn lookup(&mut self, recipe: RecipeIdentity) -> Result<Option<OuterValue>, CacheError> {
        let Some(expected_content) = self.recipes.get(&recipe).copied() else {
            self.stats.misses += 1;
            return Ok(None);
        };
        let Some(value) = self.content.get(expected_content) else {
            self.recipes.remove(&recipe);
            self.stats.misses += 1;
            self.stats.invalidations += 1;
            return Ok(None);
        };
        let actual_content = content_identity(&value).map_err(CacheError::identity)?;
        if actual_content != expected_content {
            self.recipes.remove(&recipe);
            self.content.values.remove(&expected_content);
            self.stats.misses += 1;
            self.stats.invalidations += 1;
            return Ok(None);
        }
        self.stats.hits += 1;
        Ok(Some(value))
    }

    pub fn store(
        &mut self,
        recipe: RecipeIdentity,
        value: &OuterValue,
    ) -> Result<ContentIdentity, CacheError> {
        let content = content_identity(value).map_err(CacheError::identity)?;
        if let Some(previous) = self.recipes.get(&recipe) {
            if *previous != content {
                return Err(CacheError::recipe_conflict(recipe, *previous, content));
            }
        } else {
            self.recipes.insert(recipe, content);
            self.stats.stores += 1;
        }
        self.content.insert_known(content, value.data.clone());
        Ok(content)
    }

    pub fn content(&self) -> &ContentStore {
        &self.content
    }

    pub fn invalidate_recipe(&mut self, recipe: RecipeIdentity) -> Option<ContentIdentity> {
        self.recipes.remove(&recipe)
    }

    pub fn stats(&self) -> CacheStats {
        self.stats
    }
}

impl ResultCache for TransformResultCache {
    fn remember(&mut self, value: &OuterValue) -> Result<ContentIdentity, CacheError> {
        Self::remember(self, value)
    }

    fn lookup(&mut self, recipe: RecipeIdentity) -> Result<Option<OuterValue>, CacheError> {
        Self::lookup(self, recipe)
    }

    fn store(
        &mut self,
        recipe: RecipeIdentity,
        value: &OuterValue,
    ) -> Result<ContentIdentity, CacheError> {
        Self::store(self, recipe, value)
    }

    fn materialized(
        &mut self,
        identity: ContentIdentity,
    ) -> Result<Option<OuterValue>, CacheError> {
        Ok(self.content.get(identity))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub stores: u64,
    pub invalidations: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheError {
    message: String,
}

impl CacheError {
    fn identity(error: IdentityError) -> Self {
        Self {
            message: format!("value is not content-cacheable: {error}"),
        }
    }

    fn recipe_conflict(
        recipe: RecipeIdentity,
        previous: ContentIdentity,
        actual: ContentIdentity,
    ) -> Self {
        Self {
            message: format!(
                "recipe {recipe} produced conflicting content identities {previous} and {actual}"
            ),
        }
    }

    /// Adapts a host cache/storage failure without exposing host-specific error
    /// types in the Tima runtime.
    pub fn storage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for CacheError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for CacheError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{SemanticValueIdentity, recipe_identity};
    use crate::ir::TransformId;

    fn recipe(value: i64) -> RecipeIdentity {
        let compiled = crate::compile(
            "test.tima",
            "transform keep(value: i64) -> i64 { return value }\n",
        )
        .unwrap();
        let argument = content_identity(&OuterValue::plain(ValueData::Integer(value))).unwrap();
        recipe_identity(
            compiled.identities.get(TransformId(0)),
            &[SemanticValueIdentity::Content(argument)],
            &[],
        )
    }

    #[test]
    fn recipes_share_content_and_validate_on_lookup() {
        let mut cache = TransformResultCache::default();
        let value = OuterValue::plain(ValueData::Integer(4));
        cache.store(recipe(4), &value).unwrap();
        cache.store(recipe(5), &value).unwrap();
        assert_eq!(cache.content().len(), 1);
        assert_eq!(cache.lookup(recipe(4)).unwrap(), Some(value));
        assert_eq!(cache.stats().hits, 1);
    }

    #[test]
    fn missing_content_invalidates_recipe_index() {
        let mut cache = TransformResultCache::default();
        let recipe = recipe(4);
        let value = OuterValue::plain(ValueData::Integer(4));
        let content = cache.store(recipe, &value).unwrap();
        cache.content.values.remove(&content);
        assert_eq!(cache.lookup(recipe).unwrap(), None);
        assert_eq!(cache.stats().invalidations, 1);
        assert_eq!(cache.lookup(recipe).unwrap(), None);
    }

    #[test]
    fn conflicting_content_for_one_recipe_is_rejected() {
        let mut cache = TransformResultCache::default();
        let recipe = recipe(4);
        cache
            .store(recipe, &OuterValue::plain(ValueData::Integer(4)))
            .unwrap();
        let error = cache
            .store(recipe, &OuterValue::plain(ValueData::Integer(5)))
            .unwrap_err();
        assert!(error.to_string().contains("conflicting content identities"));
        assert_eq!(cache.content().len(), 1);
    }
}
