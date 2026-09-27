use std::io::Cursor;
use std::sync::Arc;

use crate::capability::RuntimeCapabilities;
use crate::diagnostic::Diagnostic;
use crate::identity::{TransformIdentity, byte_content_identity, host_transform_identity};
use crate::lineage::{Lineage, LineageNode};
use crate::runtime::{ImageFormat, ImageValue, OuterValue, ValueData};
use crate::source::Span;

const PPM_TRANSFORM_VERSION: u32 = 1;
const PNG_DECODE_TRANSFORM_VERSION: u32 = 1;
const PNG_ENCODE_TRANSFORM_VERSION: u32 = 2;
const PNG_DEFAULT_COMPRESSION: i64 = 6;
const WEBP_ENCODE_TRANSFORM_VERSION: u32 = 1;
const WEBP_DEFAULT_QUALITY: i64 = 85;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostTransform {
    DecodePpm,
    EncodePpm,
    DecodePng,
    EncodePng,
    EncodeWebp,
}

impl HostTransform {
    pub(crate) fn find(name: &str) -> Option<Self> {
        match name {
            "decode.ppm" => Some(Self::DecodePpm),
            "encode.ppm" => Some(Self::EncodePpm),
            "decode.png" => Some(Self::DecodePng),
            "encode.png" => Some(Self::EncodePng),
            "encode.webp" => Some(Self::EncodeWebp),
            _ => None,
        }
    }

