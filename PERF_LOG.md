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
| 6 | compact rows (one name per person, u8/u32 cols) + streaming Parquet writer with dictionary pages (user_id, day, top_category, sessions, events); output 92 MB -> 37 MB; types unchanged (DATE, INT64, UTF8) | 1922 vs 2275 (A/B -18.7%) | - | ~61k | 1163 | PASS | yes |
| 7 | link build: node ids by sorting each shard's endpoints instead of 256 hash tables | 2052 vs 2079 (A/B -2.6%, noise) | - | 38.7k | anon 918 after p1 (was 981) | PASS | yes (memory) |
| 8 | 12-byte person records + purchase side stream (and 48-byte NT write-combining) | 2298 vs 1898 (A/B +21.3%); WC alone +4.8% | - | - | anon 856 after p1 | PASS | no |
| 9 | software prefetch 1 KiB ahead in line scanner | 1919 vs 2037 (A/B -6.1%; 2 KiB: -6.4%) | - | - | loads of input were the top stall | PASS | yes |
| 10 | force-inline event line handler into scanner | 2010 vs 2062 (A/B -2.3%) | - | - | below 3% bar | PASS | no |

Scale check (commit ddc5028, 2 vCPU VM): 40M events + 10M clicks (3.6 GB, seed 4242) runs in 4.44 s with peak RSS 2153 MB; output matches oracle v3 (4,427,710 user rows, 28,014 campaign rows). 60M is projected at about 3.3 GB peak RSS; the 60M generator itself runs out of memory on this 8 GB VM.

| 11 | MADV_POPULATE_READ on each input chunk before scanning | 1850 vs 1890 (A/B +0.3% paired) | - | 26k (was 38.7k) | fewer faults, no time change | PASS | no |
| 12 | PGO (profile from 20M run; built at /src like the Docker build) | A/B -2.2% (12 runs), -2.9% (8 runs) | - | - | below 3% bar; would need pinned rustc image + absolute profile path | PASS | no |
| 13 | block scanner: separator masks once per 64-byte block, walked with tzcnt (replaces per-line 128-byte window) | 1891 vs 1924 (A/B -3.7%); microbench events scan 502->435 ms | - | - | 4.3M random records identical to old scanner under random chunking; full diff + 65 fuzz seeds | PASS | yes |
| 14 | SWAR HH:MM:SS parse in ISO timestamps (unit-tested vs scalar on 20M random inputs) | A/B -2.7% (8 runs), -1.6% (12 runs) | - | - | below 3% bar | PASS | no |

## Status at commit 380b827 (v6)

All numbers come from the 2-vCPU dev VM with NCPU=2, using scripts/bench.sh medians.

| | baseline (v3) | now |
|---|---|---|
| 20M median wall | 2306 ms | 1769 ms (−23%) |
| 20M task-clock | 4577 ms | 3508 ms |
| 20M peak RSS | 1653 MB | 1098 MB |
| 40M median wall | – | 3847 ms, peak RSS 2136 MB, matches oracle |

CPU by phase at 20M (3.5 s total):

| phase | CPU | share | what it is |
|---|---|---|---|
| parse | 1690 ms | 48% | input scan, plus IDs/timestamps, plus scatter, plus the kernel zeroing fresh output pages (about 13% of all CPU) |
| 2b | 775 ms | 22% | link lookups, regrouping by person, sessions for unlinked users |
| 3 | 791 ms | 23% | sessions for linked persons and Parquet writing |
| dedup | 237 ms | 7% | |

Single-core floors from the scanner microbench (20M events, 1.66 GB):

- tokenizing alone: 435 ms (20 ns/line);
- adding the ID keys: about 4 ns/line;
- adding both timestamps: about 12 ns/line.

The leaders at 0.30 s imply roughly 2 s CPU for 20M on 8 cores. This design spends 3.5 s. Every remaining phase would need to roughly halve, and none of the single changes tried moves more than 4–7%.
| 15 | MADV_SEQUENTIAL on input mappings | warm A/B +1.3% (noise); cold (guest page cache evicted) 2467/2107 -> 2491 ms, no effect | - | - | CORRECTION: the 62 s cold run first logged here came right after a VM restart (host storage cold) and affects any reader; a clean re-test shows no difference | PASS | kept, neutral |
| 16 | readahead thread (MADV_WILLNEED in consumption order) + chunk boundaries found by workers instead of the main thread | warm A/B -0.0%; cold 20M 2107-2467 -> 1835-2098 ms; cold 40M 4.2 s vs 3.95 warm | - | - | cold run now close to warm | PASS | yes (robustness) |
| 17 | no huge pages (MADV_NOHUGEPAGE everywhere) | A/B +8.8% (296k faults vs 43k) | - | 296k | THP stays | PASS | no |
| 18 | link parse: endpoints per chunk in one flat array with shard offsets (was 256 small Vecs per chunk) | A/B -0.8% on 2 vCPU | - | - | mprotect calls 5975 -> 751; each one takes the address-space write lock and stalls page faults in all threads, so the gain is expected at 8 threads, not here | PASS | yes (scaling) |
| 19 | remove MADV_SEQUENTIAL (review agent: neutral in #15, and VM_SEQ_READ keeps input pages from being promoted in the page cache between runs) | neutral by construction | - | - | | PASS | yes (removed) |
| 20 | single glibc arena growing in 256 MiB steps (mallopt; review agent's suggestion) | task-clock 4156 vs 4082, 3959 vs 3982 ms (neutral) | - | - | removes the remaining mprotects but makes all threads share one malloc lock; can't see the 8-thread trade-off here, so not adopted | PASS | no |
<<<<<<< HEAD
| 21 | sort-based pipeline (spec from Yas): links first on all threads; events as 32-byte records in per-worker Vecs per event-id shard; counting-sort dedup per shard; person lookup on survivors; per-person-shard counting sort + (person, ts) sort; no phase 2b, no scatter arenas, no drop bitmaps; no click dedup | wall 2840 vs 1938 ms (A/B +45%); task-clock 5346 vs 4108 ms (+30%) | 1374 MB vs 1090 | 322k vs 52k | exact: full diff + 12 fuzz seeds. Phase 2 (dedup + random person lookup + regroup) costs 1.4 CPU-s against 0.28 for v7's hash dedup; dropping phase 2b saves only ~0.36. Shard sweep 128/256/512: 5.72/5.46/5.21 CPU-s. Code kept on branch sortpipe | PASS | no |
=======
>>>>>>> 11047d2c684b07821d8775f2cd3f647be1eda243
