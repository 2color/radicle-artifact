//! Key conversion between radicle and iroh identities, plus the endpoint id codec.
//!
//! Radicle and iroh both use ed25519 keys. These utilities convert between
//! the two representations, enabling a single identity to be used for both
//! Radicle COB operations and iroh-blobs networking.
//!
//! Endpoint IDs are encoded as lowercase RFC 4648 base32, no padding. We
//! use the `multibase` crate's `Base32Lower` directly (no multibase
//! prefix) since the `iroh://` URL scheme already disambiguates the
//! encoding.

use multibase::Base;
use radicle::crypto::ssh::keystore::Keystore;

use crate::share::Error;

/// Canonical wire encoding for endpoint ids: RFC 4648 base32, lowercase, no padding.
const ENDPOINT_ID_BASE: Base = Base::Base32Lower;

/// Convert a radicle DID's public key to an iroh public key.
///
/// Both are ed25519 — the 32-byte key is used directly. This allows
/// looking up iroh endpoints by the DID of the peer that registered
/// an `iroh://` location in an artifact COB.
pub fn did_to_iroh_public_key(did: &radicle::crypto::PublicKey) -> Result<iroh::PublicKey, Error> {
    let bytes = did.to_byte_array();
    iroh::PublicKey::from_bytes(&bytes)
        .map_err(|e| Error::Iroh(format!("invalid iroh public key from DID: {e}")))
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

/// Encode an iroh endpoint id as a lowercase base32 string (no `iroh://` prefix).
///
/// This is the canonical wire/storage form. Use `decode_endpoint_id` to
/// parse it back. Do not use [`std::fmt::Display`] on `EndpointId` — that
/// produces a different (z-base-32) encoding incompatible with this one.
pub fn encode_endpoint_id(id: &iroh::EndpointId) -> String {
    ENDPOINT_ID_BASE.encode(id.as_bytes())
}

/// Parse a lowercase base32 endpoint id back into an `EndpointId`.
pub fn decode_endpoint_id(s: &str) -> Result<iroh::EndpointId, Error> {
    let bytes = ENDPOINT_ID_BASE
        .decode(s)
        .map_err(|e| Error::Iroh(format!("invalid base32 endpoint id '{s}': {e}")))?;
    let arr: [u8; 32] = bytes.try_into().map_err(|v: Vec<u8>| {
        Error::Iroh(format!(
            "endpoint id must decode to 32 bytes, got {}",
            v.len()
        ))
    })?;
    iroh::EndpointId::from_bytes(&arr)
        .map_err(|e| Error::Iroh(format!("invalid endpoint id bytes: {e}")))
}

#[cfg(test)]
mod tests {
    use radicle::crypto::ssh::keystore::Passphrase;

    use super::*;

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
    fn endpoint_id_base32_round_trip() {
        // Deterministic 32-byte key.
        let sk = iroh::SecretKey::from_bytes(&[7u8; 32]);
        let id = sk.public();
        let encoded = encode_endpoint_id(&id);
        let decoded = decode_endpoint_id(&encoded).unwrap();
        assert_eq!(decoded, id);
        // Re-encoding produces the same string bit-for-bit.
        assert_eq!(encode_endpoint_id(&decoded), encoded);
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
