//! Benchmark for the artifact `Release` COB read paths.
//!
//! Builds a throwaway repository with many releases and operations, then times
//! the read operations so we can measure the impact of the SQLite cache.
//!
//! Run with:
//!
//! ```sh
//! cargo run --release --example bench_cache -p radicle-artifact
//! # scale it up:
//! BENCH_RELEASES=200 BENCH_ARTIFACTS=8 BENCH_LOCATIONS=4 \
//!     cargo run --release --example bench_cache -p radicle-artifact
//! ```
//!
//! Uses `radicle`'s `test` feature (available to examples via dev-dependencies).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::hint::black_box;
use std::time::{Duration, Instant};

use radicle::git::raw::Repository as RawRepository;
use radicle::git::Oid;
use radicle::storage::git::Repository;
use radicle::test;
use url::Url;

use radicle_artifact::{cache_db_path, Cid, ReleaseId, Releases};

/// Read a `usize` from an environment variable, or fall back to `default`.
fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Create a distinct empty commit keyed by `message` and return its OID.
fn commit(repo: &RawRepository, message: &str) -> Oid {
    let tree = {
        let tree = repo.treebuilder(None).unwrap();
        let oid = tree.write().unwrap();
        repo.find_tree(oid).unwrap()
    };
    let author = repo.signature().unwrap();
    repo.commit(None, &author, &author, message, &tree, &[])
        .unwrap()
        .into()
}

/// Build a valid CIDv1 (raw codec, sha2-256) from a 32-bit seed.
fn cid_from(seed: u32) -> Cid {
    use cid::multihash::Multihash;
    let mut digest = [0u8; 32];
    digest[..4].copy_from_slice(&seed.to_le_bytes());
    let mh = Multihash::<64>::wrap(0x12, &digest).unwrap();
    Cid::from(cid::Cid::new_v1(0x55, mh))
}

/// Time `f`, returning `(mean, cold)`. The first call is timed as `cold` and
/// excluded from `mean`, which averages `iters` warm (steady-state) calls.
///
/// `show_cold` gates only the printed `cold` column: it is a cache property
/// (the first cache-backed repo-wide read folds from git and populates the
/// cache), so it is meaningful for the cached pass but noise for the uncached
/// one, where every call folds and nothing is "cold". Because the cached reads
/// share one cache, only the first repo-wide read pays the full warm-up; later
/// operations find it already populated, so their `cold` is itself warm.
fn bench(label: &str, iters: u32, show_cold: bool, mut f: impl FnMut()) -> (Duration, Duration) {
    let cold = {
        let start = Instant::now();
        f();
        start.elapsed()
    };
    let start = Instant::now();
    for _ in 0..iters {
        f();
    }
    let mean = start.elapsed() / iters;
    if show_cold {
        println!("  {label:<22} {iters:>3} iters   mean {mean:>10.2?}   cold {cold:>10.2?}");
    } else {
        println!("  {label:<22} {iters:>3} iters   mean {mean:>10.2?}");
    }
    (mean, cold)
}

/// Run each read operation and return `(label, mean, cold)` for each.
///
/// `make` yields a fresh [`Releases`] handle for *each* operation (and, for a
/// cache-backed run, the [`tempfile::TempDir`] holding its cache). Giving each
/// op its own cache makes its `cold` a genuine cold-cache warm-up, rather than a
/// warm read served from a cache a previous op already populated.
fn run_reads<'r>(
    heading: &str,
    make: impl Fn() -> (Option<tempfile::TempDir>, Releases<'r, Repository>),
    sample_id: &ReleaseId,
    sample_oid: Oid,
    shared_cid: &Cid,
    show_cold: bool,
) -> Vec<(&'static str, Duration, Duration)> {
    println!("read operations ({heading}):");
    vec![
        {
            let (_keep, releases) = make();
            let (mean, cold) = bench("count", 10, show_cold, || {
                black_box(releases.count().unwrap());
            });
            ("count", mean, cold)
        },
        {
            let (_keep, releases) = make();
            let (mean, cold) = bench("all (drain)", 10, show_cold, || {
                let v = releases
                    .all()
                    .unwrap()
                    .into_iter()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                black_box(v);
            });
            ("all (drain)", mean, cold)
        },
        {
            let (_keep, releases) = make();
            let (mean, cold) = bench("get(one)", 50, show_cold, || {
                black_box(releases.get(sample_id).unwrap());
            });
            ("get(one)", mean, cold)
        },
        {
            let (_keep, releases) = make();
            let (mean, cold) = bench("find_by_commit", 10, show_cold, || {
                let v = releases
                    .find_by_commit(sample_oid)
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                black_box(v);
            });
            ("find_by_commit", mean, cold)
        },
        {
            let (_keep, releases) = make();
            let (mean, cold) = bench("find_by_cid", 10, show_cold, || {
                black_box(releases.find_by_cid(shared_cid).unwrap());
            });
            ("find_by_cid", mean, cold)
        },
        {
            let (_keep, releases) = make();
            let (mean, cold) = bench("locations_for", 10, show_cold, || {
                black_box(releases.locations_for(shared_cid).unwrap());
            });
            ("locations_for", mean, cold)
        },
    ]
}

