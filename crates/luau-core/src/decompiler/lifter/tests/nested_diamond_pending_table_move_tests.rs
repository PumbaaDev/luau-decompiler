//! `declared_never_assigned`, nested diamond whose arms each build a table in
//! a scratch register and MOVE it onto one reply register (RideAPet / SAE /
//! StealABrainrot PerformanceSender `captureDataModelSnapshot` reply: `sel3`).
//!
//! ```lua
//! local reply, logs, snap = nil, nil, nil
//! if not prof then reply = { error = true }
//! elseif size(prof) > 100 then reply = { size = true }
//! else reply = { meta = metaFn() }; logs = prof; if dm then snap = dm end end
//! sink(reply, logs, snap)
//! ```

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

const OP_LOADNIL: u8 = 2;
const OP_LOADN: u8 = 4;
const OP_MOVE: u8 = 6;
const OP_SETTABLEKS: u8 = 16;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;
const OP_JUMP: u8 = 23;
const OP_JUMPIF: u8 = 25;
const OP_JUMPIFNOT: u8 = 26;
const OP_JUMPIFNOTLT: u8 = 32;
const OP_DUPTABLE: u8 = 54;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((d as u16 as u32) << 16)
}
fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn build() -> Chunk {
    let code = vec![
        insn_abc(OP_LOADNIL, 5, 0, 0),     // 0
        insn_abc(OP_LOADNIL, 6, 0, 0),     // 1
        insn_abc(OP_LOADNIL, 7, 0, 0),     // 2
        insn_ad(OP_JUMPIF, 2, 3),          // 3  prof -> 7
        insn_ad(OP_DUPTABLE, 8, 3),        // 4  { error }
        insn_abc(OP_MOVE, 5, 8, 0),        // 5
        insn_ad(OP_JUMP, 0, 18),           // 6  -> 25
        insn_abc(OP_MOVE, 8, 0, 0),        // 7  size
        insn_abc(OP_MOVE, 9, 2, 0),        // 8
        insn_abc(OP_CALL, 8, 2, 2),        // 9
        insn_ad(OP_LOADN, 9, 100),         // 10
        insn_ad(OP_JUMPIFNOTLT, 9, 4),     // 11 -> 16
        8,                                 // 12 aux R8
        insn_ad(OP_DUPTABLE, 8, 4),        // 13 { size }
        insn_abc(OP_MOVE, 5, 8, 0),        // 14
        insn_ad(OP_JUMP, 0, 9),            // 15 -> 25
        insn_ad(OP_DUPTABLE, 8, 5),        // 16 { meta }
        insn_abc(OP_MOVE, 9, 1, 0),        // 17
        insn_abc(OP_CALL, 9, 1, 2),        // 18
        insn_abc(OP_SETTABLEKS, 9, 8, 0),  // 19
        2,                                 // 20 aux K"meta"
        insn_abc(OP_MOVE, 5, 8, 0),        // 21
        insn_abc(OP_MOVE, 6, 2, 0),        // 22
        insn_ad(OP_JUMPIFNOT, 3, 1),       // 23 -> 25
        insn_abc(OP_MOVE, 7, 3, 0),        // 24
        insn_abc(OP_MOVE, 8, 4, 0),        // 25
        insn_abc(OP_MOVE, 9, 5, 0),        // 26
        insn_abc(OP_MOVE, 10, 6, 0),       // 27
        insn_abc(OP_MOVE, 11, 7, 0),       // 28
        insn_abc(OP_CALL, 8, 4, 1),        // 29
        insn_abc(OP_RETURN, 0, 1, 0),      // 30
    ];
    let constants = vec![
        Constant::String("error".to_string()),
        Constant::String("size".to_string()),
        Constant::String("meta".to_string()),
        Constant::Table(vec![(0, None)]),
        Constant::Table(vec![(1, None)]),
        Constant::Table(vec![(2, None)]),
    ];
    let proto = Proto {
        max_stack_size: 16,
        num_params: 5,
        num_upvalues: 0,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants,
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("probe".to_string()),
        line_info: None,
        debug_info: None,
    };
    Chunk {
        version: 6,
        types_version: 0,
        strings: vec!["error".into(), "size".into(), "meta".into()],
        protos: vec![proto],
        main_proto: 0,
    }
}

/// KNOWN, UNFIXED (bytecode-proven): pc 15 `JUMP -> 25` skips the `{ meta }` arm,
/// yet the lifter emits `if 100 < len then v5 = {} end; v5 = { meta = .. }`, so
/// the size arm falls into the meta arm and the outer else survives only by luck.
/// The inner else arm (16..25) is lifted as two flat spans (16..23, 23..25) by
/// the `branch_region` flat path instead of one arm. Kept as a repro for the
/// next attempt; remove `#[ignore]` when fixed.
#[test]
#[ignore = "known unfixed: nested diamond whose then-arm jumps to the outer join"]
fn table_moved_onto_reply_register_in_each_arm_reaches_the_merge() {
    let chunk = build();
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    eprintln!("=== emitted ===\n{out}\n===============");
    for key in ["error", "size", "meta"] {
        assert!(out.contains(key), "arm table `{key}` lost:\n{out}");
    }
    assert!(
        !out.lines().any(|l| {
            let t = l.trim();
            t.starts_with("local ") && !t.contains('=') && !t.contains(',')
        }) || out.matches("= {").count() >= 3,
        "declared-only local with its writes lost:\n{out}"
    );
}