    pub(crate) fn from_identity(identity: TransformIdentity) -> Option<Self> {
        [
            Self::DecodePpm,
            Self::EncodePpm,
            Self::DecodePng,
            Self::EncodePng,
            Self::EncodeWebp,
        ]
        .into_iter()
        .find(|transform| transform.identity() == identity)
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::DecodePpm => "decode.ppm",
            Self::EncodePpm => "encode.ppm",
            Self::DecodePng => "decode.png",
            Self::EncodePng => "encode.png",
            Self::EncodeWebp => "encode.webp",
        }
    }

    pub(crate) const fn parameters(self) -> &'static [&'static str] {
        match self {
            Self::DecodePpm | Self::DecodePng => &["asset"],
            Self::EncodePpm => &["image"],
            Self::EncodePng => &["image", "compression"],
            Self::EncodeWebp => &["image", "quality"],
        }
    }

    pub(crate) fn default_argument(self, index: usize) -> Option<OuterValue> {
        match (self, index) {
            (Self::EncodePng, 1) => Some(OuterValue::plain(ValueData::Integer(
                PNG_DEFAULT_COMPRESSION,
            ))),
            (Self::EncodeWebp, 1) => {
                Some(OuterValue::plain(ValueData::Integer(WEBP_DEFAULT_QUALITY)))
            }
            _ => None,
        }
    }

    pub(crate) fn identity(self) -> TransformIdentity {
        let version = match self {
            Self::DecodePpm | Self::EncodePpm => PPM_TRANSFORM_VERSION,
            Self::DecodePng => PNG_DECODE_TRANSFORM_VERSION,
            Self::EncodePng => PNG_ENCODE_TRANSFORM_VERSION,
            Self::EncodeWebp => WEBP_ENCODE_TRANSFORM_VERSION,
        };
        host_transform_identity(self.name(), version)
    }

    pub(crate) fn prepare(
        self,
        mut arguments: Vec<(OuterValue, Span)>,
        capabilities: Option<&dyn RuntimeCapabilities>,
        call_span: Span,
    ) -> Result<PreparedHostInvocation, Diagnostic> {
        if arguments.len() != self.parameters().len() {
            return Err(Diagnostic::error(
                format!(
                    "{} expects exactly {} arguments",
                    self.name(),
                    self.parameters().len()
                ),
                call_span,
            ));
        }
        match self {
            Self::DecodePpm | Self::DecodePng => {
                let (asset, span) = &mut arguments[0];
                let ValueData::Asset(value) = &asset.data else {
                    return Err(Diagnostic::error(
                        format!("{} expects an asset value", self.name()),
                        *span,
                    ));
                };
                let Some(capabilities) = capabilities else {
                    return Err(Diagnostic::error(
                        "asset materialization is unavailable in this runtime",
                        *span,
                    )
                    .with_note("the Histima host must provide an asset-reading capability"));
                };
                let bytes = capabilities.read_asset(&value.locator).map_err(|error| {
                    Diagnostic::error(
                        format!("could not read asset {:?}: {error}", value.locator),
                        *span,
                    )
                })?;
                let observed = byte_content_identity(&bytes);
                if let Some(lineage) = &asset.lineage {
                    match lineage.node() {
                        LineageNode::Source(source) => {
                            if source.locator != value.locator {
                                return Err(Diagnostic::error(
                                    "asset locator does not match its source lineage",
                                    *span,
                                ));
                            }
                            if let Some(expected) = source.observed_content
                                && expected != observed
                            {
                                return Err(Diagnostic::error(
                                    format!(
                                        "asset {:?} changed: expected content {expected}, observed {observed}",
                                        value.locator
                                    ),
                                    *span,
                                )
                                .with_note("replay requires the recorded source content"));
                            }
                        }
                        _ => {
                            return Err(Diagnostic::error(
                                "asset value carries non-source lineage",
                                *span,
                            ));
                        }
                    }
                }
                asset.lineage = Some(Lineage::observed_source(value.locator.clone(), observed));
                Ok(PreparedHostInvocation {
                    arguments,
                    decode_bytes: Some(bytes),
                })
            }
            Self::EncodePpm | Self::EncodePng | Self::EncodeWebp => {
                let ValueData::Image(image) = &arguments[0].0.data else {
                    return Err(Diagnostic::error(
                        format!("{} expects an image value", self.name()),
                        arguments[0].1,
                    ));
                };
                if image.format() != ImageFormat::Rgba8 {
                    return Err(Diagnostic::error(
                        format!("{} requires an RGBA8 image", self.name()),
                        arguments[0].1,
                    ));
                }
                if self == Self::EncodePng {
                    let ValueData::Integer(compression) = arguments[1].0.data else {
                        return Err(Diagnostic::error(
                            "encode.png compression must be an integer from 1 through 9",
                            arguments[1].1,
                        ));
                    };
                    if !(1..=9).contains(&compression) {
                        return Err(Diagnostic::error(
                            "encode.png compression must be an integer from 1 through 9",
                            arguments[1].1,
                        ));
                    }
                }
                if self == Self::EncodeWebp {
                    let ValueData::Integer(quality) = arguments[1].0.data else {
                        return Err(Diagnostic::error(
                            "encode.webp quality must be an integer from 0 through 100",
                            arguments[1].1,
                        ));
                    };
                    if !(0..=100).contains(&quality) {
                        return Err(Diagnostic::error(
                            "encode.webp quality must be an integer from 0 through 100",
                            arguments[1].1,
                        ));
                    }
                }
                Ok(PreparedHostInvocation {
                    arguments,
                    decode_bytes: None,
                })
            }
        }
    }
}

pub(crate) struct PreparedHostInvocation {
    pub(crate) arguments: Vec<(OuterValue, Span)>,
    decode_bytes: Option<Vec<u8>>,
}

