use std::fmt::Write;

use crate::abi::{self, TIMA_ABI_VERSION};
use crate::ast::BinaryOp;
use crate::backend::{NativeArtifact, NativeBackend};
use crate::diagnostic::Diagnostic;
use crate::ir::{Constant, RuntimeCall, Terminator, Transform, TypedModule, ValueId, ValueKind};

#[derive(Clone, Copy, Debug, Default)]
pub struct CBackend;

pub const C_BACKEND_VERSION: &str = "8";

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
             typedef union { bool boolean; uint8_t u8_value; int64_t i64_value; float f32_value; TimaImage image; TimaImageView image_view; } TimaValue;\n\
             typedef int32_t (*TimaEnvironmentI64Fn)(void *context, uint32_t transform, uint32_t callsite, const unsigned char *name, size_t name_len, int64_t *result);\n\
             typedef struct { void *context; TimaEnvironmentI64Fn environment_i64; int32_t status; } TimaRuntime;\n\n\
             #if defined(_WIN32)\n\
             #define TIMA_EXPORT __declspec(dllexport)\n\
             int _fltused = 0;\n\
             #else\n\
             #define TIMA_EXPORT __attribute__((visibility(\"default\")))\n\
             #endif\n\n",
        );

        if module.transforms.iter().any(|transform| {
            transform
                .values
                .iter()
                .any(|value| matches!(value.kind, ValueKind::RuntimeCall(_)))
        }) {
            source.push_str(
                "static int64_t tima_environment_i64(TimaRuntime *runtime, uint32_t transform, uint32_t callsite, const unsigned char *name, size_t name_len) {\n\
                 int64_t result = 0;\n\
                 if (runtime->status != 0) return 0;\n\
                 if (runtime->environment_i64 == NULL) { runtime->status = -2; return 0; }\n\
                 runtime->status = runtime->environment_i64(runtime->context, transform, callsite, name, name_len, &result);\n\
                 return runtime->status == 0 ? result : 0;\n\
                 }\n\n",
            );
        }

        if module.transforms.iter().any(|transform| {
            transform
                .values
                .iter()
                .any(|value| matches!(value.kind, ValueKind::ImageFill { .. }))
        }) {
            source.push_str(
                "static TimaImage tima_image_fill(TimaImage image, uint8_t value) {\n\
                 size_t byte_len = image.height * image.stride;\n\
                 for (size_t index = 0; index < byte_len; ++index) image.data[index] = value;\n\
                 return image;\n\
                 }\n\n",
            );
        }

        if module.transforms.iter().any(|transform| {
            transform
                .values
                .iter()
                .any(|value| matches!(value.kind, ValueKind::ImageZero { .. }))
        }) {
            source.push_str(
                "static TimaImage tima_image_zero(TimaImage image) {\n\
                 size_t byte_len = image.height * image.stride;\n\
                 for (size_t index = 0; index < byte_len; ++index) image.data[index] = 0;\n\
                 return image;\n\
                 }\n\n",
            );
        }

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
            source.push_str(" {\n    (void)runtime;\n");
            for parameter_index in 0..transform.parameters.len() {
                writeln!(source, "    (void)p{parameter_index};").unwrap();
            }
            for (value_index, value) in transform.values.iter().enumerate() {
                if matches!(value.kind, ValueKind::Parameter { .. }) {
                    continue;
                }
                writeln!(
                    source,
                    "    {} v{};",
                    abi::lower_type(value.ty).c_name,
                    value_index,
                )
                .unwrap();
            }
            writeln!(source, "    goto tima_t{index}_b{};", transform.entry.0).unwrap();
            for (block_index, block) in transform.blocks.iter().enumerate() {
                writeln!(source, "tima_t{index}_b{block_index}:").unwrap();
                for id in &block.instructions {
                    writeln!(
                        source,
                        "    v{} = {};",
                        id.0,
                        expression(transform, index, *id)
                    )
                    .unwrap();
                    writeln!(source, "    (void)v{};", id.0).unwrap();
                }
                match block.terminator {
                    Terminator::Return(value) => {
                        writeln!(source, "    return {};", value_name(transform, value)).unwrap();
                    }
                    Terminator::Jump(target) => {
                        writeln!(source, "    goto tima_t{index}_b{};", target.0).unwrap();
                    }
                    Terminator::Branch {
                        condition,
                        then_block,
                        else_block,
                    } => {
                        writeln!(
                            source,
                            "    if ({}) goto tima_t{index}_b{}; else goto tima_t{index}_b{};",
                            value_name(transform, condition),
                            then_block.0,
                            else_block.0,
                        )
                        .unwrap();
                    }
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
        "TIMA_EXPORT int32_t tima_invoke_{index}(TimaRuntime *runtime, const TimaValue *args, TimaValue *result) {{"
    )
    .unwrap();
    output.push_str("    if (runtime == NULL) return -1;\n    runtime->status = 0;\n");
    if transform.parameters.is_empty() {
        output.push_str("    (void)args;\n");
    }
    write!(
        output,
        "    TimaValue output = {{0}};\n    output.{} = tima_transform_{index}(runtime",
        abi_field(transform.return_type),
    )
    .unwrap();
    for (argument, parameter) in transform.parameters.iter().enumerate() {
        write!(output, ", args[{argument}].{}", abi_field(parameter.ty)).unwrap();
    }
    output.push_str(
        ");\n    if (runtime->status != 0) return runtime->status;\n    *result = output;\n    return 0;\n}\n\n",
    );
}

