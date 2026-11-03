#!/usr/bin/env bash
# Copyright (c) Mysten Labs, Inc.
# SPDX-License-Identifier: Apache-2.0

# Runs one compat test under derp (github.com/mlogan/derp) for a range of
# seeds, several at once, and keeps the output of the seeds that fail.
#
#   scripts/derp-sweep.sh TEST FIRST_SEED COUNT [JOBS] [OUT_DIR]
#
# TEST is a test of crates/compat/tests/one_of_each.rs, matched exactly.
# DERP is the derp binary (default ~/repos/derp/target/release/derp). The
# test binary is built in release with `derp cargo`, which makes room for
# the stubs of a binary this big. A failing seed replays with
#
#   derp run --seed S --capture --capture-stderr --manifest OUT_DIR/run.yaml

set -euo pipefail

TEST="$1"
FIRST="$2"
COUNT="$3"
JOBS="${4:-6}"
OUT="${5:-${TMPDIR:-/tmp}/derp-sweep-$TEST}"
DERP="${DERP:-$HOME/repos/derp/target/release/derp}"
ANCHOVY="$(cd "$(dirname "$0")/.." && pwd)"

cd "$ANCHOVY"
BIN="$("$DERP" cargo test --release -p compat --test one_of_each --no-run 2>&1 |
    sed -nE 's/.*Executable .*\((target\/release\/deps\/one_of_each-[0-9a-f]+)\).*/\1/p')"
[[ -n "$BIN" ]] || { echo "the test binary did not build" >&2; exit 1; }

mkdir -p "$OUT"
cp "$BIN" "$OUT/one_of_each"
# The tests build Move packages from the sui checkout, at paths compiled in.
ln -sfn "$(cd "$ANCHOVY/.." && pwd)" "$OUT/checkout"
cat > "$OUT/run.yaml" <<EOF
allow: [checkout]
env: { RUST_LOG: "info,anemo_tower=error", RUST_BACKTRACE: "1" }
hosts:
  - name: compat
    processes:
      - [one_of_each, $TEST, --exact, --test-threads, "1", --nocapture]
EOF

run_seed() {
    local seed="$1" dir="$OUT/seed-$1"
    if (cd "$OUT" && "$DERP" run --seed "$seed" --wall-limit 1800s --capture \
        --capture-stderr --scratch "$dir" --manifest run.yaml) > "$dir.log" 2>&1 &&
        grep -q "test result: ok. 1 passed" "$dir/stdout.0"; then
        echo "seed $seed ok"
        rm -rf "$dir" "$dir.log"
    else
        echo "seed $seed FAIL: $dir"
    fi
    # derp rewrites the binary for each run (about 380 MB); a replay
    # rewrites it again.
    local f
    for f in "$OUT"/one_of_each.rw*; do
        [[ "$f" =~ /one_of_each\.rw[0-9]+-$seed-[0-9]+of ]] && rm -f "$f"
    done
}
export -f run_seed
export OUT DERP

seq "$FIRST" $((FIRST + COUNT - 1)) | xargs -P "$JOBS" -I{} bash -c 'run_seed {}' |
    tee -a "$OUT/results.txt"
