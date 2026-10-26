//! `andor-nil-value-join-lost` family: `local x = c and f(a) or nil`.
//!
//! Bytecode shapes (the joined register `Rr` is the call result):
//!   S2  `JUMPIFNOT c -> L; CALL Rr; JUMPIF Rr -> L+1; L: LOADNIL Rr`
//!   S1  `JUMPIFNOT c -> L; CALL Rr; JUMP +1;          L: LOADNIL Rr`
//! The merged register was never bound, so the output declared a phantom
//! `local vN` and read it (CameraInput `gamepadReset`, ExperienceEvents and
//! ToolDBHelper table elements).

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

const OP_LOADNIL: u8 = 2;
const OP_LOADN: u8 = 4;
const OP_LOADK: u8 = 5;
const OP_MOVE: u8 = 6;
const OP_GETGLOBAL: u8 = 7;
const OP_GETTABLEKS: u8 = 15;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;
const OP_JUMP: u8 = 23;
const OP_JUMPIF: u8 = 25;
const OP_JUMPIFNOT: u8 = 26;
const OP_NEWTABLE: u8 = 53;
const OP_SETLIST: u8 = 55;

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

fn assert_no_phantom_local(out: &str) {
    let orphan = out
        .lines()
        .any(|l| l.trim().starts_with("local v") && !l.contains('='));
    assert!(!orphan, "phantom never-assigned local:\n{out}");
}

/// CameraInput `gamepadReset`: `r = c and make("K") or nil; use(r)` over a
/// register that already held a (dead) bound value.
fn call_arm_nil_tail_over_stale_register() -> Vec<u32> {
    vec![
        insn_abc(OP_GETGLOBAL, 5, 4, 0), // R5 = src
        5,
        insn_abc(OP_GETTABLEKS, 0, 5, 0), // R0 = src.Event (stale value)
        6,
        insn_abc(OP_GETGLOBAL, 1, 0, 0), // R1 = c
        0,
        insn_ad(OP_JUMPIFNOT, 1, 5), // -> LOADNIL
        insn_abc(OP_GETGLOBAL, 0, 0, 0), // R0 = make
        1,
        insn_ad(OP_LOADK, 1, 2), // R1 = "K"
        insn_abc(OP_CALL, 0, 2, 2),
        insn_ad(OP_JUMPIF, 0, 1),
        insn_abc(OP_LOADNIL, 0, 0, 0),
        insn_abc(OP_GETGLOBAL, 2, 0, 0), // R2 = use
        3,
        insn_abc(OP_MOVE, 3, 0, 0),
        insn_abc(OP_CALL, 2, 2, 1),
        insn_abc(OP_RETURN, 0, 1, 0),
    ]
}

#[test]
fn jumpif_skip_over_loadnil_binds_the_merged_register() {
    let out = lift_single(
        call_arm_nil_tail_over_stale_register(),
        vec!["c", "make", "K", "use", "src", "Event", "Event"],
    );
    println!("{out}");
    assert_no_phantom_local(&out);
    assert!(out.contains("c and make(\"K\") or nil"), "selection lost:\n{out}");
    assert!(out.contains("use(make_2)"), "consumer lost its argument:\n{out}");
}

