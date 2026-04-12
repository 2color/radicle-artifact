//! Iroh endpoint configuration via environment variables.

use std::fmt;

use iroh::address_lookup::{DnsAddressLookup, PkarrPublisher};
use iroh::endpoint::presets::{self, Preset};
use url::Url;

use super::Error;

const ENV_IROH_PRESET: &str = "RADWORKS_IROH_PRESET";
const ENV_RELAY_URL: &str = "RADWORKS_RELAY_URL";
const ENV_PKARR_URL: &str = "RADWORKS_PKARR_URL";
const ENV_DNS_DOMAIN: &str = "RADWORKS_DNS_DOMAIN";

/// Iroh endpoint configuration.
///
/// Controls relay servers and discovery services for the endpoint.
/// Defaults to [`EndpointPreset::N0`] (n0's relay servers and DNS discovery).
///
/// Set `RADWORKS_IROH_PRESET=radworks` with `RADWORKS_RELAY_URL`,
/// `RADWORKS_PKARR_URL`, and `RADWORKS_DNS_DOMAIN` to use Radworks
/// infrastructure instead.
#[derive(Debug, Clone, Default)]
pub enum EndpointPreset {
    /// Use n0's relay servers and DNS discovery.
    #[default]
    N0,
    /// Use Radworks relay and discovery infrastructure.
    Radworks {
        /// Relay server URL.
        relay_url: Url,
        /// Pkarr relay URL for address publishing.
        pkarr_relay_url: Url,
        /// DNS origin domain for address lookup.
        dns_origin_domain: String,
    },
}

impl EndpointPreset {
    /// Build an [`EndpointPreset`] from environment variables.
    ///
    /// - `RADWORKS_IROH_PRESET`: `n0` (default) or `radworks`
    /// - When `radworks`:
    ///   - `RADWORKS_RELAY_URL`: relay server URL (required)
    ///   - `RADWORKS_PKARR_URL`: pkarr relay URL (required)
    ///   - `RADWORKS_DNS_DOMAIN`: DNS origin domain (required)
    pub fn from_env() -> Result<Self, Error> {
        Self::parse(|key| std::env::var(key).ok())
    }

    /// Parse preset configuration from a key-value lookup function.
    ///
    /// Useful for testing with custom environment mappings.
    pub fn parse(get: impl Fn(&str) -> Option<String>) -> Result<Self, Error> {
        let preset = get(ENV_IROH_PRESET).unwrap_or_default();

        match preset.as_str() {
            "" | "n0" => Ok(Self::N0),
            "radworks" => {
                let relay_url = require_url(&get, ENV_RELAY_URL)?;
                let pkarr_relay_url = require_url(&get, ENV_PKARR_URL)?;
                let dns_origin_domain = require_val(&get, ENV_DNS_DOMAIN)?;

                Ok(Self::Radworks {
                    relay_url,
                    pkarr_relay_url,
                    dns_origin_domain,
                })
            }
            other => Err(Error::Iroh(format!(
                "unknown {ENV_IROH_PRESET} value: {other:?} (expected \"n0\" or \"radworks\")"
            ))),
        }
    }
}

impl fmt::Display for EndpointPreset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::N0 => write!(f, "n0"),
            Self::Radworks { relay_url, .. } => write!(f, "radworks (relay={relay_url})"),
        }
    }
}

fn require_val(get: &impl Fn(&str) -> Option<String>, key: &str) -> Result<String, Error> {
    get(key)
        .ok_or_else(|| Error::Iroh(format!("{key} is required when {ENV_IROH_PRESET}=radworks")))
}

fn require_url(get: &impl Fn(&str) -> Option<String>, key: &str) -> Result<Url, Error> {
    let val = require_val(get, key)?;
    val.parse::<Url>()
        .map_err(|e| Error::Iroh(format!("{key} is not a valid URL: {e}")))
}

impl Preset for EndpointPreset {
    fn apply(self, builder: iroh::endpoint::Builder) -> iroh::endpoint::Builder {
        match self {
            Self::N0 => presets::N0.apply(builder),
            Self::Radworks {
                relay_url,
                pkarr_relay_url,
                dns_origin_domain,
            } => builder
                .address_lookup(PkarrPublisher::builder(pkarr_relay_url))
                .address_lookup(DnsAddressLookup::builder(dns_origin_domain))
                .relay_mode(iroh::RelayMode::custom([relay_url.into()])),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_n0() {
        let preset = EndpointPreset::default();
        assert!(matches!(preset, EndpointPreset::N0));
    }

    #[test]
    fn parse_empty_is_n0() {
        let preset = EndpointPreset::parse(|_| None).unwrap();
        assert!(matches!(preset, EndpointPreset::N0));
    }

    #[test]
    fn parse_explicit_n0() {
        let preset = EndpointPreset::parse(|key| match key {
            "RADWORKS_IROH_PRESET" => Some("n0".into()),
            _ => None,
        })
        .unwrap();
        assert!(matches!(preset, EndpointPreset::N0));
    }

    #[test]
    fn parse_radworks() {
        let preset = EndpointPreset::parse(|key| match key {
            "RADWORKS_IROH_PRESET" => Some("radworks".into()),
            "RADWORKS_RELAY_URL" => Some("https://relay.example.com".into()),
            "RADWORKS_PKARR_URL" => Some("https://pkarr.example.com".into()),
            "RADWORKS_DNS_DOMAIN" => Some("example.com".into()),
            _ => None,
        })
        .unwrap();
        assert!(matches!(preset, EndpointPreset::Radworks { .. }));
    }

    #[test]
    fn parse_radworks_missing_url() {
        let result = EndpointPreset::parse(|key| match key {
            "RADWORKS_IROH_PRESET" => Some("radworks".into()),
            _ => None,
        });
        assert!(matches!(result, Err(Error::Iroh(_))));
    }

    #[test]
    fn parse_unknown_value() {
        let result = EndpointPreset::parse(|key| match key {
            "RADWORKS_IROH_PRESET" => Some("foo".into()),
            _ => None,
        });
        assert!(matches!(result, Err(Error::Iroh(_))));
    }

    #[test]
    fn parse_invalid_url() {
        let result = EndpointPreset::parse(|key| match key {
            "RADWORKS_IROH_PRESET" => Some("radworks".into()),
            "RADWORKS_RELAY_URL" => Some("not a url".into()),
            "RADWORKS_PKARR_URL" => Some("https://pkarr.example.com".into()),
            "RADWORKS_DNS_DOMAIN" => Some("example.com".into()),
            _ => None,
        });
        assert!(matches!(result, Err(Error::Iroh(_))));
    }
}
