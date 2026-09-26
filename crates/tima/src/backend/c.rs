use std::fmt::Write;

use crate::abi::{self, TIMA_ABI_VERSION};
use crate::ast::BinaryOp;
use crate::backend::{NativeArtifact, NativeBackend};
use crate::diagnostic::Diagnostic;
use crate::ir::{Constant, Terminator, Transform, TypedModule, ValueId, ValueKind};

#[derive(Clone, Copy, Debug, Default)]
pub struct CBackend;

pub const C_BACKEND_VERSION: &str = "2";

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
             typedef struct { const unsigned char *data; size_t width; size_t height; size_t stride; } TimaImageView;\n\
             typedef union { bool boolean; int64_t i64_value; float f32_value; TimaImage image; TimaImageView image_view; } TimaValue;\n\n\
             #if defined(_WIN32)\n\
             #define TIMA_EXPORT __declspec(dllexport)\n\
             int _fltused = 0;\n\
             #else\n\
             #define TIMA_EXPORT __attribute__((visibility(\"default\")))\n\
             #endif\n\n",
        );

        writeln!(
            source,
            "TIMA_EXPORT uint32_t tima_abi_version(void) {{ return TIMA_ABI_VERSION; }}\n"
        )
        .unwrap();

        for (index, transform) in module.transforms.iter().enumerate() {
            prototype(&mut source, index, transform);
            source.push_str(";\n");
        }
        source.push('\n');
        for (index, transform) in module.transforms.iter().enumerate() {
            prototype(&mut source, index, transform);
            source.push_str(" {\n");
            let block = &transform.blocks[transform.entry.0 as usize];
            for id in &block.instructions {
                let value = transform.value(*id);
                writeln!(
                    source,
                    "    {} v{} = {};",
                    abi::lower_type(value.ty).c_name,
                    id.0,
                    expression(transform, *id)
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

        for (index, transform) in module.transforms.iter().enumerate() {
            abi_adapter(&mut source, index, transform);
        }

        Ok(NativeArtifact {
            backend: "c",
            backend_version: C_BACKEND_VERSION,
            abi_version: TIMA_ABI_VERSION,
            source,
        })
    }
}

fn abi_adapter(output: &mut String, index: usize, transform: &Transform) {
    writeln!(
        output,
        "TIMA_EXPORT int32_t tima_invoke_{index}(const TimaValue *args, TimaValue *result) {{"
    )
    .unwrap();
    if transform.parameters.is_empty() {
        output.push_str("    (void)args;\n");
    }
    write!(
        output,
        "    result->{} = tima_transform_{index}(",
        abi_field(transform.return_type),
    )
    .unwrap();
    for (argument, parameter) in transform.parameters.iter().enumerate() {
        if argument != 0 {
            output.push_str(", ");
        }
        write!(output, "args[{argument}].{}", abi_field(parameter.ty)).unwrap();
    }
    output.push_str(");\n    return 0;\n}\n\n");
}

fn abi_field(ty: crate::ir::Type) -> &'static str {
    match ty {
        crate::ir::Type::Bool => "boolean",
        crate::ir::Type::I64 => "i64_value",
        crate::ir::Type::F32 => "f32_value",
        crate::ir::Type::Image => "image",
        crate::ir::Type::ImageView => "image_view",
    }
}

fn prototype(output: &mut String, index: usize, transform: &Transform) {
    write!(
        output,
        "{} tima_transform_{index}(",
        abi::lower_type(transform.return_type).c_name,
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

fn expression(transform: &Transform, id: ValueId) -> String {
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
            let arguments = arguments
                .iter()
                .map(|argument| value_name(transform, *argument))
                .collect::<Vec<_>>()
                .join(", ");
            format!("tima_transform_{}({arguments})", callee.0)
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
                .contains("float tima_transform_0(float p0, float p1)")
        );
        assert!(artifact.source.contains("float v2 = (p0 * p1);"));
        assert!(artifact.source.contains("return v2;"));
        assert!(artifact.source.contains(
            "result->f32_value = tima_transform_0(args[0].f32_value, args[1].f32_value);"
        ));
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
                .contains("TimaImage tima_transform_0(TimaImage p0)")
        );
        assert!(
            artifact
                .source
                .contains("TimaImageView tima_transform_1(TimaImageView p0)")
        );
        assert!(
            artifact
                .source
                .contains("result->image = tima_transform_0(args[0].image);")
        );
        assert!(
            artifact
                .source
                .contains("result->image_view = tima_transform_1(args[0].image_view);")
        );
    }

    #[test]
    fn emitted_code_does_not_depend_on_nonsemantic_transform_names() {
        let first = crate::compile(
            "first.tima",
            "transform scale(x: f32) -> f32 { return x * 2.0 }\n",
        )
        .unwrap();
        let second = crate::compile(
            "second.tima",
            "transform renamed(value: f32) -> f32 { return value * 2.0 }\n",
        )
        .unwrap();
        assert_eq!(
            super::CBackend.emit(&first.transforms).unwrap().source,
            super::CBackend.emit(&second.transforms).unwrap().source
        );
    }
}
