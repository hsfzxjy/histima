//! The Tima language frontend, typed transform IR, and outer runtime.
//!
//! Tima deliberately shares one syntax tree between its dynamic outer layer
//! and statically checked `transform` declarations. Execution and future AOT
//! backends consume [`ir::TypedModule`], never syntax or a host-specific code
//! format.

use std::collections::BTreeSet;
use std::sync::Arc;

pub mod abi;
pub mod ast;
pub mod backend;
pub mod cache;
pub mod capability;
pub mod diagnostic;
pub mod fraction;
pub mod identity;
pub mod ir;
pub mod lexer;
pub mod lineage;
pub mod parser;
pub mod plugin;
mod registered;
mod registered_wasm;
pub mod runtime;
pub mod semantic;
pub mod source;

use ast::Program;
use diagnostic::Diagnostic;
use identity::{TransformIdentities, TransformIdentity};
use ir::TypedModule;
use source::SourceFile;

pub use registered::{
    BuiltinDefaultValue, BuiltinParameterInfo, BuiltinTransformInfo, BuiltinValueType,
};

/// Returns the process-wide standard transform registry in stable registry order.
pub fn builtin_transform_infos() -> impl Iterator<Item = BuiltinTransformInfo> {
    registered::RegisteredTransform::infos()
}

/// A parsed and statically checked Tima program.
#[derive(Debug)]
pub struct CompiledProgram {
    pub source: SourceFile,
    pub syntax: Program,
    pub transforms: TypedModule,
    pub identities: TransformIdentities,
    pub plugins: Arc<plugin::PluginRegistry>,
}

/// Runs the shared frontend and the inner-transform semantic pass.
pub fn compile(
    name: impl Into<String>,
    text: impl Into<String>,
) -> Result<CompiledProgram, Vec<Diagnostic>> {
    compile_with_plugins(name, text, Arc::default())
}

/// Runs the frontend with an explicit immutable registered-Wasm transform set.
pub fn compile_with_plugins(
    name: impl Into<String>,
    text: impl Into<String>,
    plugins: Arc<plugin::PluginRegistry>,
) -> Result<CompiledProgram, Vec<Diagnostic>> {
    let source = SourceFile::new(name, text);
    let syntax = parser::parse(&source)?;
    let transforms = semantic::check(&syntax)?;
    let identities = identity::transform_identities(&transforms)?;
    validate_callable_identity_assertions(&syntax, &transforms, &identities, &plugins)?;
    Ok(CompiledProgram {
        source,
        syntax,
        transforms,
        identities,
        plugins,
    })
}