fn abi_field(ty: crate::ir::Type) -> &'static str {
    match ty {
        crate::ir::Type::Bool => "boolean",
        crate::ir::Type::U8 => "u8_value",
        crate::ir::Type::I64 => "i64_value",
        crate::ir::Type::F32 => "f32_value",
        crate::ir::Type::Image => "image",
        crate::ir::Type::ImageView => "image_view",
    }
}

fn prototype(output: &mut String, index: usize, transform: &Transform) {
    write!(
        output,
        "{} tima_transform_{index}(TimaRuntime *runtime",
        abi::lower_type(transform.return_type).c_name,
    )
    .unwrap();
    for (index, parameter) in transform.parameters.iter().enumerate() {
        write!(
            output,
            ", {} p{}",
            abi::lower_type(parameter.ty).c_name,
            index
        )
        .unwrap();
    }
    output.push(')');
}

fn expression(transform: &Transform, transform_index: usize, id: ValueId) -> String {
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
                BinaryOp::Equal => "==",
                BinaryOp::NotEqual => "!=",
                BinaryOp::Less => "<",
                BinaryOp::LessEqual => "<=",
                BinaryOp::Greater => ">",
                BinaryOp::GreaterEqual => ">=",
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
            if arguments.is_empty() {
                format!("tima_transform_{}(runtime)", callee.0)
            } else {
                format!("tima_transform_{}(runtime, {arguments})", callee.0)
            }
        }
        ValueKind::ImageZero { image } => {
            format!("tima_image_zero({})", value_name(transform, *image))
        }
        ValueKind::ImageFill { image, value } => format!(
            "tima_image_fill({}, {})",
            value_name(transform, *image),
            value_name(transform, *value)
        ),
        ValueKind::RuntimeCall(RuntimeCall::EnvironmentI64 { name }) => {
            let bytes = name
                .as_bytes()
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "tima_environment_i64(runtime, {transform_index}, {}, (const unsigned char[]){{{bytes}}}, {})",
                id.0,
                name.len(),
            )
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
                .contains("float tima_transform_0(TimaRuntime *runtime, float p0, float p1)")
        );
        assert!(artifact.source.contains("float v2;"));
        assert!(artifact.source.contains("v2 = (p0 * p1);"));
        assert!(artifact.source.contains("return v2;"));
        assert!(artifact.source.contains(
            "output.f32_value = tima_transform_0(runtime, args[0].f32_value, args[1].f32_value);"
        ));
    }

    #[test]
    fn emits_cfg_branches_as_c_labels_and_gotos() {
        let compiled = crate::compile(
            "test.tima",
            "transform choose(flag: bool, left: f32, right: f32) -> f32 {\n\
                 if flag { return left } else {}\n\
                 return right\n\
             }\n",
        )
        .unwrap();
        let artifact = super::CBackend.emit(&compiled.transforms).unwrap();
        assert!(artifact.source.contains("goto tima_t0_b0;"));
        assert!(
            artifact
                .source
                .contains("if (p0) goto tima_t0_b1; else goto tima_t0_b2;")
        );
        assert!(artifact.source.contains("tima_t0_b1:"));
        assert!(artifact.source.contains("tima_t0_b2:"));
        assert!(artifact.source.contains("goto tima_t0_b3;"));
        assert!(artifact.source.contains("tima_t0_b3:"));
    }

    #[test]
    fn emits_typed_scalar_comparisons() {
        let compiled = crate::compile(
            "test.tima",
            "transform less(left: f32, right: f32) -> bool { return left < right }\n",
        )
        .unwrap();
        let artifact = super::CBackend.emit(&compiled.transforms).unwrap();
        assert!(artifact.source.contains("bool v2;"));
        assert!(artifact.source.contains("v2 = (p0 < p1);"));
        assert!(artifact.source.contains("return v2;"));
    }

    #[test]
    fn emits_owned_image_zero_from_backend_neutral_ir() {
        let compiled = crate::compile(
            "test.tima",
            "transform clear(img: Image) -> Image { return image_zero(img) }\n",
        )
        .unwrap();
        let artifact = super::CBackend.emit(&compiled.transforms).unwrap();
        assert!(
            artifact
                .source
                .contains("static TimaImage tima_image_zero(TimaImage image)")
        );
        assert!(artifact.source.contains("image.data[index] = 0;"));
        assert!(artifact.source.contains("v1 = tima_image_zero(p0);"));
    }

    #[test]
    fn emits_u8_image_fill_from_backend_neutral_ir() {
        let compiled = crate::compile(
            "test.tima",
            "transform fill(img: Image, value: u8) -> Image { return image_fill(img, value) }\n",
        )
        .unwrap();
        let artifact = super::CBackend.emit(&compiled.transforms).unwrap();
        assert!(
            artifact
                .source
                .contains("static TimaImage tima_image_fill(TimaImage image, uint8_t value)")
        );
        assert!(artifact.source.contains("image.data[index] = value;"));
        assert!(artifact.source.contains("v2 = tima_image_fill(p0, p1);"));
        assert!(artifact.source.contains("args[1].u8_value"));
    }

    #[test]
    fn emits_owned_image_moves_through_inner_calls() {
        let compiled = crate::compile(
            "test.tima",
            "transform clear(img: Image) -> Image { return image_zero(img) }\n\
             transform clear_owned(img: Image) -> Image { return clear(img) }\n",
        )
        .unwrap();
        let artifact = super::CBackend.emit(&compiled.transforms).unwrap();
        assert!(
            artifact
                .source
                .contains("TimaImage tima_transform_1(TimaRuntime *runtime, TimaImage p0)")
        );
        assert!(
            artifact
                .source
                .contains("v1 = tima_transform_0(runtime, p0);")
        );
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
                .contains("TimaImage tima_transform_0(TimaRuntime *runtime, TimaImage p0)")
        );
        assert!(
            artifact
                .source
                .contains("TimaImageView tima_transform_1(TimaRuntime *runtime, TimaImageView p0)")
        );
        assert!(
            artifact
                .source
                .contains("output.image = tima_transform_0(runtime, args[0].image);")
        );
        assert!(
            artifact
                .source
                .contains("output.image_view = tima_transform_1(runtime, args[0].image_view);")
        );
    }

    #[test]
    fn emits_environment_reads_through_the_runtime_context() {
        let compiled = crate::compile(
            "test.tima",
            "transform configured() -> i64 { return environment_i64(\"MODE\") }\n",
        )
        .unwrap();
        let artifact = super::CBackend.emit(&compiled.transforms).unwrap();
        assert!(
            artifact
                .source
                .contains("int64_t tima_transform_0(TimaRuntime *runtime)")
        );
        assert!(artifact.source.contains(
            "tima_environment_i64(runtime, 0, 0, (const unsigned char[]){77, 79, 68, 69}, 4)"
        ));
        assert!(artifact.source.contains(
            "TIMA_EXPORT int32_t tima_invoke_0(TimaRuntime *runtime, const TimaValue *args, TimaValue *result)"
        ));
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
