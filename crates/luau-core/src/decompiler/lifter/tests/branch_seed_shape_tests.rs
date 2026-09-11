//! Pre-branch seed SHAPE must not decide register identity at a branch.
//!
//! `premateralize_branch_escapes_spans` threads ONE identity through a branch
//! for a register that (a) an arm writes and (c) later code reads — but its
//! shape gate only accepted registers holding a parked LITERAL. A COMPLEX-
//! parked register — one holding a parked pure expression such as
//! `self.Amount` or `self.Amount .. " Pair"` — fell through and lost the
//! binding at the join exactly the way the literal shape used to (26f939b):
//! the post-merge read minted `v2`, free_var_decls hoisted `local v2`, and the
//! checker flagged declared_never_assigned. Census exhibits, both traced to
//! bytecode: 110_Collect_Tokens `Description` (R2 = self.Amount) and
//! 148_Match_Pairs `Description` (R2 = self.Amount .. " Pair").
//!
//! The fix widens the gate from literal-only to ANY parked pure expression,
//! materialized from the pre-branch value exactly like a literal and then
//! pinned. Park purity is the `store_complex` contract, so early evaluation
//! cannot duplicate or reorder side effects.
//!
//! MEASURED-AND-REJECTED sibling: the NAME-seeded shape, where the register is
//! already a declared local (`local X = f()`) rebound inside an arm. Pinning
//! that already-declared name across the arm ranges bought +4 clean per corpus
//! but threaded the name over reused compare/scratch operands and orphaned a
//! later reader in TWO previously-clean files (476_BadgeMenu — clean bytecode —
//! and 111_Mechsquitos). It is a genuine residual, but pin-only threading is
//! the wrong cure; excluded until a pass can tell a carried identity from a
//! repurposed register.

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, Proto};

const OP_LOADN: u8 = 4;
const OP_MOVE: u8 = 6;
const OP_CALL: u8 = 21;
const OP_RETURN: u8 = 22;
const OP_JUMP: u8 = 23;
const OP_JUMPIFNOT: u8 = 26;
const OP_ADD: u8 = 33;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    let du = d as u16 as u32;
    (op as u32) | ((a as u32) << 8) | (du << 16)
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

/// Every generated `vN` identifier the output READS must be bound somewhere.
fn assert_all_reads_bound(out: &str) {
    for cap in vn_idents(out) {
        assert!(
            out.contains(&format!("{cap} =")) || out.contains(&format!("local {cap}")),
            "post-merge read of `{cap}` has no binding:\n{out}"
        );
    }
}

/// Tiny scan for generated `vN` identifiers in the output (no regex dep).
fn vn_idents(out: &str) -> Vec<String> {
    let mut found = std::collections::BTreeSet::new();
    let bytes = out.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'v'
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_')
        {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 && (j == bytes.len() || (!bytes[j].is_ascii_alphanumeric() && bytes[j] != b'_')) {
                found.insert(out[i..j].to_string());
            }
            i = j;
        } else {
            i += 1;
        }
    }
    found.into_iter().collect()
}

