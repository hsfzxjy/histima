use std::io::Cursor;
use std::sync::Arc;

use crate::diagnostic::Diagnostic;
use crate::identity::{TransformIdentity, registered_transform_identity};
use crate::runtime::{ImageFormat, ImageValue, OuterValue, ValueData};
use crate::source::Span;

const PPM_DECODE_TRANSFORM_VERSION: u32 = 2;
const PPM_ENCODE_TRANSFORM_VERSION: u32 = 1;
const PNG_DECODE_TRANSFORM_VERSION: u32 = 1;
const PNG_ENCODE_TRANSFORM_VERSION: u32 = 2;
const PNG_DEFAULT_COMPRESSION: i64 = 6;
const WEBP_ENCODE_TRANSFORM_VERSION: u32 = 1;
const WEBP_DEFAULT_QUALITY: i64 = 85;

#[derive(Clone, Copy)]
enum DefaultValue {
    Integer(i64),
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
    parameters: &'static [&'static str],
    defaults: &'static [Option<DefaultValue>],
    validate: Validator,
    execute: Executor,
}

const NO_DEFAULTS_1: &[Option<DefaultValue>] = &[None];
const PNG_ENCODE_DEFAULTS: &[Option<DefaultValue>] =
    &[None, Some(DefaultValue::Integer(PNG_DEFAULT_COMPRESSION))];
const WEBP_ENCODE_DEFAULTS: &[Option<DefaultValue>] =
    &[None, Some(DefaultValue::Integer(WEBP_DEFAULT_QUALITY))];

const REGISTERED_TRANSFORMS: &[RegisteredTransform] = &[
    RegisteredTransform {
        name: "ppm.decode",
        semantic_version: PPM_DECODE_TRANSFORM_VERSION,
        parameters: &["bytes"],
        defaults: NO_DEFAULTS_1,
        validate: validate_bytes,
        execute: execute_decode_ppm,
    },
    RegisteredTransform {
        name: "ppm.encode",
        semantic_version: PPM_ENCODE_TRANSFORM_VERSION,
        parameters: &["image"],
        defaults: NO_DEFAULTS_1,
        validate: validate_rgba8,
        execute: execute_encode_ppm,
    },
    RegisteredTransform {
        name: "png.decode",
        semantic_version: PNG_DECODE_TRANSFORM_VERSION,
        parameters: &["bytes"],
        defaults: NO_DEFAULTS_1,
        validate: validate_bytes,
        execute: execute_decode_png,
    },
    RegisteredTransform {
        name: "png.encode",
        semantic_version: PNG_ENCODE_TRANSFORM_VERSION,
        parameters: &["image", "compression"],
        defaults: PNG_ENCODE_DEFAULTS,
        validate: validate_png_encode,
        execute: execute_encode_png,
    },
    RegisteredTransform {
        name: "webp.encode",
        semantic_version: WEBP_ENCODE_TRANSFORM_VERSION,
        parameters: &["image", "quality"],
        defaults: WEBP_ENCODE_DEFAULTS,
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

    pub(crate) const fn parameters(&self) -> &'static [&'static str] {
        self.parameters
    }

    pub(crate) fn default_argument(&self, index: usize) -> Option<OuterValue> {
        match self.defaults.get(index).copied().flatten()? {
            DefaultValue::Integer(value) => Some(OuterValue::plain(ValueData::Integer(value))),
        }
    }

    pub(crate) fn identity(&self) -> TransformIdentity {
        registered_transform_identity(self.name, self.semantic_version)
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
    Ok(OuterValue::plain(ValueData::Bytes(Arc::new(encode_ppm(
        image,
    )))))
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

fn encode_ppm(image: &ImageValue) -> Vec<u8> {
    let mut output = format!("P3\n{} {}\n255\n", image.width(), image.height());
    image.with_bytes(|bytes| {
        for y in 0..image.height() {
            for x in 0..image.width() {
                let pixel = y * image.stride() + x * 4;
                output.push_str(&format!(
                    "{} {} {}{}",
                    bytes[pixel],
                    bytes[pixel + 1],
                    bytes[pixel + 2],
                    if x + 1 == image.width() { "\n" } else { " " }
                ));
            }
        }
    });
    output.into_bytes()
}

fn decode_png(bytes: &[u8], span: Span) -> Result<ImageValue, Diagnostic> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|error| {
        Diagnostic::error(
            format!("png.decode could not read PNG metadata: {error}"),
            span,
        )
    })?;
    if reader.info().animation_control.is_some() {
        return Err(Diagnostic::error(
            "png.decode does not support animated PNG images",
            span,
        ));
    }
    let buffer_size = reader.output_buffer_size().ok_or_else(|| {
        Diagnostic::error("png.decode image dimensions exceed this runtime", span)
    })?;
    let mut decoded = vec![0; buffer_size];
    let output = reader.next_frame(&mut decoded).map_err(|error| {
        Diagnostic::error(
            format!("png.decode could not decode image data: {error}"),
            span,
        )
    })?;
    if output.bit_depth != png::BitDepth::Eight {
        return Err(Diagnostic::error(
            format!(
                "png.decode produced unsupported {:?} channel depth",
                output.bit_depth
            ),
            span,
        ));
    }
    let width = usize::try_from(output.width)
        .map_err(|_| Diagnostic::error("png.decode width exceeds usize", span))?;
    let height = usize::try_from(output.height)
        .map_err(|_| Diagnostic::error("png.decode height exceeds usize", span))?;
    let pixels = width
        .checked_mul(height)
        .ok_or_else(|| Diagnostic::error("png.decode pixel count overflows usize", span))?;
    let rgba_length = pixels
        .checked_mul(4)
        .ok_or_else(|| Diagnostic::error("png.decode RGBA byte length overflows usize", span))?;
    let channels = output.color_type.samples();
    let expected_line = width
        .checked_mul(channels)
        .ok_or_else(|| Diagnostic::error("png.decode row byte length overflows usize", span))?;
    if output.line_size != expected_line {
        return Err(Diagnostic::error(
            "png.decode returned an inconsistent row layout",
            span,
        ));
    }
    let decoded = &decoded[..output.buffer_size()];
    let mut rgba = Vec::with_capacity(rgba_length);
    for pixel in decoded.chunks_exact(channels) {
        match output.color_type {
            png::ColorType::Grayscale => {
                rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], 255])
            }
            png::ColorType::GrayscaleAlpha => {
                rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]])
            }
            png::ColorType::Rgb => rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]),
            png::ColorType::Rgba => rgba.extend_from_slice(pixel),
            png::ColorType::Indexed => {
                return Err(Diagnostic::error(
                    "png.decode palette expansion did not produce RGB pixels",
                    span,
                ));
            }
        }
    }
    if rgba.len() != rgba_length {
        return Err(Diagnostic::error(
            "png.decode returned an incomplete pixel buffer",
            span,
        ));
    }
    let stride = width
        .checked_mul(4)
        .ok_or_else(|| Diagnostic::error("png.decode RGBA stride overflows usize", span))?;
    ImageValue::new_rgba8(width, height, stride, rgba).map_err(|error| {
        Diagnostic::error(format!("png.decode produced invalid image: {error}"), span)
    })
}

