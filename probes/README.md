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

## Running one

```sh
tools/luau/luau-compile.exe --binary -O1 -g2 --fflags=false probes/DispatchProbe.luau > /tmp/p.bin
./target/release/luau-decompiler decompile /tmp/p.bin -o /tmp/p.lua
grep -c MARKER_ /tmp/p.lua      # expect one per arm
```

## `DispatchProbe.luau`

Reproduces `rc_0274_Packet_Types` proto #135 - a range dispatch where every arm
ends in `return` and small `while` loops sit between the tests.

**Status 22 Aug 2026: 2 of 9 markers survive.** Only the first arm of each side
of the outer split is emitted; the other seven are dropped. 900 bytes, decodes
in milliseconds, and it fails the same way the 640-instruction original does.