/// `local r = a(); if a then r = 7 else r = 8 end; a(r)` — NAME-seeded diamond:
///
/// ```text
/// pc0: MOVE R2, R0
/// pc1: CALL R2, 1, 2        -- r = a()  → emitted local, tracker-declared
/// pc2: JUMPIFNOT R0, +2     -- → pc5 (else)
/// pc3: LOADN R2, 7          -- then arm write
/// pc4: JUMP +1              -- → pc6 (merge)
/// pc5: LOADN R2, 8          -- else arm write
/// pc6: MOVE R3, R0          -- merge: consume r
/// pc7: MOVE R4, R2
/// pc8: CALL R3, 2, 1
/// pc9: RETURN R0, 1
/// ```
fn build_name_seeded_diamond() -> Chunk {
    let code = vec![
        insn_abc(OP_MOVE, 2, 0, 0),
        insn_abc(OP_CALL, 2, 1, 2),
        insn_ad(OP_JUMPIFNOT, 0, 2),
        insn_ad(OP_LOADN, 2, 7),
        insn_ad(OP_JUMP, 0, 1),
        insn_ad(OP_LOADN, 2, 8),
        insn_abc(OP_MOVE, 3, 0, 0),
        insn_abc(OP_MOVE, 4, 2, 0),
        insn_abc(OP_CALL, 3, 2, 1),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    chunk_of(code, 1, 8)
}

#[test]
fn name_seeded_register_keeps_identity_across_diamond() {
    let chunk = build_name_seeded_diamond();
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    eprintln!("=== emitted source ===\n{}\n======================", out);

    // Pre-fix both arm writes were parked and the join collapsed them: the
    // values 7 and 8 vanished and the consumer read either an unbound vN or
    // the stale call result. Both arm values must survive to the join.
    for v in ["7", "8"] {
        assert!(
            out.contains(v),
            "arm value {v} was deleted at the branch merge:\n{out}"
        );
    }
    assert_all_reads_bound(&out);
}

/// `local r = a + b; if a then r = 7 else r = 8 end; a(r)` — COMPLEX-parked
/// diamond. The pre-branch seed is a parked pure BinOp, not a literal:
///
/// ```text
/// pc0: ADD R2, R0, R1       -- r = a + b  → parked (leaf-leaf, inlinable)
/// pc1: JUMPIFNOT R0, +2     -- → pc4 (else)
/// pc2: LOADN R2, 7          -- then arm write
/// pc3: JUMP +1              -- → pc5 (merge)
/// pc4: LOADN R2, 8          -- else arm write
/// pc5: MOVE R3, R0          -- merge: consume r
/// pc6: MOVE R4, R2
/// pc7: CALL R3, 2, 1
/// pc8: RETURN R0, 1
/// ```
fn build_complex_parked_diamond() -> Chunk {
    let code = vec![
        insn_abc(OP_ADD, 2, 0, 1),
        insn_ad(OP_JUMPIFNOT, 0, 2),
        insn_ad(OP_LOADN, 2, 7),
        insn_ad(OP_JUMP, 0, 1),
        insn_ad(OP_LOADN, 2, 8),
        insn_abc(OP_MOVE, 3, 0, 0),
        insn_abc(OP_MOVE, 4, 2, 0),
        insn_abc(OP_CALL, 3, 2, 1),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    chunk_of(code, 2, 8)
}

#[test]
fn complex_parked_register_keeps_identity_across_diamond() {
    let chunk = build_complex_parked_diamond();
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    eprintln!("=== emitted source ===\n{}\n======================", out);

    for v in ["7", "8"] {
        assert!(
            out.contains(v),
            "arm value {v} was deleted at the branch merge:\n{out}"
        );
    }
    assert_all_reads_bound(&out);
}

/// `local r = a + b; if a then r = 7 end; a(r)` — the single-arm inline guard
/// over a complex-parked seed. The inline path hard-restores the snapshot, so
/// pre-fix the arm write was annihilated SILENTLY (the join re-read the parked
/// `a + b`, no unbound vN to flag — the checker-invisible half of the class):
///
/// ```text
/// pc0: ADD R2, R0, R1       -- r = a + b  → parked
/// pc1: JUMPIFNOT R0, +1     -- → pc3 (merge)
/// pc2: LOADN R2, 7          -- lone arm write
/// pc3: MOVE R3, R0          -- merge: consume r
/// pc4: MOVE R4, R2
/// pc5: CALL R3, 2, 1
/// pc6: RETURN R0, 1
/// ```
fn build_complex_parked_inline_guard() -> Chunk {
    let code = vec![
        insn_abc(OP_ADD, 2, 0, 1),
        insn_ad(OP_JUMPIFNOT, 0, 1),
        insn_ad(OP_LOADN, 2, 7),
        insn_abc(OP_MOVE, 3, 0, 0),
        insn_abc(OP_MOVE, 4, 2, 0),
        insn_abc(OP_CALL, 3, 2, 1),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    chunk_of(code, 2, 8)
}

#[test]
fn complex_parked_register_survives_inline_guard() {
    let chunk = build_complex_parked_inline_guard();
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    eprintln!("=== emitted source ===\n{}\n======================", out);

    assert!(
        out.contains("7"),
        "lone arm write was annihilated at the inline join:\n{out}"
    );
    assert_all_reads_bound(&out);
}

// ─── The shapes the ternary value-join CANNOT claim ────────────────────────
//
// The three tests above pass even pre-fix: literal-armed diamonds are
// absorbed by the selection-expression reconstruction before the parked-arm
// collapse can bite. The census exhibits are NOT that shape — their arms are
// self-referencing or impure, which is exactly where the binding died.

/// The identifier the final `arg1(...)` consumer reads.
fn final_call_arg(out: &str) -> String {
    let start = out.rfind("arg1(").expect("consumer call missing") + "arg1(".len();
    let rest = &out[start..];
    let end = rest.find(')').expect("unterminated consumer call");
    rest[..end].trim().to_string()
}

/// Is there a bare (re)assignment statement line — `X = ...`, `X += ...`,
/// `X ..= ...` — without a `local` prefix?
fn has_reassignment_line(out: &str, id: &str) -> bool {
    out.lines().map(|l| l.trim()).any(|l| {
        let Some(rest) = l.strip_prefix(id) else { return false };
        let rest = rest.trim_start();
        (rest.starts_with('=') && !rest.starts_with("=="))
            || ["+=", "-=", "*=", "/=", "..=", "%=", "^="]
                .iter()
                .any(|op| rest.starts_with(op))
    })
}

/// 148_Match_Pairs `Description`, minimized: a parked pure BinOp seed with a
/// SELF-REFERENCING arm write under a lone guard.
///
/// ```text
/// pc0: ADD R2, R0, R1       -- r = a + b   → parked (leaf-leaf)
/// pc1: JUMPIFNOT R0, +1     -- if a then   → pc3 (merge)
/// pc2: ADD R2, R2, R1       --   r = r + b (self-referencing, non-leaf)
/// pc3: MOVE R3, R0          -- merge: consume r
/// pc4: MOVE R4, R2
/// pc5: CALL R3, 2, 1
/// pc6: RETURN R0, 1
/// ```
///
/// Pre-fix the arm write materialized as a FRESH local inside the arm and the
/// join re-read the stale parked `a + b`: the consumer inlined the pre-branch
/// value and the arm effect vanished (148's `v2` was the region-path twin).
#[test]
fn complex_parked_selfref_arm_reaches_the_join() {
    let code = vec![
        insn_abc(OP_ADD, 2, 0, 1),
        insn_ad(OP_JUMPIFNOT, 0, 1),
        insn_abc(OP_ADD, 2, 2, 1),
        insn_abc(OP_MOVE, 3, 0, 0),
        insn_abc(OP_MOVE, 4, 2, 0),
        insn_abc(OP_CALL, 3, 2, 1),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let chunk = chunk_of(code, 2, 8);
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    eprintln!("=== emitted source ===\n{}\n======================", out);

    let id = final_call_arg(&out);
    assert!(
        !id.is_empty() && id.chars().all(|c| c.is_alphanumeric() || c == '_'),
        "consumer must read ONE bound identity, not an inlined stale \
         expression — got `{id}`:\n{out}"
    );
    assert!(
        has_reassignment_line(&out, &id),
        "the arm write never reaches `{id}` — no reassignment emitted:\n{out}"
    );
    assert_all_reads_bound(&out);
}

