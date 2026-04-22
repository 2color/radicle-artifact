//! Key conversion between radicle and iroh identities.
//!
//! Radicle and iroh both use ed25519 keys. These utilities convert between
//! the two representations, enabling a single identity to be used for both
//! Radicle COB operations and iroh-blobs networking.

use radicle::crypto::ssh::keystore::Keystore;

use super::Error;

/// Convert a radicle DID's public key to an iroh public key.
///
/// Both are ed25519 — the 32-byte key is used directly. This allows
/// looking up iroh endpoints by the DID of the peer that registered
/// an `iroh://` location in an artifact COB.
pub fn did_to_iroh_public_key(did: &radicle::crypto::PublicKey) -> Result<iroh::PublicKey, Error> {
    let pk_bytes: &[u8] = &did.to_byte_array();
    let bytes: [u8; 32] = pk_bytes
        .try_into()
        .map_err(|_| Error::Iroh("DID public key is not 32 bytes".into()))?;
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
