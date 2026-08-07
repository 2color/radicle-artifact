//! Iroh endpoint configuration via environment variables.

use std::fmt;

use iroh::address_lookup::{PkarrPublisher, PkarrResolver};
use iroh::endpoint::presets::{self, Preset};

use crate::Error;

const ENV_RELAYS: &str = "IROH_RELAYS";
const ENV_PKARR_URLS: &str = "IROH_PKARR_URLS";

const DEFAULT_RELAYS: &str = "eu-1.relay.iroh.radicle.garden,\
    1.eu.relay.iroh.radicle.network,\
    1.us.relay.iroh.radicle.network";
const DEFAULT_PKARR_URLS: &str = "https://dns.iroh.radicle.garden/pkarr,\
    https://1.eu.dns.iroh.radicle.network/pkarr,\
    https://1.us.dns.iroh.radicle.network/pkarr";

/// Iroh endpoint configuration.
///
/// Controls the relay servers and discovery services for the endpoint. Each
/// value defaults to the Radicle infrastructure but can be overridden via
/// environment variable. Both accept a comma-separated list, so a deployment
/// can name more than one server for redundancy. Iroh relays through the
/// fastest of the given relays and falls back to the others, but it publishes
/// to and resolves from every pkarr server:
///
/// - `IROH_RELAYS` — relay hosts, each served over `https://`.
///   Default: `eu-1.relay.iroh.radicle.garden`,
///   `1.eu.relay.iroh.radicle.network`, `1.us.relay.iroh.radicle.network`
/// - `IROH_PKARR_URLS` — pkarr server URLs, used for both publishing and
///   resolving. Default: `https://dns.iroh.radicle.garden/pkarr`,
///   `https://1.eu.dns.iroh.radicle.network/pkarr`,
///   `https://1.us.dns.iroh.radicle.network/pkarr`
#[derive(Debug, Clone)]
pub struct EndpointConfig {
    relay_urls: Vec<iroh::RelayUrl>,
    pkarr_urls: Vec<url::Url>,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        // Parsing the compile-time defaults is infallible.
        Self {
            relay_urls: parse_relays(DEFAULT_RELAYS, "DEFAULT_RELAYS")
                .expect("valid DEFAULT_RELAYS"),
            pkarr_urls: parse_urls(DEFAULT_PKARR_URLS, "DEFAULT_PKARR_URLS")
                .expect("valid DEFAULT_PKARR_URLS"),
        }
    }
}

