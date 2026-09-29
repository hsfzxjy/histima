use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use crate::diagnostic::Diagnostic;
use crate::identity::{
    ArtifactIdentity, ContentIdentity, TransformIdentity, byte_content_identity,
    registered_wasm_artifact_identity, registered_wasm_transform_identity,
};
use crate::registered::RegisteredTransform;
use crate::registered_wasm::{
    PLUGIN_ABI_VERSION, PluginArgument, PluginResult, PluginResultType, RegisteredWasmPlugin,
};
use crate::runtime::{ImageFormat, OuterValue, ValueData};
use crate::source::Span;

/// Value types supported by registered-Wasm ABI v3 manifests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginValueType {
    Bytes,
    Rgba8Image,
    I64,
}

impl PluginValueType {
    fn identity_tag(self) -> u8 {
        match self {
            Self::Bytes => 1,
            Self::Rgba8Image => 2,
            Self::I64 => 3,
        }
    }
}

/// One named input in an external plugin's semantic call contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginParameter {
    pub name: String,
    pub value_type: PluginValueType,
}

/// Host-validated data needed to register one workspace-approved Wasm module.
#[derive(Debug)]
pub struct PluginDefinition {
    pub name: String,
    pub semantic_version: u32,
    pub abi_version: u32,
    pub parameters: Vec<PluginParameter>,
    pub result: PluginValueType,
    pub expected_module_content: ContentIdentity,
    pub module_bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginRegistrationError {
    message: String,
}

impl PluginRegistrationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for PluginRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PluginRegistrationError {}

/// Immutable transform registry attached to one compiled Tima program.
#[derive(Default)]
pub struct PluginRegistry {
    transforms: Vec<PluginTransform>,
}

impl fmt::Debug for PluginRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PluginRegistry")
            .field(
                "transforms",
                &self
                    .transforms
                    .iter()
                    .map(|transform| transform.name())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl PluginRegistry {
    pub fn new(
        definitions: impl IntoIterator<Item = PluginDefinition>,
    ) -> Result<Self, PluginRegistrationError> {
        let mut transforms = Vec::new();
        let mut names = BTreeSet::new();
        for definition in definitions {
            validate_definition(&definition)?;
            if RegisteredTransform::find(&definition.name).is_some() {
                return Err(PluginRegistrationError::new(format!(
                    "plugin transform `{}` collides with a built-in transform",
                    definition.name
                )));
            }
            if !names.insert(definition.name.clone()) {
                return Err(PluginRegistrationError::new(format!(
                    "plugin transform `{}` is registered more than once",
                    definition.name
                )));
            }
            let observed_content = byte_content_identity(&definition.module_bytes);
            if observed_content != definition.expected_module_content {
                return Err(PluginRegistrationError::new(format!(
                    "plugin transform `{}` expected module content {} but observed {}",
                    definition.name, definition.expected_module_content, observed_content
                )));
            }
            let parameter_contract = definition
                .parameters
                .iter()
                .map(|parameter| (parameter.name.as_str(), parameter.value_type.identity_tag()))
                .collect::<Vec<_>>();
            let identity = registered_wasm_transform_identity(
                &definition.name,
                definition.semantic_version,
                definition.abi_version,
                &parameter_contract,
                definition.result.identity_tag(),
            );
            let artifact_identity = registered_wasm_artifact_identity(
                identity,
                &definition.module_bytes,
                definition.abi_version,
            );
            let implementation =
                RegisteredWasmPlugin::compile(definition.name.clone(), &definition.module_bytes)
                    .map_err(|error| {
                        PluginRegistrationError::new(format!(
                            "plugin transform `{}` could not load: {error}",
                            definition.name
                        ))
                    })?;
            transforms.push(PluginTransform {
                name: definition.name,
                parameters: definition.parameters,
                result: definition.result,
                identity,
                artifact_identity,
                implementation,
            });
        }
        transforms.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(Self { transforms })
    }

    pub(crate) fn find(&self, name: &str) -> Option<&PluginTransform> {
        self.transforms
            .binary_search_by_key(&name, |transform| transform.name.as_str())
            .ok()
            .map(|index| &self.transforms[index])
    }

    pub(crate) fn find_by_identity(&self, identity: TransformIdentity) -> Option<&PluginTransform> {
        self.transforms
            .iter()
            .find(|transform| transform.identity == identity)
    }

    pub fn len(&self) -> usize {
        self.transforms.len()
    }

    pub fn is_empty(&self) -> bool {
        self.transforms.is_empty()
    }

    pub fn artifact_identities(&self) -> impl Iterator<Item = ArtifactIdentity> + '_ {
        self.transforms
            .iter()
            .map(|transform| transform.artifact_identity)
    }
}

pub(crate) struct PluginTransform {
    name: String,
    parameters: Vec<PluginParameter>,
    result: PluginValueType,
    identity: TransformIdentity,
    artifact_identity: ArtifactIdentity,
    implementation: RegisteredWasmPlugin,
}

impl PluginTransform {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn parameters(&self) -> &[PluginParameter] {
        &self.parameters
    }

