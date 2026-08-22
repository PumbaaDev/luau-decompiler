#!/usr/bin/env bash
# Ground-truth probe suite. Compiles each probe locally, decompiles it, and
# checks the result against a source we wrote. No Roblox client, no capture.
#
#   bash tools/run_probes.sh [path/to/luau-decompiler]
#
# THE CHECK IS MARKER SURVIVAL. A MARKER_ string missing from the output NAMES
# the arm that was dropped.
#
# Count each marker ONCE. `grep -c MARKER_` counts matching LINES, not distinct
# markers, and reading it as a marker count once made a 13/13 probe look 6/13.
#
# DO NOT compare if/loop COUNTS between source and output. Tried 22 Aug 2026;
# it invents defects. `GuardProbe.guards` is a three-deep guard chain in the
# source and comes back as
#
#     return mode == "offtrack" and (flag and A or head and B or C)
#         or mode == "force" and D or E
#
# which is CORRECT: Luau compiles a guard chain that only selects a value into
# short-circuit form and the decompiler recovered it faithfully - every
# condition is present. A naive line count called that "IFS-LOST(2/9)".
#
# Structure counting IS valid between two RUNS of the decompiler, where the
# folding style is constant on both sides. That is tools/structure_diff.py.
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
    if [ ! -f "$TMP/$name.lua" ]; then
        echo "  $name: DECOMPILE FAILED"
        fail=$((fail + 1))
        continue
    fi

    want=0
    got=0
    missing=""
    for m in $(grep -oE 'MARKER_[A-Za-z0-9_]+' "$src" | sort -u); do
        want=$((want + 1))
        if grep -q "$m" "$TMP/$name.lua"; then
            got=$((got + 1))
        else
            missing="$missing $m"
        fi
    done

    if [ "$got" -eq "$want" ]; then
        printf '  %-15s %2d/%-2d markers  ok\n' "$name" "$got" "$want"
    else
        fail=$((fail + 1))
        printf '  %-15s %2d/%-2d markers  DROPPED:%s\n' "$name" "$got" "$want" "$missing"
    fi
done

echo
if [ "$fail" -gt 0 ]; then
    echo "$fail probe(s) losing arms. Each dropped marker is a branch the lifter discarded."
    exit 1
fi
echo "all probes intact"
