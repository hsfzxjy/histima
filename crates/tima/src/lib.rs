//! The Tima language frontend, typed transform IR, and outer runtime.
//!
//! Tima deliberately shares one syntax tree between its dynamic outer layer
//! and statically checked `transform` declarations. Native backends consume
//! [`ir::TypedModule`], never syntax or generated C.

pub mod abi;
pub mod ast;
pub mod backend;
pub mod diagnostic;
pub mod identity;
pub mod ir;
pub mod lexer;
pub mod parser;
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
}

/// Runs the shared frontend and the inner-transform semantic pass.
pub fn compile(
    name: impl Into<String>,
    text: impl Into<String>,
) -> Result<CompiledProgram, Vec<Diagnostic>> {
    let source = SourceFile::new(name, text);
    let syntax = parser::parse(&source)?;
    let transforms = semantic::check(&syntax)?;
    let identities = identity::transform_identities(&transforms)?;
    Ok(CompiledProgram {
        source,
        syntax,
        transforms,
        identities,
    })
}