impl PreparedHostInvocation {
    pub(crate) fn execute(self, transform: HostTransform) -> Result<OuterValue, Diagnostic> {
        match transform {
            HostTransform::DecodePpm => {
                let bytes = self.decode_bytes.expect("decode preparation retains bytes");
                decode_ppm(&bytes, self.arguments[0].1).map(OuterValue::image)
            }
            HostTransform::EncodePpm => {
                let ValueData::Image(image) = &self.arguments[0].0.data else {
                    unreachable!("prepared encode.ppm argument is an image")
                };
                Ok(OuterValue::plain(ValueData::Bytes(Arc::from(encode_ppm(
                    image,
                )))))
            }
            HostTransform::DecodePng => {
                let bytes = self.decode_bytes.expect("decode preparation retains bytes");
                decode_png(&bytes, self.arguments[0].1).map(OuterValue::image)
            }
            HostTransform::EncodePng => {
                let ValueData::Image(image) = &self.arguments[0].0.data else {
                    unreachable!("prepared encode.png argument is an image")
                };
                let ValueData::Integer(compression) = self.arguments[1].0.data else {
                    unreachable!("prepared encode.png compression is an integer")
                };
                encode_png(image, compression as u8, self.arguments[0].1)
                    .map(|bytes| OuterValue::plain(ValueData::Bytes(Arc::from(bytes))))
            }
            HostTransform::EncodeWebp => {
                let ValueData::Image(image) = &self.arguments[0].0.data else {
                    unreachable!("prepared encode.webp argument is an image")
                };
                let ValueData::Integer(quality) = self.arguments[1].0.data else {
                    unreachable!("prepared encode.webp quality is an integer")
                };
                encode_webp(image, quality as u8, self.arguments[0].1)
                    .map(|bytes| OuterValue::plain(ValueData::Bytes(Arc::from(bytes))))
            }
        }
    }
}

pub(crate) fn prepare_host_invocation(
    transform: HostTransform,
    arguments: Vec<(OuterValue, Span)>,
    capabilities: Option<&dyn RuntimeCapabilities>,
    call_span: Span,
) -> Result<PreparedHostInvocation, Diagnostic> {
    transform.prepare(arguments, capabilities, call_span)
}

fn decode_ppm(bytes: &[u8], span: Span) -> Result<ImageValue, Diagnostic> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        Diagnostic::error("decode.ppm currently requires ASCII P3 data", span)
            .with_note("binary P6 support is deferred")
    })?;
    let tokens = text
        .lines()
        .flat_map(|line| line.split('#').next().unwrap_or("").split_whitespace())
        .collect::<Vec<_>>();
    if tokens.first().copied() != Some("P3") {
        return Err(Diagnostic::error(
            "decode.ppm expected an ASCII P3 header",
            span,
        ));
    }
    if tokens.len() < 4 {
        return Err(Diagnostic::error("decode.ppm header is incomplete", span));
    }
    let width = ppm_usize(tokens[1], "width", span)?;
    let height = ppm_usize(tokens[2], "height", span)?;
    if width == 0 || height == 0 {
        return Err(Diagnostic::error(
            "decode.ppm requires non-zero dimensions",
            span,
        ));
    }
    let maximum = ppm_usize(tokens[3], "maximum channel value", span)?;
    if maximum != 255 {
        return Err(Diagnostic::error(
            "decode.ppm currently requires maximum channel value 255",
            span,
        ));
    }
    let pixels = width
        .checked_mul(height)
        .ok_or_else(|| Diagnostic::error("decode.ppm image dimensions overflow usize", span))?;
    let expected_samples = pixels
        .checked_mul(3)
        .ok_or_else(|| Diagnostic::error("decode.ppm sample count overflows usize", span))?;
    let expected_tokens = expected_samples
        .checked_add(4)
        .ok_or_else(|| Diagnostic::error("decode.ppm token count overflows usize", span))?;
    if tokens.len() != expected_tokens {
        return Err(Diagnostic::error(
            format!(
                "decode.ppm expected {expected_samples} channel samples, found {}",
                tokens.len().saturating_sub(4)
            ),
            span,
        ));
    }
    let byte_len = pixels
        .checked_mul(4)
        .ok_or_else(|| Diagnostic::error("decode.ppm RGBA byte length overflows usize", span))?;
    let stride = width
        .checked_mul(4)
        .ok_or_else(|| Diagnostic::error("decode.ppm RGBA stride overflows usize", span))?;
    let mut rgba = Vec::with_capacity(byte_len);
    for token in &tokens[4..] {
        let value = token.parse::<u8>().map_err(|_| {
            Diagnostic::error(
                format!("decode.ppm has invalid channel sample `{token}`"),
                span,
            )
        })?;
        rgba.push(value);
        if rgba.len() % 4 == 3 {
            rgba.push(255);
        }
    }
    ImageValue::new_rgba8(width, height, stride, rgba).map_err(|error| {
        Diagnostic::error(format!("decode.ppm produced invalid image: {error}"), span)
    })
}

