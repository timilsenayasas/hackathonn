# Shopping Sessions — native Rust pipeline

Single static binary (`FROM scratch`), no runtime dependencies.

- mmap every CSV; AVX2 line-at-a-time scanner implementing the rules.md dialect exactly
- exact, order-preserving 64-bit ID keys (SIMD 6-bit pack via `pext`), interned fallback for anything else
- work-stealing over 2 MiB chunks; links/union-find run concurrently with event parsing
- radix partition by event_id → dedup (min ingest_ts) → resolve person → partition by person
- per-person sessionize + attribution (sliding 7-day click window) + daily aggregation
- one Parquet file per worker

Local test: `cargo build --release` then
`DATA_DIR=dev OUTPUT_DIR=out target/x86_64-unknown-linux-gnu/release/fastsess`.