fn validate_callable_identity_assertions(
    syntax: &Program,
    transforms: &TypedModule,
    identities: &TransformIdentities,
    plugins: &plugin::PluginRegistry,
) -> Result<(), Vec<Diagnostic>> {
    let callable_assertions = syntax
        .expressions
        .iter()
        .filter_map(|expression| match &expression.kind {
            ast::ExprKind::Call { callee, .. }
                if matches!(
                    syntax.expr(*callee).kind,
                    ast::ExprKind::IdentityAsserted { .. }
                ) =>
            {
                Some(*callee)
            }
            ast::ExprKind::Pipeline { stage, .. }
                if matches!(
                    syntax.expr(*stage).kind,
                    ast::ExprKind::IdentityAsserted { .. }
                ) =>
            {
                Some(*stage)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut diagnostics = Vec::new();
    for assertion in callable_assertions {
        let ast::ExprKind::IdentityAsserted {
            value,
            prefix,
            prefix_span,
        } = &syntax.expr(assertion).kind
        else {
            unreachable!("callable assertion IDs were filtered above")
        };
        let Some(name) = callable_name(syntax, *value) else {
            diagnostics.push(Diagnostic::error(
                "a callable identity assertion must follow a transform name",
                *prefix_span,
            ));
            continue;
        };
        let actual = transforms
            .find(&name)
            .map(|(id, _)| identities.get(id))
            .or_else(|| registered::RegisteredTransform::find(&name).map(|value| value.identity()))
            .or_else(|| plugins.find(&name).map(|value| value.identity()));
        let Some(actual) = actual else {
            diagnostics.push(Diagnostic::error(
                format!("no semantic transform named `{name}` can be identity-asserted"),
                *prefix_span,
            ));
            continue;
        };
        if !actual.to_string().starts_with(prefix) {
            diagnostics.push(
                Diagnostic::error(
                    format!(
                        "transform `{name}` has semantic identity {actual}, which does not match `#{prefix}`"
                    ),
                    *prefix_span,
                )
                    .with_note("update or remove the identity assertion to use this definition"),
            );
            continue;
        }
        let matches = available_transform_identities(identities, plugins)
            .into_iter()
            .filter(|identity| identity.to_string().starts_with(prefix))
            .collect::<Vec<_>>();
        if matches.len() > 1 {
            diagnostics.push(
                Diagnostic::error(
                    format!(
                        "transform identity assertion `#{prefix}` is ambiguous locally; it matches {} and {}",
                        matches[0], matches[1]
                    ),
                    *prefix_span,
                )
                .with_note("use a longer prefix or the full Transform ID"),
            );
        }
    }
    if diagnostics.is_empty() {
        Ok(())
    } else {
        Err(diagnostics)
    }
}

pub(crate) fn available_transform_identities(
    identities: &TransformIdentities,
    plugins: &plugin::PluginRegistry,
) -> BTreeSet<TransformIdentity> {
    identities
        .iter()
        .chain(registered::RegisteredTransform::infos().map(|info| info.transform_id))
        .chain(plugins.transform_infos().map(|info| info.transform_id))
        .collect()
}

fn callable_name(program: &Program, expression: ast::ExprId) -> Option<String> {
    match &program.expr(expression).kind {
        ast::ExprKind::Name(name) => Some(name.clone()),
        ast::ExprKind::Member { receiver, name, .. } => {
            let ast::ExprKind::Name(namespace) = &program.expr(*receiver).kind else {
                return None;
            };
            Some(format!("{namespace}.{name}"))
        }
        ast::ExprKind::IdentityAsserted { .. } => None,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_identity_prefixes_pin_outer_and_inner_transform_references() {
        let definition = "transform scale(x: f32, factor: f32) -> f32 { return x * factor }\n";
        let base = compile("base.tima", definition).unwrap();
        let identity = base.identities.get(ir::TransformId(0)).to_string();
        let prefix = &identity[..12];
        let source = format!(
            "{definition}\
             transform half(x: f32) -> f32 {{ return scale#{prefix}(x, 0.5) }}\n\
             out = 4.0 | half#{}( )\n",
            &compile(
                "half.tima",
                format!(
                    "{definition}transform half(x: f32) -> f32 {{ return scale#{prefix}(x, 0.5) }}\n"
                ),
            )
            .unwrap()
            .identities
            .get(ir::TransformId(1))
            .to_string()[..16]
        );

        compile("qualified.tima", source).unwrap();
    }

    #[test]
    fn callable_identity_assertions_reject_changed_definitions() {
        let diagnostics = compile(
            "mismatch.tima",
            "transform keep(x: f32) -> f32 { return x }\nout = keep#00000000(1.0)\n",
        )
        .unwrap_err();

        assert!(diagnostics[0].message.contains("does not match"));
        assert!(diagnostics[0].message.contains("transform `keep`"));
    }

    #[test]
    fn registered_transform_references_accept_full_or_prefixed_identity() {
        let identity = registered::RegisteredTransform::find("ppm.decode")
            .unwrap()
            .identity()
            .to_string();

        compile(
            "registered.tima",
            format!("out = bytes | ppm.decode#{}\n", &identity[..10]),
        )
        .unwrap();
        compile(
            "registered-full.tima",
            format!("out = bytes | ppm.decode#{identity}\n"),
        )
        .unwrap();
    }

    #[test]
    fn callable_identity_prefixes_must_be_unique_locally() {
        let definitions = (0..17)
            .map(|index| format!("transform f{index}() -> i64 {{ return {index} }}\n"))
            .collect::<String>();
        let compiled = compile("collision-base.tima", &definitions).unwrap();
        let mut first_by_prefix = std::collections::BTreeMap::new();
        let (target, prefix) = compiled
            .identities
            .iter()
            .enumerate()
            .find_map(|(index, identity)| {
                let text = identity.to_string();
                let prefix = text[..1].to_owned();
                first_by_prefix
                    .insert(prefix.clone(), index)
                    .map(|first| (first, prefix))
            })
            .expect("17 distinct transform identities collide in one hexadecimal digit");
        let diagnostics = compile(
            "collision.tima",
            format!("{definitions}out = f{target}#{prefix}()\n"),
        )
        .unwrap_err();

        assert!(diagnostics[0].message.contains("ambiguous locally"));
        assert!(diagnostics[0].notes[0].contains("longer prefix"));
    }
}
