use std::sync::OnceLock;

use wasmi::{
    CompilationMode, Config, EnforcedLimits, Engine, Instance, Memory, Module, Store, StoreLimits,
    StoreLimitsBuilder, TypedFunc,
};

use crate::abi::ABI_IMAGE_FORMAT_RGBA8;
use crate::diagnostic::Diagnostic;
use crate::runtime::ImageValue;
use crate::source::Span;

const PLUGIN_ABI_VERSION: u32 = 3;
const VALUE_WORDS: usize = 8;
const VALUE_BYTES: usize = VALUE_WORDS * size_of::<u32>();
const VALUE_BYTES_VIEW: u32 = 1;
const VALUE_IMAGE_VIEW: u32 = 2;
const VALUE_I64: u32 = 3;
const VALUE_BYTES_RESULT: u32 = 4;
const VALUE_IMAGE_RESULT: u32 = 5;
const VALUE_DIAGNOSTIC: u32 = 255;
const MAX_LINEAR_MEMORY: usize = 64 * 1024 * 1024;
const MAX_ARGUMENT_BYTES: usize = 32 * 1024 * 1024;
const MAX_RESULT_BYTES: usize = 64 * 1024 * 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 4096;
const INVOCATION_FUEL: u64 = 100_000_000;

const PPM_DECODE_WASM: &[u8] = include_bytes!("../../../plugins/ppm-decode/ppm_decode.wasm");
const PPM_ENCODE_WASM: &[u8] = include_bytes!("../../../plugins/ppm-encode/ppm_encode.wasm");
const PNG_DECODE_WASM: &[u8] = include_bytes!("../../../plugins/png-decode/png_decode.wasm");
const PNG_ENCODE_WASM: &[u8] = include_bytes!("../../../plugins/png-encode/png_encode.wasm");

struct PluginState {
    limits: StoreLimits,
}

