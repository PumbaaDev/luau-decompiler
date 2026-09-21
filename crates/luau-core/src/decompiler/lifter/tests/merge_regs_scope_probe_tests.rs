//! Structural probe settling `docs/MERGE_REGS_SCOPE_RISK.md`.
//!
//! The claim under test: `c77dcd1` made `merge_regs` keep a register's
//! `Expr::Name` when the two arms of an if/else hold DIFFERENT simple names.
//! The Shadow write that produces those names emits its `Stat::Local` INSIDE
//! the branch block, so a read after the merge would reference a name whose
//! declaration is branch-local — lexically out of scope at the read site,
//! compiling clean but resolving to a global (nil) at runtime.
//!
//! The pair-fix B0.57 (`hoist_escaping_locals`) hoists such declarations out
//! of the branch — but only when `is_safe_hoist_init` accepts the init
//! expression (literals, pre-branch names, tables of safe values). A closure
//! init (`Expr::Function`) is NOT safe, so a NEWCLOSURE Shadow inside a branch
//! is the exact shape where the claim can bite.
//!
//! These tests construct that shape directly and assert on the structural
//! property of the emitted source, not on gate numbers — the gates are blind
//! here by construction (see the doc).

use crate::decompiler::{decompile_proto, DecompileContext};
use crate::parser::types::{Chunk, DebugInfo, LocalVar, Proto};

const OP_NEWCLOSURE: u8 = 19;
const OP_RETURN: u8 = 22;
const OP_JUMP: u8 = 23;
const OP_JUMPIFNOT: u8 = 26;

fn insn_ad(op: u8, a: u8, d: i16) -> u32 {
    let du = d as u16 as u32;
    (op as u32) | ((a as u32) << 8) | (du << 16)
}

fn insn_abc(op: u8, a: u8, b: u8, c: u8) -> u32 {
    (op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24)
}

/// A child closure with a NON-empty body (`return <n>`), so the C10r
/// empty-function-drop pass cannot delete its declaration and mask the
/// structural question this probe exists to answer.
fn make_child_proto(debug_name: &str, returns: i16) -> Proto {
    const OP_LOADN: u8 = 4;
    Proto {
        max_stack_size: 2,
        num_params: 0,
        num_upvalues: 0,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code: vec![
            insn_ad(OP_LOADN, 0, returns),
            insn_abc(OP_RETURN, 0, 2, 0),
        ],
        constants: Vec::new(),
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some(debug_name.to_string()),
        line_info: None,
        debug_info: None,
    }
}

/// Parent proto: R0 is a parameter (the branch condition), R1 is the register
/// that gets Shadow-written in both arms.
///
/// ```text
/// pc0: NEWCLOSURE R1, chunk_proto[1]   -- local alpha = function...  (pre-branch)
/// pc1: JUMPIFNOT R0, +2                -- else arm at pc4
/// pc2: NEWCLOSURE R1, chunk_proto[2]   -- then arm: Shadow -> local beta = ...
/// pc3: JUMP +1                         -- join at pc5
/// pc4: NEWCLOSURE R1, chunk_proto[3]   -- else arm: Shadow -> local gamma = ...
/// pc5: RETURN R1, 2                    -- READ of R1 after the merge
/// ```
///
/// Debug locals force the semantic names at each write pc, exactly the way
/// honest Roblox debug info would if the compiler emitted this register
/// allocation. `classify_write` sees old/new names both semantic and
/// differing => `WriteKind::Shadow` in both arms.
fn build_diamond_chunk() -> Chunk {
    let code = vec![
        insn_ad(OP_NEWCLOSURE, 1, 1),
        insn_ad(OP_JUMPIFNOT, 0, 2),
        insn_ad(OP_NEWCLOSURE, 1, 2),
        insn_ad(OP_JUMP, 0, 1),
        insn_ad(OP_NEWCLOSURE, 1, 3),
        insn_abc(OP_RETURN, 1, 2, 0),
    ];
    let debug_info = DebugInfo {
        locals: vec![
            LocalVar { name: "alpha".to_string(), start_pc: 0, end_pc: 2, reg: 1 },
            LocalVar { name: "beta".to_string(), start_pc: 2, end_pc: 4, reg: 1 },
            LocalVar { name: "gamma".to_string(), start_pc: 4, end_pc: 5, reg: 1 },
            LocalVar { name: "cond".to_string(), start_pc: 0, end_pc: 6, reg: 0 },
        ],
        upvalue_names: Vec::new(),
    };
    let parent = Proto {
        max_stack_size: 8,
        num_params: 1,
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
        debug_info: Some(debug_info),
    };
    let mut protos = vec![parent];
    protos.push(make_child_proto("alpha", 1));
    protos.push(make_child_proto("beta", 2));
    protos.push(make_child_proto("gamma", 3));
    Chunk {
        version: 6,
        types_version: 0,
        strings: Vec::new(),
        protos,
        main_proto: 0,
    }
}

