#!/usr/bin/env bash
# dp-sweep.sh — run the delivery-law live sweep across netem conditions and r values.
set -euo pipefail
cd "$(dirname "$0")/.."
DP=target/debug/delivery-probe
OUT=${OUT:?output dir}
mkdir -p "$OUT"

# peers string (8 lab peers, deterministic ids)
PEERS=""
for i in $(seq 1 8); do
  PID=$($DP peer-id --name dp-lab-$i)
  PEERS+="/ip4/10.99.99.$((10+i))/tcp/9501/p2p/$PID,"
done
PEERS=${PEERS%,}

run_condition() {
  local tag=$1 spec=$2 settle=$3
  echo "=== condition $tag: netem '$spec' ==="
  ./scripts/dp-lab.sh netem "$spec"
  sleep 2
  for r in 1 2 3; do
    echo "--- $tag r=$r ---"
    RUST_LOG=warn timeout 2400 $DP probe --peers "$PEERS" --r $r --trials 600 \
      --concurrency 16 --timeout-ms 10000 --settle-secs "$settle" \
      --out "$OUT/$tag-r$r.jsonl" 2>>"$OUT/$tag-r$r.err" | tee -a "$OUT/summary.txt"
  done
}

: > "$OUT/summary.txt"
run_condition c0   "delay 20ms"                    15
run_condition c10  "delay 20ms loss 10%"           25
run_condition c25  "delay 20ms loss 25%"           40
run_condition c40  "delay 20ms loss 40%"           60
run_condition ge30 "delay 20ms loss gemodel 15% 35%" 40
./scripts/dp-lab.sh clear-netem
echo "SWEEP COMPLETE" | tee -a "$OUT/summary.txt"
