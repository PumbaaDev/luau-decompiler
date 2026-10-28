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

/// AdoptMe VehicleClient: the join is the NEXT statement (a LOADN here), not a
/// test of the flag, so hoisting the flag load would lose the flag's value.
#[test]
fn join_that_does_not_test_the_flag_is_not_preloaded() {
    let mut code = flag_loop_code(false);
    code[9] = ad(OP_LOADN, 6, 7); // join no longer reads R5
    let found = crate::decompiler::lifter::loop_exit_preload::detect(&code, 3, 7);
    assert!(found.is_none(), "join must test the flag register");
}

#[test]
fn join_that_tests_the_flag_is_preloaded() {
    let code = flag_loop_code(false);
    let found = crate::decompiler::lifter::loop_exit_preload::detect(&code, 3, 7);
    assert!(found.is_some());
}

/// Luau inlines a helper that searches a table and returns the hit
/// (DungeonQuest sellShop `showWeaponStats`, StealAnEgg MonsterParasite
/// `BoostMultiplier`). The hit register is written by the break arm
/// (`MOVE R5 R3; JUMP join`); the normal exit runs OTHER statements (an
/// `error(...)` call in MonsterParasite) and only then `LOADNIL R5`; the join
/// READS R5 a few instructions later without testing it. The preload must
/// still apply, or every read of R5 becomes a never-assigned local.
fn inlined_search_code() -> Vec<u32> {
    vec![
        abc(OP_MOVE, 2, 1, 0),   // 0: limit
        ad(OP_LOADN, 3, 1),      // 1: step
        ad(OP_LOADN, 4, 1),      // 2: start
        ad(OP_FORNPREP, 2, 4),   // 3: exit at 8
        ad(OP_JUMPIFNOT, 0, 2),  // 4: -> 7
        abc(OP_MOVE, 5, 3, 0),   // 5: hit = i
        ad(OP_JUMP, 0, 3),       // 6: break -> 10 (join)
        ad(OP_FORNLOOP, 2, -4),  // 7: back to 4
        ad(OP_LOADN, 6, 9),      // 8: unrelated exit statement
        abc(OP_LOADB, 5, 0, 0),  // 9: hit = false (normal exit)
        abc(OP_MOVE, 6, 5, 0),   // 10: join reads hit, no test
        abc(OP_RETURN, 6, 2, 0), // 11
    ]
}

#[test]
fn exit_load_after_unrelated_statements_with_a_reading_join_is_preloaded() {
    let code = inlined_search_code();
    let found = crate::decompiler::lifter::loop_exit_preload::detect(&code, 3, 7)
        .expect("shape must be detected");
    assert_eq!(found.load_pc, 9);
    assert_eq!(found.reg, 5);
}

#[test]
fn inlined_search_keeps_the_hit_register_assigned() {
    let source = lift(inlined_search_code());
    let for_at = source.find("for ").unwrap_or_else(|| panic!("no loop:\n{source}"));
    let decl = source.find("= false").unwrap_or_else(|| panic!("no hit init:\n{source}"));
    assert!(decl < for_at, "hit must be initialised before the loop:\n{source}");
    assert!(source.contains("break"), "break lost:\n{source}");
}

#[test]
fn an_unrelated_jump_between_loop_and_exit_load_is_refused() {
    let mut code = inlined_search_code();
    code[8] = ad(OP_JUMPIFNOT, 0, 0); // control flow in the exit run
    let found = crate::decompiler::lifter::loop_exit_preload::detect(&code, 3, 7);
    assert!(found.is_none(), "exit run must be straight-line");
}

#[test]
fn a_statement_that_touches_the_flag_before_the_exit_load_is_refused() {
    let mut code = inlined_search_code();
    code[8] = ad(OP_LOADN, 5, 9); // R5 rewritten before the load
    let found = crate::decompiler::lifter::loop_exit_preload::detect(&code, 3, 7);
    assert!(found.is_none());
}
