//! Iroh endpoint configuration via environment variables.

use std::fmt;

use iroh::address_lookup::{DnsAddressLookup, PkarrPublisher};
use iroh::endpoint::presets::{self, Preset};

use super::Error;

const ENV_IROH_PRESET: &str = "RADWORKS_IROH_PRESET";

const RADWORKS_RELAY_URL: &str = "https://relay.radworks.xyz";
const RADWORKS_PKARR_URL: &str = "https://dns.radworks.xyz/pkarr";
const RADWORKS_DNS_DOMAIN: &str = "dns.radworks.xyz";

/// Iroh endpoint configuration.
///
/// Controls relay servers and discovery services for the endpoint.
/// Defaults to [`EndpointPreset::Radworks`] using the Radworks relay and
/// DNS infrastructure. Set `RADWORKS_IROH_PRESET=n0` to use n0's
/// infrastructure instead.
#[derive(Debug, Clone, Default)]
pub enum EndpointPreset {
    /// Use Radworks relay and discovery infrastructure.
    #[default]
    Radworks,
    /// Use n0's relay servers and DNS discovery.
    N0,
}

impl EndpointPreset {
    /// Build an [`EndpointPreset`] from the `RADWORKS_IROH_PRESET` environment
    /// variable. Accepts `radworks` (default when unset) or `n0`.
    pub fn from_env() -> Result<Self, Error> {
        match std::env::var(ENV_IROH_PRESET).as_deref() {
            Ok("" | "radworks") | Err(_) => Ok(Self::Radworks),
            Ok("n0") => Ok(Self::N0),
            Ok(other) => Err(Error::Iroh(format!(
                "unknown {ENV_IROH_PRESET} value: {other:?} (expected \"radworks\" or \"n0\")"
            ))),
        }
    }
}

impl fmt::Display for EndpointPreset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Radworks => write!(f, "radworks (relay={RADWORKS_RELAY_URL})"),
            Self::N0 => write!(f, "n0"),
        }
    }
}

impl Preset for EndpointPreset {
    fn apply(self, builder: iroh::endpoint::Builder) -> iroh::endpoint::Builder {
        match self {
            Self::Radworks => builder
                .address_lookup(PkarrPublisher::builder(
                    RADWORKS_PKARR_URL
                        .parse()
                        .expect("valid RADWORKS_PKARR_URL"),
                ))
                .address_lookup(DnsAddressLookup::builder(RADWORKS_DNS_DOMAIN.to_owned()))
                .relay_mode(iroh::RelayMode::custom([RADWORKS_RELAY_URL
                    .parse::<iroh::RelayUrl>()
                    .expect("valid RADWORKS_RELAY_URL")])),
            Self::N0 => presets::N0.apply(builder),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_radworks() {
        assert!(matches!(EndpointPreset::default(), EndpointPreset::Radworks));
    }
}
