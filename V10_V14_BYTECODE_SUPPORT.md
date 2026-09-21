# Luau bytecode v10-v14 support — IMPLEMENTED 2026-09-21

The parser now IMPLEMENTS the container for Luau bytecode versions v10-v14, but by
DEFAULT still accepts only **3..=9**; v10-v14 are admitted only when the env var
`LUAU_ALLOW_V10PLUS` is set (see the opcode-divergence caveat below). Ground truth
came from the Luau VM loader `VM/src/lvmload.cpp` (`loadsafe`) and
`Common/include/Luau/Bytecode.h`, not from byte-diffing — the earlier byte-diff
guess ("2-byte chunk trailer", "one unknown nested field") was wrong; the real
structure is below.

## Why v10+ is opt-in, not default (the honest limit)

Parsing the container is necessary but NOT sufficient to DECODE v10+ safely. The
lifter models ROBLOX opcode semantics, and upstream Luau diverges in opcode
NUMBERING at >= 84: upstream `CALLFB` is opcode 87, which the Roblox map reads as
`BNOT` (87). A v11/v12 chunk compiled with call-feedback ON therefore lifts to
plausible-but-wrong code (measured 2026-09-21: a call-heavy body collapsed to
`local c = c .. c; return c`). The collision is with a valid opcode, so there is no
reliable structural guard. Default behaviour is a clear rejection with an actionable
message; `LUAU_ALLOW_V10PLUS=1` forces the decode for the safe cases:
  - a chunk compiled with call-feedback OFF (`--fflags=LuauEmitCallFeedback=false`),
    which uses only shared opcodes <= 83 — decodes CORRECTLY (verified), or
  - genuine Roblox v10+ bytecode, whose opcodes match the model (forward capability;
    the corpus is 100% v9 today, so this is unmeasured until Roblox ships v10+).

## Verified deltas over v9 (all guarded, so v3-9 decode is byte-identical)

- **Constant tags** (LuauBytecodeTag): 0 NIL, 1 BOOL, 2 NUMBER, 3 STRING, 4 IMPORT,
  5 TABLE, 6 CLOSURE, 7 VECTOR (v5+), 8 TABLE_WITH_CONSTANTS (v7+),
  **9 INTEGER** (v8+: `u8 isNegative` + `varint64 magnitude`),
  **10 CLASS_SHAPE** (v10+: `varint classname` + `varint numProps` + `varint numMethods`
  + one `varint memberName` per (prop+method)),
  **11 VECTORD** (v13+: 4 x f64).
  We map INTEGER->Number, VECTORD->Vector, CLASS_SHAPE->Nil (parse-and-skip) to avoid
  rippling new `Constant` variants through every match site; faithful for real code
  (Roblox corpus contains none of CLASS_SHAPE/VECTORD).
- **Per-proto layout**:
  - **v12+**: each proto is *length-prefixed* — a leading `varint protoSize` before
    `maxstacksize`. Record `proto_start` after reading it.
  - after debug info, **v11 ONLY**: a feedback-vector description
    (`varint slotCount`, then per slot `u8 kind` (LFT_CALLTARGET=0) + `varint pc`).
  - **v12+ is self-framing**: the trailing feedback-vector and cost-function
    (`if flags & LPF_INLINABLE(0x08): varint64 cost`) sections are OPTIONAL (emitted
    only when call-feedback / cost modelling is on). We do NOT read them field by
    field; we resync `offset = proto_start + protoSize`, exactly as the VM does.
- **No chunk trailer.** After all protos: `varint mainid`, end of chunk.

## Verified end-to-end
- `t_func` compiled to v12 with call-feedback OFF -> decompiles byte-correct.
- same with call-feedback ON -> container parses to exact EOF; one call is dropped by
  the LIFTER (a separate call-feedback-opcode issue, NOT the parser).
- `t_loop` v12 -> correct (`sum += i*2`, ipairs loop).
- Gates held: `cargo test -p luau-core --release --lib` == 951/0 AND full corpus batch
  1315/1323 (99.4%), the same 8 defect files as baseline (zero regression, by
  construction: every new branch is `version >= 11/12/13`).

## Known follow-up (NOT this change)
v12 chunks compiled with call-feedback ON use feedback-carrying opcode variants
(CALLFB etc.) that the LIFTER does not yet model, so one call can be dropped. Getting
feedback-ON v12 to full parity is a lifter/opmap task, separate from container parsing.
Feedback-OFF v12 (and any v10-v14 chunk using only the v9 opcode set) decompiles fully.
