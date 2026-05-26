//! Key conversion between radicle and iroh identities, plus the endpoint id type.
//!
//! Radicle and iroh both use ed25519 keys. These utilities convert between
//! the two representations, enabling a single identity to be used for both
//! Radicle COB operations and iroh-blobs networking.
//!
//! [`EndpointId`] is the project's wire-form newtype around [`iroh::EndpointId`].
//! Its canonical text form — used by `Display`, `FromStr`, `Status.endpoint_id`
//! over IPC, node logs, and COB locations — is the `iroh://<base32>` URL.
//! The host slot is RFC 4648 base32 (lowercase, no padding); the `iroh://`
//! scheme disambiguates the encoding from iroh's own `Display`, which uses
//! a different alphabet and must never appear in user-facing output.

use std::fmt;
use std::str::FromStr;

use multibase::Base;
use radicle::crypto::ssh::keystore::Keystore;
use url::Url;

use crate::share::Error;

/// Codec for the URL host slot: RFC 4648 base32, lowercase, no padding.
const HOST_BASE: Base = Base::Base32Lower;

/// Project endpoint identifier.
///
/// Newtype wrapper around [`iroh::EndpointId`] whose `Display` / `FromStr`
/// use the `iroh://<base32>` URL form — the canonical text representation
/// across the codebase (IPC, logs, COB locations, CLI output). Convert to
/// the underlying iroh type only at the iroh-blobs API boundary via
/// [`EndpointId::into_inner`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EndpointId(iroh::EndpointId);

impl EndpointId {
    /// URL scheme of the canonical text form.
    pub const URL_SCHEME: &'static str = "iroh";

    /// Borrow the underlying iroh endpoint id (for iroh-blobs APIs).
    pub fn as_inner(&self) -> &iroh::EndpointId {
        &self.0
    }

    /// Consume into the underlying iroh endpoint id.
    pub fn into_inner(self) -> iroh::EndpointId {
        self.0
    }

    /// Build the canonical `iroh://<base32>` URL representation.
    pub fn to_url(&self) -> Url {
        // Infallible: scheme + base32 host always parses.
        Url::parse(&format!(
            "{}://{}",
            Self::URL_SCHEME,
            HOST_BASE.encode(self.0.as_bytes())
        ))
        .expect("iroh:// URL with valid base32 host always parses")
    }

    /// Parse an `iroh://<id>` URL into an endpoint id.
    ///
    /// Returns `Ok(None)` for a bare `iroh://` (no host) so callers can
    /// fall back to deriving the endpoint id from the location author's
    /// DID. Returns `Err` if the scheme is not `iroh` or the host fails
    /// to decode.
    pub fn from_url(url: &Url) -> Result<Option<Self>, Error> {
        if url.scheme() != Self::URL_SCHEME {
            return Err(Error::Iroh(format!(
                "expected {}:// scheme, got '{}'",
                Self::URL_SCHEME,
                url.scheme()
            )));
        }
        match url.host_str() {
            Some(host) if !host.is_empty() => decode_host(host).map(Some),
            _ => Ok(None),
        }
    }

    /// `true` iff `url.scheme()` is the iroh URL scheme.
    pub fn is_endpoint_url(url: &Url) -> bool {
        url.scheme() == Self::URL_SCHEME
    }
}

/// Decode an `iroh://` URL host slot back into an endpoint id.
fn decode_host(host: &str) -> Result<EndpointId, Error> {
    let bytes = HOST_BASE
        .decode(host)
        .map_err(|e| Error::Iroh(format!("invalid base32 endpoint id '{host}': {e}")))?;
    let arr: [u8; 32] = bytes.try_into().map_err(|v: Vec<u8>| {
        Error::Iroh(format!(
            "endpoint id must decode to 32 bytes, got {}",
            v.len()
        ))
    })?;
    let inner = iroh::EndpointId::from_bytes(&arr)
        .map_err(|e| Error::Iroh(format!("invalid endpoint id bytes: {e}")))?;
    Ok(EndpointId(inner))
}

impl From<iroh::EndpointId> for EndpointId {
    fn from(id: iroh::EndpointId) -> Self {
        Self(id)
    }
}

