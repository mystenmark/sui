#!/bin/bash
# Copyright (c) Mysten Labs, Inc.
# SPDX-License-Identifier: Apache-2.0
#
# Runs a cluster of three sui validators and anchovy (crashed now and
# then), a fullnode and the stress client deterministically under derp
# (https://github.com/mlogan/derp), then checks it: no node panicked, and
# anchovy certified as far as the sui validators checkpointed.
#
#   scripts/derp-cluster/run.sh [--regenesis] [derp run options...]
#
# DERP_DIR is the derp checkout (default ~/repos/derp), SCRATCH the run's
# directory (default scripts/derp-cluster/scratch: host directories,
# stdout.N and stderr.N per process life).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
anchovy_dir=$(cd "$here/../.." && pwd)
sui_dir=$(cd "$anchovy_dir/.." && pwd)
derp=${DERP_DIR:-$HOME/repos/derp}
scratch=${SCRATCH:-$here/scratch}
venv=$here/.venv

regenesis=0
if [[ ${1:-} == --regenesis ]]; then
    regenesis=1
    shift
fi

(cd "$derp" && cargo build --release --workspace)
derp_bin=$derp/target/release/derp

(cd "$sui_dir" && "$derp_bin" cargo build --release --bin sui-node --bin stress --bin sui)
(cd "$anchovy_dir" && "$derp_bin" cargo build --release --bin anchovy-node)
mkdir -p "$here/bin"
for prog in sui-node stress sui; do
    ln -sf "$sui_dir/target/release/$prog" "$here/bin/$prog"
done
ln -sf "$anchovy_dir/target/release/anchovy-node" "$here/bin/anchovy-node"

# The genesis is made natively and kept: its keys are random, and a run is
# only repeatable with the same ones.
if [[ $regenesis == 1 || ! -f $here/cluster/genesis.blob ]]; then
    if ! "$venv/bin/python" -c 'import yaml' 2>/dev/null; then
        "${PYTHON:-python3}" -m venv "$venv"
        "$venv/bin/pip" install --quiet pyyaml
    fi
    rm -rf "$here/cluster"
    "$venv/bin/python" "$here/genesis.py" "$here/bin/sui" "$here/cluster"
fi

status=0
"$derp_bin" run --manifest "$here/run.yaml" --capture --capture-stderr \
    --scratch "$scratch" "$@" || status=$?

# A crash is a SIGKILL; a panic is anything else.
if grep -l "panicked at" "$scratch"/stderr.* 2>/dev/null; then
    echo "a node panicked" >&2
    status=1
fi
last() { grep -ahoE "$1" "$scratch"/stderr.* "$scratch"/stdout.* 2>/dev/null |
    grep -oE '[0-9]+$' | sort -n | tail -1; }
sui=$(last 'Made new checkpoint seq=[0-9]+')
anchovy=$(last 'anchovy certified checkpoint [0-9]+')
echo "sui checkpointed through ${sui:-none}, anchovy certified through ${anchovy:-none}"
if [[ -z ${anchovy:-} || -z ${sui:-} || $anchovy -lt $((sui - 10)) ]]; then
    echo "anchovy fell behind" >&2
    status=1
fi
exit $status
