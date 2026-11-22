#!/bin/bash
# Interleaved A/B: scripts/ab.sh BIN_A BIN_B [ROUNDS] [NCPU] [DATA]
A=$1; B=$2; R=${3:-6}; NC=${4:-2}; DATA=${5:-/home/claude/hk/data/p20m}
cat $DATA/*/*.csv $DATA/*.csv > /dev/null
run() { rm -rf /tmp/ab_out; local s=$(date +%s%N); DATA_DIR=$DATA OUTPUT_DIR=/tmp/ab_out NCPU=$NC "$1" >/dev/null 2>&1; echo $(( ($(date +%s%N) - s) / 1000000 )); }
ta=(); tb=()
for i in $(seq $R); do ta+=($(run $A)); tb+=($(run $B)); done
A_LIST=$(IFS=,; echo "${ta[*]}"); B_LIST=$(IFS=,; echo "${tb[*]}")
python3 -c "
import statistics as s
a=[$A_LIST]; b=[$B_LIST]
print(f'A median {s.median(a):.0f} ms  min {min(a)}   {a}')
print(f'B median {s.median(b):.0f} ms  min {min(b)}   {b}')
d=[y-x for x,y in zip(a,b)]
print(f'B-A paired median {s.median(d):+.0f} ms ({s.median(d)/s.median(a)*100:+.1f}%)')
"
