use std::sync::Arc;

use crate::capability::RuntimeCapabilities;
use crate::diagnostic::Diagnostic;
use crate::identity::{TransformIdentity, byte_content_identity, host_transform_identity};
use crate::lineage::{Lineage, LineageNode};
use crate::runtime::{ImageFormat, ImageValue, OuterValue, ValueData};
use crate::source::Span;

const PPM_TRANSFORM_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostTransform {
    DecodePpm,
    EncodePpm,
}

impl HostTransform {
    pub(crate) fn find(name: &str) -> Option<Self> {
        match name {
            "decode.ppm" => Some(Self::DecodePpm),
            "encode.ppm" => Some(Self::EncodePpm),
            _ => None,
        }
    }

    pub(crate) fn from_identity(identity: TransformIdentity) -> Option<Self> {
        [Self::DecodePpm, Self::EncodePpm]
            .into_iter()
            .find(|transform| transform.identity() == identity)
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::DecodePpm => "decode.ppm",
            Self::EncodePpm => "encode.ppm",
        }
    }

    pub(crate) const fn parameters(self) -> &'static [&'static str] {
        match self {
            Self::DecodePpm => &["asset"],
            Self::EncodePpm => &["image"],
        }
    }

    pub(crate) fn identity(self) -> TransformIdentity {
        host_transform_identity(self.name(), PPM_TRANSFORM_VERSION)
    }

    pub(crate) fn prepare(
        self,
        mut arguments: Vec<(OuterValue, Span)>,
        capabilities: Option<&dyn RuntimeCapabilities>,
        call_span: Span,
    ) -> Result<PreparedHostInvocation, Diagnostic> {
        if arguments.len() != 1 {
            return Err(Diagnostic::error(
                format!("{} expects exactly one argument", self.name()),
                call_span,
            ));
        }
        match self {
            Self::DecodePpm => {
                let (asset, span) = &mut arguments[0];
                let ValueData::Asset(value) = &asset.data else {
                    return Err(Diagnostic::error(
                        "decode.ppm expects an asset value",
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
            Self::EncodePpm => {
                let ValueData::Image(image) = &arguments[0].0.data else {
                    return Err(Diagnostic::error(
                        "encode.ppm expects an image value",
                        arguments[0].1,
                    ));
                };
                if image.format() != ImageFormat::Rgba8 {
                    return Err(Diagnostic::error(
                        "encode.ppm requires an RGBA8 image",
                        arguments[0].1,
                    ));
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
}