fn main() {
    let n_releases = env_usize("BENCH_RELEASES", 80);
    let n_artifacts = env_usize("BENCH_ARTIFACTS", 5);
    let n_locations = env_usize("BENCH_LOCATIONS", 3);

    // Two nodes so locations land under multiple namespaces (multi-tip objects).
    let test::setup::NodeWithRepo {
        node: alice, repo, ..
    } = test::setup::NodeWithRepo::default();
    let test::setup::NodeWithRepo { node: bob, .. } = test::setup::NodeWithRepo::default();

    let mut releases = Releases::open(&*repo).unwrap();

    // A CID present in every release, to exercise a CID that matches many.
    let shared_cid = cid_from(u32::MAX);

    let mut total_ops: u64 = 0;
    let mut sample_id = None;
    let mut sample_oid = None;

    println!(
        "building fixture: {n_releases} releases x {n_artifacts} artifacts x {n_locations} locations ..."
    );
    let build = Instant::now();
    for i in 0..n_releases {
        let oid = commit(&repo.backend, &format!("commit {i}"));
        {
            let mut release = releases.create(oid, None, &alice.signer).unwrap();
            total_ops += 1;
            for a in 0..n_artifacts {
                let cid = cid_from((i as u32) * 1_000 + a as u32);
                release
                    .register_artifact(cid, format!("artifact-{i}-{a}"), &alice.signer)
                    .unwrap();
                total_ops += 1;
                for l in 0..n_locations {
                    let url =
                        Url::parse(&format!("https://seed{l}.example.com/{i}/{a}/file.tar.gz"))
                            .unwrap();
                    // Alternate signers so releases accrue tips in two namespaces.
                    if l % 2 == 0 {
                        release.add_location(cid, url, &alice.signer).unwrap();
                    } else {
                        release.add_location(cid, url, &bob.signer).unwrap();
                    }
                    total_ops += 1;
                }
            }
            // Register the shared CID in every release.
            release
                .register_artifact(shared_cid, "shared".into(), &alice.signer)
                .unwrap();
            release
                .add_location(
                    shared_cid,
                    Url::parse(&format!("https://shared.example.com/{i}")).unwrap(),
                    &alice.signer,
                )
                .unwrap();
            total_ops += 2;

            sample_id = Some(*release.id());
        }
        sample_oid = Some(oid);
    }
    let build = build.elapsed();
    let sample_id = sample_id.unwrap();
    let sample_oid = sample_oid.unwrap();

    println!(
        "fixture built: {total_ops} COB operations in {build:.2?} ({:.1} ops/s)\n",
        total_ops as f64 / build.as_secs_f64().max(f64::MIN_POSITIVE)
    );

    // Each op on the uncached (git-backed) path: a fresh handle per op, though
    // with no cache the handle is stateless so it makes no difference.
    let uncached = run_reads(
        "uncached, git-backed",
        || (None, Releases::open(&*repo).unwrap()),
        &sample_id,
        sample_oid,
        &shared_cid,
        false,
    );
    println!();

    // Then on a cache-backed path, with a FRESH cache per op so each op's `cold`
    // is its own cold-cache warm-up (materialize + populate), not a warm read
    // reusing a cache a previous op already filled.
    let cached = run_reads(
        "cached, fresh cache per op",
        || {
            let tmp = tempfile::tempdir().unwrap();
            let releases = Releases::open_cached(&*repo, cache_db_path(tmp.path())).unwrap();
            (Some(tmp), releases)
        },
        &sample_id,
        sample_oid,
        &shared_cid,
        true,
    );

    println!("\nspeedup (uncached mean / cached mean):");
    for ((label, u, _), (_, c, _)) in uncached.iter().zip(cached.iter()) {
        let ratio = u.as_secs_f64() / c.as_secs_f64().max(f64::MIN_POSITIVE);
        println!("  {label:<22} {ratio:>8.1}x   ({u:.2?} -> {c:.2?})");
    }
}
