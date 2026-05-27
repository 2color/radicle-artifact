//! Iroh endpoint configuration via environment variables.

use std::fmt;

use iroh::address_lookup::{DnsAddressLookup, PkarrPublisher};
use iroh::endpoint::presets::{self, Preset};

use super::Error;

const ENV_RELAY_URL: &str = "IROH_RELAY_URL";
const ENV_PKARR_URL: &str = "IROH_PKARR_URL";
const ENV_DNS_ENDPOINT_ORIGIN: &str = "IROH_DNS_ENDPOINT_ORIGIN";

const DEFAULT_RELAY_URL: &str = "https://relay.radworks.xyz";
const DEFAULT_PKARR_URL: &str = "https://dns.radworks.xyz/pkarr";
const DEFAULT_DNS_ENDPOINT_ORIGIN: &str = "dns.radworks.xyz";

/// Iroh endpoint configuration.
///
/// Controls the relay server and discovery services for the endpoint. Each
/// value defaults to the Radworks infrastructure but can be overridden via
/// environment variable:
///
/// - `IROH_RELAY_URL` (default `https://relay.radworks.xyz`)
/// - `IROH_PKARR_URL` (default `https://dns.radworks.xyz/pkarr`)
/// - `IROH_DNS_ENDPOINT_ORIGIN` (default `dns.radworks.xyz`)
#[derive(Debug, Clone)]
pub struct EndpointConfig {
    relay_url: iroh::RelayUrl,
    pkarr_url: url::Url,
    dns_endpoint_origin: String,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        // Parsing compile-time constants is infallible.
        Self {
            relay_url: DEFAULT_RELAY_URL.parse().expect("valid DEFAULT_RELAY_URL"),
            pkarr_url: DEFAULT_PKARR_URL.parse().expect("valid DEFAULT_PKARR_URL"),
            dns_endpoint_origin: DEFAULT_DNS_ENDPOINT_ORIGIN.to_owned(),
        }
    }
}

impl EndpointConfig {
    /// Build an [`EndpointConfig`] from the `IROH_RELAY_URL`, `IROH_PKARR_URL`
    /// and `IROH_DNS_ENDPOINT_ORIGIN` environment variables, falling back to the
    /// Radworks defaults when a variable is unset or empty. A malformed URL
    /// fails here so [`Preset::apply`] can consume the parsed values directly.
    pub fn from_env() -> Result<Self, Error> {
        Ok(Self {
            relay_url: parse_env(ENV_RELAY_URL, DEFAULT_RELAY_URL)?,
            pkarr_url: parse_env(ENV_PKARR_URL, DEFAULT_PKARR_URL)?,
            dns_endpoint_origin: env_or(ENV_DNS_ENDPOINT_ORIGIN, DEFAULT_DNS_ENDPOINT_ORIGIN),
        })
    }
}

/// Read an environment variable, falling back to `default` when unset or empty.
fn env_or(name: &str, default: &str) -> String {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => value,
        _ => default.to_owned(),
    }
}

/// Read and parse an environment variable, falling back to `default` when unset
/// or empty. Returns [`Error::Iroh`] if the value fails to parse.
fn parse_env<T>(name: &str, default: &str) -> Result<T, Error>
where
    T: std::str::FromStr,
    T::Err: fmt::Display,
{
    let value = env_or(name, default);
    value
        .parse()
        .map_err(|e| Error::Iroh(format!("invalid {name} value {value:?}: {e}")))
}

impl fmt::Display for EndpointConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "relay={} pkarr={} dns={}",
            self.relay_url, self.pkarr_url, self.dns_endpoint_origin
        )
    }
}

impl Preset for EndpointConfig {
    fn apply(self, builder: iroh::endpoint::Builder) -> iroh::endpoint::Builder {
        presets::Minimal
            .apply(builder)
            .address_lookup(PkarrPublisher::builder(self.pkarr_url))
            .address_lookup(DnsAddressLookup::builder(self.dns_endpoint_origin))
            .relay_mode(iroh::RelayMode::custom([self.relay_url]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_uses_radworks_endpoints() {
        // Also exercises the constant parsing in `Default`, guarding against a
        // typo'd default that would otherwise panic at startup.
        let config = EndpointConfig::default();
        assert_eq!(config.relay_url, DEFAULT_RELAY_URL.parse().unwrap());
        assert_eq!(config.pkarr_url, DEFAULT_PKARR_URL.parse().unwrap());
        assert_eq!(config.dns_endpoint_origin, DEFAULT_DNS_ENDPOINT_ORIGIN);
    }

    #[test]
    fn parse_env_rejects_malformed_value() {
        let result = parse_env::<url::Url>("IROH_UNSET_TEST_VAR", "not a url");
        assert!(matches!(result, Err(Error::Iroh(_))));
    }
}
