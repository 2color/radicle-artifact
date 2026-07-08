# Release COB cache benchmarks

Measures the impact of the SQLite cache for the artifact `Release` COB on the read paths of `radicle_artifact::Releases` (and the node-wide `discovery::Index`).

Without a cache, most reads re-materialize each release from its full action log in git: `all`/`find_by_commit`/`find_by_cid`/`locations_for` do this for **every** release in the repository. (`count` is the exception: it counts COB objects from a ref walk and never materializes, with or without a cache.) The cache stores each materialized `Release` as a JSON blob plus a normalized `locations` index, so steady-state reads become SQLite queries preceded by a cheap git-ref freshness walk, with no materialization from the log.

## Running the benchmark

From the repository root:

```sh
cargo run --release --example bench_cache -p radicle-artifact
```

Scale the fixture with environment variables (defaults shown):

```sh
BENCH_RELEASES=80 BENCH_ARTIFACTS=5 BENCH_LOCATIONS=3 \
    cargo run --release --example bench_cache -p radicle-artifact
```

Use `--release`: a debug build makes the uncached materialization artificially slow and skews the comparison. The benchmark is self-contained (it builds a throwaway repository via the test fixtures) and does not touch real storage or `~/.radicle`.

## Results

Fixture: 80 releases x 5 artifacts x 3 locations = 1840 COB operations. `--release`, single machine. Numbers reflect the SQLite cache on the per-repo read path. "Before" is the uncached git-materialization path; "after" is the cache-backed path.

| operation                            | before (no cache) | after (cached) | speedup |
| ------------------------------------ | ----------------- | -------------- | ------- |
| `Releases::count`                    | 5.64 ms           | 4.49 ms        | ~1x     |
| `Releases::all` (drain)              | 327.22 ms         | 6.71 ms        | 49x     |
| `Releases::get` (one)                | 5.33 ms           | 1.09 ms        | 4.9x    |
| `Releases::find_by_commit`           | 330.10 ms         | 6.47 ms        | 51x     |
| `Releases::find_by_cid`              | 334.80 ms         | 6.91 ms        | 49x     |
| `Releases::locations_for`            | 306.53 ms         | 8.69 ms        | 35x     |

## Interpretation

- The SQLite cache is what turns the ~320 ms materializations into single-digit-ms reads; that is essentially all of the speedup.
- `count` never materializes: it counts release COBs from a ref walk, so it is a few ms with or without the cache (the ~1x row, and a flat `cold`). It is included to show the cache is irrelevant to it.
- `get(one)` is far cheaper than the repo-wide reads because it validates a single COB object rather than walking every tip ref; it never refreshes the whole repository.
- The `cold` column (cached pass only; the uncached pass has no cache to warm) is each op's cold-cache warm-up: every op runs against its own fresh cache, so its first call materializes what it needs and populates the cache before the warm iterations. The repo-wide reads each fold every release (`all`/`find_by_commit`/`find_by_cid`/`locations_for`, ~450-550 ms cold vs ~5-7 ms warm); `get(one)` folds only the object it reads (~7 ms); `count` stays flat since it never touches the cache.
- Cached latencies (~5-9 ms) are dominated by the git `types()` freshness ref-walk, not by SQLite. So the gap widens at larger repository sizes: the uncached cost scales with the total number of actions in the log, while the cached cost scales only with the number of COB tip refs.
- Per-operation differences of ~1 ms are run-to-run noise; only order-of-magnitude comparisons are meaningful here.