enum PluginArgument<'a> {
    BytesView(&'a [u8]),
    ImageView(&'a ImageValue),
    I64(i64),
}

#[derive(Clone, Copy)]
enum PluginResultType {
    Bytes,
    Image,
}

enum PluginResult {
    Bytes(Vec<u8>),
    Image(ImageValue),
}

/// One validated, separately compiled registered-Wasm artifact.
///
/// ABI v3 admits only the concrete native-safe types exercised by the current
/// codecs: immutable byte/image views, `i64`, and owned byte/image results.
struct RegisteredWasmPlugin {
    name: &'static str,
    engine: Engine,
    module: Module,
}

impl RegisteredWasmPlugin {
    fn compile(name: &'static str, bytes: &'static [u8]) -> Result<Self, String> {
        let mut configuration = Config::default();
        configuration.consume_fuel(true);
        configuration.set_max_recursion_depth(32);
        configuration.compilation_mode(CompilationMode::Eager);
        configuration.enforced_limits(EnforcedLimits::strict());
        let engine = Engine::new(&configuration);
        let module = Module::new(&engine, bytes).map_err(|error| error.to_string())?;
        if let Some(import) = module.imports().next() {
            return Err(format!(
                "registered Wasm modules may not import `{}` from `{}`",
                import.name(),
                import.module()
            ));
        }
        Ok(Self {
            name,
            engine,
            module,
        })
    }

    fn invoke(
        &self,
        arguments: &[PluginArgument<'_>],
        result_type: PluginResultType,
        span: Span,
    ) -> Result<PluginResult, Diagnostic> {
        let limits = StoreLimitsBuilder::new()
            .memory_size(MAX_LINEAR_MEMORY)
            .build();
        let mut store = Store::new(&self.engine, PluginState { limits });
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(INVOCATION_FUEL)
            .map_err(|error| self.diagnostic(span, error))?;
        let instance = Instance::new(&mut store, &self.module, &[])
            .map_err(|error| self.diagnostic(span, format!("could not instantiate: {error}")))?;
        let memory = instance
            .get_memory(&store, "memory")
            .ok_or_else(|| self.diagnostic(span, "does not export `memory`"))?;
        self.validate_abi(&instance, &mut store, span)?;
        let reset = instance
            .get_typed_func::<(), ()>(&store, "tima_reset")
            .map_err(|error| self.diagnostic(span, format!("invalid `tima_reset`: {error}")))?;
        let allocate = instance
            .get_typed_func::<i32, i32>(&store, "tima_alloc")
            .map_err(|error| self.diagnostic(span, format!("invalid `tima_alloc`: {error}")))?;
        let transform = instance
            .get_typed_func::<(i32, i32, i32), i32>(&store, "tima_transform")
            .map_err(|error| self.diagnostic(span, format!("invalid `tima_transform`: {error}")))?;

        reset
            .call(&mut store, ())
            .map_err(|error| self.diagnostic(span, format!("reset trapped: {error}")))?;
        let descriptors_length = arguments
            .len()
            .checked_mul(VALUE_BYTES)
            .ok_or_else(|| self.diagnostic(span, "argument descriptor length overflowed"))?;
        let descriptors_pointer = self.allocate(
            &allocate,
            &mut store,
            descriptors_length,
            "argument descriptors",
            span,
        )?;
        let result_pointer = self.allocate(
            &allocate,
            &mut store,
            VALUE_BYTES,
            "result descriptor",
            span,
        )?;

        let mut descriptors = Vec::with_capacity(arguments.len());
        for argument in arguments {
            descriptors.push(self.stage_argument(argument, memory, &allocate, &mut store, span)?);
        }
        let descriptor_bytes = encode_values(&descriptors);
        memory
            .write(&mut store, descriptors_pointer as usize, &descriptor_bytes)
            .map_err(|error| {
                self.diagnostic(
                    span,
                    format!("could not copy argument descriptors: {error}"),
                )
            })?;

        let argument_count = i32::try_from(arguments.len())
            .map_err(|_| self.diagnostic(span, "argument count exceeds the Wasm32 ABI"))?;
        let status = transform
            .call(
                &mut store,
                (descriptors_pointer, argument_count, result_pointer),
            )
            .map_err(|error| self.diagnostic(span, format!("execution trapped: {error}")))?;
        if status != 0 {
            return Err(self.diagnostic(span, format!("returned unsupported status {status}")));
        }

        let result = read_value(memory, &store, result_pointer as usize).map_err(|error| {
            self.diagnostic(span, format!("invalid result descriptor: {error}"))
        })?;
        if result[0] == VALUE_DIAGNOSTIC {
            return Err(self.read_guest_diagnostic(memory, &store, result, span));
        }
        match result_type {
            PluginResultType::Bytes if result[0] == VALUE_BYTES_RESULT => self
                .read_bytes_result(memory, &store, result, span)
                .map(PluginResult::Bytes),
            PluginResultType::Image if result[0] == VALUE_IMAGE_RESULT => self
                .read_image_result(memory, &store, result, span)
                .map(PluginResult::Image),
            PluginResultType::Bytes => Err(self.diagnostic(
                span,
                format!("returned value kind {} instead of owned Bytes", result[0]),
            )),
            PluginResultType::Image => Err(self.diagnostic(
                span,
                format!("returned value kind {} instead of owned Image", result[0]),
            )),
        }
    }

    fn validate_abi(
        &self,
        instance: &Instance,
        store: &mut Store<PluginState>,
        span: Span,
    ) -> Result<(), Diagnostic> {
        let version = instance
            .get_typed_func::<(), i32>(&*store, "tima_abi_version")
            .map_err(|error| self.diagnostic(span, format!("invalid `tima_abi_version`: {error}")))?
            .call(store, ())
            .map_err(|error| self.diagnostic(span, format!("ABI query trapped: {error}")))?;
        if version == PLUGIN_ABI_VERSION as i32 {
            Ok(())
        } else {
            Err(self.diagnostic(
                span,
                format!("uses ABI version {version}; this runtime requires {PLUGIN_ABI_VERSION}"),
            ))
        }
    }

    fn stage_argument(
        &self,
        argument: &PluginArgument<'_>,
        memory: Memory,
        allocate: &TypedFunc<i32, i32>,
        store: &mut Store<PluginState>,
        span: Span,
    ) -> Result<[u32; VALUE_WORDS], Diagnostic> {
        match argument {
            PluginArgument::BytesView(bytes) => {
                let pointer = self.stage_bytes(bytes, memory, allocate, store, span)?;
                Ok([
                    VALUE_BYTES_VIEW,
                    pointer as u32,
                    bytes.len() as u32,
                    0,
                    0,
                    0,
                    0,
                    0,
                ])
            }
            PluginArgument::ImageView(image) => {
                let byte_length = image.byte_len();
                let pointer = image
                    .with_bytes(|bytes| self.stage_bytes(bytes, memory, allocate, store, span))?;
                Ok([
                    VALUE_IMAGE_VIEW,
                    pointer as u32,
                    u32_field(self, byte_length, "image byte length", span)?,
                    image.format().abi_tag(),
                    u32_field(self, image.width(), "image width", span)?,
                    u32_field(self, image.height(), "image height", span)?,
                    u32_field(self, image.stride(), "image stride", span)?,
                    0,
                ])
            }
            PluginArgument::I64(value) => {
                let bytes = value.to_le_bytes();
                Ok([
                    VALUE_I64,
                    u32::from_le_bytes(bytes[..4].try_into().expect("low i64 word")),
                    u32::from_le_bytes(bytes[4..].try_into().expect("high i64 word")),
                    0,
                    0,
                    0,
                    0,
                    0,
                ])
            }
        }
    }

    fn stage_bytes(
        &self,
        bytes: &[u8],
        memory: Memory,
        allocate: &TypedFunc<i32, i32>,
        store: &mut Store<PluginState>,
        span: Span,
    ) -> Result<i32, Diagnostic> {
        if bytes.len() > MAX_ARGUMENT_BYTES {
            return Err(self.diagnostic(
                span,
                format!(
                    "argument is {} bytes; ABI v3 permits at most {MAX_ARGUMENT_BYTES}",
                    bytes.len()
                ),
            ));
        }
        let pointer = self.allocate(allocate, store, bytes.len(), "argument payload", span)?;
        memory
            .write(&mut *store, pointer as usize, bytes)
            .map_err(|error| self.diagnostic(span, format!("could not copy argument: {error}")))?;
        Ok(pointer)
    }

    fn allocate(
        &self,
        allocate: &TypedFunc<i32, i32>,
        store: &mut Store<PluginState>,
        length: usize,
        purpose: &str,
        span: Span,
    ) -> Result<i32, Diagnostic> {
        let length = i32::try_from(length)
            .map_err(|_| self.diagnostic(span, format!("{purpose} exceeds the Wasm32 ABI")))?;
        let pointer = allocate.call(store, length).map_err(|error| {
            self.diagnostic(span, format!("{purpose} allocation trapped: {error}"))
        })?;
        if pointer <= 0 {
            Err(self.diagnostic(span, format!("{purpose} allocation failed")))
        } else {
            Ok(pointer)
        }
    }

    fn read_bytes_result(
        &self,
        memory: Memory,
        store: &Store<PluginState>,
        words: [u32; VALUE_WORDS],
        span: Span,
    ) -> Result<Vec<u8>, Diagnostic> {
        if words[3..].iter().any(|word| *word != 0) {
            return Err(self.diagnostic(span, "owned Bytes result has non-zero reserved words"));
        }
        self.read_bounded_bytes(memory, store, words[1], words[2], MAX_RESULT_BYTES, span)
    }

    fn read_image_result(
        &self,
        memory: Memory,
        store: &Store<PluginState>,
        words: [u32; VALUE_WORDS],
        span: Span,
    ) -> Result<ImageValue, Diagnostic> {
        if words[7] != 0 {
            return Err(self.diagnostic(span, "owned Image result has a non-zero reserved word"));
        }
        if words[3] != ABI_IMAGE_FORMAT_RGBA8 {
            return Err(self.diagnostic(
                span,
                format!("returned unsupported image format {}", words[3]),
            ));
        }
        let pixels =
            self.read_bounded_bytes(memory, store, words[1], words[2], MAX_RESULT_BYTES, span)?;
        ImageValue::new_rgba8(
            words[4] as usize,
            words[5] as usize,
            words[6] as usize,
            pixels,
        )
        .map_err(|error| self.diagnostic(span, format!("invalid image metadata: {error}")))
    }

    fn read_guest_diagnostic(
        &self,
        memory: Memory,
        store: &Store<PluginState>,
        words: [u32; VALUE_WORDS],
        span: Span,
    ) -> Diagnostic {
        if words[3..].iter().any(|word| *word != 0) {
            return self.diagnostic(span, "diagnostic has non-zero reserved words");
        }
        match self.read_bounded_bytes(
            memory,
            store,
            words[1],
            words[2],
            MAX_DIAGNOSTIC_BYTES,
            span,
        ) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(message) => self.diagnostic(span, message),
                Err(_) => self.diagnostic(span, "diagnostic is not UTF-8"),
            },
            Err(diagnostic) => diagnostic,
        }
    }

    fn read_bounded_bytes(
        &self,
        memory: Memory,
        store: &Store<PluginState>,
        pointer: u32,
        length: u32,
        maximum: usize,
        span: Span,
    ) -> Result<Vec<u8>, Diagnostic> {
        let length = length as usize;
        if length > maximum {
            return Err(self.diagnostic(
                span,
                format!("result is {length} bytes; ABI v3 permits at most {maximum}"),
            ));
        }
        let mut bytes = vec![0; length];
        memory
            .read(store, pointer as usize, &mut bytes)
            .map_err(|error| self.diagnostic(span, format!("invalid result bytes: {error}")))?;
        Ok(bytes)
    }

    fn diagnostic(&self, span: Span, detail: impl std::fmt::Display) -> Diagnostic {
        Diagnostic::error(format!("{} Wasm plugin: {detail}", self.name), span)
    }
}

