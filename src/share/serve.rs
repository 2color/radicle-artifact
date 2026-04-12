//! Serve artifacts via iroh-blobs.

use std::path::Path;
use std::time::Duration;

use cid::Cid;

use super::cid as cid_util;
use super::endpoint::EndpointPreset;
use super::Error;

/// An iroh-blobs server that serves content to peers.
///
/// Created via [`Server::start`] with an Ed25519 secret key. The endpoint ID
/// is derived from the key, so peers that know the corresponding public key
/// (or DID) can connect directly.
///
/// Uses an in-memory blob store (`MemStore`) — content is lost on shutdown.
/// This is designed for CLI-style "serve until Ctrl+C" workflows.
///
/// Long-running applications (e.g. a Tauri desktop app) that need persistent
/// storage should create their own `iroh::Endpoint` + `iroh::protocol::Router`
/// with an `iroh_blobs::store::fs::FsStore` instead.
pub struct Server {
    router: iroh::protocol::Router,
    blobs: iroh_blobs::BlobsProtocol,
}

impl Server {
    /// Start an iroh-blobs server bound to the given identity.
    ///
    /// Uses the provided [`EndpointPreset`] for relay and discovery config.
    /// Waits for the relay connection before returning.
    pub async fn start(secret_key: iroh::SecretKey, preset: EndpointPreset) -> Result<Self, Error> {
        let store = iroh_blobs::store::mem::MemStore::new();

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

        Ok(Self { router, blobs })
    }

    /// Get a handle to the blob store for adding content.
    pub fn store(&self) -> &iroh_blobs::api::Store {
        self.blobs.store()
    }

    /// Get the iroh endpoint (useful for reading the endpoint ID).
    pub fn endpoint(&self) -> &iroh::Endpoint {
        self.router.endpoint()
    }

    /// Gracefully shut down the server.
    pub async fn shutdown(self) -> Result<(), Error> {
        tokio::time::timeout(Duration::from_secs(2), self.router.shutdown())
            .await
            .map_err(|_| Error::Serve("shutdown timed out".into()))?
            .map_err(|e| Error::Serve(format!("shutdown: {e}")))
    }
}

/// Add a file to the blob store and verify it matches the expected CID.
pub async fn add_blob(
    store: &iroh_blobs::api::Store,
    path: &Path,
    expected: &Cid,
) -> Result<(), Error> {
    let tag = store
        .add_path(path)
        .temp_tag()
        .await
        .map_err(|e| Error::Serve(format!("add blob: {e}")))?;

    let actual_cid = cid_util::blake3_hash_to_cid(tag.hash(), cid_util::ArtifactKind::Blob);
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
/// as the name. Uses [`canonical_walk`](cid_util::canonical_walk) for
/// deterministic, cross-platform directory traversal.
pub async fn add_collection(
    store: &iroh_blobs::api::Store,
    dir: &Path,
    expected: &Cid,
) -> Result<(), Error> {
    let file_entries =
        cid_util::canonical_walk(dir).map_err(|e| Error::Serve(format!("walk directory: {e}")))?;

    let mut entries = Vec::new();
    for (name, abs) in file_entries {
        let tag = store
            .add_path(&abs)
            .temp_tag()
            .await
            .map_err(|e| Error::Serve(format!("add file: {e}")))?;
        entries.push((name, tag.hash()));
    }
    // entries are already sorted by canonical_walk

    let collection = iroh_blobs::format::collection::Collection::from_iter(entries);
    let tag = collection
        .store(store)
        .await
        .map_err(|e| Error::Serve(format!("store collection: {e}")))?;

    let actual_cid = cid_util::blake3_hash_to_cid(tag.hash(), cid_util::ArtifactKind::Collection);
    if actual_cid != *expected {
        return Err(Error::CidMismatch {
            expected: expected.to_string(),
            actual: actual_cid.to_string(),
        });
    }
    Ok(())
}
