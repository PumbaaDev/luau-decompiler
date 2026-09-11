//! Round-4: a register written by BOTH arms of an if/else diamond and read at
//! the merge, with NO pre-branch binding, must keep ONE identity across the
//! join.
//!
//! The mechanism (traced to raw operand fields under the MEASURED v9 map):
//!
//!   * 16_StickerPlacer proto 15 "GetPlaceableCanvases": pc4 `JUMPIFNOT R0`,
//!     pc5 `MOVE R2 = R0` (fallthrough arm), pc7-10 `R2 = tbl.GetLocal
//!     PlayerCanvasSet()` (jump arm), pc11 `JUMPIFNOT R2` — the merge READS
//!     R2. Pre-branch R2 is Unknown (never written), so
//!     `premateralize_branch_escapes_spans`' parked-pure gate never fired,
//!     the fallthrough MOVE parked and was annihilated by `merge_regs`, the
//!     jump arm's CALL shadow-demoted into a fresh arm-local, and the merge
//!     read minted an unbound `v2` that `free_var_decls` hoisted —
//!     `declared_never_assigned`.
//!
//!   * 162_Badges proto 10 "CheckIfSetIsReadyToCollect": same shape with
//!     inverted polarity — pc24 `JUMPIF R12`, pc25 `LOADNIL R11` (fallthrough
//!     arm), pc49 `MOVE R11 = R14` (jump arm, the GetStat call result), pc50
//!     `MOVE R6 = R11` at the merge. Output read `v11`, permanently nil.
//!
//! The fix seeds a bare `local <name>` before the branch for exactly this
//! shape — both arms provably write the register, the merge provably reads it
//! first — and pins the register across both arm spans so every arm write is
//! emitted against that one identity. Soundness: because BOTH arms write, the
//! merge read can never observe the (unknown) pre-branch value, so the bare
//! declaration is bit-exact against the bytecode regardless of what the
//! register held before.

use crate::decompiler::semantic_check::{check, Severity};
use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Constant, Proto};

// Canonical Luau opcode bytes (identity opmap, Chunk.version = 6).
const OP_LOADN: u8 = 4;
const OP_LOADNIL: u8 = 2;
const OP_MOVE: u8 = 6;
const OP_GETGLOBAL: u8 = 7;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;
const OP_JUMP: u8 = 23;
const OP_JUMPIF: u8 = 25;
const OP_JUMPIFNOT: u8 = 26;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    let du = d as u16 as u32;
    (op as u32) | ((a as u32) << 8) | (du << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

fn one_param_chunk(code: Vec<u32>) -> Chunk {
    let proto = Proto {
        max_stack_size: 8,
        num_params: 1,
        num_upvalues: 0,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants: vec![
            Constant::String("pad".to_string()),
            Constant::String("fetchCanvas".to_string()),
        ],
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("pick".to_string()),
        line_info: None,
        debug_info: None,
    };
    Chunk {
        version: 6,
        types_version: 0,
        strings: vec!["pad".to_string(), "fetchCanvas".to_string()],
        protos: vec![proto],
        main_proto: 0,
    }
}

fn decompiled(chunk: &Chunk) -> String {
    let mut ctx = DecompileContext::new(chunk);
    decompile_proto(&mut ctx, &chunk.protos[0], 0, 0)
}

fn wrong_findings(out: &str) -> Vec<String> {
    check(out, None)
        .into_iter()
        .filter(|f| f.severity == Severity::Wrong)
        .map(|f| format!("{}: {}", f.check, f.detail))
        .collect()
}

/// The merged register must come back as ONE name, declared before the
/// branch, assigned in BOTH arms, and read (not re-minted) at the merge.
fn assert_identity_survives_join(out: &str) {
    let findings = wrong_findings(out);
    assert!(
        findings.is_empty(),
        "the both-arm write must reach the merge read — no defect may remain:\n{:?}\n{out}",
        findings
    );
    assert!(
        out.contains("else"),
        "both arms must survive as an if/else:\n{out}"
    );
    // The returned name must be written at least twice (once per arm).
    let ret_name = out
        .lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix("return "))
        .expect("output must end in a return")
        .trim()
        .to_string();
    let assigns = out
        .matches(&format!("{ret_name} = "))
        .count();
    assert!(
        assigns >= 2,
        "`{ret_name}` must be assigned in both arms (found {assigns}):\n{out}"
    );
}

