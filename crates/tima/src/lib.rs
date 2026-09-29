//! The Tima language frontend, typed transform IR, and outer runtime.
//!
//! Tima deliberately shares one syntax tree between its dynamic outer layer
//! and statically checked `transform` declarations. Execution and future AOT
//! backends consume [`ir::TypedModule`], never syntax or a host-specific code
//! format.

use std::sync::Arc;

pub mod abi;
pub mod ast;
pub mod backend;
pub mod cache;
pub mod capability;
pub mod diagnostic;
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
use identity::TransformIdentities;
use ir::TypedModule;
use source::SourceFile;

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
    validate_identity_qualifiers(&syntax, &transforms, &identities, &plugins)?;
    Ok(CompiledProgram {
        source,
        syntax,
        transforms,
        identities,
        plugins,
    })
}

fn validate_identity_qualifiers(
    syntax: &Program,
    transforms: &TypedModule,
    identities: &TransformIdentities,
    plugins: &plugin::PluginRegistry,
) -> Result<(), Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();
    for expression in &syntax.expressions {
        let ast::ExprKind::IdentityQualified {
            callable,
            prefix,
            prefix_span,
        } = &expression.kind
        else {
            continue;
        };
        let Some(name) = callable_name(syntax, *callable) else {
            diagnostics.push(Diagnostic::error(
                "semantic identity qualifier must follow a transform name",
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
                format!("no semantic transform named `{name}` can be identity-qualified"),
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
                .with_note("update or remove the identity qualifier to use this definition"),
            );
        }
    }
    if diagnostics.is_empty() {
        Ok(())
    } else {
        Err(diagnostics)
    }
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
        ast::ExprKind::IdentityQualified { .. } => None,
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
    fn semantic_identity_qualifiers_reject_changed_definitions() {
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
}
