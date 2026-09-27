use crate::diagnostic::Diagnostic;
use crate::identity::{ContentIdentity, byte_content_identity};
use crate::lineage::Lineage;
use crate::source::Span;

pub const ENVIRONMENT_CAPABILITY: &str = "environment";
pub const ASSET_CAPABILITY: &str = "asset";

/// Host-mediated external state available to Tima execution.
///
/// Tima never falls back to the process environment or filesystem. Histima
/// must provide an implementation explicitly, which keeps permissions,
/// dependency capture, and output policy at the runtime boundary.
pub trait RuntimeCapabilities {
    fn environment(&self, name: &str) -> Result<Vec<u8>, String>;

    fn read_asset(&self, locator: &str) -> Result<Vec<u8>, String> {
        Err(format!("asset `{locator}` is unavailable"))
    }

    fn write_asset(&self, locator: &str, _bytes: &[u8]) -> Result<(), String> {
        Err(format!("asset output `{locator}` is unavailable"))
    }
}

pub(crate) struct CapabilitySession<'a> {
    capabilities: Option<&'a dyn RuntimeCapabilities>,
    observations: Vec<Lineage>,
}

impl<'a> CapabilitySession<'a> {
    pub(crate) fn new(capabilities: Option<&'a dyn RuntimeCapabilities>) -> Self {
        Self {
            capabilities,
            observations: Vec::new(),
        }
    }

    pub(crate) fn environment_i64(&mut self, name: &str, span: Span) -> Result<i64, Diagnostic> {
        let Some(capabilities) = self.capabilities else {
            return Err(Diagnostic::error(
                "external environment access is unavailable in this transform context",
                span,
            )
            .with_note("the Histima host must explicitly provide the environment capability"));
        };
        let bytes = capabilities.environment(name).map_err(|error| {
            Diagnostic::error(
                format!("could not read environment dependency `{name}`: {error}"),
                span,
            )
        })?;
        let text = std::str::from_utf8(&bytes).map_err(|_| {
            Diagnostic::error(
                format!("environment dependency `{name}` is not valid UTF-8"),
                span,
            )
            .with_note("environment_i64 requires a UTF-8 decimal integer")
        })?;
        let value = text.parse::<i64>().map_err(|_| {
            Diagnostic::error(
                format!("environment dependency `{name}` is not an i64"),
                span,
            )
            .with_note(format!("observed value was {text:?}"))
        })?;
        self.observations.push(Lineage::external_observation(
            ENVIRONMENT_CAPABILITY,
            name.as_bytes(),
            byte_content_identity(&bytes),
        ));
        Ok(value)
    }

    pub(crate) fn finish(self) -> Vec<Lineage> {
        self.observations
    }
}

pub(crate) fn observe_dependency(
    capabilities: &dyn RuntimeCapabilities,
    capability: &str,
    key: &[u8],
) -> Result<ContentIdentity, String> {
    match capability {
        ENVIRONMENT_CAPABILITY => {
            let name = std::str::from_utf8(key)
                .map_err(|_| "recorded environment key is not valid UTF-8".to_owned())?;
            capabilities
                .environment(name)
                .map(|value| byte_content_identity(&value))
        }
        ASSET_CAPABILITY => {
            let locator = std::str::from_utf8(key)
                .map_err(|_| "recorded asset locator is not valid UTF-8".to_owned())?;
            capabilities
                .read_asset(locator)
                .map(|value| byte_content_identity(&value))
        }
        other => Err(format!(
            "runtime does not support the recorded `{other}` capability"
        )),
    }
}
