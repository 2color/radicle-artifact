//! Serve artifacts via iroh-blobs.

use std::path::{Path, PathBuf};
use std::time::Duration;

use cid::Cid;
use iroh_blobs::api::blobs::{AddPathOptions, ImportMode};
use iroh_blobs::store::fs::FsStore;

use super::cid_utils;
use super::endpoint::EndpointPreset;
use super::Error;

/// An iroh-blobs server that serves content to peers.
///
/// Created via [`Server::start`] with an Ed25519 secret key. The endpoint ID
/// is derived from the key, so peers that know the corresponding public key
/// (or DID) can connect directly.
///
/// Uses an `FsStore` in a temporary directory so imported content is pinned
/// for the lifetime of the server. Files are referenced in-place via
/// `ImportMode::TryReference` to avoid copying large artifacts.
pub struct Server {
    router: iroh::protocol::Router,
    blobs: iroh_blobs::BlobsProtocol,
    store_dir: PathBuf,
}

impl Server {
    /// Start an iroh-blobs server bound to the given identity.
    ///
    /// Uses the provided [`EndpointPreset`] for relay and discovery config.
    /// Waits for the relay connection before returning.
    pub async fn start(secret_key: iroh::SecretKey, preset: EndpointPreset) -> Result<Self, Error> {
        let store_dir =
            std::env::temp_dir().join(format!(".rad-artifact-serve-{}", std::process::id()));
        std::fs::create_dir_all(&store_dir).map_err(|e| Error::Serve(format!("store dir: {e}")))?;
        let store = FsStore::load(&store_dir)
            .await
            .map_err(|e| Error::Serve(format!("store load: {e}")))?;

        let endpoint = iroh::Endpoint::builder(preset)
            .secret_key(secret_key)
            .alpns(vec![iroh_blobs::ALPN.to_vec()])
            .bind()
            .await
            .map_err(|e| Error::Serve(format!("endpoint bind: {e}")))?;

        // Wait for relay so our address is published and discoverable.
        let _ = endpoint.online().await;

        let blobs = iroh_blobs::BlobsProtocol::new(&store, None);
        let router = iroh::protocol::Router::builder(endpoint)
            .accept(iroh_blobs::ALPN, blobs.clone())
            .spawn();

        Ok(Self {
            router,
            blobs,
            store_dir,
        })
    }

    /// Get a handle to the blob store for adding content.
    pub fn store(&self) -> &iroh_blobs::api::Store {
        self.blobs.store()
    }

    /// Get the iroh endpoint (useful for reading the endpoint ID).
    pub fn endpoint(&self) -> &iroh::Endpoint {
        self.router.endpoint()
    }

    /// Gracefully shut down the server and remove the temporary store.
    pub async fn shutdown(self) -> Result<(), Error> {
        tokio::time::timeout(Duration::from_secs(2), self.router.shutdown())
            .await
            .map_err(|_| Error::Serve("shutdown timed out".into()))?
            .map_err(|e| Error::Serve(format!("shutdown: {e}")))?;
        std::fs::remove_dir_all(&self.store_dir).ok();
        Ok(())
    }
}

/// Import options that reference files in-place instead of copying.
fn try_reference_opts(path: &Path) -> AddPathOptions {
    AddPathOptions {
        path: path.to_path_buf(),
        format: iroh_blobs::BlobFormat::Raw,
        mode: ImportMode::TryReference,
    }
}

/// Add a file to the blob store and verify it matches the expected CID.
pub async fn add_blob(
    store: &iroh_blobs::api::Store,
    path: &Path,
    expected: &Cid,
) -> Result<(), Error> {
    let tag = store
        .add_path_with_opts(try_reference_opts(path))
        .with_tag()
        .await
        .map_err(|e| Error::Serve(format!("add blob: {e}")))?;

    let actual_cid = cid_utils::blake3_hash_to_cid(tag.hash, cid_utils::ArtifactKind::Blob);
    if actual_cid != *expected {
        return Err(Error::CidMismatch {
            expected: expected.to_string(),
            actual: actual_cid.to_string(),
        });
    }
    Ok(())
}

/// Add a directory as a named collection and verify it matches the expected CID.
///
/// Each file in the directory becomes a collection entry with its relative path
/// as the name. Uses [`canonical_walk`](cid_utils::canonical_walk) for
/// deterministic, cross-platform directory traversal.
pub async fn add_collection(
    store: &iroh_blobs::api::Store,
    dir: &Path,
    expected: &Cid,
) -> Result<(), Error> {
    let file_entries =
        cid_utils::canonical_walk(dir).map_err(|e| Error::Serve(format!("walk directory: {e}")))?;

    let mut entries = Vec::new();
    for (name, abs) in file_entries {
        let tag = store
            .add_path_with_opts(try_reference_opts(&abs))
            .with_tag()
            .await
            .map_err(|e| Error::Serve(format!("add file: {e}")))?;
        entries.push((name, tag.hash));
    }
    // entries are already sorted by canonical_walk

    let collection = iroh_blobs::format::collection::Collection::from_iter(entries);
    let tag = collection
        .store(store)
        .await
        .map_err(|e| Error::Serve(format!("store collection: {e}")))?;

    let actual_cid = cid_utils::blake3_hash_to_cid(tag.hash(), cid_utils::ArtifactKind::Collection);
    if actual_cid != *expected {
        return Err(Error::CidMismatch {
            expected: expected.to_string(),
            actual: actual_cid.to_string(),
        });
    }
    Ok(())
}
