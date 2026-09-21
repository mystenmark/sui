#!/usr/bin/env bash
# Copyright (c) Mysten Labs, Inc.
# SPDX-License-Identifier: Apache-2.0

# Runs the parse benchmark over the mainnet corpus, fetching the corpus
# first if it is missing. Each row is one message type; the last column is
# the speed-up of anchovy's deserialize-and-drop round trip over the
# baseline's. Numbers are the fastest of several rounds, so they are
# repeatable to a few percent on a quiet machine.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
COUNT="${BENCH_CHECKPOINTS:-64}"

if [[ "$(find "$ROOT/corpus/mainnet" -name '*.chk' 2>/dev/null | wc -l)" -lt "$COUNT" ]]; then
    "$ROOT/scripts/fetch-mainnet.sh" "$COUNT"
fi

cd "$ROOT"
cargo bench -q --bench parse 2>&1 | grep -vE '^(warning|\s*$)'
