# Release COB cache benchmarks

Measures the impact of the SQLite cache for the artifact `Release` COB on the read paths of `radicle_artifact::Releases` (and the node-wide `discovery::Index`).

Without a cache, most reads re-materialize each release from its full action log in git: `all`/`list`/`counts`/`find_by_commit`/`find_by_cid`/`locations_for` do this for **every** release in the repository. (`count_refs` is the exception: it counts COB objects from a ref walk and never materializes, with or without a cache.) The cache stores each materialized `Release` as a JSON blob plus a normalized `locations` index, so steady-state reads become SQLite queries preceded by a cheap git-ref freshness walk, with no materialization from the log.

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
| `Releases::count_refs`               | 4.16 ms           | 4.17 ms        | ~1x     |
| `Releases::counts`                   | 263.44 ms         | 5.78 ms        | 46x     |
| `Releases::all` (drain)              | 266.66 ms         | 5.75 ms        | 46x     |
| `Releases::list` (drain)             | 258.45 ms         | 5.73 ms        | 45x     |
| `Releases::list` (first 20)          | 258.42 ms         | 4.65 ms        | 56x     |
| `Releases::get` (one)                | 3.95 ms           | 0.98 ms        | 4.0x    |
| `Releases::find_by_commit`           | 257.57 ms         | 5.83 ms        | 44x     |
| `Releases::find_by_cid`              | 256.83 ms         | 6.14 ms        | 42x     |
| `Releases::locations_for`            | 259.52 ms         | 4.39 ms        | 59x     |

## Interpretation

- The SQLite cache is what turns the ~260 ms materializations into single-digit-ms reads; that is essentially all of the speedup.
- `count_refs` never materializes: it counts release COBs from a ref walk, so it is a few ms with or without the cache (the ~1x row, and a flat `cold`). It is included to show the cache is irrelevant to it.
- `counts` tracks `all` closely, because it folds over it: the buckets cost one pass over already-materialized releases, so the cache decides its price. It is the row to read when choosing between `count_refs` and `counts` — the ref walk is ~50x cheaper and answers a different question.
- `get(one)` is far cheaper than the repo-wide reads because it validates a single COB object rather than walking every tip ref; it never refreshes the whole repository.
- The `cold` column (cached pass only; the uncached pass has no cache to warm) is each op's cold-cache warm-up: every op runs against its own fresh cache, so its first call materializes what it needs and populates the cache before the warm iterations. The repo-wide reads each fold every release (`all`/`list`/`counts`/`find_by_commit`/`find_by_cid`/`locations_for`, ~390-405 ms cold vs ~4-6 ms warm); `get(one)` folds only the object it reads (~6 ms); `count_refs` stays flat since it never touches the cache.
- Cached latencies (~4-6 ms) are dominated by the git `types()` freshness ref-walk, not by SQLite. So the gap widens at larger repository sizes: the uncached cost scales with the total number of actions in the log, while the cached cost scales only with the number of COB tip refs.
- `list` sorts in SQLite and parses rows only as the caller pulls them, so a page parses only the rows up to that page. Without a cache it reads and sorts every release, so the first page costs the same as a drain. With a cache, the ref walk sets a floor (the `count_refs` time), and only the cost above that floor depends on page size. At 400 releases x 2 artifacts x 1 location, the cached drain costs 13.16 ms and the first 20 cost 10.82 ms, against a 9.74 ms ref walk: about 3.4 ms above the floor for the drain and 1.1 ms for the page. To remove the floor, the cache must stop checking freshness on every read.
- Per-operation differences of ~1 ms are run-to-run noise; only order-of-magnitude comparisons are meaningful here.
