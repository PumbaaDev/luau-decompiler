//! `declared_never_assigned`, parameter-slot diamond whose parameter is
//! captured BY REFERENCE by a closure created AFTER the merge (RIVALS
//! Quaternion `SlerpFunction`, 5 reads of an unbound `v1`).
//!
//! `param_slot_rebound_in_both_arms` used to refuse every parameter with a
//! by-ref CAPTURE anywhere in the proto, so the diamond fell back to a fresh
//! local per arm and the merge read minted an unbound `vN`. A closure created
//! after the merge captures whatever name the register holds then, so the
//! seed is sound there; a capture BEFORE the merge still shares the original
//! variable and must keep refusing.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Proto};

const OP_GETUPVAL: u8 = 9;
const OP_CLOSEUPVALS: u8 = 11;
const OP_MOVE: u8 = 6;
const OP_NEWCLOSURE: u8 = 19;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;
const OP_JUMP: u8 = 23;
const OP_JUMPIFNOT: u8 = 26;
const OP_CAPTURE: u8 = 70;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((d as u16 as u32) << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn proto(code: Vec<u32>, num_params: u8, num_upvalues: u8, name: &str) -> Proto {
    Proto {
        max_stack_size: 8,
        num_params,
        num_upvalues,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants: Vec::new(),
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some(name.to_string()),
        line_info: None,
        debug_info: None,
    }
}

/// `function(a, b, c) <closure over c, if captured early>
///    if a then c = a(c) else c = b() end
///    a(c)  <closure over c, if captured late> end`
fn build(capture_before: bool) -> Chunk {
    let mut code = Vec::new();
    if capture_before {
        code.push(insn_ad(OP_NEWCLOSURE, 5, 0));
        code.push(insn_abc(OP_CAPTURE, 1, 2, 0));
    }
    let base = code.len();
    let _ = base;
    code.extend([
        insn_ad(OP_JUMPIFNOT, 0, 5),
        insn_abc(OP_MOVE, 3, 0, 0),
        insn_abc(OP_MOVE, 4, 2, 0),
        insn_abc(OP_CALL, 3, 2, 2),
        insn_abc(OP_MOVE, 2, 3, 0),
        insn_ad(OP_JUMP, 0, 3),
        insn_abc(OP_MOVE, 3, 1, 0),
        insn_abc(OP_CALL, 3, 1, 2),
        insn_abc(OP_MOVE, 2, 3, 0),
        insn_abc(OP_MOVE, 3, 0, 0),
        insn_abc(OP_MOVE, 4, 2, 0),
        insn_abc(OP_CALL, 3, 2, 1),
    ]);
    if !capture_before {
        code.push(insn_ad(OP_NEWCLOSURE, 5, 0));
        code.push(insn_abc(OP_CAPTURE, 1, 2, 0));
        code.push(insn_abc(OP_CLOSEUPVALS, 2, 0, 0));
    }
    code.push(insn_abc(OP_RETURN, 5, 2, 0));
    let mut parent = proto(code, 3, 0, "parent");
    parent.child_protos = vec![1];
    let child = proto(vec![insn_abc(OP_GETUPVAL, 0, 0, 0), insn_abc(OP_RETURN, 0, 2, 0)], 0, 1, "child");
    Chunk { version: 6, types_version: 0, strings: Vec::new(), protos: vec![parent, child], main_proto: 0 }
}

fn merge_call_arg(out: &str) -> String {
    let start = out.rfind("arg1(").expect("merge call missing") + "arg1(".len();
    let rest = &out[start..];
    rest[..rest.find(')').expect("unterminated call")].trim().to_string()
}

#[test]
fn param_diamond_with_ref_capture_after_the_merge_is_bound_at_the_merge() {
    let chunk = build(false);
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    eprintln!("=== emitted source ===\n{}\n======================", out);

    let arg = merge_call_arg(&out);
    assert!(!arg.is_empty() && arg != "arg3", "merge reads the stale parameter:\n{out}");
    let assigns = out
        .lines()
        .map(|l| l.trim())
        .filter(|l| l.starts_with(&format!("{arg} =")) || l.starts_with(&format!("local {arg} =")))
        .count();
    assert!(assigns >= 3, "`{arg}` is not assigned on both arms (entry copy + 2 arm writes):\n{out}");
    assert!(
        !out.lines().any(|l| l.trim().starts_with("local v") && !l.contains('=')),
        "never-assigned hoisted local:\n{out}"
    );
}

#[test]
fn param_diamond_with_ref_capture_before_the_merge_keeps_refusing() {
    let chunk = build(true);
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    eprintln!("=== emitted source ===\n{}\n======================", out);
    assert!(
        !out.contains("local sel2 = arg3"),
        "an early by-ref capture shares the parameter variable; a fresh copy would split it:\n{out}"
    );
}
