use std::sync::OnceLock;

use wasmi::{
    Config, EnforcedLimits, Engine, Instance, Module, Store, StoreLimits, StoreLimitsBuilder,
};

use crate::abi::ABI_IMAGE_FORMAT_RGBA8;
use crate::diagnostic::Diagnostic;
use crate::runtime::ImageValue;
use crate::source::Span;

const PLUGIN_ABI_VERSION: u32 = 1;
const RESULT_WORDS: usize = 8;
const RESULT_BYTES: usize = RESULT_WORDS * size_of::<u32>();
const RESULT_IMAGE: u32 = 0;
const RESULT_DIAGNOSTIC: u32 = 1;
const MAX_LINEAR_MEMORY: usize = 64 * 1024 * 1024;
const MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_RESULT_BYTES: usize = 64 * 1024 * 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 4096;
const INVOCATION_FUEL: u64 = 100_000_000;

const PPM_DECODE_WASM: &[u8] = include_bytes!("../../../plugins/ppm-decode/ppm_decode.wasm");

struct PluginState {
    limits: StoreLimits,
}

/// One validated, separately compiled registered-Wasm artifact.
///
/// This is deliberately narrower than a general plugin framework. ABI v1 is
/// exercised by the concrete `BytesView -> Image` PPM decoder before more
/// signatures are admitted.
struct RegisteredWasmPlugin {
    engine: Engine,
    module: Module,
}

impl RegisteredWasmPlugin {
    fn compile(bytes: &'static [u8]) -> Result<Self, String> {
        let mut configuration = Config::default();
        configuration.consume_fuel(true);
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
        Ok(Self { engine, module })
    }

    fn bytes_to_image(&self, input: &[u8], span: Span) -> Result<ImageValue, Diagnostic> {
        if input.len() > MAX_INPUT_BYTES {
            return Err(plugin_diagnostic(
                span,
                format!(
                    "input is {} bytes; ABI v1 permits at most {MAX_INPUT_BYTES}",
                    input.len()
                ),
            ));
        }

        let limits = StoreLimitsBuilder::new()
            .memory_size(MAX_LINEAR_MEMORY)
            .build();
        let mut store = Store::new(&self.engine, PluginState { limits });
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(INVOCATION_FUEL)
            .map_err(|error| plugin_diagnostic(span, error.to_string()))?;
        let instance = Instance::new(&mut store, &self.module, &[])
            .map_err(|error| plugin_diagnostic(span, format!("could not instantiate: {error}")))?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| plugin_diagnostic(span, "does not export `memory`"))?;
        let abi_version = instance
            .get_typed_func::<(), i32>(&mut store, "tima_abi_version")
            .map_err(|error| {
                plugin_diagnostic(span, format!("invalid `tima_abi_version`: {error}"))
            })?
            .call(&mut store, ())
            .map_err(|error| plugin_diagnostic(span, format!("ABI query trapped: {error}")))?;
        if abi_version != PLUGIN_ABI_VERSION as i32 {
            return Err(plugin_diagnostic(
                span,
                format!(
                    "uses ABI version {abi_version}; this runtime requires {PLUGIN_ABI_VERSION}"
                ),
            ));
        }
        let reset = instance
            .get_typed_func::<(), ()>(&mut store, "tima_reset")
            .map_err(|error| plugin_diagnostic(span, format!("invalid `tima_reset`: {error}")))?;
        let allocate = instance
            .get_typed_func::<i32, i32>(&mut store, "tima_alloc")
            .map_err(|error| plugin_diagnostic(span, format!("invalid `tima_alloc`: {error}")))?;
        let transform = instance
            .get_typed_func::<(i32, i32, i32), i32>(&mut store, "tima_transform")
            .map_err(|error| {
                plugin_diagnostic(span, format!("invalid `tima_transform`: {error}"))
            })?;

        reset
            .call(&mut store, ())
            .map_err(|error| plugin_diagnostic(span, format!("reset trapped: {error}")))?;
        let input_length = i32::try_from(input.len())
            .map_err(|_| plugin_diagnostic(span, "input length exceeds the Wasm32 ABI"))?;
        let input_pointer = allocate.call(&mut store, input_length).map_err(|error| {
            plugin_diagnostic(span, format!("input allocation trapped: {error}"))
        })?;
        let result_pointer = allocate
            .call(&mut store, RESULT_BYTES as i32)
            .map_err(|error| {
                plugin_diagnostic(span, format!("result allocation trapped: {error}"))
            })?;
        if input_pointer <= 0 || result_pointer <= 0 {
            return Err(plugin_diagnostic(span, "linear-memory allocation failed"));
        }
        memory
            .write(&mut store, input_pointer as usize, input)
            .map_err(|error| plugin_diagnostic(span, format!("could not copy input: {error}")))?;
        let status = transform
            .call(&mut store, (input_pointer, input_length, result_pointer))
            .map_err(|error| plugin_diagnostic(span, format!("execution trapped: {error}")))?;
        if status != 0 {
            return Err(plugin_diagnostic(
                span,
                format!("returned unsupported status {status}"),
            ));
        }

