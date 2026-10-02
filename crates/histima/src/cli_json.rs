use std::path::Path;

use histima::{
    ArtifactInfo, ArtifactInspection, AssetSummary, AvailableTransformInfo, CatalogInfo,
    CatalogPage, CatalogSearch, CatalogStats, ContentInspection, DurableTrace, ImportedAsset,
    PipelineExecution, ProgramExecution, RecipeInspection, RecipeReplay, RecipeSummary,
    RecordedResult, WorkspaceSummary, WorkspaceVerification,
};
use serde_json::{Map, Value, json};
use tima::cache::CacheStats;
use tima::identity::SemanticValueIdentity;
use tima::lineage::{LineageNode, RecordedValue};
use tima::plugin::PluginTransformInfo;
use tima::runtime::{OuterValue, ValueData};

pub fn init(workspace: &Path, info: &CatalogInfo) -> Value {
    json!({
        "workspace": workspace.display().to_string(),
        "schema_version": info.schema_version,
        "journal_mode": info.journal_mode,
    })
}

pub fn imported(value: &ImportedAsset) -> Value {
    json!({
        "locator": value.locator,
        "content_id": value.content_id.to_string(),
        "source_id": value.source_id.to_string(),
        "byte_length": value.byte_len,
    })
}

pub fn imported_batch(values: &[ImportedAsset]) -> Value {
    json!({
        "count": values.len(),
        "assets": values.iter().map(imported).collect::<Vec<_>>(),
    })
}

pub fn stats(info: &CatalogInfo, stats: &CatalogStats) -> Value {
    json!({
        "schema_version": info.schema_version,
        "contents": stats.contents,
        "source_versions": stats.source_versions,
        "source_heads": stats.source_heads,
        "lineage_invocations": stats.lineage_invocations,
        "recipe_results": stats.recipe_results,
        "artifact_bundles": stats.artifact_bundles,
        "artifacts": stats.artifacts,
    })
}

pub fn assets(page: &CatalogPage<AssetSummary>) -> Value {
    json!({
        "count": page.items.len(),
        "truncated": page.truncated,
        "next_cursor": page.next_cursor,
        "assets": page.items.iter().map(|asset| json!({
            "locator": asset.locator,
            "source_id": asset.source_id.to_string(),
            "content_id": asset.content_id.to_string(),
            "byte_length": asset.byte_len,
        })).collect::<Vec<_>>(),
    })
}

pub fn recipes(page: &CatalogPage<RecipeSummary>) -> Value {
    json!({
        "count": page.items.len(),
        "truncated": page.truncated,
        "next_cursor": page.next_cursor,
        "recipes": page.items.iter().map(|recipe| json!({
            "recipe_id": recipe.recipe_id.to_string(),
            "transform_id": recipe.transform_id.to_string(),
            "transform_name": recipe.transform_name,
            "content_id": recipe.content_id.to_string(),
            "byte_length": recipe.byte_len,
        })).collect::<Vec<_>>(),
    })
}

pub fn search(result: &CatalogSearch) -> Value {
    json!({
        "query": result.query,
        "assets": {
            "count": result.assets.items.len(),
            "truncated": result.assets.truncated,
            "matches": result.assets.items.iter().map(|asset| json!({
                "locator": asset.locator,
                "source_id": asset.source_id.to_string(),
                "content_id": asset.content_id.to_string(),
                "byte_length": asset.byte_len,
            })).collect::<Vec<_>>(),
        },
        "recipes": {
            "count": result.recipes.items.len(),
            "truncated": result.recipes.truncated,
            "matches": result.recipes.items.iter().map(|recipe| json!({
                "recipe_id": recipe.recipe_id.to_string(),
                "transform_id": recipe.transform_id.to_string(),
                "transform_name": recipe.transform_name,
                "content_id": recipe.content_id.to_string(),
                "byte_length": recipe.byte_len,
            })).collect::<Vec<_>>(),
        },
    })
}

