//! A stale (dead) pre-branch table value must not block the diamond seed.
//!
//! Money (StealAnEgg) pc77-89: R7 held a table expression already consumed by
//! earlier stores; both arms of a real diamond rewrite R7 and the merge reads
//! it first. The stale `RegVal::Expr(Table)` matched neither seed gate, so the
//! arm writes were lost and the merge minted an unbound `vN`.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Proto};

const OP_LOADNIL: u8 = 2;
const OP_MOVE: u8 = 6;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;
const OP_JUMP: u8 = 23;
const OP_JUMPIFNOT: u8 = 26;
const OP_NEWTABLE: u8 = 53;
const OP_SETLIST: u8 = 55;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((d as u16 as u32) << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn chunk_of(code: Vec<u32>, num_params: u8, max_stack: u8) -> Chunk {
    let proto = Proto {
        max_stack_size: max_stack,
        num_params,
        num_upvalues: 0,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants: Vec::new(),
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("main".to_string()),
        line_info: None,
        debug_info: None,
    };
    Chunk { version: 6, types_version: 0, strings: Vec::new(), protos: vec![proto], main_proto: 0 }
}

/// ```text
/// pc0: NEWTABLE R3 0 0 ; aux       stale table parked in R3
/// pc2: MOVE R6 R3 (consumes the table; R6 never read)
/// pc3: JUMPIFNOT R0 -> pc6
/// pc3: MOVE R3 R1                  then arm
/// pc4: JUMP -> pc6
/// pc5: MOVE R3 R2                  else arm
/// pc6: MOVE R4 R0 ; MOVE R5 R3 ; CALL R4 2 1   merge reads R3
/// ```
fn build_stale_table_diamond() -> Chunk {
    let code = vec![
        // NEWTABLE at pc <= 1 is bound eagerly as a module `local M = {}`.
        insn_abc(OP_LOADNIL, 6, 0, 0),
        insn_abc(OP_LOADNIL, 7, 0, 0),
        insn_abc(OP_NEWTABLE, 3, 0, 0),
        0,
        insn_abc(OP_MOVE, 4, 0, 0),
        insn_abc(OP_SETLIST, 3, 4, 2),
        1,
        insn_abc(OP_MOVE, 6, 3, 0),
        insn_ad(OP_JUMPIFNOT, 0, 3),
        insn_abc(OP_MOVE, 3, 1, 0),
        insn_abc(OP_CALL, 3, 1, 2),
        insn_ad(OP_JUMP, 0, 2),
        insn_abc(OP_MOVE, 3, 2, 0),
        insn_abc(OP_CALL, 3, 1, 2),
        insn_abc(OP_MOVE, 4, 0, 0),
        insn_abc(OP_MOVE, 5, 3, 0),
        insn_abc(OP_CALL, 4, 2, 1),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    chunk_of(code, 3, 8)
}

#[test]
fn stale_table_register_is_seeded_across_diamond() {
    let chunk = build_stale_table_diamond();
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    eprintln!("=== emitted source ===\n{}\n======================", out);
    // Both arms must assign ONE seeded name and the merge must read it, never
    // an unbound generated `v3`.
    assert!(!out.contains("v3"), "merge read minted unbound v3:
{out}");
    assert!(out.contains("local sel3"), "no seed declaration:
{out}");
    assert!(out.contains("sel3 = arg2()") && out.contains("sel3 = arg3()"), "arm writes lost:
{out}");
    assert!(out.contains("arg1(sel3)"), "merge does not read the seeded name:
{out}");
}
