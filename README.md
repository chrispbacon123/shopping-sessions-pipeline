# Shopping Sessions pipeline (Builders Buildathon, Databricks track)

A single native Rust binary that implements `rules.md` exactly:
memory-mapped CSV parsing in parallel (rayon), hash-partitioned dedup on `event_id`,
union-find over `links/` for person identity, per-person sessionization,
last-touch click attribution and 7-day assists (interval sweep), and Parquet output
(arrow-rs, uncompressed).

Run: `DATA_DIR=/data OUTPUT_DIR=/output /app/shopping_sessions` (or pass the two dirs as args).
Set `VERBOSE=1` for per-phase timings on stderr.

Open-source libraries used: rayon, memmap2, memchr, xxhash-rust, arrow-rs (parquet, arrow-array, arrow-schema), mimalloc, libc.
All code was written during the Buildathon weekend. MIT licensed.
