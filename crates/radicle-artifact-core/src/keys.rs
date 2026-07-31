//! Key conversion between radicle and iroh identities, plus the endpoint id type.
//!
//! Radicle and iroh both use ed25519 keys. These utilities convert between
//! the two representations, enabling a single identity to be used for both
//! Radicle COB operations and iroh-blobs networking.
//!
//! [`EndpointId`] is the project's wire-form newtype around [`iroh_base::EndpointId`].
//! Its canonical text form — used by `Display`, `FromStr`, `Status.endpoint_id`
//! over IPC, node logs, and COB locations — is the `radiroh://<base32>` URL.
//! The host slot is RFC 4648 base32 (lowercase, no padding); the `radiroh://`
//! scheme disambiguates the encoding from iroh's own `Display`, which uses
//! a different alphabet and must never appear in user-facing output.

use std::fmt;
use std::str::FromStr;

use multibase::Base;
use radicle::crypto::ssh::keystore::Keystore;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use url::Url;

use crate::Error;

/// Codec for the URL host slot: RFC 4648 base32, lowercase, no padding.
const HOST_BASE: Base = Base::Base32Lower;

/// Project endpoint identifier.
///
/// Newtype wrapper around [`iroh_base::EndpointId`] whose `Display` / `FromStr`
/// use the `radiroh://<base32>` URL form — the canonical text representation
/// across the codebase (IPC, logs, COB locations, CLI output). Convert to
/// the underlying iroh type only at the iroh-blobs API boundary via
/// [`EndpointId::into_inner`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EndpointId(iroh_base::EndpointId);

impl EndpointId {
    /// URL scheme of the canonical text form.
    pub const URL_SCHEME: &'static str = "radiroh";

    /// Legacy URL scheme retained only so `rad-artifact reconcile` can
    /// sweep pre-`radiroh://` locations out of COBs under our DID. Not
    /// accepted on read by [`EndpointId::from_url`].
    const LEGACY_URL_SCHEME: &'static str = "iroh";

    /// Borrow the underlying iroh endpoint id (for iroh-blobs APIs).
    pub fn as_inner(&self) -> &iroh_base::EndpointId {
        &self.0
    }

    /// Consume into the underlying iroh endpoint id.
    pub fn into_inner(self) -> iroh_base::EndpointId {
        self.0
    }

    /// Build the canonical `radiroh://<base32>` URL representation.
    pub fn to_url(&self) -> Url {
        // Infallible: scheme + base32 host always parses.
        Url::parse(&format!(
            "{}://{}",
            Self::URL_SCHEME,
            HOST_BASE.encode(self.0.as_bytes())
        ))
        .expect("radiroh:// URL with valid base32 host always parses")
    }

    /// Parse a `radiroh://<id>` URL into an endpoint id.
    ///
    /// Returns `Ok(None)` for a bare `radiroh://` (no host) so callers
    /// can fall back to deriving the endpoint id from the location
    /// author's DID. Legacy `iroh://` URLs are rejected — use
    /// [`EndpointId::is_legacy_endpoint_url`] to flag them for sweep.
    pub fn from_url(url: &Url) -> Result<Option<Self>, Error> {
        if url.scheme() != Self::URL_SCHEME {
            return Err(Error::Key(format!(
                "expected {}:// scheme, got '{}'",
                Self::URL_SCHEME,
                url.scheme()
            )));
        }
        match url.host_str() {
            Some(host) if !host.is_empty() => Self::from_base32(host).map(Some),
            _ => Ok(None),
        }
    }

    /// `true` iff `url.scheme()` is the canonical endpoint URL scheme
    /// (`radiroh://`).
    pub fn is_endpoint_url(url: &Url) -> bool {
        url.scheme() == Self::URL_SCHEME
    }

    /// Whether `url` advertises this endpoint as a provider.
    ///
    /// A bare `radiroh://` (no host) matches any endpoint — it resolves
    /// to the location author's DID-derived endpoint. A hosted
    /// `radiroh://<id>` matches only when `<id>` equals this endpoint.
    /// Non-endpoint URLs (wrong scheme, malformed host) never match.
    pub fn matches_url(&self, url: &Url) -> bool {
        match Self::from_url(url) {
            Ok(Some(id)) => id == *self,
            Ok(None) => true,
            Err(_) => false,
        }
    }

    /// `true` iff `url.scheme()` is the pre-rename `iroh://` scheme.
    ///
    /// Used only by `rad-artifact reconcile` to sweep legacy locations
    /// out of COBs under our DID. Production read/write paths must use
    /// [`EndpointId::is_endpoint_url`].
    pub fn is_legacy_endpoint_url(url: &Url) -> bool {
        url.scheme() == Self::LEGACY_URL_SCHEME
    }