pub fn summary(value: &WorkspaceSummary) -> Value {
    let mut transform_section = transforms(&value.transforms);
    transform_section
        .as_object_mut()
        .expect("transform output is an object")
        .insert(
            "truncated".to_owned(),
            Value::Bool(value.transforms_truncated),
        );
    json!({
        "workspace": value.workspace.display().to_string(),
        "catalog": {
            "schema_version": value.catalog.schema_version,
            "foreign_keys_enabled": value.catalog.foreign_keys_enabled,
            "journal_mode": value.catalog.journal_mode,
        },
        "counts": {
            "contents": value.stats.contents,
            "source_versions": value.stats.source_versions,
            "source_heads": value.stats.source_heads,
            "lineage_invocations": value.stats.lineage_invocations,
            "recipe_results": value.stats.recipe_results,
            "artifact_bundles": value.stats.artifact_bundles,
            "artifacts": value.stats.artifacts,
        },
        "assets": assets(&value.assets),
        "recipes": recipes(&value.recipes),
        "transforms": transform_section,
    })
}

pub fn plugins(plugins: &[PluginTransformInfo]) -> Value {
    json!({
        "count": plugins.len(),
        "plugins": plugins.iter().map(|plugin| json!({
            "name": plugin.name,
            "semantic_version": plugin.semantic_version,
            "abi_version": plugin.abi_version,
            "transform_id": plugin.transform_id.to_string(),
            "artifact_id": plugin.artifact_id.to_string(),
            "module_content_id": plugin.module_content_id.to_string(),
            "signature": plugin.signature(),
            "parameters": plugin.parameters.iter().map(|parameter| json!({
                "name": parameter.name,
                "type": parameter.value_type.as_str(),
            })).collect::<Vec<_>>(),
            "result": plugin.result.as_str(),
        })).collect::<Vec<_>>(),
    })
}

pub fn transforms(transforms: &[AvailableTransformInfo]) -> Value {
    json!({
        "count": transforms.len(),
        "transforms": transforms.iter().map(|transform| json!({
            "name": transform.name,
            "implementation": transform.implementation.as_str(),
            "semantic_version": transform.semantic_version,
            "signature": transform.signature,
            "transform_id": transform.transform_id.to_string(),
            "abi_version": transform.abi_version,
            "artifact_id": transform.artifact_id.map(|identity| identity.to_string()),
            "module_content_id": transform.module_content_id.map(|identity| identity.to_string()),
        })).collect::<Vec<_>>(),
    })
}

pub fn verification(report: &WorkspaceVerification) -> Value {
    json!({
        "valid": report.is_valid(),
        "sqlite_valid": report.sqlite_valid,
        "objects_checked": report.objects_checked,
        "objects_valid": report.objects_valid,
        "issue_count": report.issues.len(),
        "issues": report.issues.iter().map(|issue| json!({
            "kind": issue.kind.as_str(),
            "subject": issue.subject,
            "message": issue.message,
        })).collect::<Vec<_>>(),
    })
}

pub fn content(inspection: &ContentInspection) -> Value {
    json!({
        "content_id": inspection.content_id.to_string(),
        "kind": inspection.kind,
        "byte_length": inspection.byte_len,
        "relative_path": inspection.relative_path,
        "source_references": inspection.source_references,
        "recipe_references": inspection.recipe_references,
        "valid": inspection.valid,
        "validation_error": inspection.validation_error,
    })
}

