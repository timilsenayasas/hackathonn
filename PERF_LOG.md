# PERF_LOG

Local box: 2 vCPU (Firecracker VM, THP=madvise), so benches run with NCPU=2; scorer has 8.
Dataset: generator seed 7, 20M events / 5M clicks (1.85 GB). Warm page cache. Median of 7.
Correctness gate for every row: `scripts/diff.sh` = 40 generated datasets (10 seeds x 1k/20k/200k/1M)
+ 20M vs oracle (v3), 36 fixtures, dev-answer.

| # | change | median wall (ms) | task-clock (ms) | page faults | peak RSS (MB) | diff | kept |
|---|--------|-----------------:|----------------:|------------:|--------------:|------|------|
| 0 | baseline v3 (scored 1.00 s) | 2306 | 4577 | 67,633 | 1653 | PASS | - |
| 1 | big-Vec growth via mremap (no copy into fresh pages) | 2093 | 4135 | 67,576 | 1661 | PASS | yes |
| 2 | uniform 16 KB blocks; es + consumed ps/cs blocks recycled for phase-2b output; phase 3 reads blocks in place | 2118 (A/B -6.9%) | ~4100 | 67,471 | 1402 | PASS | yes |
| 3 | pread into reused per-thread buffers instead of mmap | 2210 vs 2080 (A/B +5.3%) | - | - | - | PASS | no (slower here) |
| 4 | partitions sized to data (~40k events each, 512 at 20M; was 2048) for user, event-id and person streams: 4x less tail-block slack, WC lines fit L1/L2 | 2252 vs 2354 (A/B -4.6%) | - | ~61k | anon 981 after p1 (was 1061) | PASS | yes |
| 5 | links built on worker 0 (1 helper) instead of 2 extra threads: exactly NCPU active threads | 2192 vs 2390 (A/B -9.4%) | - | - | - | PASS | yes |
