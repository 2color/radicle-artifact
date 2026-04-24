//! Benchmark for `radicle_artifact::share::compute_content_id`.
//!
//! Creates a deterministic corpus (fixed-seed PRNG) in a temp dir, hashes it
//! N times, and prints the median wall-clock duration in milliseconds.
//! The golden hash is also printed so silent correctness regressions surface.

use std::fs;
use std::io::Write;
use std::time::Instant;

use radicle_artifact::share::compute_content_id;

/// Deterministic linear-congruential byte generator — no rand crate needed.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let bytes = self.0.to_le_bytes();
            let n = chunk.len().min(8);
            chunk[..n].copy_from_slice(&bytes[..n]);
        }
    }
}

fn build_corpus(root: &std::path::Path, files: usize, size_each: usize, seed: u64) {
    let mut rng = Lcg::new(seed);
    let mut buf = vec![0u8; size_each];
    for i in 0..files {
        rng.fill(&mut buf);
        let nested = i % 5;
        let mut p = root.to_path_buf();
        for d in 0..nested {
            p.push(format!("d{d}"));
        }
        fs::create_dir_all(&p).unwrap();
        p.push(format!("file_{i:04}.bin"));
        let mut f = fs::File::create(&p).unwrap();
        f.write_all(&buf).unwrap();
    }
}

/// Mirror of `src/share/cid_utils.rs::tests::golden_hash` so every bench run
/// catches hashing/serialisation regressions before emitting a timing number.
fn check_golden() {
    let expected = "bagaachraxw5bpahcjbvb23lan2bmgueuuidupwxy6zhmibu7g4672o7snypa";
    let golden_files: &[(&str, &[u8])] = &[
        ("hello.txt", b"hello"),
        ("sub/world.txt", b"world"),
        ("file with spaces.txt", b"spaces matter"),
        (".hidden", b"hidden file"),
        ("empty.txt", b""),
        ("archive.tar.gz", b"multi-extension"),
        ("a.txt", b"a-root"),
        ("a/b.txt", b"a-subdir"),
        ("sub2/other.txt", b"sibling dir"),
        ("deep/nested/path/file.txt", b"deeply nested"),
    ];
    let tmp = tempfile::TempDir::new().unwrap();
    for (rel, bytes) in golden_files {
        let p = tmp.path().join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, bytes).unwrap();
    }
    let got = compute_content_id(tmp.path()).unwrap();
    assert_eq!(got.to_string(), expected, "golden CID mismatch");
}

fn main() {
    check_golden();

    // Tunables via env.
    let files: usize = std::env::var("BENCH_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64);
    let size_kb: usize = std::env::var("BENCH_SIZE_KB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1024);
    let iters: usize = std::env::var("BENCH_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let seed: u64 = 0xC0FFEE_1234_5678_u64;

    let tmp = tempfile::TempDir::new().unwrap();
    let size_each = size_kb * 1024;
    eprintln!(
        "bench: files={files} size_each={size_kb}KiB total={:.1}MiB iters={iters}",
        (files * size_each) as f64 / (1024.0 * 1024.0)
    );
    build_corpus(tmp.path(), files, size_each, seed);

    // Warm file cache and sanity-check determinism.
    let warm_cid = compute_content_id(tmp.path()).unwrap();
    let warm2 = compute_content_id(tmp.path()).unwrap();
    assert_eq!(warm_cid, warm2, "compute_content_id not deterministic");
    eprintln!("corpus_cid: {warm_cid}");

    let mut samples = Vec::with_capacity(iters);
    for i in 0..iters {
        let t0 = Instant::now();
        let cid = compute_content_id(tmp.path()).unwrap();
        let elapsed = t0.elapsed();
        assert_eq!(cid, warm_cid, "non-deterministic cid on iter {i}");
        let ms = elapsed.as_secs_f64() * 1000.0;
        eprintln!("  iter {i}: {ms:.3} ms");
        samples.push(ms);
    }

    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = samples[samples.len() / 2];

    // Extractor reads this line.
    println!("duration: {median:.3} ms");
}