fn ppm_usize(token: &str, field: &str, span: Span) -> Result<usize, Diagnostic> {
    token
        .parse()
        .map_err(|_| Diagnostic::error(format!("decode.ppm has invalid {field} `{token}`"), span))
}

fn encode_ppm(image: &ImageValue) -> Vec<u8> {
    let mut output = format!("P3\n{} {}\n255\n", image.width(), image.height());
    for y in 0..image.height() {
        for x in 0..image.width() {
            let pixel = y * image.stride() + x * 4;
            let bytes = image.bytes();
            output.push_str(&format!(
                "{} {} {}{}",
                bytes[pixel],
                bytes[pixel + 1],
                bytes[pixel + 2],
                if x + 1 == image.width() { "\n" } else { " " }
            ));
        }
    }
    output.into_bytes()
}

fn decode_png(bytes: &[u8], span: Span) -> Result<ImageValue, Diagnostic> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|error| {
        Diagnostic::error(
            format!("decode.png could not read PNG metadata: {error}"),
            span,
        )
    })?;
    if reader.info().animation_control.is_some() {
        return Err(Diagnostic::error(
            "decode.png does not support animated PNG images",
            span,
        ));
    }
    let buffer_size = reader.output_buffer_size().ok_or_else(|| {
        Diagnostic::error("decode.png image dimensions exceed this runtime", span)
    })?;
    let mut decoded = vec![0; buffer_size];
    let output = reader.next_frame(&mut decoded).map_err(|error| {
        Diagnostic::error(
            format!("decode.png could not decode image data: {error}"),
            span,
        )
    })?;
    if output.bit_depth != png::BitDepth::Eight {
        return Err(Diagnostic::error(
            format!(
                "decode.png produced unsupported {:?} channel depth",
                output.bit_depth
            ),
            span,
        ));
    }
    let width = usize::try_from(output.width)
        .map_err(|_| Diagnostic::error("decode.png width exceeds usize", span))?;
    let height = usize::try_from(output.height)
        .map_err(|_| Diagnostic::error("decode.png height exceeds usize", span))?;
    let pixels = width
        .checked_mul(height)
        .ok_or_else(|| Diagnostic::error("decode.png pixel count overflows usize", span))?;
    let rgba_length = pixels
        .checked_mul(4)
        .ok_or_else(|| Diagnostic::error("decode.png RGBA byte length overflows usize", span))?;
    let channels = output.color_type.samples();
    let expected_line = width
        .checked_mul(channels)
        .ok_or_else(|| Diagnostic::error("decode.png row byte length overflows usize", span))?;
    if output.line_size != expected_line {
        return Err(Diagnostic::error(
            "decode.png returned an inconsistent row layout",
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
                    "decode.png palette expansion did not produce RGB pixels",
                    span,
                ));
            }
        }
    }
    if rgba.len() != rgba_length {
        return Err(Diagnostic::error(
            "decode.png returned an incomplete pixel buffer",
            span,
        ));
    }
    let stride = width
        .checked_mul(4)
        .ok_or_else(|| Diagnostic::error("decode.png RGBA stride overflows usize", span))?;
    ImageValue::new_rgba8(width, height, stride, rgba).map_err(|error| {
        Diagnostic::error(format!("decode.png produced invalid image: {error}"), span)
    })
}

