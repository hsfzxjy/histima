use std::sync::Arc;

use crate::diagnostic::Diagnostic;
use crate::identity::{TransformIdentity, registered_transform_identity};
use crate::runtime::{ImageFormat, ImageValue, OuterValue, ValueData};
use crate::source::Span;

const PPM_DECODE_TRANSFORM_VERSION: u32 = 2;
const PPM_ENCODE_TRANSFORM_VERSION: u32 = 2;
const PNG_DECODE_TRANSFORM_VERSION: u32 = 2;
const PNG_ENCODE_TRANSFORM_VERSION: u32 = 3;
const PNG_DEFAULT_COMPRESSION: i64 = 6;
const WEBP_ENCODE_TRANSFORM_VERSION: u32 = 2;
const WEBP_DEFAULT_QUALITY: i64 = 85;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuiltinDefaultValue {
    Integer(i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuiltinValueType {
    Bytes,
    Rgba8Image,
    I64,
}

impl BuiltinValueType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bytes => "bytes",
            Self::Rgba8Image => "rgba8-image",
            Self::I64 => "i64",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuiltinParameterInfo {
    pub name: &'static str,
    pub value_type: BuiltinValueType,
    pub default: Option<BuiltinDefaultValue>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuiltinTransformInfo {
    pub name: &'static str,
    pub semantic_version: u32,
    pub parameters: &'static [BuiltinParameterInfo],
    pub result: BuiltinValueType,
    pub transform_id: TransformIdentity,
}

impl BuiltinTransformInfo {
    pub fn signature(self) -> String {
        let parameters = self
            .parameters
            .iter()
            .map(|parameter| {
                let value_type = parameter.value_type.as_str();
                match parameter.default {
                    Some(BuiltinDefaultValue::Integer(value)) => {
                        format!("{}: {value_type} = {value}", parameter.name)
                    }
                    None => format!("{}: {value_type}", parameter.name),
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("{}({parameters}) -> {}", self.name, self.result.as_str())
    }
}

type Validator = fn(&[(OuterValue, Span)], Span) -> Result<(), Diagnostic>;
type Executor = fn(&[(OuterValue, Span)], Span) -> Result<OuterValue, Diagnostic>;

/// A deterministic transform supplied by Tima's standard registry rather than
/// authored as inner Tima code. The runtime treats descriptors uniformly for
/// calls, canonical defaults, identity, caching, lineage, and replay.
#[derive(Clone, Copy)]
pub(crate) struct RegisteredTransform {
    name: &'static str,
    semantic_version: u32,
    parameters: &'static [BuiltinParameterInfo],
    result: BuiltinValueType,
    validate: Validator,
    execute: Executor,
}

const BYTES_PARAMETER: BuiltinParameterInfo = BuiltinParameterInfo {
    name: "bytes",
    value_type: BuiltinValueType::Bytes,
    default: None,
};
const IMAGE_PARAMETER: BuiltinParameterInfo = BuiltinParameterInfo {
    name: "image",
    value_type: BuiltinValueType::Rgba8Image,
    default: None,
};
const COMPRESSION_PARAMETER: BuiltinParameterInfo = BuiltinParameterInfo {
    name: "compression",
    value_type: BuiltinValueType::I64,
    default: Some(BuiltinDefaultValue::Integer(PNG_DEFAULT_COMPRESSION)),
};
const QUALITY_PARAMETER: BuiltinParameterInfo = BuiltinParameterInfo {
    name: "quality",
    value_type: BuiltinValueType::I64,
    default: Some(BuiltinDefaultValue::Integer(WEBP_DEFAULT_QUALITY)),
};

const REGISTERED_TRANSFORMS: &[RegisteredTransform] = &[
    RegisteredTransform {
        name: "ppm.decode",
        semantic_version: PPM_DECODE_TRANSFORM_VERSION,
        parameters: &[BYTES_PARAMETER],
        result: BuiltinValueType::Rgba8Image,
        validate: validate_bytes,
        execute: execute_decode_ppm,
    },
    RegisteredTransform {
        name: "ppm.encode",
        semantic_version: PPM_ENCODE_TRANSFORM_VERSION,
        parameters: &[IMAGE_PARAMETER],
        result: BuiltinValueType::Bytes,
        validate: validate_rgba8,
        execute: execute_encode_ppm,
    },
    RegisteredTransform {
        name: "png.decode",
        semantic_version: PNG_DECODE_TRANSFORM_VERSION,
        parameters: &[BYTES_PARAMETER],
        result: BuiltinValueType::Rgba8Image,
        validate: validate_bytes,
        execute: execute_decode_png,
    },
    RegisteredTransform {
        name: "png.encode",
        semantic_version: PNG_ENCODE_TRANSFORM_VERSION,
        parameters: &[IMAGE_PARAMETER, COMPRESSION_PARAMETER],
        result: BuiltinValueType::Bytes,
        validate: validate_png_encode,
        execute: execute_encode_png,
    },
    RegisteredTransform {
        name: "webp.encode",
        semantic_version: WEBP_ENCODE_TRANSFORM_VERSION,
        parameters: &[IMAGE_PARAMETER, QUALITY_PARAMETER],
        result: BuiltinValueType::Bytes,
        validate: validate_webp_encode,
        execute: execute_encode_webp,
    },
];

impl RegisteredTransform {
    pub(crate) fn find(name: &str) -> Option<&'static Self> {
        REGISTERED_TRANSFORMS
            .iter()
            .find(|transform| transform.name == name)
    }

    pub(crate) fn from_identity(identity: TransformIdentity) -> Option<&'static Self> {
        REGISTERED_TRANSFORMS
            .iter()
            .find(|transform| transform.identity() == identity)
    }

    pub(crate) const fn name(&self) -> &'static str {
        self.name
    }

    pub(crate) const fn parameters(&self) -> &'static [BuiltinParameterInfo] {
        self.parameters
    }

    pub(crate) fn default_argument(&self, index: usize) -> Option<OuterValue> {
        match self.parameters.get(index)?.default? {
            BuiltinDefaultValue::Integer(value) => {
                Some(OuterValue::plain(ValueData::Integer(value)))
            }
        }
    }

    pub(crate) fn identity(&self) -> TransformIdentity {
        registered_transform_identity(self.name, self.semantic_version)
    }

    fn info(&self) -> BuiltinTransformInfo {
        BuiltinTransformInfo {
            name: self.name,
            semantic_version: self.semantic_version,
            parameters: self.parameters,
            result: self.result,
            transform_id: self.identity(),
        }
    }

    pub(crate) fn infos() -> impl Iterator<Item = BuiltinTransformInfo> {
        REGISTERED_TRANSFORMS.iter().map(Self::info)
    }
}

pub(crate) struct PreparedRegisteredInvocation {
    pub(crate) arguments: Vec<(OuterValue, Span)>,
    transform: &'static RegisteredTransform,
    call_span: Span,
}

impl PreparedRegisteredInvocation {
    pub(crate) fn execute(self) -> Result<OuterValue, Diagnostic> {
        (self.transform.execute)(&self.arguments, self.call_span)
    }
}

pub(crate) fn prepare_registered_invocation(
    transform: &'static RegisteredTransform,
    arguments: Vec<(OuterValue, Span)>,
    call_span: Span,
) -> Result<PreparedRegisteredInvocation, Diagnostic> {
    if arguments.len() != transform.parameters.len() {
        return Err(Diagnostic::error(
            format!(
                "{} expects exactly {} arguments",
                transform.name,
                transform.parameters.len()
            ),
            call_span,
        ));
    }
    (transform.validate)(&arguments, call_span)?;
    Ok(PreparedRegisteredInvocation {
        arguments,
        transform,
        call_span,
    })
}

fn validate_bytes(arguments: &[(OuterValue, Span)], _span: Span) -> Result<(), Diagnostic> {
    if matches!(arguments[0].0.data, ValueData::Bytes(_)) {
        Ok(())
    } else {
        Err(Diagnostic::error(
            "decoder expects immutable bytes; materialize an asset with `read` first",
            arguments[0].1,
        ))
    }
}

fn validate_rgba8(arguments: &[(OuterValue, Span)], _span: Span) -> Result<(), Diagnostic> {
    let ValueData::Image(image) = &arguments[0].0.data else {
        return Err(Diagnostic::error(
            "encoder expects an image value",
            arguments[0].1,
        ));
    };
    if image.format() != ImageFormat::Rgba8 {
        return Err(Diagnostic::error(
            "encoder requires an RGBA8 image",
            arguments[0].1,
        ));
    }
    Ok(())
}

fn validate_png_encode(arguments: &[(OuterValue, Span)], span: Span) -> Result<(), Diagnostic> {
    validate_rgba8(arguments, span)?;
    let ValueData::Integer(compression) = arguments[1].0.data else {
        return Err(Diagnostic::error(
            "png.encode compression must be an integer from 1 through 9",
            arguments[1].1,
        ));
    };
    if !(1..=9).contains(&compression) {
        return Err(Diagnostic::error(
            "png.encode compression must be an integer from 1 through 9",
            arguments[1].1,
        ));
    }
    Ok(())
}

fn validate_webp_encode(arguments: &[(OuterValue, Span)], span: Span) -> Result<(), Diagnostic> {
    validate_rgba8(arguments, span)?;
    let ValueData::Integer(quality) = arguments[1].0.data else {
        return Err(Diagnostic::error(
            "webp.encode quality must be an integer from 0 through 100",
            arguments[1].1,
        ));
    };
    if !(0..=100).contains(&quality) {
        return Err(Diagnostic::error(
            "webp.encode quality must be an integer from 0 through 100",
            arguments[1].1,
        ));
    }
    Ok(())
}

fn execute_decode_ppm(
    arguments: &[(OuterValue, Span)],
    _span: Span,
) -> Result<OuterValue, Diagnostic> {
    let ValueData::Bytes(bytes) = &arguments[0].0.data else {
        unreachable!()
    };
    decode_ppm(bytes, arguments[0].1).map(OuterValue::image)
}

fn execute_encode_ppm(
    arguments: &[(OuterValue, Span)],
    _span: Span,
) -> Result<OuterValue, Diagnostic> {
    let ValueData::Image(image) = &arguments[0].0.data else {
        unreachable!()
    };
    encode_ppm(image, arguments[0].1)
        .map(|bytes| OuterValue::plain(ValueData::Bytes(Arc::new(bytes))))
}

fn execute_decode_png(
    arguments: &[(OuterValue, Span)],
    _span: Span,
) -> Result<OuterValue, Diagnostic> {
    let ValueData::Bytes(bytes) = &arguments[0].0.data else {
        unreachable!()
    };
    decode_png(bytes, arguments[0].1).map(OuterValue::image)
}

fn execute_encode_png(
    arguments: &[(OuterValue, Span)],
    _span: Span,
) -> Result<OuterValue, Diagnostic> {
    let ValueData::Image(image) = &arguments[0].0.data else {
        unreachable!()
    };
    let ValueData::Integer(compression) = arguments[1].0.data else {
        unreachable!()
    };
    encode_png(image, compression as u8, arguments[0].1)
        .map(|bytes| OuterValue::plain(ValueData::Bytes(Arc::new(bytes))))
}

fn execute_encode_webp(
    arguments: &[(OuterValue, Span)],
    _span: Span,
) -> Result<OuterValue, Diagnostic> {
    let ValueData::Image(image) = &arguments[0].0.data else {
        unreachable!()
    };
    let ValueData::Integer(quality) = arguments[1].0.data else {
        unreachable!()
    };
    encode_webp(image, quality as u8, arguments[0].1)
        .map(|bytes| OuterValue::plain(ValueData::Bytes(Arc::new(bytes))))
}

fn decode_ppm(bytes: &[u8], span: Span) -> Result<ImageValue, Diagnostic> {
    crate::registered_wasm::decode_ppm(bytes, span)
}

fn encode_ppm(image: &ImageValue, span: Span) -> Result<Vec<u8>, Diagnostic> {
    crate::registered_wasm::encode_ppm(image, span)
}

fn decode_png(bytes: &[u8], span: Span) -> Result<ImageValue, Diagnostic> {
    crate::registered_wasm::decode_png(bytes, span)
}

fn encode_png(image: &ImageValue, compression: u8, span: Span) -> Result<Vec<u8>, Diagnostic> {
    crate::registered_wasm::encode_png(image, i64::from(compression), span)
}

fn encode_webp(image: &ImageValue, quality: u8, span: Span) -> Result<Vec<u8>, Diagnostic> {
    crate::registered_wasm::encode_webp(image, i64::from(quality), span)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::byte_content_identity;

    #[test]
    fn builtin_inspection_is_derived_from_registry_contracts() {
        let transforms = RegisteredTransform::infos().collect::<Vec<_>>();

        assert_eq!(transforms.len(), REGISTERED_TRANSFORMS.len());
        assert_eq!(transforms[0].name, "ppm.decode");
        assert_eq!(
            transforms[0].signature(),
            "ppm.decode(bytes: bytes) -> rgba8-image"
        );
        assert_eq!(
            transforms[0].transform_id,
            REGISTERED_TRANSFORMS[0].identity()
        );
        assert_eq!(
            transforms[3].signature(),
            "png.encode(image: rgba8-image, compression: i64 = 6) -> bytes"
        );
        assert_eq!(
            transforms[4].signature(),
            "webp.encode(image: rgba8-image, quality: i64 = 85) -> bytes"
        );
    }

    #[test]
    fn ppm_codec_rejects_incomplete_pixels() {
        let diagnostic = decode_ppm(b"P3\n1 1\n255\n1 2\n", Span::default()).unwrap_err();

        assert!(
            diagnostic
                .message
                .contains("channel sample count is incomplete")
        );
    }

    #[test]
    fn ppm_encoding_ignores_alpha_and_row_padding() {
        let image = ImageValue::new_rgba8(1, 1, 6, vec![1, 2, 3, 4, 99, 100]).unwrap();

        assert_eq!(
            encode_ppm(&image, Span::default()).unwrap(),
            b"P3\n1 1\n255\n1 2 3\n"
        );
    }

    #[test]
    fn png_encoding_is_deterministic_and_ignores_row_padding() {
        let image =
            ImageValue::new_rgba8(2, 1, 10, vec![1, 2, 3, 4, 200, 150, 100, 50, 99, 100]).unwrap();

        let first = encode_png(&image, 6, Span::default()).unwrap();
        let second = encode_png(&image, 6, Span::default()).unwrap();
        let low_compression = encode_png(&image, 1, Span::default()).unwrap();
        let decoded = decode_png(&first, Span::default()).unwrap();

        assert_eq!(first, second);
        assert_ne!(first, low_compression);
        assert_eq!(&first[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(decoded.width(), 2);
        assert_eq!(decoded.height(), 1);
        assert_eq!(decoded.stride(), 8);
        assert_eq!(decoded.bytes(), &[1, 2, 3, 4, 200, 150, 100, 50]);
        assert_eq!(
            byte_content_identity(&first).to_string(),
            "01d6d2f53cdbc115059b2116ff7b33acbe95f4c5ba138980086b705a9fd0cee0"
        );
    }

    #[test]
    fn png_compression_is_source_spanned_and_range_checked() {
        let image = OuterValue::image(ImageValue::new_rgba8(1, 1, 4, vec![1, 2, 3, 4]).unwrap());
        let compression_span = Span::new(17, 18);

        for compression in [0, 10] {
            let diagnostic = match prepare_registered_invocation(
                RegisteredTransform::find("png.encode").unwrap(),
                vec![
                    (image.clone(), Span::new(0, 5)),
                    (
                        OuterValue::plain(ValueData::Integer(compression)),
                        compression_span,
                    ),
                ],
                Span::new(0, 18),
            ) {
                Err(diagnostic) => diagnostic,
                Ok(_) => panic!("out-of-range compression was accepted"),
            };

            assert_eq!(diagnostic.labels[0].span, compression_span);
            assert!(diagnostic.message.contains("integer from 1 through 9"));
        }

        let diagnostic = match prepare_registered_invocation(
            RegisteredTransform::find("png.encode").unwrap(),
            vec![
                (image, Span::new(0, 5)),
                (OuterValue::plain(ValueData::Float(6.0)), compression_span),
            ],
            Span::new(0, 18),
        ) {
            Err(diagnostic) => diagnostic,
            Ok(_) => panic!("non-integer compression was accepted"),
        };
        assert_eq!(diagnostic.labels[0].span, compression_span);
        assert!(diagnostic.message.contains("integer from 1 through 9"));
    }

    #[test]
    fn webp_encoding_is_deterministic_preserves_alpha_and_ignores_row_padding() {
        let image =
            ImageValue::new_rgba8(2, 1, 10, vec![1, 2, 3, 4, 200, 150, 100, 50, 99, 100]).unwrap();

        let first = encode_webp(&image, 85, Span::default()).unwrap();
        let second = encode_webp(&image, 85, Span::default()).unwrap();
        let low_quality = encode_webp(&image, 25, Span::default()).unwrap();
        let decoded = webp_rust::decode(&first).unwrap();

        assert_eq!(first, second);
        assert_ne!(first, low_quality);
        assert_eq!(&first[..4], b"RIFF");
        assert_eq!(&first[8..12], b"WEBP");
        assert_eq!(decoded.width, 2);
        assert_eq!(decoded.height, 1);
        assert_eq!(decoded.rgba[3], 4);
        assert_eq!(decoded.rgba[7], 50);
        assert_eq!(
            byte_content_identity(&first).to_string(),
            "6400962f906dc5e89d77e8683feef279f5515e2d346423e763feffb207b22c74"
        );
    }

    #[test]
    fn webp_encoding_handles_a_nontrivial_image_within_the_sandbox_budget() {
        let mut pixels = Vec::with_capacity(32 * 32 * 4);
        for y in 0..32_u8 {
            for x in 0..32_u8 {
                pixels.extend_from_slice(&[x.wrapping_mul(7), y.wrapping_mul(7), x ^ y, 255]);
            }
        }
        let image = ImageValue::new_rgba8(32, 32, 32 * 4, pixels).unwrap();

        let encoded = encode_webp(&image, 85, Span::default()).unwrap();
        let decoded = webp_rust::decode(&encoded).unwrap();

        assert_eq!(decoded.width, 32);
        assert_eq!(decoded.height, 32);
    }

    #[test]
    fn webp_quality_is_source_spanned_and_range_checked() {
        let image = OuterValue::image(ImageValue::new_rgba8(1, 1, 4, vec![1, 2, 3, 4]).unwrap());
        let quality_span = Span::new(17, 20);

        for quality in [-1, 101] {
            let diagnostic = match prepare_registered_invocation(
                RegisteredTransform::find("webp.encode").unwrap(),
                vec![
                    (image.clone(), Span::new(0, 5)),
                    (OuterValue::plain(ValueData::Integer(quality)), quality_span),
                ],
                Span::new(0, 20),
            ) {
                Err(diagnostic) => diagnostic,
                Ok(_) => panic!("out-of-range quality was accepted"),
            };

            assert_eq!(diagnostic.labels[0].span, quality_span);
            assert!(diagnostic.message.contains("integer from 0 through 100"));
        }

        let diagnostic = match prepare_registered_invocation(
            RegisteredTransform::find("webp.encode").unwrap(),
            vec![
                (image, Span::new(0, 5)),
                (OuterValue::plain(ValueData::Float(85.0)), quality_span),
            ],
            Span::new(0, 20),
        ) {
            Err(diagnostic) => diagnostic,
            Ok(_) => panic!("non-integer quality was accepted"),
        };
        assert_eq!(diagnostic.labels[0].span, quality_span);
        assert!(diagnostic.message.contains("integer from 0 through 100"));
    }

    #[test]
    fn png_decoding_normalizes_grayscale_to_rgba8() {
        let mut encoded = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut encoded, 2, 1);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[5, 200]).unwrap();
            writer.finish().unwrap();
        }

        let decoded = decode_png(&encoded, Span::default()).unwrap();

        assert_eq!(decoded.format(), ImageFormat::Rgba8);
        assert_eq!(decoded.bytes(), &[5, 5, 5, 255, 200, 200, 200, 255]);
    }

    #[test]
    fn png_decoding_expands_palette_transparency_to_rgba8() {
        let mut encoded = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut encoded, 2, 1);
            encoder.set_color(png::ColorType::Indexed);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_palette(&[10, 20, 30, 200, 150, 100]);
            encoder.set_trns(&[7, 255]);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[0, 1]).unwrap();
            writer.finish().unwrap();
        }

        let decoded = decode_png(&encoded, Span::default()).unwrap();

        assert_eq!(decoded.format(), ImageFormat::Rgba8);
        assert_eq!(decoded.bytes(), &[10, 20, 30, 7, 200, 150, 100, 255]);
    }

    #[test]
    fn png_decoding_reports_invalid_input() {
        let diagnostic = decode_png(b"not a PNG", Span::default()).unwrap_err();

        assert!(
            diagnostic
                .message
                .contains("png.decode could not read PNG metadata")
        );
    }

    #[test]
    fn png_decoding_rejects_animation() {
        let mut encoded = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut encoded, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_animated(2, 0).unwrap();
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[1, 2, 3, 4]).unwrap();
            writer.write_image_data(&[5, 6, 7, 8]).unwrap();
            writer.finish().unwrap();
        }

        let diagnostic = decode_png(&encoded, Span::default()).unwrap_err();

        assert!(diagnostic.message.contains("does not support animated PNG"));
    }
}