/// True iff `local <name>` is declared at column 0 (top level of this proto's
/// emitted body) somewhere in `out`. A declaration that exists only INSIDE a
/// block (indented), or does not exist at all, both leave a top-level read of
/// `<name>` lexically unbound — the silent scope defect this probe hunts.
fn has_top_level_decl(out: &str, name: &str) -> bool {
    let decl_assign = format!("local {} =", name);
    let decl_fn = format!("local function {}(", name);
    let decl_bare = format!("local {}", name);
    out.lines().any(|line| {
        line.starts_with(&decl_assign)
            || line.starts_with(&decl_fn)
            || line.trim_end() == decl_bare
            || line.starts_with(&format!("{}, ", decl_bare))
    })
}

#[test]
fn probe_shadow_diff_names_read_after_merge_structure() {
    let chunk = build_diamond_chunk();
    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    eprintln!("=== emitted source ===\n{}\n======================", out);

    // The merge keeps ONE of the two branch names (self side = else path =>
    // "gamma" under c77dcd1). Whichever survives, the structural question is
    // the same: is the post-merge read referencing a name whose `local` sits
    // inside a branch block?
    let survivor = if out.contains("return gamma") {
        Some("gamma")
    } else if out.contains("return beta") {
        Some("beta")
    } else {
        None
    };

    match survivor {
        Some(name) => {
            // The read after the merge references `name`. CONFIRMED iff no
            // top-level declaration of `name` exists — whether the `local`
            // sits inside a branch block or was dropped entirely, the
            // top-level read is lexically unbound and resolves to a global
            // (nil) at runtime.
            assert!(
                has_top_level_decl(&out, name),
                "SCOPE DEFECT CONFIRMED: `return {n}` reads a name with no \
                 top-level `local {n}` declaration in scope (branch-local or \
                 missing). Emitted source:\n{out}",
                n = name,
                out = out
            );
        }
        None => {
            // No branch name survived to the read: the register merged to
            // Unknown (or was re-materialized). That is the visible-vN
            // failure mode, not the silent scope defect.
            eprintln!("no branch name survived to the post-merge read; scope defect not present");
        }
    }
}

