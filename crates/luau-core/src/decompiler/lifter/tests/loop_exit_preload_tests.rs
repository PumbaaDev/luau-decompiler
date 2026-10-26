//! A `for` loop whose normal exit loads a constant flag that a `break` arm
//! jumps past (BigNum `__pow`, PetSim Vending `machineLoaded`): the join's
//! guard and everything after it must survive.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Proto};

const OP_LOADB: u8 = 3;
const OP_LOADN: u8 = 4;
const OP_MOVE: u8 = 6;
const OP_RETURN: u8 = 22;
const OP_JUMP: u8 = 23;
const OP_JUMPIFNOT: u8 = 26;
const OP_FORNPREP: u8 = 56;
const OP_FORNLOOP: u8 = 57;

fn ad(op: u8, a: u8, d: i16) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((d as u16 as u32) << 16)
}

fn abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn lift(code: Vec<u32>) -> String {
    let proto = Proto {
        max_stack_size: 8,
        num_params: 2,
        num_upvalues: 0,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants: Vec::new(),
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("flag_loop".to_string()),
        line_info: None,
        debug_info: None,
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

/// ```lua
/// for i = 1, n do
///     if t then   -- JUMPIFNOT R0 -> FORNLOOP
///         flag = false; break
///     end
/// end                      -- normal exit: flag = true
/// if flag then return t end
/// return n
/// ```
fn flag_loop_code(arm_after_load: bool) -> Vec<u32> {
    let _ = arm_after_load;
    vec![
        abc(OP_MOVE, 2, 1, 0),   // 0: limit
        ad(OP_LOADN, 3, 1),      // 1: step
        ad(OP_LOADN, 4, 1),      // 2: start
        ad(OP_FORNPREP, 2, 4),   // 3: exit at 8
        ad(OP_JUMPIFNOT, 0, 2),  // 4: -> 7
        abc(OP_LOADB, 5, 0, 0),  // 5: flag = false
        ad(OP_JUMP, 0, 2),       // 6: break -> 9 (join)
        ad(OP_FORNLOOP, 2, -4),  // 7: back to 4
        abc(OP_LOADB, 5, 1, 0),  // 8: flag = true (normal exit)
        ad(OP_JUMPIFNOT, 5, 1),  // 9: join -> 11
        abc(OP_RETURN, 0, 2, 0), // 10: return t
        abc(OP_RETURN, 1, 2, 0), // 11: return n
    ]
}

#[test]
fn break_arm_flag_survives_to_the_post_loop_guard() {
    let source = lift(flag_loop_code(false));
    let for_at = source.find("for ").unwrap_or_else(|| panic!("no loop:
{source}"));
    let true_at = source.find("true").unwrap_or_else(|| panic!("no flag init:\n{source}"));
    assert!(true_at < for_at, "flag must be initialised before the loop:\n{source}");
    assert!(source.contains("false"), "break arm write lost:\n{source}");
    assert!(source.contains("return arg1"), "guarded return lost:\n{source}");
    assert!(source.contains("return arg2"), "continuation lost:\n{source}");
}
