//! Iroh endpoint configuration via environment variables.

use std::fmt;

use iroh::address_lookup::{PkarrPublisher, PkarrResolver};
use iroh::endpoint::presets::{self, Preset};

use crate::Error;

const ENV_RELAY_HOSTS: &str = "IROH_RELAY_HOSTS";
const ENV_PKARR_URL: &str = "IROH_PKARR_URL";

const DEFAULT_RELAY_HOSTS: &str = "eu-1.relay.iroh.radicle.garden";
const DEFAULT_PKARR_URL: &str = "https://dns.iroh.radicle.garden/pkarr";

/// Iroh endpoint configuration.
///
/// Controls the relay server and discovery services for the endpoint. Each
/// value defaults to the Radicle infrastructure but can be overridden via
/// environment variable:
///
/// - `IROH_RELAY_HOSTS` (default `eu-1.relay.iroh.radicle.garden`) — comma-separated list of relay hosts, each served over `https://`
/// - `IROH_PKARR_URL` (default `https://dns.iroh.radicle.garden/pkarr`)
#[derive(Debug, Clone)]
pub struct EndpointConfig {
    relay_urls: Vec<iroh::RelayUrl>,
    pkarr_url: url::Url,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        // Parsing the compile-time defaults is infallible.
        Self {
            relay_urls: parse_relay_hosts(DEFAULT_RELAY_HOSTS, "DEFAULT_RELAY_HOSTS")
                .expect("valid DEFAULT_RELAY_HOSTS"),
            pkarr_url: DEFAULT_PKARR_URL.parse().expect("valid DEFAULT_PKARR_URL"),
        }
    }
}

impl EndpointConfig {
    /// Build an [`EndpointConfig`] from the `IROH_RELAY_HOSTS` and
    /// `IROH_PKARR_URL` environment variables, falling back to the Radicle
    /// defaults when a variable is unset or empty. A malformed value fails here
    /// so [`Preset::apply`] can consume the parsed values directly.
    pub fn from_env() -> Result<Self, Error> {
        Ok(Self {
            relay_urls: parse_relay_hosts(
                &env_or(ENV_RELAY_HOSTS, DEFAULT_RELAY_HOSTS),
                ENV_RELAY_HOSTS,
            )?,
            pkarr_url: parse_env(ENV_PKARR_URL, DEFAULT_PKARR_URL)?,
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

/// Parse a comma-separated list of relay hosts into URLs, serving each over
/// `https://`. Listing bare hosts avoids repeating the scheme per entry, which
/// is error-prone to maintain. Parse errors are attributed to `name` (the
/// source environment variable or constant).
fn parse_relay_hosts(value: &str, name: &str) -> Result<Vec<iroh::RelayUrl>, Error> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|host| {
            format!("https://{host}")
                .parse()
                .map_err(|e| Error::Iroh(format!("invalid {name} value {host:?}: {e}")))
        })
        .collect()
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
        let relay_urls = self
            .relay_urls
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");
        write!(f, "relay={} pkarr={}", relay_urls, self.pkarr_url)
    }
}

impl Preset for EndpointConfig {
    fn apply(self, builder: iroh::endpoint::Builder) -> iroh::endpoint::Builder {
        presets::Minimal
            .apply(builder)
            .address_lookup(PkarrPublisher::builder(self.pkarr_url.clone()))
            // Resolve peers over HTTPS to the pkarr server, rather than
            // unencrypted DNS. Relay and pkarr hostnames still use system DNS.
            .address_lookup(PkarrResolver::builder(self.pkarr_url))
            .relay_mode(iroh::RelayMode::custom(self.relay_urls))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_uses_radicle_endpoints() {
        // Also exercises the host-to-URL formatting in `Default`, guarding
        // against a typo'd default that would otherwise panic at startup.
        let config = EndpointConfig::default();
        assert_eq!(
            config.relay_urls,
            vec!["https://eu-1.relay.iroh.radicle.garden"
                .parse::<iroh::RelayUrl>()
                .unwrap()]
        );
        assert_eq!(config.pkarr_url, DEFAULT_PKARR_URL.parse().unwrap());
    }

    #[test]
    fn parse_relay_hosts_comma_separated() {
        let urls =
            parse_relay_hosts("relay1.example.org,relay2.example.org", "IROH_RELAY_HOSTS").unwrap();
        assert_eq!(urls.len(), 2);
        assert_eq!(
            urls[0],
            "https://relay1.example.org"
                .parse::<iroh::RelayUrl>()
                .unwrap()
        );
        assert_eq!(
            urls[1],
            "https://relay2.example.org"
                .parse::<iroh::RelayUrl>()
                .unwrap()
        );
    }

    #[test]
    fn parse_relay_hosts_rejects_malformed() {
        let result = parse_relay_hosts("not a host", "IROH_RELAY_HOSTS");
        assert!(matches!(result, Err(Error::Iroh(_))));
    }

    #[test]
    fn parse_env_rejects_malformed_value() {
        let result = parse_env::<url::Url>("IROH_UNSET_TEST_VAR", "not a url");
        assert!(matches!(result, Err(Error::Iroh(_))));
    }
}