fn encode_png(image: &ImageValue, compression: u8, span: Span) -> Result<Vec<u8>, Diagnostic> {
    let width = u32::try_from(image.width())
        .map_err(|_| Diagnostic::error("png.encode width exceeds PNG limits", span))?;
    let height = u32::try_from(image.height())
        .map_err(|_| Diagnostic::error("png.encode height exceeds PNG limits", span))?;
    let row_length = image
        .width()
        .checked_mul(4)
        .ok_or_else(|| Diagnostic::error("png.encode row byte length overflows usize", span))?;
    let packed_length = row_length
        .checked_mul(image.height())
        .ok_or_else(|| Diagnostic::error("png.encode image byte length overflows usize", span))?;
    let mut packed = Vec::with_capacity(packed_length);
    image.with_bytes(|bytes| -> Result<(), Diagnostic> {
        for row in 0..image.height() {
            let start = row
                .checked_mul(image.stride())
                .ok_or_else(|| Diagnostic::error("png.encode row offset overflows usize", span))?;
            packed.extend_from_slice(&bytes[start..start + row_length]);
        }
        Ok(())
    })?;

    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_deflate_compression(png::DeflateCompression::Level(compression));
        encoder.set_filter(png::Filter::Paeth);
        let mut writer = encoder.write_header().map_err(|error| {
            Diagnostic::error(
                format!("png.encode could not write PNG header: {error}"),
                span,
            )
        })?;
        writer.write_image_data(&packed).map_err(|error| {
            Diagnostic::error(
                format!("png.encode could not write image data: {error}"),
                span,
            )
        })?;
        writer.finish().map_err(|error| {
            Diagnostic::error(format!("png.encode could not finish image: {error}"), span)
        })?;
    }
    Ok(encoded)
}

fn encode_webp(image: &ImageValue, quality: u8, span: Span) -> Result<Vec<u8>, Diagnostic> {
    const MAX_DIMENSION: usize = 16_383;
    if image.width() == 0
        || image.height() == 0
        || image.width() > MAX_DIMENSION
        || image.height() > MAX_DIMENSION
    {
        return Err(Diagnostic::error(
            "webp.encode dimensions must each be from 1 through 16383",
            span,
        ));
    }
    let row_length = image
        .width()
        .checked_mul(4)
        .ok_or_else(|| Diagnostic::error("webp.encode row byte length overflows usize", span))?;
    let packed_length = row_length
        .checked_mul(image.height())
        .ok_or_else(|| Diagnostic::error("webp.encode image byte length overflows usize", span))?;
    let mut packed = Vec::with_capacity(packed_length);
    image.with_bytes(|bytes| -> Result<(), Diagnostic> {
        for row in 0..image.height() {
            let start = row
                .checked_mul(image.stride())
                .ok_or_else(|| Diagnostic::error("webp.encode row offset overflows usize", span))?;
            packed.extend_from_slice(&bytes[start..start + row_length]);
        }
        Ok(())
    })?;

    let input = webp_rust::ImageBuffer {
        width: image.width(),
        height: image.height(),
        rgba: packed,
    };
    let config = webp_rust::LossyEncodingConfig {
        quality: f32::from(quality),
        ..webp_rust::LossyEncodingConfig::default()
    };
    webp_rust::encode_lossy_with_config(&input, &config, None).map_err(|error| {
        Diagnostic::error(format!("webp.encode could not encode image: {error}"), span)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::byte_content_identity;

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

        assert_eq!(encode_ppm(&image), b"P3\n1 1\n255\n1 2 3\n");
    }

    #[test]
    fn png_encoding_is_deterministic_and_ignores_row_padding() {
        let image =
            ImageValue::new_rgba8(2, 1, 10, vec![1, 2, 3, 4, 200, 150, 100, 50, 99, 100]).unwrap();

        let first = encode_png(&image, 6, Span::default()).unwrap();
        let second = encode_png(&image, 6, Span::default()).unwrap();
        let decoded = decode_png(&first, Span::default()).unwrap();

        assert_eq!(first, second);
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
