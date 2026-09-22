//! OR-chain return guard whose interior link needs a PURE preamble.
//!
//! `if not x or not x.id then warn() return false end return true` compiles to
//! two conditional blocks sharing one terminal `warn(); return false` block;
//! the second link must compute `x.id` (GETTABLEKS) before its branch.
//! `find_or_chain_return_guard` used to accept only BARE interior links, so the
//! chain stopped after one link, the plain if/else structurer flattened the
//! guard, and the shared terminal was emitted after `return true` and removed as
//! dead code — the call vanished (`calls_not_emitted`; reproduced from canonical
//! `luau-compile -O1` and on AdoptMe `ToppingsHelper`-style guards). These tests
//! hand-assemble that shape and pin that the terminal's call survives, guarded,
//! before the trailing `return true`.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

const OP_LOADB: u8 = 3;
const OP_MOVE: u8 = 6;
const OP_GETTABLEKS: u8 = 15;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;
const OP_JUMPIF: u8 = 25;
const OP_JUMPIFNOT: u8 = 26;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    let du = d as u16 as u32;
    (op as u32) | ((a as u32) << 8) | (du << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn chunk_of(code: Vec<u32>, constants: Vec<Constant>, num_params: u8) -> Chunk {
    let proto = Proto {
        max_stack_size: 8,
        num_params,
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
    Chunk {
        version: 6,
        types_version: 0,
        strings: Vec::new(),
        protos: vec![proto],
        main_proto: 0,
    }
}

/// The entry link computes its tested value with a CALL (as `ToppingsHelper`'s
/// `get_equipped_by_category(..)[1]` does), which keeps the shape out of
/// `Region::GuardedTailIf` (that matcher wants a pure entry) and routes it to
/// the OR-chain matcher — the path that used to reject the second link.
///
/// ```lua
/// -- params: get (R0), warn (R1)
/// local x = get()
/// if not x or not x.id then
///     warn()
///     return false
/// end
/// return true
/// ```
///
/// ```text
/// w0  MOVE       R2 R0
/// w1  CALL       R2 (0 args, 1 result)  ; x = get()   (entry preamble)
/// w2  JUMPIFNOT  R2 -> w6               ; link 1: not x -> T
/// w3  GETTABLEKS R3 R2 "id"             ; link 2 PREAMBLE (pure read)
/// w4  <aux = K0 "id">
/// w5  JUMPIF     R3 -> w10              ; link 2: x.id -> continuation
/// w6  MOVE       R4 R1                  ; T: warn()
/// w7  CALL       R4 (0 args, 0 results)
/// w8  LOADB      R4 false
/// w9  RETURN     R4 (1 value)           ; return false
/// w10 LOADB      R4 true                ; C
/// w11 RETURN     R4 (1 value)           ; return true
/// ```
fn build_or_chain_chunk() -> Chunk {
    let code = vec![
        insn_abc(OP_MOVE, 2, 0, 0),       // w0  R2 = R0
        insn_abc(OP_CALL, 2, 1, 2),       // w1  R2 = R2()
        insn_ad(OP_JUMPIFNOT, 2, 3),      // w2  -> w6
        insn_abc(OP_GETTABLEKS, 3, 2, 0), // w3  R3 = R2.id
        0u32,                             // w4  aux = K0 "id"
        insn_ad(OP_JUMPIF, 3, 4),         // w5  -> w10
        insn_abc(OP_MOVE, 4, 1, 0),       // w6  R4 = R1
        insn_abc(OP_CALL, 4, 1, 1),       // w7  R4()
        insn_abc(OP_LOADB, 4, 0, 0),      // w8  R4 = false
        insn_abc(OP_RETURN, 4, 2, 0),     // w9  return R4
        insn_abc(OP_LOADB, 4, 1, 0),      // w10 R4 = true
        insn_abc(OP_RETURN, 4, 2, 0),     // w11 return R4
    ];
    chunk_of(code, vec![Constant::String("id".to_string())], 2)
}

#[test]
fn or_chain_with_pure_preamble_link_keeps_the_shared_terminal() {
    let chunk = build_or_chain_chunk();
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    eprintln!("=== emitted source ===\n{}\n======================", out);

    let code_lines: Vec<&str> = out
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with("--"))
        .collect();
    // Nothing may follow a TOP-LEVEL `return`: code there is dead (the defect
    // emitted the terminal's `warn(); return false` after `return true`).
    let first_top_return = code_lines
        .iter()
        .position(|l| l.starts_with("return"))
        .unwrap_or_else(|| panic!("no top-level return:\n{out}"));
    assert_eq!(
        first_top_return,
        code_lines.len() - 1,
        "code emitted after a top-level return (dead):\n{out}"
    );
    // The shared terminal's call must be emitted.
    let has_call = code_lines.iter().any(|l| {
        let t = l.trim();
        t.ends_with("()") && !t.starts_with("local ") && !t.contains('=')
    });
    assert!(has_call, "the terminal's warn() call was dropped:\n{out}");
    // Both `return false` (guard failed) and `return true` must be reachable.
    assert!(out.contains("return false"), "`return false` missing:\n{out}");
    assert!(out.contains("return true"), "`return true` missing:\n{out}");
    // The second link's test reads `.id` in a condition, not a dead local.
    assert!(
        code_lines.iter().any(|l| l.trim_start().starts_with("if ") && l.contains(".id")),
        "second link's `.id` test is missing from any condition:\n{out}"
    );
}