impl EndpointConfig {
    /// Build an [`EndpointConfig`] from the `IROH_RELAYS` and
    /// `IROH_PKARR_URLS` environment variables, falling back to the Radicle
    /// defaults when a variable is unset or empty. A value that is malformed, or
    /// that lists no entries, fails here so [`Preset::apply`] can consume the
    /// parsed values directly.
    pub fn from_env() -> Result<Self, Error> {
        Ok(Self {
            relay_urls: parse_relays(&env_or(ENV_RELAYS, DEFAULT_RELAYS), ENV_RELAYS)?,
            pkarr_urls: parse_urls(&env_or(ENV_PKARR_URLS, DEFAULT_PKARR_URLS), ENV_PKARR_URLS)?,
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

/// Split a comma-separated list, trimming entries and dropping empty ones.
fn entries(value: &str) -> impl Iterator<Item = &str> {
    value.split(',').map(str::trim).filter(|s| !s.is_empty())
}

/// Parse a comma-separated list of relay hosts into URLs, serving each over
/// `https://`. Listing bare hosts avoids repeating the scheme per entry, which
/// is error-prone to maintain. Parse errors are attributed to `name` (the
/// source environment variable or constant).
fn parse_relays(value: &str, name: &str) -> Result<Vec<iroh::RelayUrl>, Error> {
    let urls = entries(value)
        .map(|host| parse_relay_host(host, name))
        .collect::<Result<Vec<_>, _>>()?;

    non_empty(urls, name)
}

/// Parse one bare host into an `https://` URL. An entry that spells out a
/// scheme still parses, as `https://https//host`, which puts the node on a
/// relay it can never reach, so reject it and name the expected form.
fn parse_relay_host(host: &str, name: &str) -> Result<iroh::RelayUrl, Error> {
    if host.contains("://") {
        return Err(Error::Iroh(format!(
            "invalid {name} value {host:?}: expected a host without a scheme"
        )));
    }

    format!("https://{host}")
        .parse()
        .map_err(|e| Error::Iroh(format!("invalid {name} value {host:?}: {e}")))
}

/// Parse a comma-separated list of full URLs. Unlike relay hosts these carry a
/// path (`/pkarr`), so each entry spells out its scheme.
fn parse_urls(value: &str, name: &str) -> Result<Vec<url::Url>, Error> {
    let urls = entries(value)
        .map(|entry| parse_http_url(entry, name))
        .collect::<Result<Vec<_>, _>>()?;

    non_empty(urls, name)
}

/// Parse one URL and require an `https` or `http` scheme. Any other scheme
/// still parses, but iroh only logs a warning when it cannot publish, so the
/// node would run on unreachable discovery. `http` stays allowed for a pkarr
/// server on the local network.
fn parse_http_url(entry: &str, name: &str) -> Result<url::Url, Error> {
    let url: url::Url = entry
        .parse()
        .map_err(|e| Error::Iroh(format!("invalid {name} value {entry:?}: {e}")))?;

    match url.scheme() {
        "https" | "http" => Ok(url),
        scheme => Err(Error::Iroh(format!(
            "invalid {name} value {entry:?}: expected scheme https or http, found {scheme}"
        ))),
    }
}

/// Reject a value that lists no entries, such as `" "` or `","`. Accepting it
/// would leave the node without relays, or without discovery, and unreachable
/// with no error to show why.
fn non_empty<T>(values: Vec<T>, name: &str) -> Result<Vec<T>, Error> {
    if values.is_empty() {
        return Err(Error::Iroh(format!("{name} lists no entries")));
    }
    Ok(values)
}

impl fmt::Display for EndpointConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "relay={} pkarr={}",
            join(&self.relay_urls),
            join(&self.pkarr_urls)
        )
    }
}

fn join<T: fmt::Display>(values: &[T]) -> String {
    values
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

impl Preset for EndpointConfig {
    fn apply(self, builder: iroh::endpoint::Builder) -> iroh::endpoint::Builder {
        let mut builder = presets::Minimal.apply(builder);
        // Publish to and resolve from every pkarr server for redundancy; iroh
        // combines the address lookup services.
        for url in self.pkarr_urls {
            builder = builder
                .address_lookup(PkarrPublisher::builder(url.clone()))
                // Resolve peers over HTTPS to the pkarr server, rather than
                // unencrypted DNS. Relay and pkarr hostnames still use system DNS.
                .address_lookup(PkarrResolver::builder(url));
        }
        builder.relay_mode(iroh::RelayMode::custom(self.relay_urls))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_parses_radicle_endpoints() {
        // `Default` panics on a constant that fails to parse, so this guards
        // startup. Spelling out every entry also catches a dropped comma and a
        // typo'd host, both of which still parse: the node would then run on a
        // dead relay or a pkarr server that never answers, and iroh only logs a
        // warning when publishing fails.
        let config = EndpointConfig::default();
        assert_eq!(
            config.relay_urls,
            [
                "https://eu-1.relay.iroh.radicle.garden",
                "https://1.eu.relay.iroh.radicle.network",
                "https://1.us.relay.iroh.radicle.network",
            ]
            .map(|u| u.parse::<iroh::RelayUrl>().unwrap())
        );
        assert_eq!(
            config.pkarr_urls,
            [
                "https://dns.iroh.radicle.garden/pkarr",
                "https://1.eu.dns.iroh.radicle.network/pkarr",
                "https://1.us.dns.iroh.radicle.network/pkarr",
            ]
            .map(|u| u.parse::<url::Url>().unwrap())
        );
    }

    #[test]
    fn parse_relays_trims_and_skips_empty() {
        let urls = parse_relays("  a.org , , b.org ,", "IROH_RELAYS").unwrap();
        assert_eq!(
            urls,
            vec![
                "https://a.org".parse::<iroh::RelayUrl>().unwrap(),
                "https://b.org".parse::<iroh::RelayUrl>().unwrap(),
            ]
        );
    }

    #[test]
    fn parse_urls_keeps_scheme_and_path() {
        let urls = parse_urls(
            "https://a.example.org/pkarr, https://b.example.org/pkarr",
            "IROH_PKARR_URLS",
        )
        .unwrap();
        assert_eq!(
            urls,
            ["https://a.example.org/pkarr", "https://b.example.org/pkarr"]
                .map(|u| u.parse::<url::Url>().unwrap())
        );
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(matches!(
            parse_relays("not a host", "IROH_RELAYS"),
            Err(Error::Iroh(_))
        ));
        assert!(matches!(
            parse_urls("not a url", "IROH_PKARR_URLS"),
            Err(Error::Iroh(_))
        ));
    }

    #[test]
    fn parse_relays_rejects_entries_with_a_scheme() {
        // `https://https://a.org` parses, leaving the host as `https` and the
        // rest as a path, so only this check stops a node from starting on a
        // relay it can never reach. Pasting a full URL is the likely mistake
        // after the rename from `IROH_RELAY_URL`.
        for value in ["https://a.org", "http://a.org"] {
            assert!(matches!(
                parse_relays(value, "IROH_RELAYS"),
                Err(Error::Iroh(_))
            ));
        }
    }

    #[test]
    fn parse_urls_rejects_other_schemes() {
        // A scheme typo parses as a valid URL, so this check is what stops the
        // node from starting with discovery it can never reach.
        for value in ["htp://a.example.org/pkarr", "ftp://a.example.org/pkarr"] {
            assert!(matches!(
                parse_urls(value, "IROH_PKARR_URLS"),
                Err(Error::Iroh(_))
            ));
        }
    }

    #[test]
    fn parse_rejects_lists_without_entries() {
        // `env_or` only falls back when the variable is exactly empty, so a
        // value of whitespace or commas reaches the parsers and must be
        // rejected instead of yielding no relays and no discovery.
        for value in [" ", ","] {
            assert!(matches!(
                parse_relays(value, "IROH_RELAYS"),
                Err(Error::Iroh(_))
            ));
            assert!(matches!(
                parse_urls(value, "IROH_PKARR_URLS"),
                Err(Error::Iroh(_))
            ));
        }
    }
}