fn encode_values(values: &[[u32; VALUE_WORDS]]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(values.len() * VALUE_BYTES);
    for value in values {
        for word in value {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
    }
    bytes
}

fn read_value(
    memory: Memory,
    store: &Store<PluginState>,
    pointer: usize,
) -> Result<[u32; VALUE_WORDS], wasmi::errors::MemoryError> {
    let mut encoded = [0_u8; VALUE_BYTES];
    memory.read(store, pointer, &mut encoded)?;
    let mut words = [0_u32; VALUE_WORDS];
    let (word_bytes, remainder) = encoded.as_chunks::<4>();
    debug_assert!(remainder.is_empty());
    for (word, bytes) in words.iter_mut().zip(word_bytes) {
        *word = u32::from_le_bytes(*bytes);
    }
    Ok(words)
}

fn u32_field(
    plugin: &RegisteredWasmPlugin,
    value: usize,
    field: &str,
    span: Span,
) -> Result<u32, Diagnostic> {
    u32::try_from(value).map_err(|_| plugin.diagnostic(span, format!("{field} exceeds Wasm32")))
}

static PPM_DECODER: OnceLock<Result<RegisteredWasmPlugin, String>> = OnceLock::new();
static PPM_ENCODER: OnceLock<Result<RegisteredWasmPlugin, String>> = OnceLock::new();
static PNG_DECODER: OnceLock<Result<RegisteredWasmPlugin, String>> = OnceLock::new();
static PNG_ENCODER: OnceLock<Result<RegisteredWasmPlugin, String>> = OnceLock::new();

fn ppm_decoder() -> Result<&'static RegisteredWasmPlugin, &'static str> {
    PPM_DECODER
        .get_or_init(|| RegisteredWasmPlugin::compile("ppm.decode", PPM_DECODE_WASM))
        .as_ref()
        .map_err(String::as_str)
}

fn ppm_encoder() -> Result<&'static RegisteredWasmPlugin, &'static str> {
    PPM_ENCODER
        .get_or_init(|| RegisteredWasmPlugin::compile("ppm.encode", PPM_ENCODE_WASM))
        .as_ref()
        .map_err(String::as_str)
}

fn png_encoder() -> Result<&'static RegisteredWasmPlugin, &'static str> {
    PNG_ENCODER
        .get_or_init(|| RegisteredWasmPlugin::compile("png.encode", PNG_ENCODE_WASM))
        .as_ref()
        .map_err(String::as_str)
}

fn png_decoder() -> Result<&'static RegisteredWasmPlugin, &'static str> {
    PNG_DECODER
        .get_or_init(|| RegisteredWasmPlugin::compile("png.decode", PNG_DECODE_WASM))
        .as_ref()
        .map_err(String::as_str)
}

pub(crate) fn decode_ppm(input: &[u8], span: Span) -> Result<ImageValue, Diagnostic> {
    let plugin = ppm_decoder()
        .map_err(|error| Diagnostic::error(format!("ppm.decode Wasm plugin: {error}"), span))?;
    match plugin.invoke(
        &[PluginArgument::BytesView(input)],
        PluginResultType::Image,
        span,
    )? {
        PluginResult::Image(image) => Ok(image),
        PluginResult::Bytes(_) => unreachable!(),
    }
}

pub(crate) fn encode_ppm(image: &ImageValue, span: Span) -> Result<Vec<u8>, Diagnostic> {
    let plugin = ppm_encoder()
        .map_err(|error| Diagnostic::error(format!("ppm.encode Wasm plugin: {error}"), span))?;
    match plugin.invoke(
        &[PluginArgument::ImageView(image)],
        PluginResultType::Bytes,
        span,
    )? {
        PluginResult::Bytes(bytes) => Ok(bytes),
        PluginResult::Image(_) => unreachable!(),
    }
}

pub(crate) fn encode_png(
    image: &ImageValue,
    compression: i64,
    span: Span,
) -> Result<Vec<u8>, Diagnostic> {
    let plugin = png_encoder()
        .map_err(|error| Diagnostic::error(format!("png.encode Wasm plugin: {error}"), span))?;
    match plugin.invoke(
        &[
            PluginArgument::ImageView(image),
            PluginArgument::I64(compression),
        ],
        PluginResultType::Bytes,
        span,
    )? {
        PluginResult::Bytes(bytes) => Ok(bytes),
        PluginResult::Image(_) => unreachable!(),
    }
}

pub(crate) fn decode_png(input: &[u8], span: Span) -> Result<ImageValue, Diagnostic> {
    let plugin = png_decoder()
        .map_err(|error| Diagnostic::error(format!("png.decode Wasm plugin: {error}"), span))?;
    match plugin.invoke(
        &[PluginArgument::BytesView(input)],
        PluginResultType::Image,
        span,
    )? {
        PluginResult::Image(image) => Ok(image),
        PluginResult::Bytes(_) => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{registered_transform_identity, registered_wasm_artifact_identity};

    #[test]
    fn embedded_plugins_have_no_imports_and_module_sensitive_artifacts() {
        let decoder = ppm_decoder().unwrap();
        let encoder = ppm_encoder().unwrap();
        let png_decoder = png_decoder().unwrap();
        let png_encoder = png_encoder().unwrap();
        assert_eq!(decoder.module.imports().count(), 0);
        assert_eq!(encoder.module.imports().count(), 0);
        assert_eq!(png_decoder.module.imports().count(), 0);
        assert_eq!(png_encoder.module.imports().count(), 0);

        let semantic = registered_transform_identity("ppm.decode", 2);
        let artifact =
            registered_wasm_artifact_identity(semantic, PPM_DECODE_WASM, PLUGIN_ABI_VERSION);
        let changed =
            registered_wasm_artifact_identity(semantic, PPM_ENCODE_WASM, PLUGIN_ABI_VERSION);
        assert_ne!(artifact, changed);
    }

    #[test]
    fn embedded_ppm_plugins_round_trip_comments_and_padded_pixels() {
        let decoded = decode_ppm(
            b"P3\n# generated fixture\n2 1\n255\n1 2 3 4 5 6\n",
            Span::default(),
        )
        .unwrap();
        assert_eq!(decoded.width(), 2);
        assert_eq!(decoded.height(), 1);
        assert_eq!(decoded.stride(), 8);
        assert_eq!(decoded.bytes(), &[1, 2, 3, 255, 4, 5, 6, 255]);

        let padded =
            ImageValue::new_rgba8(1, 2, 6, vec![1, 2, 3, 4, 99, 100, 4, 5, 6, 7, 101, 102])
                .unwrap();
        assert_eq!(
            encode_ppm(&padded, Span::default()).unwrap(),
            b"P3\n1 2\n255\n1 2 3\n4 5 6\n"
        );
    }
}
