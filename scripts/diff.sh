#!/bin/bash
# Differential test: oracle vs new binary on many generated datasets, plus fixtures and dev answer.
# usage: scripts/diff.sh [NEW_BIN] [quick]
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
NEW=${1:-$ROOT/target/x86_64-unknown-linux-gnu/release/fastsess}
ORACLE=$ROOT/oracle/fastsess-v3
GEN=${GEN:-/home/claude/hk/tools/gen/target/release/generate}
DATA=${DIFF_DATA:-/home/claude/hk/data/diff}
FIX=${FIXTURES:-/home/claude/hk/work/fixtures}
DEV=${DEV_DATA:-/home/claude/hk/data/dev}
DEVANS=${DEV_ANSWER:-/home/claude/hk/work/dev-answer}
BIG=${BIG_DATA:-/home/claude/hk/data/p20m}
TMP=$(mktemp -d)
fail=0
cmp_run() { # name datadir
  rm -rf "$TMP/a" "$TMP/b"
  DATA_DIR="$2" OUTPUT_DIR="$TMP/a" "$ORACLE" >/dev/null 2>&1 || { echo "FAIL $1 (oracle crashed)"; fail=1; return; }
  DATA_DIR="$2" OUTPUT_DIR="$TMP/b" "$NEW" >/dev/null 2>"$TMP/err" || { echo "FAIL $1 (new crashed)"; cat "$TMP/err" | tail -3; fail=1; return; }
  if python3 "$ROOT/scripts/compare.py" "$TMP/b" "$TMP/a" > "$TMP/cmp" 2>&1 && grep -q MATCH "$TMP/cmp"; then echo "PASS $1"; else echo "FAIL $1"; head -20 "$TMP/cmp"; fail=1; fi
}
mkdir -p "$DATA"
SIZES="1000 20000 200000 1000000"; SEEDS="1 2 3 4 5 6 7 8 9 10"
[ "${2:-}" = quick ] && SIZES="1000 20000 200000" && SEEDS="1 2 3"
for size in $SIZES; do for seed in $SEEDS; do
  d="$DATA/s${seed}_${size}"
  [ -f "$d/manifest.json" ] || "$GEN" --seed $seed --events $size --clicks $((size/4)) --output "$d" >/dev/null 2>&1
  cmp_run "seed=$seed size=$size" "$d"
done; done
python3 "$ROOT/scripts/run_fixtures.py" "$FIX" -- "$NEW" 2>&1 | tail -1 | grep -q "36/36" && echo "PASS fixtures 36/36" || { echo "FAIL fixtures"; fail=1; }
rm -rf "$TMP/d"; DATA_DIR="$DEV" OUTPUT_DIR="$TMP/d" "$NEW" >/dev/null 2>&1; python3 "$ROOT/scripts/compare.py" "$TMP/d" "$DEVANS" | grep -q MATCH && echo "PASS dev-answer" || { echo "FAIL dev-answer"; fail=1; }
[ "${2:-}" = quick ] || cmp_run "20M" "$BIG"
rm -rf "$TMP"
[ $fail = 0 ] && echo "ALL PASS" || { echo "SOME FAILED"; exit 1; }
