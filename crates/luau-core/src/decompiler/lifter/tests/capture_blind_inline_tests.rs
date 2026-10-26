//! A call result held in a register that a later NEWCLOSURE captures by value
//! must stay a named local. Inlining the call into its single visible read
//! left the closure's upvalue referring to an undeclared name
//! (AdoptMe LoginHandouts_spec: `v96:mark_claimed(..)` with no `local v96`).

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

const OP_LOADNIL: u8 = 2;
const OP_MOVE: u8 = 6;
const OP_FORGPREP: u8 = 58;
const OP_FORGLOOP: u8 = 59;
const OP_LOADB: u8 = 3;
const OP_LOADK: u8 = 5;
const OP_NAMECALL: u8 = 20;
const OP_GETGLOBAL: u8 = 7;
const OP_GETUPVAL: u8 = 9;
const OP_GETTABLEKS: u8 = 15;
const OP_NEWCLOSURE: u8 = 19;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;
const OP_CAPTURE: u8 = 70;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((d as u16 as u32) << 16)
}
fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}
fn proto(code: Vec<u32>, constants: Vec<Constant>, num_upvalues: u8, name: &str) -> Proto {
    Proto {
        max_stack_size: 16,
        num_params: 0,
        num_upvalues,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants,
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some(name.to_string()),
        line_info: None,
        debug_info: None,
    }
}