impl From<EndpointId> for iroh::EndpointId {
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

    /// Parse the canonical `iroh://<base32>` URL form.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let url = Url::parse(s).map_err(|e| Error::Iroh(format!("invalid URL '{s}': {e}")))?;
        Self::from_url(&url)?
            .ok_or_else(|| Error::Iroh(format!("URL '{s}' has no endpoint id host")))
    }
}

/// Derive an [`EndpointId`] from a radicle DID's public key.
///
/// Both are ed25519 — the 32-byte key is used directly. This allows
/// looking up iroh endpoints by the DID of the peer that registered
/// an `iroh://` location in an artifact COB.
impl TryFrom<&radicle::identity::Did> for EndpointId {
    type Error = Error;

    fn try_from(did: &radicle::identity::Did) -> Result<Self, Self::Error> {
        let bytes = did.to_byte_array();
        let inner = iroh::EndpointId::from_bytes(&bytes)
            .map_err(|e| Error::Iroh(format!("invalid iroh public key from DID: {e}")))?;
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
) -> Result<iroh::SecretKey, Error> {
    let sk = keystore
        .secret_key(passphrase)
        .map_err(|e| Error::Iroh(format!("failed to read radicle secret key: {e}")))?
        .ok_or_else(|| Error::Iroh("radicle secret key not found".into()))?;

    let seed = sk.seed();
    let seed_bytes: &[u8; 32] = &seed;

    Ok(iroh::SecretKey::from_bytes(seed_bytes))
}

#[cfg(test)]
mod tests {
    use radicle::crypto::ssh::keystore::Passphrase;

    use super::*;

    fn fixed_id() -> EndpointId {
        iroh::SecretKey::from_bytes(&[7u8; 32]).public().into()
    }

    #[test]
    fn radicle_and_iroh_keys_share_same_public_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let keystore = Keystore::new(&tmp);
        keystore
            .init("test", None, radicle::crypto::Seed::generate())
            .unwrap();

        let iroh_sk = radicle_secret_to_iroh(&keystore, None).unwrap();

        let radicle_pk = keystore.public_key().unwrap().unwrap();
        let iroh_pk = iroh_sk.public();

        assert_eq!(&radicle_pk.to_byte_array(), iroh_pk.as_bytes());
    }

    #[test]
    fn display_is_endpoint_url() {
        let id = fixed_id();
        let s = id.to_string();
        assert!(s.starts_with("iroh://"));
        // The portion after the scheme is the base32 host.
        assert!(!s["iroh://".len()..].is_empty());
    }

    #[test]
    fn display_differs_from_iroh_default() {
        // Regression guard: the newtype's Display must not pass through
        // to iroh's own `Display`, which uses a different encoding.
        let inner = iroh::SecretKey::from_bytes(&[7u8; 32]).public();
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
        let url = Url::parse("iroh://").unwrap();
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
        let url = Url::parse("iroh://abc123").unwrap();
        assert!(EndpointId::from_url(&url).is_err());
    }

    #[test]
    fn is_endpoint_url_only_matches_endpoint_scheme() {
        assert!(EndpointId::is_endpoint_url(
            &Url::parse("iroh://abc").unwrap()
        ));
        assert!(!EndpointId::is_endpoint_url(
            &Url::parse("https://example.com").unwrap()
        ));
    }

    #[test]
    fn encrypted_keystore_requires_passphrase() {
        let tmp = tempfile::tempdir().unwrap();
        let keystore = Keystore::new(&tmp);
        keystore
            .init(
                "test",
                Some(Passphrase::new("hunter2".into())),
                radicle::crypto::Seed::generate(),
            )
            .unwrap();

        // Fails without passphrase
        assert!(radicle_secret_to_iroh(&keystore, None).is_err());

        // Succeeds with correct passphrase and produces matching public key
        let iroh_sk =
            radicle_secret_to_iroh(&keystore, Some(Passphrase::new("hunter2".into()))).unwrap();
        let radicle_pk = keystore.public_key().unwrap().unwrap();
        assert_eq!(&radicle_pk.to_byte_array(), iroh_sk.public().as_bytes());
    }
}
