use std::fmt::Write;

use crate::abi::{self, TIMA_ABI_VERSION};
use crate::ast::BinaryOp;
use crate::backend::{NativeArtifact, NativeBackend};
use crate::diagnostic::Diagnostic;
use crate::ir::{Constant, Terminator, Transform, TypedModule, ValueId, ValueKind};

#[derive(Clone, Copy, Debug, Default)]
pub struct CBackend;

impl NativeBackend for CBackend {
    fn emit(&self, module: &TypedModule) -> Result<NativeArtifact, Vec<Diagnostic>> {
        let mut source = String::new();
        writeln!(source, "/* Generated from typed Tima IR. */").unwrap();
        writeln!(source, "#include <stdbool.h>").unwrap();
        writeln!(source, "#include <stddef.h>").unwrap();
        writeln!(source, "#include <stdint.h>\n").unwrap();
        writeln!(source, "#define TIMA_ABI_VERSION {}\n", TIMA_ABI_VERSION).unwrap();
        source.push_str(
            "typedef struct { unsigned char *data; size_t width; size_t height; size_t stride; } TimaImage;\n\
             typedef struct { const unsigned char *data; size_t width; size_t height; size_t stride; } TimaImageView;\n\n",
        );

        for transform in &module.transforms {
            prototype(&mut source, transform);
            source.push_str(";\n");
        }
        source.push('\n');
        for transform in &module.transforms {
            prototype(&mut source, transform);
            source.push_str(" {\n");
            let block = &transform.blocks[transform.entry.0 as usize];
            for id in &block.instructions {
                let value = transform.value(*id);
                writeln!(
                    source,
                    "    {} v{} = {};",
                    abi::lower_type(value.ty).c_name,
                    id.0,
                    expression(module, transform, *id)
                )
                .unwrap();
            }
            match block.terminator {
                Terminator::Return(value) => {
                    writeln!(source, "    return {};", value_name(transform, value)).unwrap();
                }
            }
            source.push_str("}\n\n");
        }

        Ok(NativeArtifact {
            backend: "c",
            abi_version: TIMA_ABI_VERSION,
            source,
        })
    }
}

fn prototype(output: &mut String, transform: &Transform) {
    write!(
        output,
        "{} tima_{}(",
        abi::lower_type(transform.return_type).c_name,
        transform.name
    )
    .unwrap();
    for (index, parameter) in transform.parameters.iter().enumerate() {
        if index != 0 {
            output.push_str(", ");
        }
        write!(
            output,
            "{} p{}",
            abi::lower_type(parameter.ty).c_name,
            index
        )
        .unwrap();
    }
    if transform.parameters.is_empty() {
        output.push_str("void");
    }
    output.push(')');
}

fn expression(module: &TypedModule, transform: &Transform, id: ValueId) -> String {
    match &transform.value(id).kind {
        ValueKind::Parameter { index } => format!("p{index}"),
        ValueKind::Constant(Constant::Bool(value)) => value.to_string(),
        ValueKind::Constant(Constant::I64(value)) => format!("INT64_C({value})"),
        ValueKind::Constant(Constant::F32(value)) => format!("{value:?}f"),
        ValueKind::Binary { op, left, right } => {
            let operator = match op {
                BinaryOp::Add => "+",
                BinaryOp::Subtract => "-",
                BinaryOp::Multiply => "*",
                BinaryOp::Divide => "/",
            };
            format!(
                "({} {operator} {})",
                value_name(transform, *left),
                value_name(transform, *right)
            )
        }
        ValueKind::Call {
            transform: callee,
            arguments,
        } => {
            let callee = module.get(*callee);
            let arguments = arguments
                .iter()
                .map(|argument| value_name(transform, *argument))
                .collect::<Vec<_>>()
                .join(", ");
            format!("tima_{}({arguments})", callee.name)
        }
    }
}

fn value_name(transform: &Transform, id: ValueId) -> String {
    match transform.value(id).kind {
        ValueKind::Parameter { index } => format!("p{index}"),
        _ => format!("v{}", id.0),
    }
}

#[cfg(test)]
mod tests {
    use crate::backend::NativeBackend;

    #[test]
    fn emits_c_from_typed_ir() {
        let compiled = crate::compile(
            "test.tima",
            "transform scale(x: f32, factor: f32) -> f32 { return x * factor }\n",
        )
        .unwrap();
        let artifact = super::CBackend.emit(&compiled.transforms).unwrap();
        assert!(
            artifact
                .source
                .contains("float tima_scale(float p0, float p1)")
        );
        assert!(artifact.source.contains("float v2 = (p0 * p1);"));
        assert!(artifact.source.contains("return v2;"));
    }

    #[test]
    fn expresses_owned_and_view_image_abi_separately() {
        let compiled = crate::compile(
            "test.tima",
            "transform freeze(img: Image) -> Image { return img }\n\
             transform inspect(img: ImageView) -> ImageView { return img }\n",
        )
        .unwrap();
        let artifact = super::CBackend.emit(&compiled.transforms).unwrap();
        assert!(
            artifact
                .source
                .contains("TimaImage tima_freeze(TimaImage p0)")
        );
        assert!(
            artifact
                .source
                .contains("TimaImageView tima_inspect(TimaImageView p0)")
        );
    }
}
