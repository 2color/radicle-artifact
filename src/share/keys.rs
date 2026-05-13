//! Key conversion between radicle and iroh identities.
//!
//! Radicle DIDs are ed25519 public keys. iroh endpoint ids are also
//! ed25519 public keys. The conversion below is used by the read-side
//! back-compat path for COBs that registered a bare `iroh://` URL when
//! the seeding identity was derived from the radicle DID. New share
//! invocations use a freestanding key (see [`super::identity`]) and
//! always write explicit `iroh://<endpoint-id>` URLs.

use url::Url;

use super::Error;

/// Convert a radicle DID's public key to an iroh public key.
///
/// Both are ed25519 — the 32-byte key is used directly. Only used as a
/// fallback when resolving bare `iroh://` URLs in COBs written before
/// seeding identities were decoupled from the DID.
pub fn did_to_iroh_public_key(did: &radicle::crypto::PublicKey) -> Result<iroh::PublicKey, Error> {
    let bytes = did.to_byte_array();
    iroh::PublicKey::from_bytes(&bytes)
        .map_err(|e| Error::Iroh(format!("invalid iroh public key from DID: {e}")))
}

/// Parse the iroh endpoint id encoded in an `iroh://<endpoint-id>` URL.
///
/// Returns `Ok(None)` for a bare `iroh://` (no host). The fetch path
/// treats `None` as a signal to fall back to the location author's DID;
/// the `locate add` path rejects `None` because new writes must carry an
/// explicit endpoint id. The scheme is not validated here; callers
/// should gate on `url.scheme() == "iroh"` before invoking.
pub fn endpoint_id_from_iroh_url(url: &Url) -> Result<Option<iroh::EndpointId>, Error> {
    match url.host_str() {
        Some(host) if !host.is_empty() => host
            .parse::<iroh::EndpointId>()
            .map(Some)
            .map_err(|e| Error::Iroh(format!("invalid endpoint id '{host}': {e}"))),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_id_from_bare_iroh_url_is_none() {
        let url = Url::parse("iroh://").unwrap();
        assert_eq!(endpoint_id_from_iroh_url(&url).unwrap(), None);
    }

    #[test]
    fn endpoint_id_from_iroh_url_round_trips() {
        // Use a deterministic key so the test is reproducible.
        let sk = iroh::SecretKey::from_bytes(&[7u8; 32]);
        let pk = sk.public();
        let url = Url::parse(&format!("iroh://{pk}")).unwrap();
        let parsed = endpoint_id_from_iroh_url(&url)
            .expect("valid host should parse")
            .expect("host present");
        assert_eq!(parsed, pk);
    }

    #[test]
    fn endpoint_id_from_iroh_url_with_garbage_host_errors() {
        let url = Url::parse("iroh://abc123").unwrap();
        assert!(endpoint_id_from_iroh_url(&url).is_err());
    }
}
