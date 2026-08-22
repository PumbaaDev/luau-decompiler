#!/usr/bin/env bash
# Ground-truth probe suite. Compiles each probe locally, decompiles it, and
# reports how many MARKER_ strings survived. No Roblox client, no capture.
#
#   bash tools/run_probes.sh [path/to/luau-decompiler]
#
# A marker missing from the output NAMES the arm that was dropped.
set -u
DEC="${1:-./target/release/luau-decompiler}"
LC="./tools/luau/luau-compile.exe"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fail=0
for src in probes/*.luau; do
    name="$(basename "$src" .luau)"
    # --fflags=false emits bytecode v6; the default is v11, which the 3-9 gate
    # rejects because v10/v11 changed the proto layout.
    "$LC" --binary -O1 -g2 --fflags=false "$src" > "$TMP/$name.bin" 2>/dev/null
    "$DEC" decompile "$TMP/$name.bin" -o "$TMP/$name.lua" >/dev/null 2>&1
    want=$(grep -oE 'MARKER_[A-Za-z0-9_]+' "$src" | sort -u | wc -l)
    got=0
    missing=""
    for m in $(grep -oE 'MARKER_[A-Za-z0-9_]+' "$src" | sort -u); do
        if grep -q "$m" "$TMP/$name.lua" 2>/dev/null; then
            got=$((got + 1))
        else
            missing="$missing $m"
        fi
    done
    if [ "$got" -eq "$want" ]; then
        printf '  %-16s %2d/%-2d ok\n' "$name" "$got" "$want"
    else
        fail=$((fail + 1))
        printf '  %-16s %2d/%-2d DROPPED:%s\n' "$name" "$got" "$want" "$missing"
    fi
done
echo
if [ "$fail" -gt 0 ]; then
    echo "$fail probe(s) losing arms. Each dropped marker is a branch the lifter discarded."
    exit 1
fi
echo "all probes intact"
