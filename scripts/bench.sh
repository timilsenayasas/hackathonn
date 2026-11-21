#!/bin/bash
# usage: scripts/bench.sh [BIN] [RUNS] [NCPU] [DATA]
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${1:-$ROOT/target/x86_64-unknown-linux-gnu/release/fastsess}
RUNS=${2:-7}; NC=${3:-8}; DATA=${4:-/home/claude/hk/data/p20m}
PERF=${PERF:-/usr/lib/linux-tools/6.8.0-151-generic/perf}
cat $DATA/*/*.csv $DATA/*.csv > /dev/null   # warm page cache
python3 - "$BIN" "$RUNS" "$NC" "$DATA" "$PERF" <<'PY'
import sys, subprocess, os, shutil, statistics, re
binp, runs, nc, data, perf = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5]
walls, laps, tc, pf = [], [], [], []
for r in range(runs):
    shutil.rmtree("/tmp/bench_out", ignore_errors=True)
    env = dict(os.environ, TIMING="1", NCPU=nc, DATA_DIR=data, OUTPUT_DIR="/tmp/bench_out")
    out = subprocess.run([perf, "stat", "-x", ",", "-e", "task-clock,page-faults", binp], env=env, capture_output=True, text=True).stderr
    cur = []
    for l in out.splitlines():
        if " ms  cpu " in l:
            m = re.match(r"\s*([\d.]+) ms  cpu\s+([\d.]+) ms  flt\s+(\d+)\s+rss\s+(\d+) MB  anon\s+\d+ MB  (.*)", l)
            cur.append((m.group(5)[:30], float(m.group(1)), float(m.group(2)), int(m.group(3)), int(m.group(4))))
        elif l.endswith(",task-clock,") or ",task-clock," in l:
            tc.append(float(l.split(",")[0]))
        elif ",page-faults," in l:
            pf.append(int(l.split(",")[0]))
    laps.append(cur); walls.append(cur[-1][1])
med = statistics.median(walls)
i = walls.index(sorted(walls)[len(walls)//2])
print(f"median wall {med:.0f} ms  (min {min(walls):.0f}, max {max(walls):.0f}, n={runs}, NCPU={nc})")
print(f"task-clock median {statistics.median(tc):.0f} ms   page-faults median {statistics.median(pf):.0f}   peak RSS {laps[i][-1][4]} MB")
print(f"{'phase (median run)':<32}{'wall':>8}{'dwall':>8}{'dcpu':>8}{'dflt':>9}")
pw = pc = pfl = 0
for n, w, c, f, rss in laps[i]:
    print(f"{n:<32}{w:>8.0f}{w-pw:>8.0f}{c-pc:>8.0f}{f-pfl:>9}")
    pw, pc, pfl = w, c, f
PY
