//! Helpers for the `iroh://<endpoint-id>` location URL scheme.
//!
//! Centralises the scheme literal and URL shape so a future scheme rename
//! is a single-file edit. The endpoint id base32 codec lives in
//! [`crate::seeder::keys`].
//!
//! Callers that filter mixed-scheme location URLs should gate on
//! [`matches`] before calling [`endpoint_id`].

use url::Url;

use crate::seeder::keys::{decode_endpoint_id, encode_endpoint_id};
use crate::share::Error;

/// Scheme for iroh location URLs.
pub const SCHEME: &str = "iroh";

/// Build `iroh://<base32-endpoint-id>` for a typed endpoint id.
pub fn build(id: &iroh::EndpointId) -> Url {
    // Infallible: scheme + valid base32 host always parses.
    Url::parse(&format!("{SCHEME}://{}", encode_endpoint_id(id)))
        .expect("iroh:// URL with valid base32 host always parses")
}

/// Build `iroh://<id>` from a wire-form string id (e.g. `SeedReceipt.endpoint_id`).
///
/// Validates the id by round-tripping through the base32 codec — returns
/// `Err` for a malformed id rather than silently producing a garbage URL.
pub fn build_from_id_str(id: &str) -> Result<Url, Error> {
    let parsed = decode_endpoint_id(id)?;
    Ok(build(&parsed))
}

/// `true` iff `url.scheme() == SCHEME`.
pub fn matches(url: &Url) -> bool {
    url.scheme() == SCHEME
}

/// Parse the endpoint id from an `iroh://<id>` URL host.
///
/// Returns `Ok(None)` for a bare `iroh://` (no host) so callers can fall
/// back to deriving the endpoint id from the location author's DID. `Err`
/// is returned only when a non-empty host fails to decode. Does **not**
/// check the scheme — gate with [`matches`] first.
pub fn endpoint_id(url: &Url) -> Result<Option<iroh::EndpointId>, Error> {
    match url.host_str() {
        Some(host) if !host.is_empty() => decode_endpoint_id(host).map(Some),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_round_trip() {
        let sk = iroh::SecretKey::from_bytes(&[7u8; 32]);
        let id = sk.public();
        let url = build(&id);
        assert_eq!(url.scheme(), SCHEME);
        let parsed = endpoint_id(&url).unwrap().unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn build_from_id_str_rejects_garbage() {
        // '1' is not in the base32 alphabet (a-z + 2-7).
        assert!(build_from_id_str("abc123").is_err());
    }

    #[test]
    fn build_from_id_str_round_trip() {
        let sk = iroh::SecretKey::from_bytes(&[7u8; 32]);
        let id = sk.public();
        let encoded = encode_endpoint_id(&id);
        let url = build_from_id_str(&encoded).unwrap();
        assert_eq!(endpoint_id(&url).unwrap().unwrap(), id);
    }

    #[test]
    fn endpoint_id_from_bare_url_is_none() {
        let url = Url::parse("iroh://").unwrap();
        assert_eq!(endpoint_id(&url).unwrap(), None);
    }

    #[test]
    fn endpoint_id_with_garbage_host_errors() {
        let url = Url::parse("iroh://abc123").unwrap();
        assert!(endpoint_id(&url).is_err());
    }

    #[test]
    fn matches_only_iroh_scheme() {
        assert!(matches(&Url::parse("iroh://abc").unwrap()));
        assert!(!matches(&Url::parse("https://example.com").unwrap()));
        assert!(!matches(&Url::parse("ipfs://Qm…").unwrap()));
    }
}
