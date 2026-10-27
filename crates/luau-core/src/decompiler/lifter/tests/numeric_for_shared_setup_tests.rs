//! A numeric-for setup local that something else also reads must stay a local.
//!
//! Real shapes: PetSim99 `AOC` (a `DIV` step local read again inside the
//! body) and StealAnEgg `SammyEvent_UISystem` (a call-result limit local also
//! read by a pending `DIV` that the body consumes). Absorbing the declaration
//! into the `for` header left those readers on a never-bound `vN`.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Proto};

const OP_MOVE: u8 = 6;
const OP_SUB: u8 = 34;
const OP_DIV: u8 = 36;
const OP_MUL: u8 = 35;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;
const OP_FORNPREP: u8 = 56;
const OP_FORNLOOP: u8 = 57;

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
        debug_name: Some("shared_setup".to_string()),
        line_info: None,
        debug_info: None,
    };
    Chunk { version: 6, types_version: 0, strings: Vec::new(), protos: vec![proto], main_proto: 0 }
}

fn lift(chunk: &Chunk) -> String {
    let mut ctx = DecompileContext::new(chunk);
    decompile_proto(&mut ctx, &chunk.protos[0], 0, 0)
}

fn for_header(source: &str) -> &str {
    source.lines().find(|l| l.trim_start().starts_with("for ")).expect("a for header")
}

/// ```lua
/// local function f(fn, a, b)
///     local step = (a - b) / b          -- R3
///     for i = a, b, step do fn(i * step) end
/// end
/// ```
/// Loop control R4..R6 (limit, step, start); the body reads the step local R3.
#[test]
fn step_local_read_in_body_is_not_absorbed_into_the_header() {
    let code = vec![
        insn_abc(OP_SUB, 7, 1, 2),    // 0: R7 = R1 - R2
        insn_abc(OP_DIV, 3, 7, 2),    // 1: R3 = R7 / R2        (step local)
        insn_abc(OP_MOVE, 6, 1, 0),   // 2: R6 = R1             (start, A+2)
        insn_abc(OP_MOVE, 4, 2, 0),   // 3: R4 = R2             (limit, A)
        insn_abc(OP_MOVE, 5, 3, 0),   // 4: R5 = R3             (step, A+1)
        insn_ad(OP_FORNPREP, 4, 4),   // 5: loop at 9
        insn_abc(OP_MOVE, 7, 0, 0),   // 6: R7 = fn
        insn_abc(OP_MUL, 8, 6, 3),    // 7: R8 = i * step
        insn_abc(OP_CALL, 7, 2, 1),   // 8: fn(R8)
        insn_ad(OP_FORNLOOP, 4, -4),  // 9
        insn_abc(OP_RETURN, 0, 1, 0), // 10
    ];
    let src = lift(&chunk_of(code, 3, 9));
    let header = for_header(&src);
    assert!(
        !header.contains('/'),
        "step expression was inlined into the header while the body still reads it:\n{src}"
    );
}