fn encode_png(image: &ImageValue, compression: u8, span: Span) -> Result<Vec<u8>, Diagnostic> {
    let width = u32::try_from(image.width())
        .map_err(|_| Diagnostic::error("encode.png width exceeds PNG limits", span))?;
    let height = u32::try_from(image.height())
        .map_err(|_| Diagnostic::error("encode.png height exceeds PNG limits", span))?;
    let row_length = image
        .width()
        .checked_mul(4)
        .ok_or_else(|| Diagnostic::error("encode.png row byte length overflows usize", span))?;
    let packed_length = row_length
        .checked_mul(image.height())
        .ok_or_else(|| Diagnostic::error("encode.png image byte length overflows usize", span))?;
    let mut packed = Vec::with_capacity(packed_length);
    for row in 0..image.height() {
        let start = row
            .checked_mul(image.stride())
            .ok_or_else(|| Diagnostic::error("encode.png row offset overflows usize", span))?;
        packed.extend_from_slice(&image.bytes()[start..start + row_length]);
    }

    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_deflate_compression(png::DeflateCompression::Level(compression));
        encoder.set_filter(png::Filter::Paeth);
        let mut writer = encoder.write_header().map_err(|error| {
            Diagnostic::error(
                format!("encode.png could not write PNG header: {error}"),
                span,
            )
        })?;
        writer.write_image_data(&packed).map_err(|error| {
            Diagnostic::error(
                format!("encode.png could not write image data: {error}"),
                span,
            )
        })?;
        writer.finish().map_err(|error| {
            Diagnostic::error(format!("encode.png could not finish image: {error}"), span)
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
            "encode.webp dimensions must each be from 1 through 16383",
            span,
        ));
    }
    let row_length = image
        .width()
        .checked_mul(4)
        .ok_or_else(|| Diagnostic::error("encode.webp row byte length overflows usize", span))?;
    let packed_length = row_length
        .checked_mul(image.height())
        .ok_or_else(|| Diagnostic::error("encode.webp image byte length overflows usize", span))?;
    let mut packed = Vec::with_capacity(packed_length);
    for row in 0..image.height() {
        let start = row
            .checked_mul(image.stride())
            .ok_or_else(|| Diagnostic::error("encode.webp row offset overflows usize", span))?;
        packed.extend_from_slice(&image.bytes()[start..start + row_length]);
    }

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
        Diagnostic::error(format!("encode.webp could not encode image: {error}"), span)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ppm_codec_rejects_incomplete_pixels() {
        let diagnostic = decode_ppm(b"P3\n1 1\n255\n1 2\n", Span::default()).unwrap_err();

        assert!(diagnostic.message.contains("expected 3 channel samples"));
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
            let diagnostic = match HostTransform::EncodePng.prepare(
                vec![
                    (image.clone(), Span::new(0, 5)),
                    (
                        OuterValue::plain(ValueData::Integer(compression)),
                        compression_span,
                    ),
                ],
                None,
                Span::new(0, 18),
            ) {
                Err(diagnostic) => diagnostic,
                Ok(_) => panic!("out-of-range compression was accepted"),
            };

            assert_eq!(diagnostic.labels[0].span, compression_span);
            assert!(diagnostic.message.contains("integer from 1 through 9"));
        }

        let diagnostic = match HostTransform::EncodePng.prepare(
            vec![
                (image, Span::new(0, 5)),
                (OuterValue::plain(ValueData::Float(6.0)), compression_span),
            ],
            None,
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
            let diagnostic = match HostTransform::EncodeWebp.prepare(
                vec![
                    (image.clone(), Span::new(0, 5)),
                    (OuterValue::plain(ValueData::Integer(quality)), quality_span),
                ],
                None,
                Span::new(0, 20),
            ) {
                Err(diagnostic) => diagnostic,
                Ok(_) => panic!("out-of-range quality was accepted"),
            };

            assert_eq!(diagnostic.labels[0].span, quality_span);
            assert!(diagnostic.message.contains("integer from 0 through 100"));
        }

        let diagnostic = match HostTransform::EncodeWebp.prepare(
            vec![
                (image, Span::new(0, 5)),
                (OuterValue::plain(ValueData::Float(85.0)), quality_span),
            ],
            None,
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
                .contains("decode.png could not read PNG metadata")
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