    pub(crate) fn identity(&self) -> TransformIdentity {
        self.identity
    }
}

pub(crate) struct PreparedPluginInvocation<'a> {
    pub(crate) arguments: Vec<(OuterValue, Span)>,
    transform: &'a PluginTransform,
    call_span: Span,
}

impl PreparedPluginInvocation<'_> {
    pub(crate) fn execute(self) -> Result<OuterValue, Diagnostic> {
        let mut arguments = Vec::with_capacity(self.arguments.len());
        for (parameter, (value, _)) in self.transform.parameters.iter().zip(&self.arguments) {
            arguments.push(match (parameter.value_type, &value.data) {
                (PluginValueType::Bytes, ValueData::Bytes(bytes)) => {
                    PluginArgument::BytesView(bytes)
                }
                (PluginValueType::Rgba8Image, ValueData::Image(image)) => {
                    PluginArgument::ImageView(image)
                }
                (PluginValueType::I64, ValueData::Integer(value)) => PluginArgument::I64(*value),
                _ => unreachable!("plugin arguments are validated before execution"),
            });
        }
        let result_type = match self.transform.result {
            PluginValueType::Bytes => PluginResultType::Bytes,
            PluginValueType::Rgba8Image => PluginResultType::Image,
            PluginValueType::I64 => unreachable!("scalar plugin results are rejected at load"),
        };
        match self
            .transform
            .implementation
            .invoke(&arguments, result_type, self.call_span)?
        {
            PluginResult::Bytes(bytes) => Ok(OuterValue::plain(ValueData::Bytes(Arc::new(bytes)))),
            PluginResult::Image(image) => Ok(OuterValue::image(image)),
        }
    }
}

pub(crate) fn prepare_plugin_invocation<'a>(
    transform: &'a PluginTransform,
    arguments: Vec<(OuterValue, Span)>,
    call_span: Span,
) -> Result<PreparedPluginInvocation<'a>, Diagnostic> {
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
    for (parameter, (argument, span)) in transform.parameters.iter().zip(&arguments) {
        let valid = match (parameter.value_type, &argument.data) {
            (PluginValueType::Bytes, ValueData::Bytes(_)) => true,
            (PluginValueType::Rgba8Image, ValueData::Image(image)) => {
                image.format() == ImageFormat::Rgba8
            }
            (PluginValueType::I64, ValueData::Integer(_)) => true,
            _ => false,
        };
        if !valid {
            return Err(Diagnostic::error(
                format!(
                    "plugin transform `{}` parameter `{}` expects {}",
                    transform.name,
                    parameter.name,
                    type_name(parameter.value_type)
                ),
                *span,
            ));
        }
    }
    Ok(PreparedPluginInvocation {
        arguments,
        transform,
        call_span,
    })
}

fn validate_definition(definition: &PluginDefinition) -> Result<(), PluginRegistrationError> {
    if definition.abi_version != PLUGIN_ABI_VERSION {
        return Err(PluginRegistrationError::new(format!(
            "plugin transform `{}` declares ABI version {}; this runtime requires {}",
            definition.name, definition.abi_version, PLUGIN_ABI_VERSION
        )));
    }
    let Some((namespace, local_name)) = definition.name.split_once('.') else {
        return Err(PluginRegistrationError::new(format!(
            "plugin transform name `{}` must contain one namespace separator",
            definition.name
        )));
    };
    if local_name.contains('.') || !valid_identifier(namespace) || !valid_identifier(local_name) {
        return Err(PluginRegistrationError::new(format!(
            "plugin transform name `{}` must be two Tima identifiers separated by one dot",
            definition.name
        )));
    }
    if definition.result == PluginValueType::I64 {
        return Err(PluginRegistrationError::new(format!(
            "plugin transform `{}` has unsupported scalar result type i64",
            definition.name
        )));
    }
    let mut parameter_names = BTreeSet::new();
    for parameter in &definition.parameters {
        if !valid_identifier(&parameter.name) {
            return Err(PluginRegistrationError::new(format!(
                "plugin transform `{}` has invalid parameter name `{}`",
                definition.name, parameter.name
            )));
        }
        if !parameter_names.insert(parameter.name.as_str()) {
            return Err(PluginRegistrationError::new(format!(
                "plugin transform `{}` has duplicate parameter `{}`",
                definition.name, parameter.name
            )));
        }
    }
    Ok(())
}

