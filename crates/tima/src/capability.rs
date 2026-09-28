use crate::diagnostic::Diagnostic;
use crate::identity::{ContentIdentity, byte_content_identity};
use crate::lineage::{Lineage, LineageNode};
use crate::source::Span;

pub const ENVIRONMENT_CAPABILITY: &str = "environment";
pub const ASSET_CAPABILITY: &str = "asset";
pub const FILE_READ_CAPABILITY: &str = "file.read";
pub const HTTP_GET_CAPABILITY: &str = "http.get";

/// Host-mediated external state available to Tima execution.
///
/// Tima never falls back to the process environment or filesystem. Histima
/// must provide an implementation explicitly, which keeps permissions,
/// dependency capture, and output policy at the runtime boundary.
pub trait World {
    fn environment(&self, name: &str) -> Result<Vec<u8>, String>;

    fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        Err(format!("file `{path}` is unavailable"))
    }

    fn http_get(&self, url: &str) -> Result<Vec<u8>, String> {
        Err(format!("HTTP URL `{url}` is unavailable"))
    }

    fn read_asset(&self, locator: &str) -> Result<Vec<u8>, String> {
        Err(format!("asset `{locator}` is unavailable"))
    }

    fn write_asset(&self, locator: &str, _bytes: &[u8]) -> Result<(), String> {
        Err(format!("asset output `{locator}` is unavailable"))
    }
}

/// Compatibility name for the original host boundary. New code should call
/// this boundary [`World`] because it mediates all observable external state,
/// not only a bag of optional runtime callbacks.
pub use World as RuntimeCapabilities;

pub(crate) struct CapabilitySession<'a> {
    capabilities: Option<&'a dyn World>,
    observations: Vec<Lineage>,
}

impl<'a> CapabilitySession<'a> {
    pub(crate) fn new(capabilities: Option<&'a dyn World>) -> Self {
        Self {
            capabilities,
            observations: Vec::new(),
        }
    }

    pub(crate) fn environment_i64(&mut self, name: &str, span: Span) -> Result<i64, Diagnostic> {
        let text = self.environment(name, span)?;
        let value = text.parse::<i64>().map_err(|_| {
            Diagnostic::error(
                format!("environment dependency `{name}` is not an i64"),
                span,
            )
            .with_note(format!("observed value was {text:?}"))
        })?;
        Ok(value)
    }

    pub(crate) fn environment(&mut self, name: &str, span: Span) -> Result<String, Diagnostic> {
        let capabilities = self.world("environment", span)?;
        let bytes = capabilities.environment(name).map_err(|error| {
            Diagnostic::error(
                format!("could not read environment dependency `{name}`: {error}"),
                span,
            )
        })?;
        let text = String::from_utf8(bytes.clone()).map_err(|_| {
            Diagnostic::error(
                format!("environment dependency `{name}` is not valid UTF-8"),
                span,
            )
        })?;
        self.record_observation(ENVIRONMENT_CAPABILITY, name.as_bytes(), &bytes, span)?;
        Ok(text)
    }

    pub(crate) fn read_file(&mut self, path: &str, span: Span) -> Result<Vec<u8>, Diagnostic> {
        let capabilities = self.world("filesystem", span)?;
        let bytes = capabilities.read_file(path).map_err(|error| {
            Diagnostic::error(
                format!("could not read file dependency `{path}`: {error}"),
                span,
            )
        })?;
        self.record_observation(FILE_READ_CAPABILITY, path.as_bytes(), &bytes, span)?;
        Ok(bytes)
    }

    pub(crate) fn http_get(&mut self, url: &str, span: Span) -> Result<Vec<u8>, Diagnostic> {
        let capabilities = self.world("network", span)?;
        let bytes = capabilities.http_get(url).map_err(|error| {
            Diagnostic::error(
                format!("could not read HTTP dependency `{url}`: {error}"),
                span,
            )
        })?;
        self.record_observation(HTTP_GET_CAPABILITY, url.as_bytes(), &bytes, span)?;
        Ok(bytes)
    }

    fn record_observation(
        &mut self,
        capability: &str,
        key: &[u8],
        bytes: &[u8],
        span: Span,
    ) -> Result<(), Diagnostic> {
        let content = byte_content_identity(bytes);
        for existing in &self.observations {
            let LineageNode::ExternalObservation(existing) = existing.node() else {
                unreachable!("capability sessions contain only external observations")
            };
            if existing.capability.as_ref() == capability
                && existing.key.as_ref() == key
                && existing.observed_content != content
            {
                return Err(Diagnostic::error(
                    format!(
                        "external `{capability}` dependency {:?} changed during one transform invocation",
                        String::from_utf8_lossy(key)
                    ),
                    span,
                )
                .with_note(format!(
                    "first observed {}, then observed {content}",
                    existing.observed_content
                )));
            }
        }
        self.observations
            .push(Lineage::external_observation(capability, key, content));
        Ok(())
    }

    fn world(&self, capability: &str, span: Span) -> Result<&dyn World, Diagnostic> {
        self.capabilities.ok_or_else(|| {
            Diagnostic::error(
                format!("external {capability} access is unavailable in this transform context"),
                span,
            )
            .with_note("the Histima host must explicitly provide a World implementation")
        })
    }

    pub(crate) fn finish(self) -> Vec<Lineage> {
        self.observations
    }
}

pub(crate) fn observe_dependency(
    capabilities: &dyn World,
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
        FILE_READ_CAPABILITY => {
            let path = std::str::from_utf8(key)
                .map_err(|_| "recorded file path is not valid UTF-8".to_owned())?;
            capabilities
                .read_file(path)
                .map(|value| byte_content_identity(&value))
        }
        HTTP_GET_CAPABILITY => {
            let url = std::str::from_utf8(key)
                .map_err(|_| "recorded HTTP URL is not valid UTF-8".to_owned())?;
            capabilities
                .http_get(url)
                .map(|value| byte_content_identity(&value))
        }
        other => Err(format!(
            "runtime does not support the recorded `{other}` capability"
        )),
    }
}
