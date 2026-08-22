# Ground-truth probes

Luau sources whose CORRECT decompilation is known before the run, because we
wrote them. Every branch carries a distinct `MARKER_*` string, so a marker
missing from the output NAMES the arm that was dropped - no score, no
judgement, no corpus.

## The thing that makes this work locally

`tools/luau/luau-compile.exe` emits **bytecode v11** by default, which this
decompiler cannot read (gate is 3-9, and v10/v11 changed the proto layout -
see `parser/mod.rs`). But:

```sh
tools/luau/luau-compile.exe --binary -O1 -g2 --fflags=false probes/X.luau > X.bin
```

`--fflags=false` turns off the flags that bump the container and emits
**v6**, which is in range. So ground truth needs NO Roblox client and no live
capture - it is a second of local work.

This was believed to be blocked on a live client until 22 Aug 2026. It was not.

## Running the suite

```sh
bash tools/run_probes.sh
```

Count each marker ONCE. `grep -c MARKER_` counts matching LINES, not distinct
markers, and reading it as a marker count made `GuardProbe` look like a 6/13
failure when it was 13/13 intact.

## Baseline, 22 Aug 2026

    DispatchProbe     2/9   real defect
    GuardProbe      13/13   intact - it exists to catch regressions
    ModuleProbe       6/8   real defect

## `DispatchProbe.luau`

Reproduces `rc_0274_Packet_Types` proto #135 - a range dispatch where every arm
ends in `return` and small `while` loops sit between the tests.

**Status 22 Aug 2026: 2 of 9 markers survive.** Only the first arm of each side
of the outer split is emitted; the other seven are dropped. 900 bytes, decodes
in milliseconds, and it fails the same way the 640-instruction original does.


## `ModuleProbe.luau`

The shape the small probes could not catch. A change that took `DispatchProbe`
from 2/9 to 9/9 simultaneously collapsed `rc_0043_GlovesShop` from **1,588
lines to 73** (calls 230 -> 40) - the whole module body gone - and cost 146
files structure corpus-wide while the semantic score reported only 28. A few
hundred bytes of probe cannot see that.

This mirrors the collapsing shape: top-level constant tables, several functions
closing over them, branches with loops inside.

**Baseline: 6 of 8.** Both dropped markers (`MARKER_M_miss`,
`MARKER_M_unknown`) are the fall-through `return` AFTER a branch whose body
contains a loop - the same family as DispatchProbe, from the other end.

## Accepting a fix

A structuring change must satisfy BOTH, or it does not ship:

1. `bash tools/run_probes.sh` - no probe loses an arm it currently keeps
2. `python tools/structure_diff.py <control_run> <patched_run>` - ZERO files
   lose an `if`, a call or a loop