/// Mirrors AdoptMe LoginHandouts_spec Proto 10: the call result is the self of
/// a NAMECALL nested in call arguments, then captured by value.
fn build() -> Chunk {
    let code = vec![
        insn_abc(OP_GETUPVAL, 0, 0, 0),
        insn_ad(OP_LOADK, 2, 0),
        insn_abc(OP_NAMECALL, 0, 0, 0),
        1, // AUX: "get"
        insn_abc(OP_CALL, 0, 3, 2),
        insn_ad(OP_GETGLOBAL, 1, 0),
        2, // AUX: "expect"
        insn_abc(OP_NAMECALL, 2, 0, 0),
        3, // AUX: "can_claim"
        insn_ad(OP_LOADK, 4, 4),
        insn_abc(OP_CALL, 2, 3, 0),
        insn_abc(OP_CALL, 1, 0, 2),
        insn_abc(OP_GETTABLEKS, 1, 1, 0),
        5, // "to"
        insn_abc(OP_GETTABLEKS, 1, 1, 0),
        6, // "equal"
        insn_abc(OP_LOADB, 2, 0, 0),
        insn_abc(OP_CALL, 1, 2, 1),
        insn_ad(OP_GETGLOBAL, 1, 0),
        2,
        insn_ad(OP_NEWCLOSURE, 2, 0),
        insn_abc(OP_CAPTURE, 0, 0, 0),
        insn_abc(OP_CALL, 1, 2, 2),
        insn_abc(OP_GETTABLEKS, 1, 1, 0),
        5,
        insn_abc(OP_GETTABLEKS, 1, 1, 0),
        7, // "throw"
        insn_abc(OP_CALL, 1, 1, 1),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let consts = vec![
        Constant::String("login_handouts_manager".into()), // K0
        Constant::String("get".into()),                     // K1
        Constant::String("expect".into()),                  // K2
        Constant::String("can_claim".into()),               // K3
        Constant::String("test_invalid_key".into()),        // K4
        Constant::String("to".into()),                      // K5
        Constant::String("equal".into()),                   // K6
        Constant::String("throw".into()),                   // K7
    ];
    let mut main = proto(code, consts, 1, "main");
    main.child_protos = vec![1];
    let child = proto(
        vec![
            insn_abc(OP_GETUPVAL, 0, 0, 0),
            insn_ad(OP_LOADK, 2, 0),
            insn_abc(OP_NAMECALL, 0, 0, 0),
            1,
            insn_abc(OP_CALL, 0, 3, 1),
            insn_abc(OP_RETURN, 0, 1, 0),
        ],
        vec![
            Constant::String("test_invalid_key".into()),
            Constant::String("mark_claimed".into()),
        ],
        1,
        "child",
    );
    Chunk {
        version: 6,
        types_version: 0,
        strings: vec![],
        protos: vec![main, child],
        main_proto: 0,
    }
}

#[test]
fn val_captured_call_result_stays_a_declared_local() {
    let chunk = build();
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    assert!(
        out.contains("local get = lib:get(\"login_handouts_manager\")"),
        "the captured call result must stay bound to a local:
{out}"
    );
    assert!(
        out.contains("expect(get:can_claim("),
        "the single visible read must use the local, not the inlined call:
{out}"
    );
    assert!(
        out.contains("get:mark_claimed("),
        "the closure still reads the declared local:
{out}"
    );
}

/// Mirrors AdoptMe InteriorsM (Proto 9): `local parts = f(); for k, v in parts`
/// (the generic-for iterator is a MOVE of the call result), and a later
/// closure captures the call-result register by value.
fn build_for_in() -> Chunk {
    let code = vec![
        insn_ad(OP_GETGLOBAL, 7, 0), // 0
        0,                           // 1 aux "workspace"
        insn_abc(OP_NAMECALL, 6, 7, 0), // 2
        1,                           // 3 aux "GetParts"
        insn_ad(OP_LOADK, 8, 2),     // 4
        insn_abc(OP_CALL, 6, 3, 2),  // 5
        insn_abc(OP_MOVE, 7, 6, 0),  // 6
        insn_abc(OP_LOADNIL, 8, 0, 0), // 7
        insn_abc(OP_LOADNIL, 9, 0, 0), // 8
        insn_ad(OP_FORGPREP, 7, 4),  // 9 -> 14
        insn_ad(OP_GETGLOBAL, 12, 3), // 10
        3,                           // 11 aux "print"
        insn_abc(OP_MOVE, 13, 11, 0), // 12
        insn_abc(OP_CALL, 12, 2, 1), // 13
        insn_ad(OP_FORGLOOP, 7, -5), // 14 -> 10
        2,                           // 15 aux nresults
        insn_ad(OP_GETGLOBAL, 7, 4), // 16
        4,                           // 17 aux "spawn"
        insn_ad(OP_NEWCLOSURE, 8, 0), // 18
        insn_abc(OP_CAPTURE, 0, 6, 0), // 19 val R6
        insn_abc(OP_CALL, 7, 2, 1),  // 20
        insn_abc(OP_RETURN, 0, 1, 0), // 21
    ];
    let consts = vec![
        Constant::String("workspace".into()),
        Constant::String("GetParts".into()),
        Constant::String("arg".into()),
        Constant::String("print".into()),
        Constant::String("spawn".into()),
    ];
    let mut main = proto(code, consts, 0, "main");
    main.child_protos = vec![1];
    let child = proto(
        vec![
            insn_abc(OP_GETUPVAL, 0, 0, 0), // 0
            insn_abc(OP_LOADNIL, 1, 0, 0),  // 1
            insn_abc(OP_LOADNIL, 2, 0, 0),  // 2
            insn_ad(OP_FORGPREP, 0, 3),     // 3 -> 7
            insn_ad(OP_GETGLOBAL, 5, 0),    // 4
            0,                              // 5 aux "print"
            insn_abc(OP_CALL, 5, 1, 1),     // 6
            insn_ad(OP_FORGLOOP, 0, -4),    // 7 -> 4
            2,                              // 8
            insn_abc(OP_RETURN, 0, 1, 0),   // 9
        ],
        vec![Constant::String("print".into())],
        1,
        "child",
    );
    Chunk {
        version: 6,
        types_version: 0,
        strings: vec![],
        protos: vec![main, child],
        main_proto: 0,
    }
}

#[test]
fn val_captured_for_in_iterator_call_stays_a_declared_local() {
    let chunk = build_for_in();
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    let decl = out
        .lines()
        .find(|l| l.trim_start().starts_with("local ") && l.contains("GetParts"));
    assert!(decl.is_some(), "the captured iterator call must stay a local:
{out}");
    assert!(
        out.contains("for k, v in getparts"),
        "the loop iterates over the local:
{out}"
    );
}
