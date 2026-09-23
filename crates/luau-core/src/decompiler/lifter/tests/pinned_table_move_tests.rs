//! A pending table literal MOVEd into a PINNED merge slot must be emitted as
//! an assignment to the pinned binding, exactly like any other arm write.
//!
//! Bytecode exhibit: PS99 `ReplicatedStorage_Library_Universal_Charms` (and 17
//! sibling `Library_Universal_*` modules built from the same template):
//!
//! ```text
//!   14: LOADNIL    R1                       ; merge seed (pinned)
//!   24: JUMPIFNOT  R2 -> else
//!   36: DUPTABLE   R3 K14                   ; then: { GetPower = ... }
//!   39: SETTABLEKS R3."GetPower" R4
//!   41: MOVE       R1 R3
//!   42: JUMP       -> join
//!   56: DUPTABLE   R3 K14                   ; else: same template
//!   59: SETTABLEKS R3."GetPower" R4
//!   61: MOVE       R1 R3
//!   64: MOVE       R3 R1                    ; join: table.freeze(R1)
//! ```
//!
//! The MOVE handler used to exclude table sources from the pin, parking the
//! literal in R1; the two arms then reached the join holding different pending
//! tables, `merge_regs` reset R1 to Unknown, and `table.freeze(v1)` read an
//! unbound name. The fix emits `bound = {..}` and re-points the source register
//! at `bound` -- but only while the literal is still unconsumed, so a table
//! that was already stored elsewhere is never duplicated into a second object.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