        let mut encoded = [0_u8; RESULT_BYTES];
        memory
            .read(&store, result_pointer as usize, &mut encoded)
            .map_err(|error| {
                plugin_diagnostic(span, format!("invalid result descriptor: {error}"))
            })?;
        let mut words = [0_u32; RESULT_WORDS];
        let (word_bytes, remainder) = encoded.as_chunks::<4>();
        debug_assert!(remainder.is_empty());
        for (word, bytes) in words.iter_mut().zip(word_bytes) {
            *word = u32::from_le_bytes(*bytes);
        }

        match words[0] {
            RESULT_IMAGE => read_image_result(memory, &store, words, span),
            RESULT_DIAGNOSTIC => Err(read_guest_diagnostic(memory, &store, words, span)),
            kind => Err(plugin_diagnostic(
                span,
                format!("returned unknown result kind {kind}"),
            )),
        }
    }
}

fn read_image_result(
    memory: wasmi::Memory,
    store: &Store<PluginState>,
    words: [u32; RESULT_WORDS],
    span: Span,
) -> Result<ImageValue, Diagnostic> {
    let byte_length = words[2] as usize;
    if byte_length > MAX_RESULT_BYTES {
        return Err(plugin_diagnostic(
            span,
            format!("result is {byte_length} bytes; ABI v1 permits at most {MAX_RESULT_BYTES}"),
        ));
    }
    if words[3] != ABI_IMAGE_FORMAT_RGBA8 {
        return Err(plugin_diagnostic(
            span,
            format!("returned unsupported image format {}", words[3]),
        ));
    }
    let width = words[4] as usize;
    let height = words[5] as usize;
    let stride = words[6] as usize;
    let mut pixels = vec![0; byte_length];
    memory
        .read(store, words[1] as usize, &mut pixels)
        .map_err(|error| plugin_diagnostic(span, format!("invalid result pixels: {error}")))?;
    ImageValue::new_rgba8(width, height, stride, pixels)
        .map_err(|error| plugin_diagnostic(span, format!("invalid image metadata: {error}")))
}

fn read_guest_diagnostic(
    memory: wasmi::Memory,
    store: &Store<PluginState>,
    words: [u32; RESULT_WORDS],
    span: Span,
) -> Diagnostic {
    let length = words[2] as usize;
    if length > MAX_DIAGNOSTIC_BYTES {
        return plugin_diagnostic(
            span,
            format!("diagnostic is {length} bytes; ABI v1 permits at most {MAX_DIAGNOSTIC_BYTES}"),
        );
    }
    let mut bytes = vec![0; length];
    if let Err(error) = memory.read(store, words[1] as usize, &mut bytes) {
        return plugin_diagnostic(span, format!("invalid diagnostic bytes: {error}"));
    }
    match String::from_utf8(bytes) {
        Ok(message) => plugin_diagnostic(span, message),
        Err(_) => plugin_diagnostic(span, "diagnostic is not UTF-8"),
    }
}

fn plugin_diagnostic(span: Span, detail: impl std::fmt::Display) -> Diagnostic {
    Diagnostic::error(format!("ppm.decode Wasm plugin: {detail}"), span)
}

static PPM_DECODER: OnceLock<Result<RegisteredWasmPlugin, String>> = OnceLock::new();

fn ppm_decoder() -> Result<&'static RegisteredWasmPlugin, &'static str> {
    PPM_DECODER
        .get_or_init(|| RegisteredWasmPlugin::compile(PPM_DECODE_WASM))
        .as_ref()
        .map_err(String::as_str)
}

pub(crate) fn decode_ppm(input: &[u8], span: Span) -> Result<ImageValue, Diagnostic> {
    let plugin = ppm_decoder().map_err(|error| plugin_diagnostic(span, error))?;
    plugin.bytes_to_image(input, span)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{registered_transform_identity, registered_wasm_artifact_identity};

    #[test]
    fn embedded_ppm_plugin_has_no_imports_and_a_distinct_artifact_identity() {
        let plugin = ppm_decoder().unwrap();
        assert_eq!(plugin.module.imports().count(), 0);

        let semantic = registered_transform_identity("ppm.decode", 2);
        let artifact =
            registered_wasm_artifact_identity(semantic, PPM_DECODE_WASM, PLUGIN_ABI_VERSION);
        assert_ne!(artifact.as_bytes(), semantic.as_bytes());
    }

    #[test]
    fn embedded_ppm_plugin_decodes_comments_and_freezes_host_pixels() {
        let image = decode_ppm(
            b"P3\n# generated fixture\n2 1\n255\n1 2 3 4 5 6\n",
            Span::default(),
        )
        .unwrap();

        assert_eq!(image.width(), 2);
        assert_eq!(image.height(), 1);
        assert_eq!(image.stride(), 8);
        assert_eq!(image.bytes(), &[1, 2, 3, 255, 4, 5, 6, 255]);
    }
}