fn valid_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn type_name(value_type: PluginValueType) -> &'static str {
    match value_type {
        PluginValueType::Bytes => "immutable bytes",
        PluginValueType::Rgba8Image => "an RGBA8 image",
        PluginValueType::I64 => "an integer",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Span;

    const PPM_DECODE: &[u8] = include_bytes!("../../../plugins/ppm-decode/ppm_decode.wasm");
    const PNG_DECODE: &[u8] = include_bytes!("../../../plugins/png-decode/png_decode.wasm");

    fn decoder_definition(name: &str, bytes: Vec<u8>) -> PluginDefinition {
        PluginDefinition {
            name: name.to_owned(),
            semantic_version: 1,
            abi_version: PLUGIN_ABI_VERSION,
            parameters: vec![PluginParameter {
                name: "bytes".to_owned(),
                value_type: PluginValueType::Bytes,
            }],
            result: PluginValueType::Rgba8Image,
            expected_module_content: byte_content_identity(&bytes),
            module_bytes: bytes,
        }
    }

    #[test]
    fn registers_and_invokes_a_hashed_external_module() {
        let registry =
            PluginRegistry::new([decoder_definition("fixture.decode", PPM_DECODE.to_vec())])
                .unwrap();
        let transform = registry.find("fixture.decode").unwrap();
        assert_eq!(registry.artifact_identities().count(), 1);

        let input = OuterValue::plain(ValueData::Bytes(Arc::new(
            b"P3\n1 1\n255\n2 4 8\n".to_vec(),
        )));
        let output =
            prepare_plugin_invocation(transform, vec![(input, Span::default())], Span::default())
                .unwrap()
                .execute()
                .unwrap();

        let ValueData::Image(image) = output.data else {
            panic!("decoder did not return an image")
        };
        assert_eq!(image.to_vec(), [2, 4, 8, 255]);
    }

    #[test]
    fn rejects_tampering_collisions_and_invalid_exports_at_registration() {
        let mut tampered = decoder_definition("fixture.decode", PPM_DECODE.to_vec());
        tampered.expected_module_content = byte_content_identity(b"different module");
        assert!(
            PluginRegistry::new([tampered])
                .unwrap_err()
                .to_string()
                .contains("expected module content")
        );

        assert!(
            PluginRegistry::new([decoder_definition("ppm.decode", PPM_DECODE.to_vec())])
                .unwrap_err()
                .to_string()
                .contains("collides with a built-in")
        );

        let mut unsupported = decoder_definition("fixture.scalar", PPM_DECODE.to_vec());
        unsupported.result = PluginValueType::I64;
        assert!(
            PluginRegistry::new([unsupported])
                .unwrap_err()
                .to_string()
                .contains("unsupported scalar result")
        );

        let mut invalid_exports = PPM_DECODE.to_vec();
        let export = b"tima_reset";
        let start = invalid_exports
            .windows(export.len())
            .position(|window| window == export)
            .expect("fixture exports tima_reset");
        invalid_exports[start + export.len() - 1] = b'x';
        assert!(
            PluginRegistry::new([decoder_definition("fixture.decode", invalid_exports)])
                .unwrap_err()
                .to_string()
                .contains("invalid `tima_reset`")
        );
    }

    #[test]
    fn semantic_contract_and_module_artifact_identities_remain_distinct() {
        let ppm = PluginRegistry::new([decoder_definition("fixture.decode", PPM_DECODE.to_vec())])
            .unwrap();
        let png = PluginRegistry::new([decoder_definition("fixture.decode", PNG_DECODE.to_vec())])
            .unwrap();

        assert_eq!(
            ppm.find("fixture.decode").unwrap().identity(),
            png.find("fixture.decode").unwrap().identity()
        );
        assert_ne!(
            ppm.artifact_identities().next().unwrap(),
            png.artifact_identities().next().unwrap()
        );
    }
}
