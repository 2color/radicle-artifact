//! Persistent iroh identity for seeding artifacts.
//!
//! The seeding identity is an ed25519 key generated on first use and
//! reused across runs. Keeping it separate from the radicle DID means
//! `rad-artifact serve` can run unattended (cron, systemd, a desktop
//! background process) without unlocking the radicle keystore.
//!
//! The corresponding endpoint id is written into the artifact COB as an
//! explicit `iroh://<endpoint-id>` location; the bare `iroh://` form (which
//! used to mean "derive from the author's DID") is no longer produced.

use std::path::Path;

use super::Error;

/// Load the iroh secret key at `path`, generating and persisting a new
/// one if the file does not exist. On Unix the file is created with
/// `0600` permissions.
pub fn load_or_generate_key(path: &Path) -> Result<iroh::SecretKey, Error> {
    if path.exists() {
        let bytes = std::fs::read(path)?;
        let bytes: [u8; 32] = bytes.try_into().map_err(|raw: Vec<u8>| {
            Error::Iroh(format!(
                "iroh key at {} has wrong length: expected 32 bytes, got {}",
                path.display(),
                raw.len()
            ))
        })?;
        Ok(iroh::SecretKey::from_bytes(&bytes))
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let secret = iroh::SecretKey::generate();
        write_key(path, &secret)?;
        Ok(secret)
    }
}

#[cfg(unix)]
fn write_key(path: &Path, secret: &iroh::SecretKey) -> Result<(), Error> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(&secret.to_bytes())?;
    Ok(())
}

#[cfg(not(unix))]
fn write_key(path: &Path, secret: &iroh::SecretKey) -> Result<(), Error> {
    std::fs::write(path, secret.to_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_generates_then_reloads_same_key() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("iroh.key");

        let sk1 = load_or_generate_key(&path).unwrap();
        assert!(path.exists());

        let sk2 = load_or_generate_key(&path).unwrap();
        assert_eq!(sk1.to_bytes(), sk2.to_bytes());
    }

    #[test]
    fn creates_missing_parent_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/dir/iroh.key");
        load_or_generate_key(&path).unwrap();
        assert!(path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_chmod_600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("iroh.key");
        load_or_generate_key(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn malformed_key_file_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("iroh.key");
        std::fs::write(&path, b"too short").unwrap();
        assert!(load_or_generate_key(&path).is_err());
    }
}