/// `JUMPIFNOT c; CALL Rn; JUMP +1; LOADNIL Rn` -- an ordinary if/else diamond.
#[test]
fn call_arm_jump_over_nil_keeps_the_value() {
    let code = vec![
        insn_abc(OP_GETGLOBAL, 1, 4, 0), // R1 = c
        5,
        insn_ad(OP_JUMPIFNOT, 1, 5), // -> LOADNIL
        insn_abc(OP_GETGLOBAL, 0, 0, 0), // R0 = make
        1,
        insn_ad(OP_LOADK, 1, 2), // R1 = "K"
        insn_abc(OP_CALL, 0, 2, 2),
        insn_ad(OP_JUMP, 0, 1),
        insn_abc(OP_LOADNIL, 0, 0, 0),
        insn_abc(OP_GETGLOBAL, 2, 0, 0), // R2 = use
        3,
        insn_abc(OP_MOVE, 3, 0, 0),
        insn_abc(OP_CALL, 2, 2, 1),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let out = lift_single(code, vec!["make", "make", "K", "use", "c", "c"]);
    println!("{out}");
    assert_no_phantom_local(&out);
    assert!(out.contains("make(\"K\")"), "call lost:\n{out}");
    assert!(out.contains("use(make_2)"), "consumer lost its argument:\n{out}");
}

/// ExperienceEvents proto 4: `{ delay(300), c and f(c.x) }` -- the call arm
/// is jumped over a LOADNIL and the result lands in a SETLIST element.
#[test]
fn setlist_element_call_arm_keeps_its_value() {
    let code = vec![
        insn_abc(OP_NEWTABLE, 1, 0, 0), // R1 = {}
        0,
        insn_abc(OP_GETGLOBAL, 2, 0, 0), // R2 = delay
        0,
        insn_ad(OP_LOADN, 3, 300),
        insn_abc(OP_CALL, 2, 2, 2), // R2 = delay(300)
        insn_abc(OP_GETGLOBAL, 4, 0, 0), // R4 = c
        1,
        insn_ad(OP_JUMPIFNOT, 4, 8), // -> LOADNIL
        insn_abc(OP_GETGLOBAL, 3, 0, 0), // R3 = f
        2,
        insn_abc(OP_GETGLOBAL, 4, 0, 0), // R4 = c
        1,
        insn_abc(OP_GETTABLEKS, 4, 4, 0), // R4 = c.x
        3,
        insn_abc(OP_CALL, 3, 2, 2),
        insn_ad(OP_JUMP, 0, 1),
        insn_abc(OP_LOADNIL, 3, 0, 0),
        insn_abc(OP_SETLIST, 1, 2, 3), // t[1], t[2] = R2, R3
        1,
        insn_abc(OP_GETGLOBAL, 6, 0, 0), // R6 = use
        4,
        insn_abc(OP_MOVE, 7, 1, 0),
        insn_abc(OP_CALL, 6, 2, 1),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let out = lift_single(code, vec!["delay", "c", "f", "x", "use"]);
    println!("{out}");
    assert_no_phantom_local(&out);
    assert!(
        out.contains("local f_2\n") && out.contains("f_2 = f(c.x)") && out.contains("f_2 = nil"),
        "value not bound across the join:\n{out}"
    );
}

/// BigNum `change`: the register holds a stale `nil`, the then arm assigns it
/// from a call and the else arm only calls `error(..)` -- it never writes the
/// register. No `len = nil` may be invented in the else arm.
#[test]
fn else_arm_that_never_writes_the_register_gets_no_nil_store() {
    let code = vec![
        insn_abc(OP_LOADNIL, 5, 0, 0), // 0: R5 = nil (stale)
        insn_abc(OP_GETGLOBAL, 1, 0, 0), // 1: R1 = c
        0,
        insn_ad(OP_JUMPIFNOT, 1, 5), // 3: -> 9 (else arm)
        insn_abc(OP_GETGLOBAL, 5, 1, 0), // 4: R5 = make
        1,
        insn_ad(OP_LOADK, 6, 2), // 6: R6 = "K"
        insn_abc(OP_CALL, 5, 2, 2), // 7: R5 = make("K")
        insn_ad(OP_JUMP, 0, 4), // 8: -> 13
        insn_abc(OP_GETGLOBAL, 7, 4, 0), // 9: R7 = error
        4,
        insn_ad(OP_LOADK, 8, 5), // 11
        insn_abc(OP_CALL, 7, 2, 1), // 12: error("bad")
        insn_abc(OP_RETURN, 0, 1, 0), // 13
    ];
    let out = lift_single(code, vec!["c", "make", "K", "use", "error", "bad"]);
    println!("{out}");
    assert!(!out.lines().any(|l| l.trim().starts_with("make_2 = nil")), "invented a nil store the bytecode lacks:\n{out}");
}
