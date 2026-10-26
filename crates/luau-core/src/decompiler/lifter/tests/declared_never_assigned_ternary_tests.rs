//! `declared_never_assigned` family, Com_Animals `refreshAfford` shape:
//! `local ok = eq(a, ">=", b); if t then t:Cancel() end;
//! t = TweenService:Create(i, info, { Value = ok and 1 or 0 })`.
//! The ternary arms (LOADN R1 1 / LOADN R1 0) were dropped and the table
//! field read a never-assigned `v1`.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

const OP_LOADN: u8 = 4;
const OP_MOVE: u8 = 6;
const OP_GETGLOBAL: u8 = 7;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;
const OP_JUMP: u8 = 23;
const OP_JUMPIFNOT: u8 = 26;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((d as u16 as u32) << 16)
}
fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn lift_single(code: Vec<u32>, consts: Vec<&str>) -> String {
    let constants = consts.iter().map(|s| Constant::String(s.to_string())).collect();
    let proto = Proto {
        max_stack_size: 16,
        num_params: 0,
        num_upvalues: 0,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants,
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("main".to_string()),
        line_info: None,
        debug_info: None,
    };
    let chunk = Chunk {
        version: 6,
        types_version: 0,
        strings: consts.iter().map(|s| s.to_string()).collect(),
        protos: vec![proto],
        main_proto: 0,
    };
    let mut ctx = DecompileContext::new(&chunk);
    decompile_proto(&mut ctx, &chunk.protos[0], 0, 0)
}

fn assert_no_orphan(out: &str) {
    let declared_only = out
        .lines()
        .any(|l| l.trim().starts_with("local v") && !l.contains('='));
    assert!(!declared_only, "never-assigned local:\n{out}");
    assert!(
        out.contains("and 1 or 0") || (out.contains("= 1") && out.contains("= 0")),
        "ternary arms lost:\n{out}"
    );
}

#[test]
fn numeric_ternary_into_fresh_register_keeps_its_arms() {
    let code = vec![
        insn_ad(OP_GETGLOBAL, 0, 0),
        0,
        insn_abc(OP_CALL, 0, 1, 2),
        insn_ad(OP_JUMPIFNOT, 0, 2),
        insn_ad(OP_LOADN, 1, 1),
        insn_ad(OP_JUMP, 0, 1),
        insn_ad(OP_LOADN, 1, 0),
        insn_ad(OP_GETGLOBAL, 2, 0),
        1,
        insn_abc(OP_MOVE, 3, 1, 0),
        insn_abc(OP_CALL, 2, 2, 1),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let out = lift_single(code, vec!["f", "g"]);
    println!("{out}");
    assert_no_orphan(&out);
}

#[test]
fn numeric_ternary_survives_an_intervening_if() {
    let code = vec![
        insn_ad(OP_GETGLOBAL, 0, 0),
        0,
        insn_abc(OP_CALL, 0, 1, 2),
        insn_ad(OP_JUMPIFNOT, 0, 2),
        insn_ad(OP_LOADN, 1, 1),
        insn_ad(OP_JUMP, 0, 1),
        insn_ad(OP_LOADN, 1, 0),
        insn_ad(OP_GETGLOBAL, 2, 0),
        1,
        insn_ad(OP_JUMPIFNOT, 2, 3),
        insn_ad(OP_GETGLOBAL, 2, 0),
        1,
        insn_abc(OP_CALL, 2, 1, 1),
        insn_ad(OP_GETGLOBAL, 2, 0),
        2,
        insn_abc(OP_MOVE, 3, 1, 0),
        insn_abc(OP_CALL, 2, 2, 1),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let out = lift_single(code, vec!["f", "h", "g"]);
    println!("{out}");
    assert_no_orphan(&out);
}

/// Faithful copy of Com_Animals proto 7 `refreshAfford` (upvalue reads,
/// early returns, an `x.eq` call whose result is only the ternary condition).
#[test]
fn refresh_afford_shape_keeps_its_ternary() {
    const OP_LOADK: u8 = 5;
    const OP_GETUPVAL: u8 = 9;
    const OP_GETTABLEKS: u8 = 15;
    let code = vec![
        insn_abc(OP_GETUPVAL, 0, 0, 0),
        insn_ad(OP_JUMPIFNOT, 0, 2),
        insn_abc(OP_GETUPVAL, 0, 1, 0),
        insn_ad(25, 0, 1), // JUMPIF
        insn_abc(OP_RETURN, 0, 1, 0),
        insn_abc(OP_GETUPVAL, 0, 2, 0),
        insn_abc(OP_GETTABLEKS, 0, 0, 0),
        0,
        insn_abc(OP_GETUPVAL, 1, 0, 0),
        insn_ad(OP_LOADK, 2, 1),
        insn_abc(OP_GETUPVAL, 3, 1, 0),
        insn_abc(OP_CALL, 0, 4, 2),
        0, // NOP word after the CALL, as in the live v14 listing
        insn_ad(OP_JUMPIFNOT, 0, 2),
        insn_ad(OP_LOADN, 1, 1),
        insn_ad(OP_JUMP, 0, 1),
        insn_ad(OP_LOADN, 1, 0),
        insn_abc(OP_GETUPVAL, 2, 3, 0),
        insn_ad(OP_JUMPIFNOT, 2, 5),
        insn_abc(OP_GETUPVAL, 2, 3, 0),
        insn_abc(20, 2, 2, 0), // NAMECALL R2 R3:"Cancel"
        4,
        insn_abc(OP_CALL, 2, 2, 1),
        0,
        insn_abc(OP_GETUPVAL, 2, 3, 0),
        insn_abc(OP_GETUPVAL, 4, 3, 0),
        insn_abc(OP_GETUPVAL, 5, 3, 0),
        insn_abc(53, 6, 0, 0), // NEWTABLE R6
        0,
        insn_abc(16, 1, 6, 0), // SETTABLEKS R6."Value" R1
        2,
        insn_abc(20, 2, 2, 0), // NAMECALL R2 R2:"Create"
        3,
        insn_abc(OP_CALL, 2, 5, 2),
        insn_abc(10, 2, 3, 0), // SETUPVAL R2 U3
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let constants = vec![
        Constant::String("eq".to_string()),
        Constant::String(">=".to_string()),
        Constant::String("Value".to_string()),
        Constant::String("Create".to_string()),
        Constant::String("Cancel".to_string()),
    ];
    let proto = Proto {
        max_stack_size: 8,
        num_params: 0,
        num_upvalues: 4,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants,
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("refreshAfford".to_string()),
        line_info: None,
        debug_info: Some(crate::parser::types::DebugInfo {
            locals: Vec::new(),
            upvalue_names: ["numberValue", "limit", "x", "Value"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }),
    };
    let chunk = Chunk {
        version: 6,
        types_version: 0,
        strings: vec!["eq".to_string(), ">=".to_string()],
        protos: vec![proto],
        main_proto: 0,
    };
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    println!("{out}");
    assert_no_orphan(&out);
}