/// Companion probe: same diamond, but the read consumes the register INSIDE
/// a call so the defect cannot be masked by a `return`-specific path.
#[test]
fn probe_shadow_diff_names_call_after_merge_structure() {
    // pc0: NEWCLOSURE R1, [1]      -- local alpha = fn
    // pc1: JUMPIFNOT R0, +2
    // pc2: NEWCLOSURE R1, [2]      -- Shadow: local beta = fn
    // pc3: JUMP +1
    // pc4: NEWCLOSURE R1, [3]      -- Shadow: local gamma = fn
    // pc5: MOVE R2, R1             -- read R1 after merge
    // pc6: CALL R2, 1, 1           -- call it (0 args, 0 results)
    // pc7: RETURN R0, 1
    const OP_MOVE: u8 = 6;
    const OP_CALL: u8 = 21;
    let code = vec![
        insn_ad(OP_NEWCLOSURE, 1, 1),
        insn_ad(OP_JUMPIFNOT, 0, 2),
        insn_ad(OP_NEWCLOSURE, 1, 2),
        insn_ad(OP_JUMP, 0, 1),
        insn_ad(OP_NEWCLOSURE, 1, 3),
        insn_abc(OP_MOVE, 2, 1, 0),
        insn_abc(OP_CALL, 2, 1, 1),
        insn_abc(OP_RETURN, 0, 1, 0),
    ];
    let debug_info = DebugInfo {
        locals: vec![
            LocalVar { name: "alpha".to_string(), start_pc: 0, end_pc: 2, reg: 1 },
            LocalVar { name: "beta".to_string(), start_pc: 2, end_pc: 4, reg: 1 },
            LocalVar { name: "gamma".to_string(), start_pc: 4, end_pc: 8, reg: 1 },
            LocalVar { name: "cond".to_string(), start_pc: 0, end_pc: 8, reg: 0 },
        ],
        upvalue_names: Vec::new(),
    };
    let parent = Proto {
        max_stack_size: 8,
        num_params: 1,
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
        debug_info: Some(debug_info),
    };
    let mut protos = vec![parent];
    protos.push(make_child_proto("alpha", 1));
    protos.push(make_child_proto("beta", 2));
    protos.push(make_child_proto("gamma", 3));
    let chunk = Chunk {
        version: 6,
        types_version: 0,
        strings: Vec::new(),
        protos,
        main_proto: 0,
    };

    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    eprintln!("=== emitted source (call variant) ===\n{}\n=====================================", out);

    for name in ["gamma", "beta"] {
        let used_at_top = out
            .lines()
            .any(|l| !l.starts_with(char::is_whitespace) && !l.contains("local") && l.contains(name));
        if used_at_top {
            assert!(
                has_top_level_decl(&out, name),
                "SCOPE DEFECT CONFIRMED (call variant): top-level use of `{n}` \
                 with no top-level `local {n}` declaration (branch-local or \
                 missing):\n{out}",
                n = name,
                out = out
            );
        }
    }
}

/// The half c77dcd1 got RIGHT, which the pre-branch-match rule must retain:
/// when only ONE arm rewrites the register, the fall-through side still holds
/// the PRE-BRANCH binding — a name whose `local` is emitted before the `if`
/// and therefore lexically live after the merge. Keeping it must survive the
/// fix (resetting it to Unknown would resurrect the hoisted-`local vN` class
/// the commit measurably removed).
#[test]
fn probe_single_arm_shadow_keeps_prebranch_name() {
    // pc0: NEWCLOSURE R1, [1]      -- local alpha = fn  (pre-branch, top level)
    // pc1: JUMPIFNOT R0, +1        -- join at pc3 (no else arm)
    // pc2: NEWCLOSURE R1, [2]      -- then arm: Shadow -> local beta = fn
    // pc3: RETURN R1, 2            -- read after merge
    let code = vec![
        insn_ad(OP_NEWCLOSURE, 1, 1),
        insn_ad(OP_JUMPIFNOT, 0, 1),
        insn_ad(OP_NEWCLOSURE, 1, 2),
        insn_abc(OP_RETURN, 1, 2, 0),
    ];
    let debug_info = DebugInfo {
        locals: vec![
            LocalVar { name: "alpha".to_string(), start_pc: 0, end_pc: 2, reg: 1 },
            LocalVar { name: "beta".to_string(), start_pc: 2, end_pc: 4, reg: 1 },
            LocalVar { name: "cond".to_string(), start_pc: 0, end_pc: 4, reg: 0 },
        ],
        upvalue_names: Vec::new(),
    };
    let parent = Proto {
        max_stack_size: 8,
        num_params: 1,
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
        debug_info: Some(debug_info),
    };
    let mut protos = vec![parent];
    protos.push(make_child_proto("alpha", 1));
    protos.push(make_child_proto("beta", 2));
    let chunk = Chunk {
        version: 6,
        types_version: 0,
        strings: Vec::new(),
        protos,
        main_proto: 0,
    };

    let mut ctx = DecompileContext::new(&chunk);
    let out = decompile_proto(&mut ctx, &chunk.protos[0], 0, 0);
    eprintln!("=== emitted source (single-arm) ===\n{}\n===================================", out);

    // Whatever name the post-merge read uses, its declaration must be at
    // top level. (With the pre-branch-match rule the survivor is "alpha",
    // whose `local` precedes the `if`.)
    for name in ["alpha", "beta"] {
        if out.contains(&format!("return {}", name)) {
            assert!(
                has_top_level_decl(&out, name),
                "post-merge read of `{n}` lacks a top-level declaration:\n{out}",
                n = name,
                out = out
            );
        }
    }
}