/// StickerPlacer shape: JUMPIFNOT entry, fallthrough arm `MOVE R2 = R0`,
/// jump arm `R2 = fetchCanvas()`, merge `RETURN R2`.
///
/// ```text
///   0: JUMPIFNOT R0 +2      -> 3
///   1: MOVE      R2, R0
///   2: JUMP      +3          -> 6
///   3: GETGLOBAL R2, K1 (+AUX)
///   5: CALL      R2, 0 args, 1 result
///   6: RETURN    R2, 1
/// ```
#[test]
fn both_arm_write_survives_merge_read() {
    let out = decompiled(&one_param_chunk(vec![
        insn_ad(OP_JUMPIFNOT, 0, 2),
        insn_abc(OP_MOVE, 2, 0, 0),
        insn_ad(OP_JUMP, 0, 3),
        insn_ad(OP_GETGLOBAL, 2, 1),
        1u32, // AUX: constant index of "fetchCanvas"
        insn_abc(OP_CALL, 2, 1, 2),
        insn_abc(OP_RETURN, 2, 2, 0),
    ]));
    assert_identity_survives_join(&out);
}

/// Badges polarity: JUMPIF entry, fallthrough arm `LOADNIL R2`, jump arm
/// `R2 = fetchCanvas()`, merge `RETURN R2`. The nil arm must survive as an
/// explicit `= nil` assignment (or an else keeping the name bound) — not be
/// deleted.
#[test]
fn loadnil_arm_survives_merge_read() {
    let out = decompiled(&one_param_chunk(vec![
        insn_ad(OP_JUMPIF, 0, 2),
        insn_abc(OP_LOADNIL, 2, 0, 0),
        insn_ad(OP_JUMP, 0, 3),
        insn_ad(OP_GETGLOBAL, 2, 1),
        1u32,
        insn_abc(OP_CALL, 2, 1, 2),
        insn_abc(OP_RETURN, 2, 2, 0),
    ]));
    let findings = wrong_findings(&out);
    assert!(
        findings.is_empty(),
        "the diamond's value must reach the return:\n{:?}\n{out}",
        findings
    );
    assert!(
        !out.contains("return v2"),
        "no minted `v2` may replace the diamond's value:\n{out}"
    );
}

// ── Guards: the seed must NOT fire where it cannot be proven ─────────────

/// Single-arm write (no else): on the fallthrough path the merge read
/// observes the PRE-branch register, which is unknown to us — seeding a nil
/// declaration would silently assert a value we cannot prove. The honest
/// output keeps the defect visible (minted name, flagged by the checker).
#[test]
fn single_arm_write_stays_unseeded() {
    let out = decompiled(&one_param_chunk(vec![
        insn_ad(OP_JUMPIFNOT, 0, 1),
        insn_abc(OP_MOVE, 2, 0, 0),
        insn_abc(OP_RETURN, 2, 2, 0),
    ]));
    let bare_locals = out
        .lines()
        .filter(|l| {
            let t = l.trim();
            t.starts_with("local ") && !t.contains('=')
        })
        .count();
    assert_eq!(
        bare_locals, 0,
        "a single-arm write must not grow a seeded declaration:\n{out}"
    );
}

/// Dead on entry to the merge: the merge redefines the register before any
/// read, so the arm writes never escape — no seed, no pin, no declaration.
#[test]
fn merge_redefinition_stays_unseeded() {
    let out = decompiled(&one_param_chunk(vec![
        insn_ad(OP_JUMPIFNOT, 0, 2),
        insn_abc(OP_MOVE, 2, 0, 0),
        insn_ad(OP_JUMP, 0, 3),
        insn_ad(OP_GETGLOBAL, 2, 1),
        1u32,
        insn_abc(OP_CALL, 2, 1, 2),
        insn_ad(OP_LOADN, 2, 9),
        insn_abc(OP_RETURN, 2, 2, 0),
    ]));
    let bare_locals = out
        .lines()
        .filter(|l| {
            let t = l.trim();
            t.starts_with("local ") && !t.contains('=')
        })
        .count();
    assert_eq!(
        bare_locals, 0,
        "a merge-redefined register must not grow a seeded declaration:\n{out}"
    );
    assert!(
        out.contains("return 9"),
        "the merge redefinition must win the return:\n{out}"
    );
}
