#!/usr/bin/env bash
# Copyright (c) Mysten Labs, Inc.
# SPDX-License-Identifier: Apache-2.0

# Downloads COUNT mainnet checkpoints, STRIDE apart and ending near the
# current one, into corpus/mainnet/. Each file is one byte (1, meaning BCS)
# followed by a BCS CheckpointData. The bucket only keeps recent checkpoints.

set -euo pipefail

COUNT="${1:-64}"
STRIDE="${2:-997}"
DIR="$(cd "$(dirname "$0")/.." && pwd)/corpus/mainnet"
mkdir -p "$DIR"

LATEST="$(curl -sS -m 30 https://graphql.mainnet.sui.io/graphql \
    -H 'content-type: application/json' \
    -d '{"query":"{ checkpoint { sequenceNumber } }"}' |
    sed -E 's/.*"sequenceNumber":([0-9]+).*/\1/')"

for ((i = 1; i <= COUNT; i++)); do
    SEQ=$((LATEST - i * STRIDE))
    if [[ ! -s "$DIR/$SEQ.chk" ]]; then
        curl -sS -f -m 60 -o "$DIR/$SEQ.chk" "https://checkpoints.mainnet.sui.io/$SEQ.chk"
    fi
done
ls "$DIR" | wc -l