    /// Decode the base32 encoded endpoint ID.
    fn from_base32(host: &str) -> Result<Self, Error> {
        let bytes = HOST_BASE
            .decode(host)
            .map_err(|e| Error::Key(format!("invalid base32 endpoint id '{host}': {e}")))?;
        let arr: [u8; 32] = bytes.try_into().map_err(|v: Vec<u8>| {
            Error::Key(format!(
                "endpoint id must decode to 32 bytes, got {}",
                v.len()
            ))
        })?;
        let inner = iroh_base::EndpointId::from_bytes(&arr)
            .map_err(|e| Error::Key(format!("invalid endpoint id bytes: {e}")))?;
        Ok(Self(inner))
    }
}

impl From<iroh_base::EndpointId> for EndpointId {
    fn from(id: iroh_base::EndpointId) -> Self {
        Self(id)
    }
}

impl From<EndpointId> for iroh_base::EndpointId {
    fn from(id: EndpointId) -> Self {
        id.0
    }
}

impl fmt::Display for EndpointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}://{}",
            Self::URL_SCHEME,
            HOST_BASE.encode(self.0.as_bytes())
        )
    }
}

// Debug deliberately defers to Display so logs always see the URL form —
// never iroh's bare `Debug`, which renders the inner key differently.
impl fmt::Debug for EndpointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EndpointId({self})")
    }
}

impl FromStr for EndpointId {
    type Err = Error;

    /// Parse the canonical `radiroh://<base32>` URL form.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let url = Url::parse(s).map_err(|e| Error::Key(format!("invalid URL '{s}': {e}")))?;
        Self::from_url(&url)?
            .ok_or_else(|| Error::Key(format!("URL '{s}' has no endpoint id host")))
    }
}

// Wire form is the canonical `radiroh://<base32>` URL string, matching
// `Display`/`FromStr`. Deserialization validates, so a malformed endpoint
// id fails at the protocol decode boundary rather than downstream.
impl Serialize for EndpointId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for EndpointId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(de::Error::custom)
    }
}

/// Derive an [`EndpointId`] from a radicle DID's public key.
///
/// Both are ed25519 — the 32-byte key is used directly. This allows
/// looking up iroh endpoints by the DID of the peer that registered
/// a `radiroh://` location in an artifact COB.
impl TryFrom<&radicle::identity::Did> for EndpointId {
    type Error = Error;

    fn try_from(did: &radicle::identity::Did) -> Result<Self, Self::Error> {
        let bytes = did.as_key().into_inner();
        let inner = iroh_base::EndpointId::from_bytes(&bytes)
            .map_err(|e| Error::Key(format!("invalid iroh public key from DID: {e}")))?;
        Ok(EndpointId(inner))
    }
}

/// Convert a radicle secret key to an iroh secret key.
///
/// Both are ed25519 — the 32-byte seed is extracted from the radicle key
/// and used to construct the iroh key. The resulting iroh endpoint ID
/// will match the radicle DID's public key, so peers can derive the
/// iroh address from the DID alone.
pub fn radicle_secret_to_iroh(
    keystore: &Keystore,
    passphrase: Option<radicle::crypto::ssh::keystore::Passphrase>,
) -> Result<iroh_base::SecretKey, Error> {
    let sk = keystore
        .secret_key(passphrase)
        .map_err(|e| Error::Key(format!("failed to read radicle secret key: {e}")))?
        .ok_or_else(|| Error::Key("radicle secret key not found".into()))?;

    Ok(iroh_base::SecretKey::from_bytes(sk.as_bytes()))
}

#[cfg(test)]
mod tests {
    use radicle::crypto::ssh::keystore::Passphrase;

    use super::*;

    fn fixed_id() -> EndpointId {
        iroh_base::SecretKey::from_bytes(&[7u8; 32]).public().into()
    }

    #[test]
    fn radicle_and_iroh_keys_share_same_public_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let keystore = Keystore::new(&tmp);
        keystore
            .init("test", None, radicle::crypto::Seed::new([1u8; 32]))
            .unwrap();

        let iroh_sk = radicle_secret_to_iroh(&keystore, None).unwrap();

        let radicle_pk = keystore.public_key().unwrap().unwrap();
        let iroh_pk = iroh_sk.public();

