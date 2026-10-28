//! Value joins whose arm holds arithmetic: `n = n + 1 <= 8 and n + 1 or 1`
//! (Enchants `SetNextIncremental`, GUIFX GIF frame counter). The arm's ADDK
//! used to disqualify the join, so the arm value went to a fresh local that was
//! never read and the upvalue got the constant 1 on every path.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, DebugInfo, Proto};

const OP_LOADN: u8 = 4;
const OP_GETUPVAL: u8 = 9;
const OP_SETUPVAL: u8 = 10;
const OP_RETURN: u8 = 22;
const OP_JUMPIF: u8 = 25;
const OP_JUMPIFNOTLE: u8 = 31;
const OP_ADDK: u8 = 39;

fn ad(op: u8, a: u8, d: i16) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((d as u16 as u32) << 16)
}
fn abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn lift(code: Vec<u32>) -> String {
    let proto = Proto {
        max_stack_size: 8,
        num_params: 0,
        num_upvalues: 1,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants: vec![Constant::Number(1.0)],
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("SetNextIncremental".to_string()),
        line_info: None,
        debug_info: Some(DebugInfo {
            locals: Vec::new(),
            upvalue_names: vec!["count".to_string()],
        }),
    };
    let chunk = Chunk {
        version: 6,
        types_version: 0,
        strings: Vec::new(),
        protos: vec![proto],
        main_proto: 0,
    };
    let mut ctx = DecompileContext::new(&chunk);
    decompile_proto(&mut ctx, &chunk.protos[0], 0, 0)
}

#[test]
fn counter_wrap_keeps_its_incremented_arm() {
    // GETUPVAL R2 U0; ADDK R1 R2+K0; LOADN R2 8; JUMPIFNOTLE R1 R2 -> else
    // GETUPVAL R1 U0; ADDK R0 R1+K0; JUMPIF R0 -> store; else: LOADN R0 1
    // store: SETUPVAL R0 U0; RETURN
    let code = vec![
        abc(OP_GETUPVAL, 2, 0, 0),
        abc(OP_ADDK, 1, 2, 0),
        ad(OP_LOADN, 2, 8),
        ad(OP_JUMPIFNOTLE, 1, 4),
        2,
        abc(OP_GETUPVAL, 1, 0, 0),
        abc(OP_ADDK, 0, 1, 0),
        ad(OP_JUMPIF, 0, 1),
        ad(OP_LOADN, 0, 1),
        abc(OP_SETUPVAL, 0, 0, 0),
        abc(OP_RETURN, 0, 1, 0),
    ];
    let out = lift(code);
    println!("{out}");
    assert!(
        out.contains("count + 1 <= 8 and count + 1 or 1"),
        "arm value must stay in the ternary:\n{out}"
    );
    assert!(!out.contains("local v0"), "no discarded arm local:\n{out}");
}