pub fn recipe(inspection: &RecipeInspection) -> Result<Value, String> {
    let LineageNode::Invocation(invocation) = inspection.lineage.node() else {
        return Err(format!(
            "recorded recipe {} does not have invocation lineage",
            inspection.recipe_id
        ));
    };
    let arguments = invocation
        .arguments
        .iter()
        .map(|argument| {
            json!({
                "name": argument.name.as_ref(),
                "semantic_identity": semantic_identity(argument.semantic_identity),
                "recorded_value": recorded_value(&argument.value),
                "parent": argument.lineage.as_ref().map(|lineage| lineage_identity(lineage.node())),
            })
        })
        .collect::<Vec<_>>();
    let observations = invocation
        .observations
        .iter()
        .map(|lineage| {
            let LineageNode::ExternalObservation(observation) = lineage.node() else {
                return Err(format!(
                    "recipe {} has a non-external observation",
                    inspection.recipe_id
                ));
            };
            Ok(json!({
                "dependency_id": observation.dependency_id.to_string(),
                "capability": observation.capability.as_ref(),
                "key": observation_key(&observation.key),
                "content_id": observation.observed_content.to_string(),
            }))
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(json!({
        "recipe_id": inspection.recipe_id.to_string(),
        "content_id": inspection.content_id.to_string(),
        "transform_name": invocation.transform_name.as_ref(),
        "transform_id": invocation.transform_id.to_string(),
        "content_valid": inspection.content.valid,
        "content_validation_error": inspection.content.validation_error,
        "arguments": arguments,
        "observations": observations,
        "trace": inspection.rendered,
    }))
}

pub fn artifact(inspection: &ArtifactInspection) -> Value {
    json!({
        "requested_id": inspection.requested_id,
        "matched_artifact": inspection.artifact.as_ref().map(|artifact| json!({
            "artifact_id": artifact.artifact_id.to_string(),
            "transform_id": artifact.transform_id.to_string(),
        })),
        "bundle_count": inspection.bundles.items.len(),
        "bundles_truncated": inspection.bundles.truncated,
        "bundles": inspection.bundles.items.iter().map(|bundle| json!({
            "bundle_id": bundle.bundle_id.to_string(),
            "backend": bundle.backend,
            "backend_version": bundle.backend_version,
            "compiler_version": bundle.compiler_version,
            "target": bundle.target,
            "cpu_features": bundle.cpu_features,
            "optimization": bundle.optimization,
            "abi_version": bundle.abi_version,
            "artifact_content_id": bundle.artifact_content_id.to_string(),
            "artifact_byte_length": bundle.artifact_byte_len,
            "artifact_relative_path": bundle.artifact_relative_path,
            "identity_valid": bundle.identity_valid,
            "artifact_valid": bundle.artifact_valid,
            "valid": bundle.valid,
            "members": bundle.members.iter().map(|member| json!({
                "index": member.index,
                "artifact_id": member.artifact_id.to_string(),
                "transform_id": member.transform_id.to_string(),
            })).collect::<Vec<_>>(),
            "validation_errors": bundle.validation_errors,
        })).collect::<Vec<_>>(),
    })
}

pub fn materialized(content_id: impl ToString, destination: &Path) -> Value {
    json!({
        "content_id": content_id.to_string(),
        "materialized": destination.display().to_string(),
    })
}

pub fn recipe_expression(
    recipe_id: impl ToString,
    starting_input: Option<&str>,
    expression: &str,
) -> Value {
    json!({
        "recipe_id": recipe_id.to_string(),
        "starting_input": starting_input,
        "expression": expression,
    })
}

pub fn run(result: &ProgramExecution, recorded: Option<(&str, &RecordedResult)>) -> Value {
    let bindings = result
        .execution
        .bindings
        .iter()
        .map(|(name, value)| (name.clone(), outer_value(value)))
        .collect::<Map<_, _>>();
    let trace = result.execution.last_value.as_ref().and_then(|last| {
        let ValueData::Lineage(lineage) = &last.data else {
            return None;
        };
        (!result
            .execution
            .bindings
            .values()
            .any(|value| value == last))
        .then(|| lineage.render())
    });
    json!({
        "execution_engine": "interpreter",
        "artifact_cache": Value::Null,
        "result_cache": cache_stats(result.result_cache),
        "artifact": result.artifact.as_ref().map(artifact_info),
        "bindings": bindings,
        "trace": trace,
        "recorded": recorded.map(|(binding, recorded)| json!({
            "binding": binding,
            "recipe_id": recorded.recipe_id.to_string(),
            "content_id": recorded.content_id.to_string(),
            "byte_length": recorded.byte_len,
        })),
    })
}

pub fn pipeline(result: &PipelineExecution, stocked: Option<&RecordedResult>) -> Value {
    json!({
        "result_cache": cache_stats(result.result_cache),
        "result": outer_value(&result.value),
        "stocked": stocked.map(|recorded| json!({
            "recipe_id": recorded.recipe_id.to_string(),
            "content_id": recorded.content_id.to_string(),
            "byte_length": recorded.byte_len,
        })),
        "trace": result.value.lineage.as_ref().map(|lineage| lineage.render()),
    })
}

pub fn replay(recipe_id: impl ToString, content_id: impl ToString, result: &RecipeReplay) -> Value {
    json!({
        "execution_engine": "interpreter",
        "replay_policy": result.policy.name(),
        "artifact_cache": Value::Null,
        "result_cache": cache_stats(result.result_cache),
        "artifact": result.artifact.as_ref().map(artifact_info),
        "recipe_id": recipe_id.to_string(),
        "content_id": content_id.to_string(),
        "replayed": outer_value(&result.value),
        "trace": result.value.lineage.as_ref().map(|lineage| lineage.render()),
    })
}

pub fn trace(trace: &DurableTrace) -> Value {
    json!({
        "recipe_id": trace.recipe_id.to_string(),
        "content_id": trace.content_id.to_string(),
        "trace": trace.rendered,
    })
}

fn cache_stats(stats: CacheStats) -> Value {
    json!({
        "hits": stats.hits,
        "misses": stats.misses,
        "stores": stats.stores,
        "invalidations": stats.invalidations,
    })
}

fn artifact_info(artifact: &ArtifactInfo) -> Value {
    json!({
        "bundle_id": artifact.bundle_id.to_string(),
        "artifact_ids": artifact.artifact_ids.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "artifact_content_id": artifact.artifact_content_id.to_string(),
    })
}

fn semantic_identity(identity: SemanticValueIdentity) -> Value {
    match identity {
        SemanticValueIdentity::Content(identity) => {
            json!({"kind": "content", "id": identity.to_string()})
        }
        SemanticValueIdentity::Source(identity) => {
            json!({"kind": "source", "id": identity.to_string()})
        }
        SemanticValueIdentity::Recipe(identity) => {
            json!({"kind": "recipe", "id": identity.to_string()})
        }
    }
}

fn lineage_identity(node: &LineageNode) -> Value {
    match node {
        LineageNode::Source(source) => json!({
            "kind": "source",
            "id": source.source_id.map(|identity| identity.to_string()),
            "locator": source.locator.as_ref(),
        }),
        LineageNode::Invocation(invocation) => {
            json!({"kind": "recipe", "id": invocation.recipe_id.to_string()})
        }
        LineageNode::ExternalObservation(observation) => {
            json!({"kind": "dependency", "id": observation.dependency_id.to_string()})
        }
    }
}

fn recorded_value(value: &RecordedValue) -> Value {
    match value {
        RecordedValue::Null => json!({"kind": "null"}),
        RecordedValue::Bool(value) => json!({"kind": "bool", "value": value}),
        RecordedValue::Integer(value) => json!({"kind": "i64", "value": value}),
        RecordedValue::Float(value) => json!({
            "kind": "f32",
            "bits": format!("{:08x}", value.to_bits()),
            "display": value.to_string(),
        }),
        RecordedValue::Fraction(value) => json!({
            "kind": "fraction",
            "numerator": value.numerator(),
            "denominator": value.denominator(),
        }),
        RecordedValue::String(value) => {
            json!({"kind": "string", "value": value.as_ref()})
        }
        RecordedValue::Materialized { kind, content_id } => json!({
            "kind": "materialized",
            "value_kind": kind,
            "content_id": content_id.to_string(),
        }),
        RecordedValue::Source { locator, source_id } => json!({
            "kind": "source",
            "locator": locator.as_ref(),
            "source_id": source_id.to_string(),
        }),
    }
}

fn observation_key(key: &[u8]) -> Value {
    match std::str::from_utf8(key) {
        Ok(text) => json!({"encoding": "utf8", "value": text}),
        Err(_) => json!({"encoding": "hex", "value": hex(key)}),
    }
}

fn outer_value(value: &OuterValue) -> Value {
    match &value.data {
        ValueData::Null => json!({"type": "null"}),
        ValueData::Bool(value) => json!({"type": "bool", "value": value}),
        ValueData::Integer(value) => json!({"type": "i64", "value": value}),
        ValueData::Float(value) => json!({
            "type": "f32",
            "bits": format!("{:08x}", value.to_bits()),
            "display": value.to_string(),
        }),
        ValueData::Fraction(value) => json!({
            "type": "fraction",
            "numerator": value.numerator(),
            "denominator": value.denominator(),
        }),
        ValueData::String(value) => json!({"type": "string", "value": value.as_ref()}),
        ValueData::Bytes(value) => json!({"type": "bytes", "byte_length": value.len()}),
        ValueData::List(values) => json!({
            "type": "list",
            "items": values.iter().map(outer_value).collect::<Vec<_>>(),
        }),
        ValueData::Record(values) => json!({
            "type": "record",
            "fields": values.iter().map(|(name, value)| (name.clone(), outer_value(value))).collect::<Map<_, _>>(),
        }),
        ValueData::Asset(asset) => {
            json!({"type": "asset", "locator": asset.locator.as_ref()})
        }
        ValueData::Buffer(buffer) => json!({
            "type": "buffer",
            "shape": buffer.shape(),
            "outer_stride": buffer.outer_stride(),
            "byte_length": buffer.byte_len(),
        }),
        ValueData::Transform(id) => json!({"type": "transform", "index": id.0}),
        ValueData::Lineage(lineage) => json!({"type": "lineage", "trace": lineage.render()}),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
