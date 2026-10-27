//! `a and typeof(r) == "string" and r or "fallback"`: the inner conditional of an
//! and-chain jumps to the ELSE value (end + 1, past the else-skip JUMP), which
//! used to fall into the out-of-range handler and vanish, leaving the compared
//! CALL as an unused local and the value unconditionally `r`.
//!
//! Real shape: GhostHunter `converse` (v14) and `ws ~= nil and ws:IsA("Model")
//! and ws or nil` in HalloweenDimensionController `syncChase`.

use crate::ast::Expr;
use crate::decompiler::emit::emit_block;
use crate::decompiler::lifter::naming::{LocalTracker, RegVal};
use crate::decompiler::lifter::opcode_handlers::lift_instruction_range;
use crate::decompiler::{analyze_register_usage, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

const OP_LOADNIL: u8 = 2;
const OP_LOADN: u8 = 4;
const OP_LOADK: u8 = 5;
const OP_MOVE: u8 = 6;
const OP_GETGLOBAL: u8 = 7;
const OP_SETTABLEKS: u8 = 16;
const OP_NAMECALL: u8 = 20;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;
const OP_JUMP: u8 = 23;
const OP_JUMPIFNOT: u8 = 26;
const OP_JUMPXEQKNIL: u8 = 78;
const OP_JUMPXEQKS: u8 = 81;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((d as u16 as u32) << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn chunk_of(code: Vec<u32>, constants: Vec<Constant>) -> Chunk {
    let proto = Proto {
        max_stack_size: 8,
        num_params: 2,
        num_upvalues: 0,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants,
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("andor".to_string()),
        line_info: None,
        debug_info: None,
    };
    Chunk { version: 6, types_version: 0, strings: Vec::new(), protos: vec![proto], main_proto: 0 }
}

/// Lift the whole proto as ONE flat range, the way a `Region::Linear` is
/// lifted (the structured regions do not claim every and/or value; the real
/// GhostHunter and syncChase shapes reach the conditional-jump arms through
/// this flat path). Returns the emitted source of the raw statements.
fn lift(chunk: &Chunk) -> String {
    let proto = &chunk.protos[0];
    let mut ctx = DecompileContext::new(chunk);
    let hints = analyze_register_usage(proto, &ctx.chunk.strings, None, Some(&ctx.chunk.protos));
    ctx.init_proto_naming(0, hints, &[]);
    ctx.current_proto_index = Some(0);
    let mut regs = vec![RegVal::Unknown; 256];
    let mut locals = LocalTracker::new(proto.num_params as usize);
    for i in 0..proto.num_params {
        let name = ctx.reg_name(proto, i, 0);
        regs[i as usize] = RegVal::Expr(Expr::Name(name.clone()));
        locals.record_name(i as usize, &name);
    }
    let mut stmts = Vec::new();
    lift_instruction_range(
        &mut ctx, proto, 0, 0, 0, proto.code.len(), &mut regs, &mut locals, &mut stmts, false,
    );
    let mut out = String::new();
    emit_block(&mut out, &stmts, 0);
    out
}

fn strs(names: &[&str]) -> Vec<Constant> {
    names.iter().map(|s| Constant::String((*s).to_string())).collect()
}

/// ```lua
/// -- params: ok (R0), r (R1)
/// r.Color = ok and typeof(r) == "string" and r or "fallback"
/// ```
/// `between` is spliced in right after the outer test; it must not change the
/// jump layout, so callers pass a block of known length via `pad`.
fn typeof_chunk(between: &[u32]) -> Chunk {
    let n = between.len() as i16;
    let mut code = vec![
        insn_ad(OP_JUMPIFNOT, 0, 8 + n),     // outer: ok falsy -> else
    ];
    code.extend_from_slice(between);
    code.extend_from_slice(&[
        insn_abc(OP_GETGLOBAL, 3, 0, 0),      // R3 = typeof
        0,                                    // aux K0 "typeof"
        insn_abc(OP_MOVE, 4, 1, 0),           // R4 = r
        insn_abc(OP_CALL, 3, 2, 2),           // R3 = typeof(r)
        insn_ad(OP_JUMPXEQKS, 3, 3),          // R3 ~= "string" -> else
        1 | 0x8000_0000,                      // aux K1 "string", NOT
        insn_abc(OP_MOVE, 2, 1, 0),           // R2 = r
        insn_ad(OP_JUMP, 0, 1),               // else-skip
        insn_ad(OP_LOADK, 2, 2),              // else: R2 = "fallback"
        insn_abc(OP_SETTABLEKS, 2, 1, 0),     // r.Color = R2
        4,                                    // aux K4 "Color"
        insn_abc(OP_GETGLOBAL, 5, 0, 0),
        3,                                    // aux K3 "print"
        insn_abc(OP_MOVE, 6, 1, 0),
        insn_abc(OP_CALL, 5, 2, 1),           // print(r)
        insn_abc(OP_RETURN, 0, 1, 0),
    ]);
    chunk_of(code, strs(&["typeof", "string", "fallback", "print", "Color"]))
}

#[test]
fn typeof_compare_between_and_links_is_kept_in_the_condition() {
    let out = lift(&typeof_chunk(&[]));
    eprintln!("{out}");
    assert!(out.contains("typeof("), "typeof call lost:\n{out}");
    assert!(out.contains("== \"string\""), "comparison dropped:\n{out}");
    assert!(!out.contains("local result"), "unused typeof local left behind:\n{out}");
}

/// ```lua
/// -- params: ws (R0)
/// ws.Target = ws ~= nil and ws:IsA("Model") and ws or nil
/// ```
fn isa_chunk() -> Chunk {
    let code = vec![
        insn_ad(OP_JUMPXEQKNIL, 0, 8),        // ws == nil -> else
        0,                                    // aux: nil, not negated
        insn_abc(OP_NAMECALL, 2, 0, 0),       // R2 = ws.IsA, R3 = ws
        0,                                    // aux K0 "IsA"
        insn_ad(OP_LOADK, 4, 1),              // R4 = "Model"
        insn_abc(OP_CALL, 2, 3, 2),           // R2 = R2(R3, R4)
        insn_ad(OP_JUMPIFNOT, 2, 2),          // not R2 -> else
        insn_abc(OP_MOVE, 5, 0, 0),           // R5 = ws
        insn_ad(OP_JUMP, 0, 1),               // else-skip
        insn_abc(OP_LOADNIL, 5, 0, 0),        // else: R5 = nil
        insn_abc(OP_SETTABLEKS, 5, 0, 0),     // ws.Target = R5
        2,                                    // aux K2 "Target"
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    chunk_of(code, strs(&["IsA", "Model", "Target"]))
}

#[test]
fn isa_test_between_and_links_is_kept_in_the_condition() {
    let out = lift(&isa_chunk());
    eprintln!("{out}");
    assert!(out.contains(":IsA(\"Model\")"), "IsA test dropped:\n{out}");
    assert!(
        !out.lines().any(|l| l.trim_start().starts_with("local ") && l.contains(":IsA(")),
        "IsA(..) parked in a local instead of the condition:\n{out}"
    );
}

/// A side-effecting call between the tests must not be folded: the call has to
/// stay a statement inside the guarded region, never hoisted into the condition.
#[test]
fn side_effecting_call_between_tests_is_not_folded() {
    let print_call = [
        insn_abc(OP_GETGLOBAL, 5, 0, 0),
        3,                                    // aux K3 "print"
        insn_ad(OP_LOADN, 6, 1),
        insn_abc(OP_CALL, 5, 2, 1),           // print(1)
    ];
    let out = lift(&typeof_chunk(&print_call));
    eprintln!("{out}");
    assert!(out.contains("print(1)"), "side effect lost:\n{out}");
    assert!(
        !out.lines().any(|l| l.contains("print(1)") && l.contains(" and ")),
        "side-effecting call folded into an expression:\n{out}"
    );
}
