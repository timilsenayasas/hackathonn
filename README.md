# shopping-sessions

Rust, no dependencies besides libc. Builds a static binary that runs in an empty image.

Rough flow:

1. mmap the CSVs and parse them on all cores in 2 MB chunks (AVX2 line splitting).
2. Build the linked-ID groups with a parallel union-find while the parse runs.
3. Dedup events by event_id (earliest ingest_ts wins).
4. Map user IDs to people, then sessionize, attribute clicks and count assists per person.
5. Write the two tables with a small Parquet writer, one file per thread.

Local run:

    cargo build --release
    DATA_DIR=path/to/data OUTPUT_DIR=out ./target/x86_64-unknown-linux-gnu/release/fastsess

Set `TIMING=1` to print how long each step takes.