// Canonical Luau opcode bytes (identity opmap, Chunk.version = 6).
const OP_LOADNIL: u8 = 2;
const OP_MOVE: u8 = 6;
const OP_GETUPVAL: u8 = 9;
const OP_GETIMPORT: u8 = 12;
const OP_SETTABLE: u8 = 14;
const OP_SETTABLEKS: u8 = 16;
const OP_NEWCLOSURE: u8 = 19;
const OP_RETURN: u8 = 22;
const OP_JUMP: u8 = 23;
const OP_JUMPIFNOT: u8 = 26;
const OP_DIV: u8 = 36;
const OP_NEWTABLE: u8 = 53;
const OP_CAPTURE: u8 = 70;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    let du = d as u16 as u32;
    (op as u32) | ((a as u32) << 8) | (du << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn pack_import(ids: &[u32]) -> u32 {
    let count = (ids.len() as u32) & 0x3;
    let mut v = count << 30;
    if !ids.is_empty() {
        v |= (ids[0] & 0x3FF) << 20;
    }
    v
}

fn proto(code: Vec<u32>, constants: Vec<Constant>, num_upvalues: u8, name: &str) -> Proto {
    Proto {
        max_stack_size: 16,
        num_params: 1,
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

/// Parent captures `Config` / `Backup` into the child; the child runs a
/// diamond where each arm builds `{ Power = <upvalue> }` in R3 and MOVEs it
/// into the nil-seeded merge slot R1, then returns R1 after the join.
/// `extra` is spliced into the THEN arm right after its SETTABLEKS (used to
/// hand the literal out before the MOVE).
fn table_diamond_chunk(extra: Vec<u32>) -> Chunk {
    let import_a = pack_import(&[2]);
    let import_b = pack_import(&[4]);
    let parent_code = vec![
        insn_ad(OP_GETIMPORT, 1, 0),
        import_a, // AUX
        insn_ad(OP_GETIMPORT, 2, 3),
        import_b, // AUX
        insn_ad(OP_NEWCLOSURE, 3, 0),
        insn_abc(OP_CAPTURE, 0, 1, 0),
        insn_abc(OP_CAPTURE, 0, 2, 0),
        insn_abc(OP_RETURN, 3, 2, 0),
    ];
    let parent_constants = vec![
        Constant::Import(import_a),
        Constant::String("unused0".to_string()),
        Constant::String("Config".to_string()),
        Constant::Import(import_b),
        Constant::String("Backup".to_string()),
    ];

    let mut then_arm = vec![
        insn_abc(OP_GETUPVAL, 2, 0, 0),
        insn_abc(OP_DIV, 0, 0, 2),
        insn_abc(OP_NEWTABLE, 3, 0, 0),
        0, // AUX: array size
        insn_abc(OP_SETTABLEKS, 2, 3, 0),
        0, // AUX: K0 = "Power"
    ];
    then_arm.extend(extra);
    then_arm.push(insn_abc(OP_MOVE, 1, 3, 0));
    let else_arm = vec![
        insn_abc(OP_GETUPVAL, 2, 1, 0),
        insn_abc(OP_DIV, 0, 0, 2),
        insn_abc(OP_NEWTABLE, 3, 0, 0),
        0,
        insn_abc(OP_SETTABLEKS, 2, 3, 0),
        0,
        insn_abc(OP_MOVE, 1, 3, 0),
    ];
    // JUMPIFNOT at pc 1 skips the then-arm plus its trailing JUMP.
    let to_else = (then_arm.len() + 1) as i16;
    let to_join = else_arm.len() as i16;
    let mut child_code = vec![insn_abc(OP_LOADNIL, 1, 0, 0), insn_ad(OP_JUMPIFNOT, 0, to_else)];
    child_code.extend(then_arm);
    child_code.push(insn_ad(OP_JUMP, 0, to_join));
    child_code.extend(else_arm);
    child_code.extend([
        insn_abc(OP_MOVE, 3, 1, 0),
        insn_abc(OP_MOVE, 4, 0, 0),
        insn_abc(OP_RETURN, 3, 3, 0),
    ]);

    let mut parent = proto(parent_code, parent_constants, 0, "table_parent");
    parent.child_protos = vec![1];
    let child = proto(
        child_code,
        vec![Constant::String("Power".to_string())],
        2,
        "chooser",
    );
    Chunk {
        version: 6,
        types_version: 0,
        strings: vec![
            "unused0".to_string(),
            "unused1".to_string(),
            "Config".to_string(),
            "Power".to_string(),
            "Backup".to_string(),
        ],
        protos: vec![parent, child],
        main_proto: 0,
    }
}

/// The name `N` that the child's join returns: the first value of the FIRST
/// `return N, ..` line (the chunk's own trailing `return chooser` comes last).
fn returned_name(out: &str) -> Option<String> {
    out.lines()
        .map(str::trim_start)
        .filter_map(|l| l.strip_prefix("return "))
        .filter(|r| !r.starts_with("function"))
        .map(|r| r.split(',').next().unwrap_or("").trim().to_string())
        .next()
}

#[test]
fn table_moved_into_pinned_slot_is_assigned_on_both_arms() {
    let chunk = table_diamond_chunk(Vec::new());
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    let ret = returned_name(&out).unwrap_or_default();
    assert!(
        !ret.is_empty() && !ret.starts_with('{'),
        "the join must return the pinned binding, not an orphan or a clone:\n{out}"
    );
    let assigns = out
        .lines()
        .filter(|l| l.trim_start() == format!("{ret} = {{"))
        .count();
    assert_eq!(
        assigns, 2,
        "each arm must assign its table literal to the pinned binding `{ret}`:\n{out}"
    );
    assert!(
        out.contains("Power = Config") && out.contains("Power = Backup"),
        "both arms' field writes must survive inside their literals:\n{out}"
    );
}

/// Guard: when the THEN arm has already stored the literal somewhere
/// (`R0[R2] = R3` before the MOVE), re-emitting `bound = {..}` at the MOVE would
/// build a SECOND table. That arm must NOT be materialised a second time.
#[test]
fn consumed_table_is_not_duplicated_at_the_move() {
    let chunk = table_diamond_chunk(vec![insn_abc(OP_SETTABLE, 3, 0, 2)]);
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);

    let literals_with_config = out.matches("Power = Config").count();
    assert!(
        literals_with_config <= 1,
        "the then-arm literal was already handed out; it must not be \
         rebuilt as a second object at the MOVE:\n{out}"
    );
}
