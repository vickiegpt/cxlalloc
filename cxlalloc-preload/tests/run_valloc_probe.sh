#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
probe_bin=$(mktemp --tmpdir cxlalloc-valloc-probe.XXXXXX)
trap 'rm -f "$probe_bin"' EXIT

cc -std=c11 -O0 -fno-tree-vectorize -Wall -Wextra -Werror \
    "$repo_root/cxlalloc-preload/tests/valloc_probe.c" -o "$probe_bin"

if "$probe_bin"; then
    echo "negative control unexpectedly passed without LD_PRELOAD" >&2
    exit 1
else
    echo "negative_control=PASS"
fi

CXLALLOC_BACKEND=dax-mmap \
CXLALLOC_DAX_DEVICES=/dev/dax0.0 \
CXLALLOC_MIN_SIZE=2097152 \
LD_PRELOAD="$repo_root/target/debug/libcxlalloc_preload.so" \
    "$probe_bin"
