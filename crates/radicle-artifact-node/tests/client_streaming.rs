//! Exercise the streaming client reader (`has`, `export`, `fetch`,
//! `download`) end-to-end against a real node, over both transports.

use std::str::FromStr;
use std::time::Duration;

use radicle::identity::RepoId;
use radicle_artifact_client::{tokio::Client, DownloadArgs, FetchArgs};
use radicle_artifact_core::cid::{compute_blob_cid, ArtifactKind};
use radicle_artifact_core::protocol::ImportMode;
use radicle_artifact_core::ARTIFACTS_DIR;
use radicle_artifact_node::node;

#[test]
fn streaming_methods_round_trip() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let home = tempfile::tempdir().unwrap();
        let payload = b"hello client streaming";
        let blob_path = home.path().join("payload.bin");
        std::fs::write(&blob_path, payload).unwrap();
        let cid = compute_blob_cid(&blob_path).unwrap();
        let rid = RepoId::from_str("rad:z2u2CP3ZJzB7ZqE8jHrau19yjpdip").unwrap();

        let secret = iroh::SecretKey::from_bytes(&[8u8; 32]);
        let home_path = home.path().to_path_buf();
        let node = tokio::spawn(async move { node::run(&home_path, secret).await });

        let socket = home.path().join(ARTIFACTS_DIR).join("control.sock");
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(socket.exists());

        let client = Client::new(socket.clone());
        client
            .seed(rid, cid, &blob_path, ArtifactKind::Blob, ImportMode::Copy)
            .await
            .unwrap();

        // has
        let h = client.has(cid).await.unwrap();
        assert!(h.present && h.complete);
        assert_eq!(h.bytes, payload.len() as u64);

        // export streams to disk; count the progress frames.
        let dest = home.path().join("exported.bin");
        let mut progress = 0;
        let receipt = client
            .export(cid, dest.clone(), Duration::from_secs(30), |_| {
                progress += 1
            })
            .await
            .unwrap();
        assert_eq!(receipt.bytes, payload.len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), payload);
        assert!(progress >= 1, "expected at least one progress frame");

        // fetch fast-path: already local, no locations, no disk write.
        let fetched = client
            .fetch(
                FetchArgs {
                    rid,
                    cid,
                    locations: vec![],
                    seed: false,
                },
                Duration::from_secs(30),
                |_| {},
            )
            .await
            .unwrap();
        assert!(fetched.from_cache);
        assert_eq!(fetched.bytes, payload.len() as u64);

        // download fast-path: already local, exported to disk.
        let dl_dest = home.path().join("downloaded.bin");
        let downloaded = client
            .download(
                DownloadArgs {
                    rid,
                    cid,
                    locations: vec![],
                    dest: dl_dest.clone(),
                    seed: false,
                },
                Duration::from_secs(30),
                |_| {},
            )
            .await
            .unwrap();
        assert!(downloaded.from_cache);
        assert_eq!(downloaded.bytes, payload.len() as u64);
        assert_eq!(std::fs::read(&dl_dest).unwrap(), payload);

        // The sync client speaks the same socket: probe and unseed with it
        // from a blocking thread.
        let sync_client = radicle_artifact_client::sync::Client::new(socket);
        let (alive, unseeded) = tokio::task::spawn_blocking(move || {
            let alive = sync_client.is_running();
            let receipt = sync_client.unseed(rid, cid).unwrap();
            (alive, receipt.was_removed)
        })
        .await
        .unwrap();
        assert!(alive);
        assert!(unseeded);

        client.shutdown().await.unwrap();
        node.await.unwrap().unwrap();
    });
}