        assert_eq!(&radicle_pk.into_inner(), iroh_pk.as_bytes());
    }

    #[test]
    fn display_is_endpoint_url() {
        let id = fixed_id();
        let s = id.to_string();
        assert!(s.starts_with("radiroh://"));
        // The portion after the scheme is the base32 host.
        assert!(!s["radiroh://".len()..].is_empty());
    }

    #[test]
    fn display_differs_from_iroh_default() {
        // Regression guard: the newtype's Display must not pass through
        // to iroh's own `Display`, which uses a different encoding.
        let inner = iroh_base::SecretKey::from_bytes(&[7u8; 32]).public();
        let wrapped = EndpointId::from(inner);
        assert_ne!(wrapped.to_string(), inner.to_string());
    }

    #[test]
    fn url_round_trip() {
        let id = fixed_id();
        let url = id.to_url();
        let parsed = EndpointId::from_url(&url).unwrap().unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn fromstr_round_trip() {
        let id = fixed_id();
        let s = id.to_string();
        let parsed: EndpointId = s.parse().unwrap();
        assert_eq!(parsed, id);
        // Re-formatting produces the same string bit-for-bit.
        assert_eq!(parsed.to_string(), s);
    }

    #[test]
    fn from_url_bare_is_none() {
        let url = Url::parse("radiroh://").unwrap();
        assert_eq!(EndpointId::from_url(&url).unwrap(), None);
    }

    #[test]
    fn from_url_wrong_scheme_errors() {
        let url = Url::parse("https://example.com").unwrap();
        assert!(EndpointId::from_url(&url).is_err());
    }

    #[test]
    fn from_url_garbage_host_errors() {
        // '1' is not in the base32 alphabet (a-z + 2-7).
        let url = Url::parse("radiroh://abc123").unwrap();
        assert!(EndpointId::from_url(&url).is_err());
    }

    #[test]
    fn from_url_rejects_legacy_iroh_scheme() {
        // Hard-break regression: legacy `iroh://` URLs must not parse,
        // so an accidental relaxation of the rename trips this test.
        let bare = Url::parse("iroh://").unwrap();
        assert!(EndpointId::from_url(&bare).is_err());
        let id = fixed_id();
        let with_host = Url::parse(&format!(
            "iroh://{}",
            HOST_BASE.encode(id.as_inner().as_bytes())
        ))
        .unwrap();
        assert!(EndpointId::from_url(&with_host).is_err());
    }

    #[test]
    fn is_endpoint_url_only_matches_endpoint_scheme() {
        assert!(EndpointId::is_endpoint_url(
            &Url::parse("radiroh://abc").unwrap()
        ));
        assert!(!EndpointId::is_endpoint_url(
            &Url::parse("https://example.com").unwrap()
        ));
        // Legacy scheme is *not* an endpoint URL on read.
        assert!(!EndpointId::is_endpoint_url(
            &Url::parse("iroh://abc").unwrap()
        ));
    }

    #[test]
    fn is_legacy_endpoint_url_matches_only_iroh_scheme() {
        assert!(EndpointId::is_legacy_endpoint_url(
            &Url::parse("iroh://abc").unwrap()
        ));
        assert!(EndpointId::is_legacy_endpoint_url(
            &Url::parse("iroh://").unwrap()
        ));
        assert!(!EndpointId::is_legacy_endpoint_url(
            &Url::parse("radiroh://abc").unwrap()
        ));
        assert!(!EndpointId::is_legacy_endpoint_url(
            &Url::parse("https://example.com").unwrap()
        ));
    }

    #[test]
    fn matches_url_rules() {
        let id = fixed_id();
        let other = iroh_base::SecretKey::from_bytes(&[9u8; 32]).public().into();
        // Bare radiroh:// matches any endpoint (DID-derived fallback).
        assert!(id.matches_url(&Url::parse("radiroh://").unwrap()));
        // Hosted matches only the same endpoint.
        assert!(id.matches_url(&id.to_url()));
        assert!(!id.matches_url(&EndpointId::to_url(&other)));
        // Wrong scheme and malformed host never match.
        assert!(!id.matches_url(&Url::parse("https://example.com").unwrap()));
        assert!(!id.matches_url(&Url::parse("radiroh://abc123").unwrap()));
    }

    #[test]
    fn encrypted_keystore_requires_passphrase() {
        let tmp = tempfile::tempdir().unwrap();
        let keystore = Keystore::new(&tmp);
        keystore
            .init(
                "test",
                Some(Passphrase::new("hunter2".into())),
                radicle::crypto::Seed::new([2u8; 32]),
            )
            .unwrap();

        // Fails without passphrase
        assert!(radicle_secret_to_iroh(&keystore, None).is_err());

        // Succeeds with correct passphrase and produces matching public key
        let iroh_sk =
            radicle_secret_to_iroh(&keystore, Some(Passphrase::new("hunter2".into()))).unwrap();
        let radicle_pk = keystore.public_key().unwrap().unwrap();
        assert_eq!(&radicle_pk.into_inner(), iroh_sk.public().as_bytes());
    }
}
