//! Per-opcode lifting — the inner match arms that turn each bytecode
//! instruction into an AST node update.
//!
//! Extracted from `lifter.rs` as part of Phase B0.52P6 (part 3/3).
//! The entry point is `lift_instruction_range`, a ~1700-line function
//! whose body is one giant `match op { ... }` over every Luau opcode.
//!
//! Roughly half of the helpers invoked from this file live here (private
//! top-level `fn` with no explicit visibility), and the rest (register
//! setup, store/emit primitives, aux-string resolution, import decoding)
//! are imported from `super::*` — i.e., the `lifter/mod.rs` that owns
//! `DecompileContext`-adjacent state.
//!
//! Because the match body directly manipulates `regs: &mut Vec<RegVal>`
//! and a `LocalTracker`, both of those types are re-exported with
//! `pub(super)` visibility from `naming.rs`; we pull them in via
//! `super::{RegVal, LocalTracker, WriteKind}`.

use crate::analysis::bool_idiom::{recognize_bool_idiom, recognize_or_and_chain};
use crate::ast::{BinOp, Expr, Stat, TableField, UnOp};
use crate::decompiler::{constant_to_expr, is_stdlib_shadow_name, is_valid_luau_identifier, DecompileContext};
use crate::parser::opcodes::{builtin_name, LuauOpcode};
use crate::parser::types::{
    decode_import, insn_a, insn_b, insn_c, insn_d, insn_e, insn_op,
    Constant, Proto,
};

use super::{
    // Register-write primitives.
    emit_assign,
    emit_local_or_assign,
    ensure_lvalue_base_materialized,
    ensure_table_reg_declared,
    // Per-proto naming types (declared in `naming.rs`).
    LocalTracker,
    RegVal,
    WriteKind,
    // Constant / global / aux-string resolvers.
    get_const_expr,
    get_method_string_from_aux,
    get_table_string_from_aux,
    resolve_global_name,
    // Small expression builders.
    is_roblox_method_lvalue_artifact,
    is_self_referential_field_assign,
    method_receiver_expr,
    mk_binop,
    stmt_reads_name,
    mk_binop_k,
    mk_concat,
    mk_unop,
    reg_expr,
    sanitize_leaked_global_string,
    simplify_expr,
    premateralize_branch_escapes,
    store_complex,
    table_expr,
    // Recursive closure lifting — called on every `NewClosure` opcode.
    lift_proto_inner,
    // Phase C1: proto-wide statement budget introspection.
    note_stmts_pushed,
    stmt_budget_tripped,
};

/// Phase C4: emit a `Stat::Comment` when a self-arithmetic instruction
/// (A==B==C) operates on a register whose value is provably non-numeric —
/// specifically a `Function` literal or `Nil`.  These patterns are always
/// the result of a misidentified opmap (real Luau code never compiles
/// `x = x op x` where x is a function or nil), so surfacing a Comment in
/// the emitted source preserves full_moon parse-validity while making the
/// corruption visible.
fn maybe_emit_self_arith_guard(
    regs: &[RegVal],
    stmts: &mut Vec<Stat>,
    a: usize,
    insn: u32,
    op_name: &str,
) {
    let bad_kind: Option<&'static str> = match regs.get(a) {
        Some(RegVal::Expr(Expr::Function { .. })) => Some("Function"),
        Some(RegVal::Expr(Expr::Nil)) => Some("Nil"),
        _ => None,
    };
    if let Some(k) = bad_kind {
        let b0 = (insn & 0xFF) as u8;
        let b1 = ((insn >> 8) & 0xFF) as u8;
        let b2 = ((insn >> 16) & 0xFF) as u8;
        let b3 = ((insn >> 24) & 0xFF) as u8;
        stmts.push(Stat::Comment(format!(
            "-- lifter error: self-arith {} on non-numeric {}  raw={:02x}{:02x}{:02x}{:02x}",
            op_name, k, b0, b1, b2, b3
        )));
    }
}

/// B0.54B helper — is this a generic-family register name that shouldn't
/// be used to promote a CALL arg from another generic (lateral swap)?
/// Matches `v12`, `fn3`, `call7`, `arg4`, `upval_2`. Rejects real names
/// that happen to share a prefix (`value`, `vec3`, `argon`, `calling`).
fn is_generic_placeholder(name: &str) -> bool {
    let rest = name.strip_prefix("upval_")
        .or_else(|| name.strip_prefix("call"))
        .or_else(|| name.strip_prefix("arg"))
        .or_else(|| name.strip_prefix("fn"))
        .or_else(|| name.strip_prefix('v'));
    match rest {
        Some(r) if !r.is_empty() && r.chars().all(|c| c.is_ascii_digit()) => true,
        _ => false,
    }
}

/// Detect the `if/else` diamond behind an inline forward branch.
///
/// Given a then-body spanning `[then_start, target)`, check whether its final
/// instruction is an unconditional forward JUMP that lands past `target` but
/// still inside `end`. That jump is the compiler's else-skip, so the real then
/// body stops before it and `[target, jump_target)` is the else body.
///
/// Returns `(then_end, else_range)`; `else_range` is `None` when the shape does
/// not match, in which case `then_end == target` and the caller behaves exactly
/// as it did before.
fn detect_else_skip(
    code: &[u32],
    then_start: usize,
    target: usize,
    end: usize,
) -> (usize, Option<(usize, usize)>) {
    if target <= then_start || target > code.len() {
        return (target, None);
    }
    let last_pc = target - 1;
    if last_pc < then_start {
        return (target, None);
    }
    let last = code[last_pc];
    let lop = LuauOpcode::from_u8(insn_op(last));
    let jump_target = match lop {
        LuauOpcode::Jump => (last_pc as i32 + insn_d(last) as i32 + 1) as usize,
        LuauOpcode::JumpX => (last_pc as i32 + insn_e(last) + 1) as usize,
        _ => return (target, None),
    };
    if jump_target > target && jump_target <= end {
        (last_pc, Some((target, jump_target)))
    } else {
        (target, None)
    }
}

/// Is `op` a control-flow instruction (any branch, jump, loop header or return)?
fn is_control_flow_op(op: LuauOpcode) -> bool {
    matches!(
        op,
        LuauOpcode::Jump
            | LuauOpcode::JumpBack
            | LuauOpcode::JumpX
            | LuauOpcode::JumpIf
            | LuauOpcode::JumpIfNot
            | LuauOpcode::JumpIfEq
            | LuauOpcode::JumpIfNotEq
            | LuauOpcode::JumpIfLE
            | LuauOpcode::JumpIfNotLE
            | LuauOpcode::JumpIfLT
            | LuauOpcode::JumpIfNotLT
            | LuauOpcode::JumpXEqKNil
            | LuauOpcode::JumpXEqKB
            | LuauOpcode::JumpXEqKN
            | LuauOpcode::JumpXEqKS
            | LuauOpcode::ForNPrep
            | LuauOpcode::ForNLoop
            | LuauOpcode::ForGPrep
            | LuauOpcode::ForGPrepINext
            | LuauOpcode::ForGPrepNext
            | LuauOpcode::ForGLoop
            | LuauOpcode::Return
    )
}

/// Is register `a` a live local that an earlier closure captured BY REFERENCE?
///
/// Luau's register allocator may not reuse a REF-captured slot for a *different*
/// local before a CLOSEUPVALS on it, so every write to such a register between
/// the CAPTURE and that CLOSEUPVALS is by construction an assignment to the SAME
/// variable — never the declaration of a new one. Without this the classic
/// forward-declaration shape
///
/// ```luau
/// local isOdd
/// local function isEven(n) ... isOdd(n - 1) ... end   -- CAPTURE REF R0
/// function isOdd(n) ... end                           -- DUPCLOSURE R0
/// ```
///
/// re-derives R0's name through the uniquing allocator, gets `isOdd3`, and
/// declares a second binding — leaving `isEven`'s upvalue pointing at the
/// still-nil original.
///
/// Only a properly remapped `CAPTURE` counts. The structural fallback used
/// elsewhere for shuffled Roblox bytecode merely *guesses*, and a misfire must
/// not be escalated into silently dropping a binding.
fn reg_is_open_ref_capture(code: &[u32], upto_pc: usize, a: usize) -> bool {
    let mut open = false;
    let mut i = 0usize;
    let end = upto_pc.min(code.len());
    while i < end {
        let insn = code[i];
        let op = LuauOpcode::from_u8(insn_op(insn));
        match op {
            // CAPTURE A B: A is the capture kind (1 == LCL_REF), B the register.
            LuauOpcode::Capture => {
                if insn_a(insn) == 1 && insn_b(insn) as usize == a {
                    open = true;
                }
            }
            // CLOSEUPVALS A closes every upvalue at or above register A, which
            // is exactly where Luau is free to rebind the slot.
            LuauOpcode::CloseUpvals => {
                if (insn_a(insn) as usize) <= a {
                    open = false;
                }
            }
            _ => {}
        }
        i += if op.has_aux() { 2 } else { 1 };
    }
    open
}

/// Does this opcode define (write) register R(A)?
///
/// Used by `table_needs_binding` to stop scanning once the constructor's
/// register has been reused for an unrelated value. Written as an EXCLUSION
/// list so an unrecognised or Roblox-extension opcode counts as a definition:
/// stopping the scan early under-counts reads, which is the safe direction —
/// it keeps the historical park-the-literal behaviour.
fn insn_defines_reg_a(op: LuauOpcode) -> bool {
    !matches!(
        op,
        LuauOpcode::Nop
            | LuauOpcode::Break
            | LuauOpcode::Coverage
            // A is a value being READ out of, not written into.
            | LuauOpcode::SetGlobal
            | LuauOpcode::SetUpval
            | LuauOpcode::SetTable
            | LuauOpcode::SetTableKS
            | LuauOpcode::SetTableN
            // A is the table / range base, not a fresh definition.
            | LuauOpcode::SetList
            | LuauOpcode::CloseUpvals
            | LuauOpcode::Return
            // A is a tested register.
            | LuauOpcode::Jump
            | LuauOpcode::JumpBack
            | LuauOpcode::JumpX
            | LuauOpcode::JumpIf
            | LuauOpcode::JumpIfNot
            | LuauOpcode::JumpIfEq
            | LuauOpcode::JumpIfNotEq
            | LuauOpcode::JumpIfLE
            | LuauOpcode::JumpIfNotLE
            | LuauOpcode::JumpIfLT
            | LuauOpcode::JumpIfNotLT
            | LuauOpcode::JumpXEqKNil
            | LuauOpcode::JumpXEqKB
            | LuauOpcode::JumpXEqKN
            | LuauOpcode::JumpXEqKS
            // A is a capture kind / builtin id / param count.
            | LuauOpcode::Capture
            | LuauOpcode::FastCall
            | LuauOpcode::FastCall1
            | LuauOpcode::FastCall2
            | LuauOpcode::FastCall2K
            | LuauOpcode::FastCall3
            | LuauOpcode::PrepVarargs
    )
}

/// How far ahead `table_needs_binding` will look for a cross-branch use.
/// Bounded so a proto with many table constructors stays linear-ish; protos
/// longer than this keep the historical park-the-literal behaviour.
const TABLE_USE_SCAN_LIMIT: usize = 1024;

/// Should the table created at `pc` in register `a` be materialised as a real
/// `local NAME = {}` binding instead of being parked as a pending literal?
///
/// A parked table is handed out as a fresh CLONE at every read (`reg_expr`),
/// which is correct only while the whole constructor is one straight-line run.
/// The moment the table is filled or read on the far side of a branch — the
/// `local t = {} ; for i = 1, n do t[i] = ... end ; print(#t)` shape — cloning
/// silently forks it: the loop body mutates a throwaway copy and the read after
/// the loop sees an undeclared name. Binding it first keeps one identity.
///
/// Cloning also forks the table when it is simply READ twice in straight-line
/// code — `local names = {...} ; table.sort(names) ; table.concat(names, ",")`
/// compiles to `MOVE Rarg, Rtable` twice with no branch anywhere between the
/// constructor and either read. Each read would materialise an independent
/// literal, so `table.sort` sorts a throwaway and `table.concat` reads a
/// different one. Two or more reads therefore force a binding regardless of
/// branches.
///
/// Returns false for straight-line constructors read at most once, so
/// one-shot literals like `print(#{1, 2, 3})` and `table.concat({}, ",")`
/// still inline exactly as before.
fn table_needs_binding(code: &[u32], pc: usize, a: usize) -> bool {
    if pc >= code.len() {
        return false;
    }
    let start_op = LuauOpcode::from_u8(insn_op(code[pc]));
    let mut i = pc + if start_op.has_aux() { 2 } else { 1 };
    let scan_end = code.len().min(pc + TABLE_USE_SCAN_LIMIT);
    let mut crossed_branch = false;
    let mut reads = 0usize;
    while i < scan_end {
        let insn = code[i];
        let op = LuauOpcode::from_u8(insn_op(insn));
        let ia = insn_a(insn) as usize;
        let ib = insn_b(insn) as usize;
        if is_control_flow_op(op) {
            crossed_branch = true;
        } else {
            // A multret SETLIST (`{ 1, 2, f() }`) splices ALL of the call's
            // results into the array part. There is no statement form that
            // reproduces that against a bound name — `t[21] = f()` truncates to
            // one value — so such a table must stay a pending literal and be
            // emitted as a constructor, whatever its read count.
            if matches!(op, LuauOpcode::SetList) && ia == a && insn_c(insn) == 0 {
                return false;
            }
            // A read hands the table's value to another register or consumer.
            // The constructor's own fill run (SETLIST with A == a, SETTABLE*
            // with B == a) is deliberately NOT counted here — it builds the
            // literal rather than aliasing it.
            let reads_table = matches!(
                op,
                LuauOpcode::GetTable
                    | LuauOpcode::GetTableKS
                    | LuauOpcode::GetTableN
                    | LuauOpcode::NameCall
                    | LuauOpcode::Length
                    | LuauOpcode::Move
            ) && ib == a;
            if reads_table {
                reads += 1;
                if reads >= 2 {
                    return true;
                }
            }
            if crossed_branch {
                let touches = match op {
                    // R(A) is the table being filled.
                    LuauOpcode::SetList => ia == a,
                    // R(B) is the table being indexed, read or copied out.
                    LuauOpcode::SetTable
                    | LuauOpcode::SetTableKS
                    | LuauOpcode::SetTableN
                    | LuauOpcode::GetTable
                    | LuauOpcode::GetTableKS
                    | LuauOpcode::GetTableN
                    | LuauOpcode::NameCall
                    | LuauOpcode::Length
                    | LuauOpcode::Move => ib == a,
                    _ => false,
                };
                if touches {
                    return true;
                }
            }
            // The register now holds an unrelated value; every later mention of
            // it refers to a different object, so stop counting.
            if ia == a && insn_defines_reg_a(op) {
                return false;
            }
        }
        i += if op.has_aux() { 2 } else { 1 };
    }
    false
}

/// Recover SETLIST values when the destination register no longer holds a
/// pending `Expr::Table`.
///
/// The NEWTABLE arm materializes `local M = {}` eagerly for a table at the
/// start of a proto (the Roblox ModuleScript shape), which overwrites `regs[a]`
/// with an `Expr::Name`. A later SETLIST then found no pending table and — with
/// no `else` on its match — silently discarded every value: no field added, no
/// assignment emitted, no diagnostic. In canonical Luau a main proto always
/// starts with PREPVARARGS, so a top-level table literal on the first source
/// line always tripped this and `{10, 20, 30}` decompiled to `{}`.
///
/// Preferred recovery is to back-patch the `local NAME = {}` that the NEWTABLE
/// arm just emitted, which restores the clean `local t = {10, 20, 30}`.
/// Otherwise fall back to explicit `t[i] = v` assignments, which are correct
/// for any shape (including a table that reached this register some other way).
fn store_setlist_values(
    regs: &mut [RegVal],
    stmts: &mut Vec<Stat>,
    a: usize,
    aux: Option<u32>,
    values: Vec<Expr>,
) {
    if values.is_empty() {
        return;
    }
    // AUX is the 1-based index of the first value.
    let start_index = aux.unwrap_or(1).max(1) as usize;

    let target = match regs.get(a) {
        Some(RegVal::Expr(Expr::Name(n))) => n.clone(),
        _ => return,
    };

    // Back-patch the immediately preceding `local <target> = { .. }`.
    if start_index == 1 {
        if let Some(Stat::Local { names, values: lvals }) = stmts.last_mut() {
            if names.len() == 1 && names[0] == target && lvals.len() == 1 {
                if let Expr::Table { fields } = &mut lvals[0] {
                    for val in values {
                        fields.push(TableField::Sequential(val));
                    }
                    return;
                }
            }
        }
    }

    for (i, val) in values.into_iter().enumerate() {
        stmts.push(Stat::Assign {
            targets: vec![Expr::Index {
                object: Box::new(Expr::Name(target.clone())),
                key: Box::new(Expr::Number((start_index + i) as f64)),
            }],
            values: vec![val],
        });
    }
}

/// Phase C6: extract a semantic name from a `require(arg)` argument expression.
/// Returns the rightmost identifier of the path when it's a valid Luau identifier.
///
/// Handled shapes:
///   `require(script.Foo)`              → "Foo"
///   `require(game.X.Y.Module)`         → "Module"
///   `require(script.Parent:WaitForChild("Foo"))` → "Foo"
///   `require(x:GetService("Players"))` → "Players"
///   `require(importedPath)`            → "importedPath" when identifier
///
/// Returns None when the arg is dynamic (Index, Call with non-string arg, etc.).
fn require_arg_to_name(arg: &Expr) -> Option<String> {
    match arg {
        Expr::Field { field, .. } => {
            if is_valid_luau_identifier(field) && !is_generic_placeholder(field) {
                Some(field.clone())
            } else {
                None
            }
        }
        Expr::MethodCall { method, args, .. } => {
            if matches!(method.as_str(), "WaitForChild" | "FindFirstChild") {
                if let Some(Expr::String(s)) = args.first() {
                    if is_valid_luau_identifier(s) && !is_generic_placeholder(s) {
                        return Some(s.clone());
                    }
                }
            }
            if matches!(method.as_str(), "GetService") {
                if let Some(Expr::String(s)) = args.first() {
                    if is_valid_luau_identifier(s) {
                        return Some(s.clone());
                    }
                }
            }
            None
        }
        Expr::Name(n) => {
            if is_valid_luau_identifier(n) && !is_generic_placeholder(n)
                && !matches!(n.as_str(), "script" | "game" | "workspace" | "require")
            {
                Some(n.clone())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Lift a range of instructions [start, end) into statements
pub(super) fn lift_instruction_range(
    ctx: &mut DecompileContext,
    proto: &Proto,
    proto_index: usize,
    depth: usize,
    start: usize,
    end: usize,
    regs: &mut Vec<RegVal>,
    locals: &mut LocalTracker,
    stmts: &mut Vec<Stat>,
    in_loop: bool,
) {
    // Prevent exponential nesting from recursive inline-if lifting
    if depth > 50 {
        stmts.push(Stat::Comment(format!("-- max nesting depth reached at [{}-{})", start, end)));
        return;
    }

    let code = &proto.code;
    let mut pc = start;
    // Track pending FastCall builtin ID so the next CALL can use it
    let mut pending_fastcall: Option<(u8, usize)> = None; // (builtin_id, target_reg)
    // A CALL with nresults==0 produces a VARIABLE number of results, but it is
    // materialized as a plain `local` (so the expression is not duplicated at
    // every use site), which loses its multret-ness. Remember where that
    // statement went so an immediately following RETURN can fold it back in.
    // (result_reg, index into `stmts`)
    //
    // Deliberately narrow: the slot is cleared at the start of EVERY
    // instruction, so it only ever survives into the instruction directly
    // after the CALL. That is the shape the compiler emits for
    // `return a, f(...)`, and it makes it impossible to attach an unrelated
    // call to a RETURN across a branch or label — a silent semantic
    // corruption that would be very hard to spot.
    let mut pending_multret: Option<(usize, usize)> = None;
    // Registers this range explicitly wrote with LOADNIL. `RegVal::Expr(Expr::Nil)`
    // alone cannot distinguish a real `f(x, nil)` argument from a stale nil left
    // over in an adjacent register, so the trailing-nil leak guard below used to
    // discard genuine nil arguments (`tostring(nil)` became `tostring()`).
    let mut explicit_nil_regs: std::collections::HashSet<usize> = std::collections::HashSet::new();
    // Phase C1: per-instruction budget accounting. Records how many Stats
    // this dispatch loop appended since the previous tick, and stops early
    // once the proto-wide cap has been hit.
    let mut last_len = stmts.len();
    while pc < end && pc < code.len() {
        // Charge the budget for whatever got appended during the previous
        // instruction. `note_stmts_pushed` stamps the sentinel comment and
        // trims the block the moment we cross [`MAX_STMTS_PER_PROTO`].
        let delta = stmts.len().saturating_sub(last_len);
        if delta > 0 {
            note_stmts_pushed(stmts, delta);
        }
        if stmt_budget_tripped() {
            return;
        }
        last_len = stmts.len();
        // Consume (and thereby clear) any multret CALL recorded by the
        // PREVIOUS instruction. Only the Return arm looks at this.
        let multret_from_prev = pending_multret.take();
        let insn = code[pc];
        let op = LuauOpcode::from_u8(insn_op(insn));
        let a = insn_a(insn) as usize;
        let b = insn_b(insn) as usize;
        let c = insn_c(insn) as usize;
        let d = insn_d(insn);
        let e = insn_e(insn);
        let aux = if op.has_aux() && pc + 1 < code.len() { Some(code[pc + 1]) } else { None };

        // Ensure register vector is large enough for any operand we might access
        let max_reg = a.max(b).max(c) + 1;
        if max_reg > regs.len() {
            regs.resize(max_reg + 8, RegVal::Unknown);
        }

        match op {
            LuauOpcode::Nop | LuauOpcode::Break | LuauOpcode::Coverage
            | LuauOpcode::NativeCall | LuauOpcode::CloseUpvals
            | LuauOpcode::PrepVarargs | LuauOpcode::Capture => {}

            LuauOpcode::LoadNil => {
                // Just store nil in register for inlining — don't emit `local vN = nil`.
                // Downstream code that uses this register will inline the nil.
                // This matches LOADK's store_complex behavior (nil is "simple").
                regs[a] = RegVal::Expr(Expr::Nil);
                explicit_nil_regs.insert(a);
            }
            LuauOpcode::LoadB => {
                if c != 0 {
                    // Boolean chain: branch skips over the next LOADB so both
                    // paths assign the same register.  Must emit a statement
                    // (local / assign) so the two assignments are visible.
                    emit_local_or_assign(ctx, proto, regs, locals, stmts, a, pc, Expr::Bool(b != 0));
                    pc += c;
                } else {
                    // Standalone boolean load -- keep in register for inlining
                    // (e.g. `foo(true)` instead of `local v5 = true; foo(v5)`).
                    store_complex(ctx, proto, regs, locals, stmts, a, pc, Expr::Bool(b != 0));
                }
            }
            LuauOpcode::LoadN => {
                // Integer literal -- keep in register for inlining
                // (e.g. `foo(100)` instead of `local v5 = 100; foo(v5)`).
                store_complex(ctx, proto, regs, locals, stmts, a, pc, Expr::Number(d as f64));
            }
            LuauOpcode::LoadK => {
                let d_unsigned = d as u16 as usize;
                let expr = if let Some(k) = proto.constants.get(d_unsigned) {
                    constant_to_expr(k, &ctx.chunk.strings, &proto.constants)
                } else {
                    // Constant index out of range — try chunk.strings as fallback
                    // (handles cases where D is actually a string table reference)
                    get_const_expr(proto, &ctx.chunk.strings, d_unsigned as u32)
                };
                // Use store_complex so simple constants (strings, numbers, bools)
                // stay in the register for inlining at the use site, rather than
                // emitting an unnecessary `local vN = "..."` statement. This prevents
                // the "string-as-object" pattern where LOADK's string gets materialized
                // as a named variable and then used as a table object by GETTABLEKS/
                // SETTABLEKS/NAMECALL.
                store_complex(ctx, proto, regs, locals, stmts, a, pc, expr);
            }
            LuauOpcode::LoadKX => {
                if let Some(kidx) = aux {
                    // Special-case Closure constants: lift the child proto body
                    // so we get `function(...) ... end` instead of `<closure>`.
                    let expr = if let Some(Constant::Closure(child_const_idx)) =
                        proto.constants.get(kidx as usize)
                    {
                        // LBC_CONSTANT_CLOSURE stores a DIRECT chunk.protos index (not a
                        // child_protos local index like DupClosure's D operand).
                        let global_idx = *child_const_idx as usize;
                        if let Some(child) = ctx.chunk.protos.get(global_idx) {
                            let is_recursive = ctx.proto_stack.contains(&global_idx);
                            if is_recursive {
                                Expr::Function {
                                    params: vec![],
                                    is_vararg: child.is_vararg,
                                    body: vec![Stat::Comment("-- recursive closure".to_string())],
                                }
                            } else {
                                // B0.64: child-proto param names must come from
                                // the child's hint cache, not the parent's.
                                // But analyze_register_usage (which populates
                                // that cache) runs INSIDE lift_proto_inner, so
                                // we have to lift body first, THEN compute
                                // params while current_proto_index is the
                                // child's. lift_proto_inner restores
                                // current_proto_index before returning, so we
                                // swap again for param extraction.
                                // B0.135c: lift_proto_inner owns the proto_stack
                                // push/pop (B0.134b). No need to push here — doing
                                // so would duplicate the stack entry (cosmetic only,
                                // contains() unaffected, but misleading in diagnostics).
                                let body = lift_proto_inner(ctx, child, global_idx, depth + 1);
                                let saved = ctx.current_proto_index;
                                ctx.current_proto_index = Some(global_idx);
                                let params: Vec<String> = (0..child.num_params)
                                    .map(|i| ctx.reg_name(child, i, 0))
                                    .collect();
                                ctx.current_proto_index = saved;
                                Expr::Function { params, is_vararg: child.is_vararg, body }
                            }
                        } else if let Some(k) = proto.constants.get(kidx as usize) {
                            // Closure index out-of-range in chunk.protos — fall
                            // back to the constant as-is (bounds-checked).
                            constant_to_expr(k, &ctx.chunk.strings, &proto.constants)
                        } else {
                            // Malformed bytecode — emit nil sentinel.
                            Expr::Nil
                        }
                    } else if let Some(k) = proto.constants.get(kidx as usize) {
                        constant_to_expr(k, &ctx.chunk.strings, &proto.constants)
                    } else {
                        // Constant index out of range — try chunk.strings as fallback
                        get_const_expr(proto, &ctx.chunk.strings, kidx)
                    };
                    store_complex(ctx, proto, regs, locals, stmts, a, pc, expr);
                }
            }
            LuauOpcode::Move => {
                // Copy the expression from source register.
                // If the source is a named reference, just propagate it.
                // If the source is a complex unnamed expression, give the
                // destination a name to prevent future duplication.
                // Table literals are kept pending so SETTABLEKS can fill them in-place.
                let src_expr = reg_expr(regs, b);
                // A loop-carried register that `premateralize_loop_carried`
                // bound to a real local keeps its identity: copy-propagating
                // over it drops the assignment entirely and silently rebinds the
                // name. `acc = bit32.bxor(...)` compiles to `CALL R4` +
                // `MOVE R0 R4`, and swallowing that MOVE left `acc` never
                // updated while `acc`'s later reads resolved to the loop-body
                // temp. Table sources stay pending so SETTABLEKS can still fill
                // them in place.
                let pinned_dest = match regs.get(a) {
                    Some(RegVal::Expr(Expr::Name(n)))
                        if ctx.is_pinned_reg(a as u8, pc)
                            && !matches!(&src_expr, Expr::Table { .. })
                            && !matches!(&src_expr, Expr::Name(s) if *s == *n) =>
                    {
                        Some(n.clone())
                    }
                    _ => None,
                };
                if let Some(bound) = pinned_dest {
                    stmts.push(Stat::Assign {
                        targets: vec![Expr::Name(bound.clone())],
                        values: vec![src_expr],
                    });
                    regs[a] = RegVal::Expr(Expr::Name(bound));
                    pc += 1;
                    continue;
                }
                match &src_expr {
                    Expr::Name(_) | Expr::Nil | Expr::Bool(_) | Expr::Number(_)
                    | Expr::String(_) | Expr::Varargs | Expr::Table { .. } => {
                        regs[a] = RegVal::Expr(src_expr);
                    }
                    _ => {
                        // Complex expression being moved — emit a local so it gets a name
                        store_complex(ctx, proto, regs, locals, stmts, a, pc, src_expr);
                    }
                }
            }

            LuauOpcode::GetGlobal => {
                // GETGLOBAL A D [AUX]: K[D] is the global name, AUX is a hash/index.
                // Try K[D] first (canonical), then AUX multi-fallback.
                // When all resolution fails, the instruction is likely a misaligned
                // data word — absorb silently instead of emitting garbage.
                if let Some(name) = resolve_global_name(proto, &ctx.chunk.strings, d, aux) {
                    regs[a] = RegVal::Expr(Expr::Name(name));
                } else {
                    regs[a] = RegVal::Unknown;
                }
            }
            LuauOpcode::SetGlobal => {
                // SETGLOBAL A D [AUX]: same encoding as GETGLOBAL.
                if let Some(name) = resolve_global_name(proto, &ctx.chunk.strings, d, aux) {
                    // B0.127: sanitize stdlib-name strings in assignment value.
                    emit_assign(stmts, Expr::Name(name), sanitize_leaked_global_string(reg_expr(regs, a)));
                }
                // If all resolution fails, suppress the assignment entirely --
                // the instruction is likely misaligned AUX data, not a real SETGLOBAL.
            }

            LuauOpcode::GetUpval => {
                let name = ctx.upval_name(proto, proto_index, b as u8);
                // Phase B0.52: if R[a] is already a DECLARED LOCAL (e.g., a
                // B0.51-seeded module table) and the bytecode attempts to
                // overwrite it with an upval reference, the subsequent
                // SETTABLEKS/RETURN reads clearly still refer to the local.
                // This can happen when opcode shuffle detection misidentifies
                // another opcode as GETUPVAL, or when the bytecode really IS
                // overwriting but our upval inference produced a stale name
                // (e.g., "game") that breaks downstream table operations.
                // Keep the local binding; don't clobber regs[a] with the upval
                // name. If the upval was genuinely needed, the downstream
                // misalignment will surface as a separate test failure to fix.
                //
                // The guard must not be tripped by this handler's OWN
                // `pre_declare` below. A proto that reads two different
                // upvalues through one scratch register —
                // `GETUPVAL R2 U0 … GETUPVAL R2 U1` — would otherwise keep the
                // first alias forever, and `reads = reads + 1` lifted as
                // `reads = n + 1`, silently reading the wrong upvalue. A
                // register whose current binding is itself an upvalue alias is
                // scratch, never the seeded module-table local guarded here.
                let holds_upval_alias = match locals.current_name(a) {
                    Some(cur) => (0..proto.num_upvalues)
                        .any(|u| ctx.upval_name(proto, proto_index, u) == cur),
                    None => false,
                };
                // Third disjunct: the register is DECLARED but currently holds
                // nothing. The guard above protects a live binding from being
                // clobbered — but it never asked whether the binding still has
                // a value. It usually does not: CALL clears every slot from its
                // frame base upwards (see the SETLIST/CALL handler below), so
                // the common shape
                //     GETTABLEKS R2 …  /  CALL R2  /  GETUPVAL R2 U0
                // hits this handler with R2 declared and empty. Skipping the
                // store there guards a binding that no longer exists, the
                // upvalue read produces nothing, and the next read of R2 falls
                // through to the `v{idx}` arm in reg_expr — which is emitted as
                // a chunk-top `local v2` that is never assigned.
                //
                // Measured over 628 files: this single case accounts for 606 of
                // 1807 hoisted-and-never-assigned names (33.5%) across 218 of
                // the 403 affected files, the largest cause by a wide margin.
                //
                // Widening only on EMPTY cannot reintroduce the B0.52 clobber
                // this guard was added for. The diagnosis split the guard's
                // misses into reg-was-empty and reg-had-value; the had-value
                // half produced zero flagged names and zero fallback events. So
                // the guard is right exactly when it has something to protect,
                // and wrong exactly when it does not.
                let reg_is_empty = matches!(regs.get(a), None | Some(RegVal::Unknown));
                if !locals.declared.contains(&a) || holds_upval_alias || reg_is_empty {
                    regs[a] = RegVal::Expr(Expr::Name(name.clone()));
                    // The register is now an ALIAS of an existing binding (the
                    // upvalue), not a fresh slot. Without recording that, the
                    // read-modify-write `GETUPVAL / ADDK / SETUPVAL` lowered its
                    // middle step through `needs_local` and emitted
                    // `local hits = hits + 1` — a shadow that made the following
                    // `hits = hits` a self-assign, which dead-store elimination
                    // deleted, silently dropping the upvalue write entirely.
                    locals.pre_declare(a);
                    locals.record_name(a, &name);
                }
            }
            LuauOpcode::SetUpval => {
                // Phase C2 pass #1 — SETUPVAL upval backscan.
                //
                // `SETUPVAL U(b) = R(a)` strongly implies the upvalue at slot
                // `b` is semantically the value held by R(a).  If R(a) carries
                // a meaningful `Expr::Name(n)` (from GETIMPORT / GETGLOBAL /
                // MOVE-propagation / GETTABLEKS / NAMECALL etc.) AND the
                // current proto has no name recorded for slot `b` yet, adopt
                // `n` as the inferred upvalue name.  This mirrors Medal's
                // pre-populate-from-debug-info trick but drives it from
                // SETUPVAL evidence, which survives strip.
                //
                // Collisions: the FIRST name wins — later stores are more
                // likely to be mutations than initializations.  The test
                // `c2_setupval_first_name_wins_on_collision` locks this.
                //
                // Propagation to child closures: `upval_parent_links` maps
                // `child_idx -> Vec<(child_slot, parent_idx, parent_slot)>`.
                // When we name slot `b` of the current proto, any child that
                // captured (parent_idx==proto_index, parent_slot==b) inherits
                // the name on its own slot if it was still empty.  We reuse
                // the existing two-phase plumbing — do NOT invent new links.
                let upval_idx = b;
                if upval_idx < proto.num_upvalues as usize {
                    if let RegVal::Expr(Expr::Name(ref candidate)) = regs[a] {
                        let is_generic_placeholder_name = is_generic_placeholder(candidate);
                        if !candidate.is_empty()
                            && is_valid_luau_identifier(candidate)
                            && !is_stdlib_shadow_name(candidate)
                            && !is_generic_placeholder_name
                        {
                            // 1. Record on the current proto if the slot is
                            //    still empty (first-wins on collision).
                            let num_upvals = proto.num_upvalues as usize;
                            let entry = ctx
                                .inferred_upvalue_names
                                .entry(proto_index)
                                .or_insert_with(|| vec![String::new(); num_upvals]);
                            if entry.len() < num_upvals {
                                entry.resize(num_upvals, String::new());
                            }
                            if entry[upval_idx].is_empty() {
                                entry[upval_idx] = candidate.clone();

                                // 2. Propagate to any child closure whose
                                //    upval_parent_links record a capture of
                                //    (proto_index, upval_idx).
                                //
                                //    Two-step read/write to avoid holding a
                                //    mutable borrow of `ctx.upval_parent_links`
                                //    while we mutate `inferred_upvalue_names`.
                                let updates: Vec<(usize, usize)> = ctx
                                    .upval_parent_links
                                    .iter()
                                    .flat_map(|(child_idx, links)| {
                                        let child_idx = *child_idx;
                                        links.iter().filter_map(move |(cs, pi, pu)| {
                                            if *pi == proto_index && *pu as usize == upval_idx {
                                                Some((child_idx, *cs))
                                            } else {
                                                None
                                            }
                                        })
                                    })
                                    .collect();
                                let name_to_set = candidate.clone();
                                for (child_idx, child_slot) in updates {
                                    if let Some(child_names) =
                                        ctx.inferred_upvalue_names.get_mut(&child_idx)
                                    {
                                        if let Some(slot) = child_names.get_mut(child_slot) {
                                            if slot.is_empty() || slot.starts_with("upval_") {
                                                *slot = name_to_set.clone();
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                let name = ctx.upval_name(proto, proto_index, b as u8);
                // B0.127: sanitize stdlib-name strings in assignment value.
                emit_assign(stmts, Expr::Name(name), sanitize_leaked_global_string(reg_expr(regs, a)));
            }

            LuauOpcode::GetImport => {
                // GETIMPORT A D [AUX]
                // AUX and K[D] both contain packed import path: count<<30 | id0<<20 | id1<<10 | id2
                // The 10-bit IDs (id0, id1, id2) are 0-based indices into proto.constants,
                // where each referenced constant should be a Constant::String. The Luau
                // compiler adds each path component as a String constant to k[], then packs
                // those k[] indices into the Import u32. The VM resolves them via k[id].
                // K[D] is an Import constant in proto.constants whose u32 value has the same format.
                let import_val = aux.unwrap_or_else(|| {
                    let d_unsigned = d as u16 as usize;
                    match proto.constants.get(d_unsigned) {
                        Some(Constant::Import(v)) => *v,
                        _ => 0,
                    }
                });

                // Also try K[D] as Import constant for the packed value
                let import_val2 = {
                    let d_unsigned = d as u16 as usize;
                    match proto.constants.get(d_unsigned) {
                        Some(Constant::Import(v)) => Some(*v),
                        _ => None,
                    }
                };

                // Resolve packed import IDs to string names.
                // IDs are 0-based indices into proto.constants (the proto's constant
                // table), where each referenced constant should be a Constant::String.
                // This matches the Luau VM which resolves via k[id].
                let resolve_import = |val: u32| -> Vec<String> {
                    let ids = decode_import(val);
                    let expected_count = ids.len();
                    let parts: Vec<String> = ids.iter()
                        .filter_map(|&id| {
                            // Primary: proto.constants (the authoritative source per Luau VM k[id])
                            if let Some(Constant::String(s)) = proto.constants.get(id as usize) {
                                return Some(s.clone());
                            }
                            // Fallback: chunk.strings in case of unusual encoding
                            if let Some(s) = ctx.chunk.strings.get(id as usize) {
                                return Some(s.clone());
                            }
                            None
                        })
                        .collect();
                    // Only return if we resolved ALL expected parts (no partial results)
                    if parts.len() == expected_count { parts } else { vec![] }
                };

                let mut parts = resolve_import(import_val);

                // If primary resolution failed, try the K[D] Import constant value
                if parts.is_empty() {
                    if let Some(val2) = import_val2 {
                        if val2 != import_val {
                            parts = resolve_import(val2);
                        }
                    }
                }

                // If full resolution failed, try partial -- resolve what we can
                if parts.is_empty() {
                    let try_vals = [import_val].into_iter()
                        .chain(import_val2.filter(|v2| *v2 != import_val));
                    for val in try_vals {
                        let ids = decode_import(val);
                        let partial: Vec<String> = ids.iter()
                            .filter_map(|&id| {
                                // Primary: proto.constants (Luau VM k[id])
                                if let Some(Constant::String(s)) = proto.constants.get(id as usize) {
                                    return Some(s.clone());
                                }
                                // Fallback: chunk.strings
                                if let Some(s) = ctx.chunk.strings.get(id as usize) {
                                    return Some(s.clone());
                                }
                                None
                            })
                            .collect();
                        if partial.len() > parts.len() {
                            parts = partial;
                        }
                    }
                }

                // If still empty, try treating K[D] directly as the result
                if parts.is_empty() {
                    let d_unsigned = d as u16 as usize;
                    if let Some(k) = proto.constants.get(d_unsigned) {
                        match k {
                            Constant::String(s) if is_valid_luau_identifier(s) => parts = vec![s.clone()],
                            Constant::Import(v) => {
                                // Resolve through proto.constants (primary, Luau VM k[id])
                                let ids = decode_import(*v);
                                for &id in &ids {
                                    if let Some(Constant::String(s)) = proto.constants.get(id as usize) {
                                        parts.push(s.clone());
                                    } else if let Some(s) = ctx.chunk.strings.get(id as usize) {
                                        parts.push(s.clone());
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }

                // Guard: if any resolved part is not a valid identifier, the
                // import resolution produced data strings, not global/field names.
                // Fall through to get_const_expr which emits Expr::String (quoted).
                let all_valid_ids = !parts.is_empty()
                    && parts.iter().all(|p| crate::decompiler::is_import_path_identifier(p));

                let expr = if all_valid_ids && parts.len() == 1 {
                    Expr::Name(parts[0].clone())
                } else if all_valid_ids && parts.len() >= 2 {
                    let mut expr = Expr::Name(parts[0].clone());
                    for part in &parts[1..] {
                        expr = Expr::Field {
                            object: Box::new(expr),
                            field: part.clone(),
                        };
                    }
                    expr
                } else {
                    // Last resort: resolve K[D] as a general constant expression.
                    get_const_expr(proto, &ctx.chunk.strings, d as u16 as u32)
                };
                store_complex(ctx, proto, regs, locals, stmts, a, pc, expr);
            }

            LuauOpcode::GetTable => {
                let expr = Expr::Index {
                    object: Box::new(table_expr(regs, b)),
                    key: Box::new(reg_expr(regs, c)),
                };
                store_complex(ctx, proto, regs, locals, stmts, a, pc, expr);
            }
            LuauOpcode::SetTable => {
                // SETTABLE A B C: R(B)[R(C)] = R(A)  — A=value, B=table, C=key
                // Phase C5: if table is still pending AND the key is a Number/
                // String constant, append inline. Dynamic keys close the pending
                // literal via the fallback path (a statement assign).
                let key_expr = reg_expr(regs, c);
                let val = sanitize_leaked_global_string(reg_expr(regs, a));
                let is_pending_table = matches!(&regs[b], RegVal::Expr(Expr::Table { .. }));
                let key_is_const = matches!(&key_expr, Expr::Number(_) | Expr::String(_));
                if is_pending_table && key_is_const {
                    if let RegVal::Expr(Expr::Table { fields }) = &regs[b] {
                        let mut new_fields = fields.clone();
                        // Named-field shortcut when key is a valid identifier string.
                        match &key_expr {
                            Expr::String(s) if super::table_reconstruction::is_valid_luau_identifier(s) => {
                                let mut replaced = false;
                                for f in &mut new_fields {
                                    if let TableField::Named(n, _) = f {
                                        if n == s {
                                            *f = TableField::Named(s.clone(), val.clone());
                                            replaced = true;
                                            break;
                                        }
                                    }
                                }
                                if !replaced {
                                    new_fields.push(TableField::Named(s.clone(), val));
                                }
                            }
                            Expr::Number(n) => {
                                let seq_count = new_fields.iter()
                                    .filter(|f| matches!(f, TableField::Sequential(_)))
                                    .count();
                                if (seq_count + 1) as f64 == *n && *n > 0.0 && n.fract() == 0.0 {
                                    new_fields.push(TableField::Sequential(val));
                                } else {
                                    new_fields.push(TableField::Indexed(Expr::Number(*n), val));
                                }
                            }
                            _ => {
                                new_fields.push(TableField::Indexed(key_expr.clone(), val));
                            }
                        }
                        regs[b] = RegVal::Expr(Expr::Table { fields: new_fields });
                    }
                } else {
                    // Phase B0.51: materialize R(B) as a local if never written.
                    ensure_table_reg_declared(ctx, proto, regs, locals, stmts, b, pc);
                    ensure_lvalue_base_materialized(ctx, proto, regs, locals, stmts, b, pc);
                    let target = Expr::Index { object: Box::new(table_expr(regs, b)), key: Box::new(key_expr) };
                    if !is_self_referential_field_assign(&target, &val) {
                        emit_assign(stmts, target, val);
                    }
                }
            }
            LuauOpcode::GetTableKS => {
                // AUX word holds a 0-based index into proto.constants for the field name
                let field = aux.map(|ax| get_table_string_from_aux(proto, &ctx.chunk.strings, ax))
                    .unwrap_or_else(|| format!("field_{}", c));
                // B0.122: materialize Table/Call/MethodCall base before field read.
                // Without this, `local x = ({}).CFrame` is emitted when register b
                // holds a Table expression — nonsensical and unreadable.
                ensure_lvalue_base_materialized(ctx, proto, regs, locals, stmts, b, pc);
                let expr = Expr::Field {
                    object: Box::new(table_expr(regs, b)),
                    field,
                };
                store_complex(ctx, proto, regs, locals, stmts, a, pc, expr);
            }
            LuauOpcode::SetTableKS => {
                // SETTABLEKS A B AUX: R(B)[K(AUX)] = R(A)  — A=value, B=table, AUX=string key
                let field = aux.map(|ax| get_table_string_from_aux(proto, &ctx.chunk.strings, ax))
                    .unwrap_or_else(|| format!("field_{}", c));
                // B0.127: sanitize stdlib-name strings in assignment value.
                let val = sanitize_leaked_global_string(reg_expr(regs, a));

                // Phase B0.51: if R(B) was never written (typical of
                // module-style patterns where NEWTABLE is unmapped), emit
                // `local vB = {}` first so subsequent `vB.field = val`
                // assigns have a declared target that B0.47/B0.48 can absorb.
                ensure_table_reg_declared(ctx, proto, regs, locals, stmts, b, pc);

                // Check if the table is still "pending" (hasn't been emitted as a statement yet).
                // A table is pending if the register contains a Table literal directly (not a Name ref).
                let is_pending_table = matches!(&regs[b], RegVal::Expr(Expr::Table { .. }));

                if is_pending_table {
                    // Table hasn't been emitted yet — we can add the field inline
                    if let RegVal::Expr(Expr::Table { fields }) = &regs[b] {
                        let mut new_fields = fields.clone();
                        let mut found = false;
                        for f in &mut new_fields {
                            if let TableField::Named(name, _) = f {
                                if name == &field {
                                    *f = TableField::Named(field.clone(), val.clone());
                                    found = true;
                                    break;
                                }
                            }
                        }
                        if !found {
                            new_fields.push(TableField::Named(field.clone(), val.clone()));
                        }
                        regs[b] = RegVal::Expr(Expr::Table { fields: new_fields });
                    }
                } else {
                    // Table already emitted as a local/assign — emit field assignment.
                    // B0.117: if the table base is a complex expression (MethodCall, Call,
                    // Function, etc.), materialize it as a local first so the assignment
                    // target is a valid Luau lvalue.
                    ensure_lvalue_base_materialized(ctx, proto, regs, locals, stmts, b, pc);
                    // Suppress self-referential assignments like `pairs.GetService = pairs`
                    // which arise when a NAMECALL is misidentified as SETTABLEKS.
                    let target = Expr::Field { object: Box::new(table_expr(regs, b)), field };
                    // C10e: drop `x.MethodName = y` where MethodName is a
                    // known Roblox Instance method — always a decompiler
                    // artifact (you can't mutate inherited methods).
                    if !is_self_referential_field_assign(&target, &val)
                        && !is_roblox_method_lvalue_artifact(&target)
                    {
                        emit_assign(stmts, target, val);
                    }
                }
            }
            LuauOpcode::GetTableN => {
                // B0.122: materialize complex base before index read.
                ensure_lvalue_base_materialized(ctx, proto, regs, locals, stmts, b, pc);
                let expr = Expr::Index {
                    object: Box::new(table_expr(regs, b)),
                    key: Box::new(Expr::Number((c + 1) as f64)),
                };
                store_complex(ctx, proto, regs, locals, stmts, a, pc, expr);
            }
            LuauOpcode::SetTableN => {
                // SETTABLEN A B C: R(B)[C+1] = R(A)  — A=value, B=table, C=index-1
                // Phase C5: if the table is still pending (regs[b] holds an
                // Expr::Table literal), append the entry inline so
                //     t[1] = a ; t[2] = b ; return t
                // collapses into `return { a, b }` without leaking v\d+ temps.
                // Falls through to the B0.51 path for already-materialized tables.
                let val = sanitize_leaked_global_string(reg_expr(regs, a));
                let idx1 = (c + 1) as f64;
                let is_pending_table = matches!(&regs[b], RegVal::Expr(Expr::Table { .. }));
                if is_pending_table {
                    if let RegVal::Expr(Expr::Table { fields }) = &regs[b] {
                        let mut new_fields = fields.clone();
                        // Sequential-part append when key is the next contiguous
                        // 1-based index (counting only Sequential entries already
                        // present). Otherwise emit as Indexed.
                        let seq_count = new_fields.iter()
                            .filter(|f| matches!(f, TableField::Sequential(_)))
                            .count();
                        if (seq_count + 1) as f64 == idx1 {
                            new_fields.push(TableField::Sequential(val));
                        } else {
                            new_fields.push(TableField::Indexed(Expr::Number(idx1), val));
                        }
                        regs[b] = RegVal::Expr(Expr::Table { fields: new_fields });
                    }
                } else {
                    // Phase B0.51: materialize R(B) as a local table if never written.
                    ensure_table_reg_declared(ctx, proto, regs, locals, stmts, b, pc);
                    ensure_lvalue_base_materialized(ctx, proto, regs, locals, stmts, b, pc);
                    let target = Expr::Index { object: Box::new(table_expr(regs, b)), key: Box::new(Expr::Number(idx1)) };
                    if !is_self_referential_field_assign(&target, &val) {
                        emit_assign(stmts, target, val);
                    }
                }
            }

            // ── Arithmetic ──
            // All arithmetic/logic ops use store_complex() to prevent expression
            // duplication cascades (e.g., `game % "game"` garbage in output).
            //
            // B0.71: When A==B==C for Sub/Div/Mod/IDiv, the instruction is
            // provably not real arithmetic (x%x=0, x/x=1, x-x=0 — the Luau
            // compiler would use LOADN instead). These are misidentified
            // Roblox passthrough/type-annotation opcodes. Treat as register
            // propagation (no-op when A==B, copy when A≠B in theory, but
            // A==B==C by definition here).
            LuauOpcode::Add => {
                if a == b && b == c {
                    // B0.73: self-add passthrough guard — Roblox repurposed ADD A,A,A
                    // as type-annotation passthrough (6 corpus hits, all nonsensical).
                    // Phase C4: if the register holds a non-numeric value
                    // (Function or Nil), emit a Comment so the corruption is
                    // visible in the output.
                    maybe_emit_self_arith_guard(regs, stmts, a, insn, "ADD");
                } else {
                    let e = mk_binop(regs, b, c, BinOp::Add); store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
                }
            }
            LuauOpcode::Sub => {
                if a == b && b == c {
                    // B0.71: self-sub passthrough guard
                    maybe_emit_self_arith_guard(regs, stmts, a, insn, "SUB");
                } else {
                    let e = mk_binop(regs, b, c, BinOp::Sub); store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
                }
            }
            LuauOpcode::Mul => {
                if a == b && b == c {
                    // B0.73: self-mul passthrough guard — corpus evidence:
                    //  StatsUtil.lua (12×): callback closures with `v0 * v0` body, no return
                    //  Season.lua (6×): `math.clamp(table * table, ...)` — nonsensical
                    //  PetRender.lua: `v10 * v10 * (v10 * v10)` in GroupIndex context
                    //  Enchanter.lua: `math.rad(CFrame * CFrame)` — nonsensical
                    maybe_emit_self_arith_guard(regs, stmts, a, insn, "MUL");
                } else {
                    let e = mk_binop(regs, b, c, BinOp::Mul); store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
                }
            }
            LuauOpcode::Div => {
                if a == b && b == c {
                    // B0.71: self-div passthrough guard
                    maybe_emit_self_arith_guard(regs, stmts, a, insn, "DIV");
                } else {
                    let e = mk_binop(regs, b, c, BinOp::Div); store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
                }
            }
            LuauOpcode::Mod => {
                if a == b && b == c {
                    // B0.71: self-mod passthrough guard
                    maybe_emit_self_arith_guard(regs, stmts, a, insn, "MOD");
                } else {
                    let e = mk_binop(regs, b, c, BinOp::Mod); store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
                }
            }
            LuauOpcode::Pow => {
                if a == b && b == c {
                    // B0.73: self-pow passthrough guard — corpus evidence:
                    //  arrow.lua: v18 = v18 ^ v18 — self-assignment passthrough
                    //  QuestHUDTask.lua: for i = v6, v3^v3, v5 — nonsensical loop bound
                    //  SiteEventsBoard.lua: v0^v0 & "table" — pow AND string
                    maybe_emit_self_arith_guard(regs, stmts, a, insn, "POW");
                } else {
                    let e = mk_binop(regs, b, c, BinOp::Pow); store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
                }
            }
            LuauOpcode::IDiv => {
                if a == b && b == c {
                    // B0.71: self-idiv passthrough guard
                    maybe_emit_self_arith_guard(regs, stmts, a, insn, "IDIV");
                } else {
                    let e = mk_binop(regs, b, c, BinOp::IDiv); store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
                }
            }

            LuauOpcode::AddK => { let e = mk_binop_k(proto, &ctx.chunk.strings, regs, b, c as u32, BinOp::Add); store_complex(ctx, proto, regs, locals, stmts, a, pc, e); }
            LuauOpcode::SubK => { let e = mk_binop_k(proto, &ctx.chunk.strings, regs, b, c as u32, BinOp::Sub); store_complex(ctx, proto, regs, locals, stmts, a, pc, e); }
            LuauOpcode::MulK => { let e = mk_binop_k(proto, &ctx.chunk.strings, regs, b, c as u32, BinOp::Mul); store_complex(ctx, proto, regs, locals, stmts, a, pc, e); }
            LuauOpcode::DivK => { let e = mk_binop_k(proto, &ctx.chunk.strings, regs, b, c as u32, BinOp::Div); store_complex(ctx, proto, regs, locals, stmts, a, pc, e); }
            LuauOpcode::ModK => { let e = mk_binop_k(proto, &ctx.chunk.strings, regs, b, c as u32, BinOp::Mod); store_complex(ctx, proto, regs, locals, stmts, a, pc, e); }
            LuauOpcode::PowK => { let e = mk_binop_k(proto, &ctx.chunk.strings, regs, b, c as u32, BinOp::Pow); store_complex(ctx, proto, regs, locals, stmts, a, pc, e); }
            LuauOpcode::IDivK => { let e = mk_binop_k(proto, &ctx.chunk.strings, regs, b, c as u32, BinOp::IDiv); store_complex(ctx, proto, regs, locals, stmts, a, pc, e); }

            LuauOpcode::SubRK => {
                // SUBRK A B C: R(A) = K(B) - R(C)  (reversed operand: constant on left)
                let k = get_const_expr(proto, &ctx.chunk.strings, b as u32);
                let e = if !matches!(k, Expr::Number(_)) {
                    k // non-numeric constant = dead code, just use the constant
                } else {
                    Expr::BinOp { left: Box::new(k), op: BinOp::Sub, right: Box::new(reg_expr(regs, c)) }
                };
                store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
            }
            LuauOpcode::DivRK => {
                // DIVRK A B C: R(A) = K(B) / R(C)  (reversed operand: constant on left)
                let k = get_const_expr(proto, &ctx.chunk.strings, b as u32);
                let e = if !matches!(k, Expr::Number(_)) {
                    k // non-numeric constant = dead code, just use the constant
                } else {
                    Expr::BinOp { left: Box::new(k), op: BinOp::Div, right: Box::new(reg_expr(regs, c)) }
                };
                store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
            }

            // B0.73: AND/OR passthrough when B==C (x and x = x, x or x = x).
            // Handles both A==B==C (all same) and A!=B, B==C (idempotent).
            // 27 corpus B==C patterns: Bubble.lua ReplicatedStorage chains, etc.
            LuauOpcode::And => {
                if b == c {
                    let val = regs.get(b).cloned().unwrap_or(RegVal::Unknown);
                    if a < regs.len() { regs[a] = val; }
                } else {
                    let e = mk_binop(regs, b, c, BinOp::And); store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
                }
            }
            LuauOpcode::Or => {
                if b == c {
                    let val = regs.get(b).cloned().unwrap_or(RegVal::Unknown);
                    if a < regs.len() { regs[a] = val; }
                } else {
                    let e = mk_binop(regs, b, c, BinOp::Or); store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
                }
            }
            LuauOpcode::AndK => { let e = mk_binop_k(proto, &ctx.chunk.strings, regs, b, c as u32, BinOp::And); store_complex(ctx, proto, regs, locals, stmts, a, pc, e); }
            LuauOpcode::OrK => { let e = mk_binop_k(proto, &ctx.chunk.strings, regs, b, c as u32, BinOp::Or); store_complex(ctx, proto, regs, locals, stmts, a, pc, e); }

            LuauOpcode::Concat => {
                if a == b && b == c {
                    // B0.73: single-register concat (range B..C = one register) = passthrough.
                } else {
                    // B0.68: build the chained concat through `mk_concat`, which
                    // guards each step against operand types that Luau's `..`
                    // operator cannot consume (Bool, Nil, Function, Table,
                    // Varargs, stdlib-shadow Name). When a guard fires, the
                    // salvage collapses to the valid side, preventing garbage
                    // like `((v1 .. false) .. v3) .. false` from reaching the
                    // emitter.
                    let mut expr = reg_expr(regs, b);
                    for r in (b + 1)..=c {
                        expr = mk_concat(expr, reg_expr(regs, r));
                    }
                    store_complex(ctx, proto, regs, locals, stmts, a, pc, expr);
                }
            }

            LuauOpcode::Not => {
                // B0.70: Roblox repurposed standard NOT (50) as a passthrough
                // (type-annotation propagation, like RbxExt93/94/98).
                // Real logical NOT uses RBX_EXT_96.
                // Evidence: 13,425 spurious `not vN` in corpus — `Stat("Coins", not "Coins")`,
                // `NumberRange.new(not v2, not v9)`, etc. — all contexts requiring values, not booleans.
                //
                // Phase C4: guard against impossible-NOT — `not <function>` is
                // a strong signal of bad opmap detection. Emit a Comment when
                // the source register holds a Function literal.
                if ctx.unary.not == crate::parser::opmap_db::UnarySem::Operator {
                    // This build really does use NOT as the `not` operator -
                    // either canonical Luau, or a client whose own compiler was
                    // observed emitting this opcode for `not x`.
                    let e = Expr::UnOp {
                        op: UnOp::Not,
                        operand: Box::new(reg_expr(regs, b as usize)),
                    };
                    store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
                } else {
                    if let Some(RegVal::Expr(Expr::Function { .. })) = regs.get(b as usize) {
                        stmts.push(Stat::Comment(format!(
                            "-- lifter error: NOT on Function  raw_opcode=0x{:08x}",
                            insn
                        )));
                    }
                    let val = regs.get(b as usize).cloned().unwrap_or(RegVal::Unknown);
                    if a < regs.len() {
                        regs[a] = val;
                    }
                }
            }
            LuauOpcode::Minus => {
                // B0.73: Roblox repurposed standard MINUS as passthrough
                // (same pattern as NOT/BNOT). Evidence (150 corpus hits):
                //   `v8.Parent = -v0` — assigning negation as parent
                //   `table.sort(-v0, Level7)` — sorting negated value
                //   `(-v0):Destroy()` — calling Destroy on negated value
                //   `(-v0)(clone2.Button, Blue)` — calling negation as function
                //   `warn.Zone = -v0` — nonsensical zone assignment
                // Previously B0.67 had partial passthrough for non-numeric types;
                // now ALL cases are passthrough since no RBX_EXT for real MINUS exists.
                //
                // Phase C4: guard against `-<bool>`, `-<string>`, `-<function>`.
                // Luau coerces strings to numbers in some arithmetic contexts
                // but never bool/function; either way it's a bad-opmap signal.
                if ctx.unary.minus == crate::parser::opmap_db::UnarySem::Operator {
                    // This build really does use MINUS as unary `-`.
                    let e = Expr::UnOp {
                        op: UnOp::Negate,
                        operand: Box::new(reg_expr(regs, b as usize)),
                    };
                    store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
                } else {
                    let bad_kind: Option<&'static str> = match regs.get(b as usize) {
                        Some(RegVal::Expr(Expr::Bool(_))) => Some("Bool"),
                        Some(RegVal::Expr(Expr::String(_))) => Some("String"),
                        Some(RegVal::Expr(Expr::Function { .. })) => Some("Function"),
                        _ => None,
                    };
                    if let Some(k) = bad_kind {
                        stmts.push(Stat::Comment(format!(
                            "-- lifter error: MINUS on {}  raw_opcode=0x{:08x}",
                            k, insn
                        )));
                    }
                    let val = regs.get(b as usize).cloned().unwrap_or(RegVal::Unknown);
                    if a < regs.len() {
                        regs[a] = val;
                    }
                }
            }
            LuauOpcode::Length => {
                // Canonical Luau uses LENGTH as the real `#` operator, so build
                // a genuine UnOp there. So does any client whose own compiler
                // was OBSERVED emitting this opcode for `#x` (see
                // `parser::opmap_db::UnarySemantics`). The Roblox passthrough
                // below remains the default: `ctx.unary` starts all-passthrough,
                // so an inferred decode never reaches this branch.
                //
                // B0.73: Roblox repurposed standard LENGTH as passthrough
                // (same pattern as NOT/BNOT/MINUS). Evidence (380 corpus hits):
                //   `Color3.fromRGB(Image2, Item6, #v15)` — length as color channel
                //   `CFrame.new(#v23, #v23, #v23, ...)` — length as CFrame components
                //   `(#v14)(v14, v15)` — calling length result as function
                //   `ShrineUtil = #v44` — assigning length to module name
                //   `v0.ThrottleFloat = throttle >> #v8` — shift by length
                // No RBX_EXT slot for real LENGTH identified.
                //
                // Phase C4: guard against `#<bool>`, `#<number>`, `#<function>` —
                // all three are runtime errors in Luau and strong signals of a
                // misidentified LENGTH opcode.
                if ctx.unary.length == crate::parser::opmap_db::UnarySem::Operator {
                    // This build really does use LENGTH as the `#` operator.
                    let e = Expr::UnOp {
                        op: UnOp::Length,
                        operand: Box::new(reg_expr(regs, b as usize)),
                    };
                    store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
                } else {
                    let bad_kind: Option<&'static str> = match regs.get(b as usize) {
                        Some(RegVal::Expr(Expr::Bool(_))) => Some("Bool"),
                        Some(RegVal::Expr(Expr::Number(_))) => Some("Number"),
                        Some(RegVal::Expr(Expr::Function { .. })) => Some("Function"),
                        _ => None,
                    };
                    if let Some(k) = bad_kind {
                        stmts.push(Stat::Comment(format!(
                            "-- lifter error: LENGTH on {}  raw_opcode=0x{:08x}",
                            k, insn
                        )));
                    }
                    let val = regs.get(b as usize).cloned().unwrap_or(RegVal::Unknown);
                    if a < regs.len() {
                        regs[a] = val;
                    }
                }
            }

            LuauOpcode::NewTable => {
                // Module-table pattern: a NEWTABLE at the very start of a proto
                // is almost always `local M = {}` followed by dozens of field
                // assignments (often interleaved with NEWCLOSURE statements).
                // We can't reconstruct `{a = ..., b = ...}` inline because the
                // closures between SETTABLEKS ops require their own statements,
                // so we materialize the table eagerly. This lets subsequent
                // SETTABLEKS ops emit proper `M.foo = ...` assignments instead
                // of silently cloning a pending Table literal that never gets
                // consumed.
                //
                // For non-proto-start NEWTABLE (locally scoped tables, loop
                // bodies, expression positions), keep the pending-table
                // behavior so that short `{a=1, b=2}` literals can inline.
                // Also materialise when the table is filled or read on the far
                // side of a branch: a parked literal is cloned at every read, so
                // `local t = {} ; for i = 1, n do t[i] = x end` would mutate a
                // throwaway copy and leave the later `#t` reading an undeclared
                // name. See `table_needs_binding`.
                let is_proto_start = pc <= 1 || table_needs_binding(code, pc, a);
                if is_proto_start {
                    // Phase B0.49: classify_write for shadow-on-rename.
                    let new_name = ctx.reg_name(proto, a as u8, pc);
                    let empty = Expr::Table { fields: vec![] };
                    let (kind, name) = locals.classify_write(a, &new_name);
                    match kind {
                        WriteKind::FirstDecl | WriteKind::Shadow => {
                            stmts.push(Stat::Local {
                                names: vec![name.clone()],
                                values: vec![empty],
                            });
                        }
                        WriteKind::Reassign => {
                            stmts.push(Stat::Assign {
                                targets: vec![Expr::Name(name.clone())],
                                values: vec![empty],
                            });
                        }
                    }
                    regs[a] = RegVal::Expr(Expr::Name(name));
                } else {
                    regs[a] = RegVal::Expr(Expr::Table { fields: vec![] });
                }
            }
            LuauOpcode::DupTable => {
                // DupTable uses a Table constant that contains key indices
                // This is used for tables with known string keys: {a = ..., b = ...}
                let d_unsigned = d as u16 as usize;
                let expr = if let Some(const_val) = proto.constants.get(d_unsigned) {
                    // Use constant_to_expr which now properly handles Table constants
                    constant_to_expr(const_val, &ctx.chunk.strings, &proto.constants)
                } else {
                    Expr::Table { fields: vec![] }
                };
                regs[a] = RegVal::Expr(expr);
            }
            LuauOpcode::SetList => {
                // SETLIST A B C AUX: bulk-set sequential table entries in R(A)
                // A = table register
                // B = first value register
                // C = nvalues + 1 (raw C == 0 is the multret sentinel; the VM
                //     does `int c = LUAU_INSN_C(insn) - 1` with -1 == LUA_MULTRET)
                // AUX = table index offset (1-based start index, usually 1)
                // Source values are in registers B through B+count-1
                //
                // The `- 1` is load-bearing: reading C as the count directly
                // appends one phantom element read from an uninitialized
                // register, so `print({"a","b","c"})` decompiled to
                // `print({"a","b","c",v5})`. Split the multret sentinel out
                // explicitly rather than reusing `count == 0`, so a legitimate
                // zero-element SETLIST (raw C == 1) is not misread as multret.
                let is_multret = c == 0;
                let count = c.saturating_sub(1);
                // Ensure register vector covers the source range
                let needed = b + count.max(1);
                if needed > regs.len() {
                    regs.resize(needed + 8, RegVal::Unknown);
                }
                if is_multret {
                    // C=0: vararg/multret - the preceding CALL or GETVARARGS set
                    // a variable number of results starting at register B.
                    // Scan forward from B collecting known values until we hit Unknown.
                    let mut values = Vec::new();
                    let mut last_reg: Option<usize> = None;
                    let limit = proto.max_stack_size as usize;
                    for i in b..limit.min(regs.len()).min(b + 256) {
                        match regs.get(i) {
                            Some(RegVal::Expr(e)) if !matches!(e, Expr::Nil) => {
                                values.push(e.clone());
                                last_reg = Some(i);
                            }
                            Some(RegVal::Unknown) => break,
                            _ => break,
                        }
                    }
                    // A multret CALL immediately before a multret SETLIST was
                    // materialized as `local t = f()` and collected above as the
                    // plain name `t` — that truncates the call to ONE result.
                    // Restore the call expression as the final field so its
                    // results expand into the table tail, exactly as the RETURN
                    // B=0 arm does. Gated identically: the temp must be the last
                    // statement, hold a single name/value, and be unreferenced.
                    if let (Some((m, idx)), false) = (multret_from_prev, values.is_empty()) {
                        if last_reg == Some(m) && idx + 1 == stmts.len() {
                            let folded = match &stmts[idx] {
                                Stat::Local { names, values: lv }
                                    if names.len() == 1 && lv.len() == 1 =>
                                {
                                    let name = names[0].clone();
                                    let matches_tail =
                                        matches!(values.last(), Some(Expr::Name(n)) if *n == name);
                                    if !matches_tail
                                        || stmts[..idx].iter().any(|s| stmt_reads_name(s, &name))
                                    {
                                        None
                                    } else {
                                        Some(lv[0].clone())
                                    }
                                }
                                _ => None,
                            };
                            if let Some(call_expr) = folded {
                                stmts.remove(idx);
                                let last = values.len() - 1;
                                values[last] = call_expr;
                            }
                        }
                    }
                    if let RegVal::Expr(Expr::Table { fields }) = &regs[a] {
                        let mut new_fields = fields.clone();
                        for val in values {
                            new_fields.push(TableField::Sequential(val));
                        }
                        regs[a] = RegVal::Expr(Expr::Table { fields: new_fields });
                    } else {
                        store_setlist_values(regs, stmts, a, aux, values);
                    }
                } else {
                    // Fixed count: collect exactly `count` values from B..B+count-1
                    let values: Vec<Expr> = (0..count.min(256)).map(|i| reg_expr(regs, b + i)).collect();
                    if let RegVal::Expr(Expr::Table { fields }) = &regs[a] {
                        let mut new_fields = fields.clone();
                        for val in values {
                            new_fields.push(TableField::Sequential(val));
                        }
                        regs[a] = RegVal::Expr(Expr::Table { fields: new_fields });
                    } else {
                        store_setlist_values(regs, stmts, a, aux, values);
                    }
                }
            }

            LuauOpcode::NewClosure | LuauOpcode::DupClosure => {
                // In standard Luau, D is an index into proto.child_protos which maps to
                // a global chunk proto index.  Roblox strips child_protos from all protos
                // (they are always empty), so we fall back to treating D as a direct
                // global chunk.protos index.
                let d_unsigned = d as u16 as usize;
                let proto_idx = if op == LuauOpcode::NewClosure {
                    // Standard path: child_protos[D] → global proto index.
                    proto.child_protos.get(d_unsigned).copied()
                        // B0.133: child-relative offset heuristic.
                        // When child_protos is empty (Roblox), D is a child-
                        // relative index. In standard Luau compiler layout,
                        // children of proto N start at N+1 (depth-first order).
                        // Try parent_idx + 1 + D first, rejecting any proto
                        // already on the decompilation stack.
                        .or_else(|| {
                            if proto.child_protos.is_empty() {
                                let parent_idx = ctx.proto_stack.last().copied().unwrap_or(0);
                                let candidate = parent_idx + 1 + d_unsigned;
                                if candidate < ctx.chunk.protos.len()
                                    && !ctx.proto_stack.contains(&candidate)
                                {
                                    return Some(candidate as u32);
                                }
                            }
                            None
                        })
                        // Roblox fallback: D IS the global chunk proto index directly.
                        .or_else(|| {
                            if d_unsigned < ctx.chunk.protos.len() {
                                Some(d_unsigned as u32)
                            } else {
                                None
                            }
                        })
                        // B0.132b: Roblox NEWCLOSURE high-byte fallback.
                        // Some Roblox bytecode versions pack extra data in the
                        // high byte of D, with the real proto index in the low
                        // byte. Observed: D=0x0201→proto 1, D=0x0402→proto 2.
                        // The low byte is a CHILD-RELATIVE index, not a global
                        // index. We approximate the global index as
                        // current_parent + 1 + low_byte (works for non-nested
                        // layouts). Skip if it would hit a proto already on the
                        // decompilation stack (false recursion).
                        .or_else(|| {
                            let low = d_unsigned & 0xFF;
                            if low == d_unsigned { return None; }
                            // Try child-relative offset from current proto
                            let parent_idx = ctx.proto_stack.last().copied().unwrap_or(0);
                            let candidate = parent_idx + 1 + low;
                            if candidate < ctx.chunk.protos.len()
                                && !ctx.proto_stack.contains(&candidate)
                            {
                                return Some(candidate as u32);
                            }
                            // Direct low-byte as global index (only if not recursive)
                            if low < ctx.chunk.protos.len()
                                && !ctx.proto_stack.contains(&low)
                            {
                                return Some(low as u32);
                            }
                            None
                        })
                } else {
                    // DupClosure: D indexes the constants table to a Constant::Closure.
                    // Per Luau's loader, LBC_CONSTANT_CLOSURE holds a GLOBAL chunk
                    // proto index (`protos[fid]`), NOT a child-list position — so the
                    // global lookup must be tried FIRST.  Resolving through
                    // `child_protos` first silently binds the wrong proto whenever the
                    // parent's child list is not the identity map, which happens as
                    // soon as an earlier sibling function itself contains a nested
                    // function.  Roblox protos have an empty child_protos, so they were
                    // already taking the global path; this only changes the populated,
                    // non-identity case.
                    let from_const = match proto.constants.get(d_unsigned) {
                        Some(Constant::Closure(child_idx)) => {
                            let global_idx = *child_idx as usize;
                            if global_idx < ctx.chunk.protos.len() {
                                Some(*child_idx)
                            } else {
                                proto.child_protos.get(global_idx).copied()
                            }
                        }
                        _ => None,
                    };
                    // Final fallback: D as direct child_protos index, then as global index.
                    from_const
                        .or_else(|| proto.child_protos.get(d_unsigned).copied())
                        // B0.133: child-relative offset for DupClosure too.
                        .or_else(|| {
                            if proto.child_protos.is_empty() {
                                let parent_idx = ctx.proto_stack.last().copied().unwrap_or(0);
                                let candidate = parent_idx + 1 + d_unsigned;
                                if candidate < ctx.chunk.protos.len()
                                    && !ctx.proto_stack.contains(&candidate)
                                {
                                    return Some(candidate as u32);
                                }
                            }
                            None
                        })
                        .or_else(|| {
                            if d_unsigned < ctx.chunk.protos.len() {
                                Some(d_unsigned as u32)
                            } else {
                                None
                            }
                        })
                        // B0.132b: same high-byte fallback for DupClosure.
                        .or_else(|| {
                            let low = d_unsigned & 0xFF;
                            if low == d_unsigned { return None; }
                            let parent_idx = ctx.proto_stack.last().copied().unwrap_or(0);
                            let candidate = parent_idx + 1 + low;
                            if candidate < ctx.chunk.protos.len()
                                && !ctx.proto_stack.contains(&candidate)
                            {
                                return Some(candidate as u32);
                            }
                            if low < ctx.chunk.protos.len()
                                && !ctx.proto_stack.contains(&low)
                            {
                                return Some(low as u32);
                            }
                            None
                        })
                };

                // Recursion guard: prevent decompiling a proto that's already on our stack
                let child_idx_usize = proto_idx.map(|idx| idx as usize);
                let is_recursive = child_idx_usize.map_or(false, |idx| ctx.proto_stack.contains(&idx));

                if is_recursive {
                    // B0.134 / B0.135 / B0.135b: silently skip ALL is_recursive
                    // NEWCLOSURE/DupClosure cases. Luau's compiler never emits a
                    // NEWCLOSURE whose target is an ancestor proto — recursion
                    // is implemented via GETUPVAL + CALL, and children are
                    // serialized before parents (children cannot exist before
                    // the proto that contains them). Therefore every path that
                    // resolves D to a proto already on ctx.proto_stack is an
                    // opmap misidentification (or corrupt bytecode).
                    //
                    // Prior phases emitted a diagnostic comment, but B0.132-B0.134
                    // heuristics surfaced 77 such cases on the real corpus, of
                    // which 67 were the self-reference pattern (proto=X, D=X,
                    // parent=X). Since all cases are invalid regardless of sub-
                    // pattern, we silently skip: register keeps its prior value,
                    // no statement emitted. We do NOT attempt CAPTURE-skip since
                    // without a real child proto we don't know num_upvalues,
                    // and `A <= 2` is too broad (would eat RETURN/NAMECALL).
                } else if let Some(child) = proto_idx.and_then(|idx| ctx.chunk.protos.get(idx as usize)) {
                    // B0.64: we must compute params AFTER lift_proto_inner
                    // populates the child's proto_naming hint cache via
                    // analyze_register_usage. Param computation is deferred
                    // to after the body lift below; initialize empty here.
                    let child_global_idx = proto_idx.unwrap() as usize;
                    let mut params: Vec<String> = Vec::new();

                    // Collect CAPTURE instructions and infer upvalue names for the child proto.
                    // CAPTURE format: A = capture type, B = index
                    //   A=0 (LCL_VAL): capture by value from parent register B
                    //   A=1 (LCL_REF): capture by reference from parent register B
                    //   A=2 (LCL_UPVAL): re-capture parent's upvalue index B
                    //
                    // CAPTURE instructions always immediately follow NEWCLOSURE in standard
                    // Luau bytecode. However, remap_chunk's AUX validation cascade can
                    // skip CAPTURE instructions (treating them as AUX data of a previous
                    // instruction), leaving them un-remapped. To handle this, we also
                    // recognize CAPTUREs by their structural pattern: A ≤ 2.
                    let mut cap_pc = pc + if op.has_aux() { 2 } else { 1 };
                    let mut inferred_upval_names: Vec<String> = Vec::new();
                    let expected_upvals = child.num_upvalues as usize;
                    // NOTE: CAPTUREs always immediately follow NEWCLOSURE/DUPCLOSURE
                    // in Luau bytecode, regardless of control-flow region boundaries.
                    // We must NOT bound this loop by `end` — if NEWCLOSURE sits at a
                    // region edge, CAPTUREs will be in the next region but still belong
                    // to this instruction sequence.
                    while cap_pc < code.len() && inferred_upval_names.len() < expected_upvals {
                        let cap_insn = code[cap_pc];
                        let cap_op = LuauOpcode::from_u8(insn_op(cap_insn));
                        let cap_a = insn_a(cap_insn);

                        // Check for CAPTURE: either properly remapped (opcode == Capture)
                        // or structurally matching with multiple constraints.
                        //
                        // Per the Luau bytecode spec, exactly `num_upvalues` CAPTURE
                        // instructions follow every NEWCLOSURE/DUPCLOSURE. The shuffled
                        // CAPTURE byte can collide with ANY other opcode in remap_chunk,
                        // so we cannot restrict the fallback to Nop/Unknown.
                        //
                        // Structural constraints for un-remapped CAPTUREs:
                        //   - A (capture type) must be 0, 1, or 2
                        //   - C field must be 0 (Luau compiler always emits CAPTURE
                        //     with LUAU_INSN_ABC(LOP_CAPTURE, type, id, 0))
                        //   - B (index) must be plausible: for types 0/1 (register
                        //     capture), B < max_stack_size; for type 2 (upval re-capture),
                        //     B < parent's num_upvalues
                        let cap_c = insn_c(cap_insn);
                        let cap_b = insn_b(cap_insn);
                        let b_plausible = match cap_a {
                            0 | 1 => cap_b < proto.max_stack_size,
                            2 => cap_b < proto.num_upvalues || proto.num_upvalues == 0,
                            _ => false,
                        };
                        let is_capture = cap_op == LuauOpcode::Capture
                            || (cap_a <= 2 && cap_c == 0 && b_plausible
                                && inferred_upval_names.len() < expected_upvals);

                        if is_capture {
                            let cap_type = cap_a;
                            let cap_idx = cap_b;
                            let name = match cap_type {
                                // LCL_VAL or LCL_REF: captures local register B from parent
                                0 | 1 => {
                                    // Try current register state first (has best name info).
                                    // Priority: Name > extract from complex expr > debug info > v{N}
                                    match regs.get(cap_idx as usize) {
                                        Some(RegVal::Expr(Expr::Name(n))) => n.clone(),
                                        // A loop variable is by construction a real,
                                        // correctly-named binding. Without this arm it
                                        // fell through to the raw-bytecode backscan,
                                        // which walks backwards past the loop header and
                                        // returns whatever LOADK last wrote that register
                                        // (e.g. the strings staged for the iterated table).
                                        Some(RegVal::LoopVar(n)) => n.clone(),
                                        Some(RegVal::Expr(expr)) => {
                                            // Extract a name from complex expressions:
                                            // Field access (e.g., game.Foo) -> use the field name
                                            // MethodCall (e.g., x:GetService("Y")) -> use "Y" or method name
                                            // Call -> try to extract a meaningful identifier
                                            match expr {
                                                Expr::Field { field, .. } => field.clone(),
                                                Expr::MethodCall { method, args, .. } => {
                                                    // For :GetService("X") or :WaitForChild("X"), use X
                                                    if !args.is_empty() {
                                                        if let Expr::String(s) = &args[0] {
                                                            s.clone()
                                                        } else {
                                                            method.clone()
                                                        }
                                                    } else {
                                                        method.clone()
                                                    }
                                                }
                                                Expr::Call { func, args, .. } => {
                                                    // For require(script.X) patterns, try X
                                                    if let Expr::Name(f) = func.as_ref() {
                                                        if f == "require" && !args.is_empty() {
                                                            if let Expr::Field { field, .. } = &args[0] {
                                                                field.clone()
                                                            } else {
                                                                ctx.reg_name(proto, cap_idx, cap_pc)
                                                            }
                                                        } else {
                                                            ctx.reg_name(proto, cap_idx, cap_pc)
                                                        }
                                                    } else {
                                                        ctx.reg_name(proto, cap_idx, cap_pc)
                                                    }
                                                }
                                                _ => ctx.reg_name(proto, cap_idx, cap_pc),
                                            }
                                        }
                                        _ => {
                                            // B0.131c: register is Unknown — backscan
                                            // the raw bytecode for the last instruction
                                            // that wrote to cap_idx and extract a name.
                                            let mut backscanned = None;
                                            for back in 1..=10usize {
                                                if pc < back { break; }
                                                let prev_pc = pc - back;
                                                let prev_insn = code[prev_pc];
                                                let prev_op = LuauOpcode::from_u8(insn_op(prev_insn));
                                                let prev_a = insn_a(prev_insn) as usize;
                                                if prev_a != cap_idx as usize { continue; }
                                                match prev_op {
                                                    LuauOpcode::GetImport => {
                                                        // GETIMPORT A D [AUX]
                                                        let aux_val = code.get(prev_pc + 1).copied().unwrap_or(0);
                                                        let ids = decode_import(aux_val);
                                                        // Use last segment of import path
                                                        if let Some(&last_id) = ids.last() {
                                                            if let Some(Constant::String(s)) = proto.constants.get(last_id as usize) {
                                                                if !s.is_empty() {
                                                                    backscanned = Some(s.clone());
                                                                }
                                                            }
                                                        }
                                                        break;
                                                    }
                                                    LuauOpcode::GetGlobal => {
                                                        let d_field = insn_d(prev_insn);
                                                        let aux_word = code.get(prev_pc + 1).copied();
                                                        if let Some(name) = resolve_global_name(proto, &ctx.chunk.strings, d_field, aux_word) {
                                                            backscanned = Some(name);
                                                        }
                                                        break;
                                                    }
                                                    LuauOpcode::GetTableKS => {
                                                        let aux = code.get(prev_pc + 1).copied().unwrap_or(0);
                                                        let field = get_table_string_from_aux(proto, &ctx.chunk.strings, aux);
                                                        if !field.starts_with("field_") {
                                                            backscanned = Some(field);
                                                        }
                                                        break;
                                                    }
                                                    LuauOpcode::LoadK => {
                                                        let d_field = insn_d(prev_insn);
                                                        let kidx = d_field as u16 as usize;
                                                        if let Some(Constant::String(s)) = proto.constants.get(kidx) {
                                                            backscanned = Some(s.clone());
                                                        }
                                                        break;
                                                    }
                                                    _ => {
                                                        // This instruction writes to cap_idx
                                                        // but we don't know how to extract a name
                                                        break;
                                                    }
                                                }
                                            }
                                            backscanned.unwrap_or_else(|| ctx.reg_name(proto, cap_idx, cap_pc))
                                        },
                                    }
                                }
                                // LCL_UPVAL: re-captures parent's upvalue at index B
                                2 => {
                                    let parent_name = ctx.upval_name(proto, proto_index, cap_idx);
                                    // If the parent's upvalue is still unresolved (upval_N),
                                    // don't cascade the placeholder — return empty so the
                                    // child's usage-based inference can fill it in later.
                                    // Also record the link for two-phase resolution: after
                                    // rename_upvals resolves the parent, we re-propagate.
                                    if parent_name.starts_with("upval_") {
                                        if let Some(child_idx) = child_idx_usize {
                                            ctx.upval_parent_links
                                                .entry(child_idx)
                                                .or_default()
                                                .push((inferred_upval_names.len(), proto_index, cap_idx));
                                        }
                                        String::new()
                                    } else {
                                        parent_name
                                    }
                                }
                                _ => String::new(),
                            };

                            // A register captured by CAPTURE VAL/REF is by
                            // definition a named, addressable location — but
                            // `store_complex` parks "inlinable" values in `regs`
                            // without emitting a statement, so `local n = 0`
                            // never reaches the parent. The child then correctly
                            // emits `n += 1` against an upvalue that was never
                            // declared, i.e. a nil global:
                            //   "attempt to perform arithmetic (add) on nil".
                            // Force the captured register to become a real local
                            // before the child proto is lifted.
                            //
                            // Two guards. (1) Only when the slot holds a concrete
                            // value — never `Unknown`, or we would emit a
                            // declaration with nothing to declare. (2) Only when
                            // this really is a CAPTURE, never when the structural
                            // fallback above merely guessed one: on shuffled
                            // Roblox bytecode a misfire currently costs a wrong
                            // upvalue name, and must not be escalated into an
                            // injected statement.
                            // Self-recursive closure: this CAPTURE grabs the
                            // very register the closure is about to be stored
                            // in (`local function fact` capturing `fact`).
                            //
                            // `reg_name` memoizes on the (reg, pc) PAIR and
                            // `unique_name` bumps its counter on every miss, so
                            // naming the register here at `cap_pc` and again at
                            // the declaration site below at `pc` allocated TWO
                            // names: the body called `fact` while the statement
                            // declared `fact3` — a call to a nil global. Look
                            // the name up at the declaration site's pc so the
                            // later call hits the memo and exactly one name is
                            // ever allocated.
                            let name = if matches!(cap_type, 0 | 1) && cap_idx as usize == a {
                                ctx.reg_name(proto, a as u8, pc)
                            } else {
                                name
                            };

                            let name = if matches!(cap_type, 0 | 1)
                                && cap_idx as usize != a
                                && cap_op == LuauOpcode::Capture
                            {
                                let pending = match regs.get(cap_idx as usize) {
                                    Some(RegVal::Expr(e)) if !matches!(e, Expr::Name(_)) => {
                                        Some(e.clone())
                                    }
                                    _ => None,
                                };
                                match pending {
                                    Some(value) => {
                                        emit_local_or_assign(
                                            ctx, proto, regs, locals, stmts,
                                            cap_idx as usize, cap_pc, value,
                                        );
                                        // Re-read the name that was actually
                                        // bound: `classify_write` may shadow or
                                        // rename, and a divergence here would
                                        // just swap one nil global for another.
                                        match regs.get(cap_idx as usize) {
                                            Some(RegVal::Expr(Expr::Name(n))) => n.clone(),
                                            _ => name,
                                        }
                                    }
                                    None => name,
                                }
                            } else {
                                name
                            };

                            inferred_upval_names.push(name);
                            cap_pc += 1;
                        } else {
                            break;
                        }
                    }

                    // Pad to expected length so the merge logic in lift_proto_inner
                    // recognizes unfilled slots as gaps for fallback inference
                    // (infer_main_proto_upval_names / infer_upval_names_from_setupval).
                    if inferred_upval_names.len() < expected_upvals {
                        inferred_upval_names.resize(expected_upvals, String::new());
                    }

                    // Store inferred upvalue names for the child proto so that
                    // GETUPVAL/SETUPVAL inside the child can use real names
                    if !inferred_upval_names.is_empty() {
                        if let Some(child_idx) = child_idx_usize {
                            ctx.inferred_upvalue_names.insert(child_idx, inferred_upval_names);
                        }
                    }

                    // B0.135c: lift_proto_inner owns the proto_stack push/pop (B0.134b).
                    let body = lift_proto_inner(ctx, child, child_idx_usize.unwrap_or(0), depth + 1);

                    // B0.64 (continued): now that the child's proto_naming is
                    // populated by lift_proto_inner, we can compute params
                    // against its hint cache. Swap current_proto_index to the
                    // child briefly for the reg_name lookups.
                    {
                        let saved = ctx.current_proto_index;
                        ctx.current_proto_index = Some(child_global_idx);
                        for i in 0..child.num_params {
                            params.push(ctx.reg_name(child, i, 0));
                        }
                        ctx.current_proto_index = saved;
                    }

                    let func_expr = Expr::Function {
                        params,
                        is_vararg: child.is_vararg,
                        body,
                    };

                    // Emit a local declaration for the closure so it gets a name.
                    //
                    // Phase B0.49: use `classify_write` instead of bare
                    // `needs_local`.  When R(A) was previously declared with a
                    // different semantic name (e.g., a prior NEWCLOSURE on the
                    // same register but with a different debug_name), emit a
                    // shadowing `local` so the subsequent code references the
                    // new name as a local — NOT a global.  Fixes the
                    // `reverse_k_arith = function()...end` bug in
                    // `ModuleScript.lua` where two NEWCLOSUREs to R1 with
                    // distinct debug_names used to produce a global write.
                    // Storing into a register that an earlier closure captured by
                    // reference, and which still holds that binding: this is an
                    // assignment to the existing variable, not a new declaration.
                    // Re-deriving the name here would allocate a fresh `isOdd3`
                    // and strand the captured `isOdd` at nil.
                    let carried_ref_binding = match regs.get(a) {
                        Some(RegVal::Expr(Expr::Name(n)))
                            if locals.current_name(a) == Some(n.as_str())
                                && reg_is_open_ref_capture(code, pc, a) =>
                        {
                            Some(n.clone())
                        }
                        _ => None,
                    };
                    if let Some(existing) = carried_ref_binding {
                        stmts.push(Stat::Assign {
                            targets: vec![Expr::Name(existing.clone())],
                            values: vec![func_expr],
                        });
                        regs[a] = RegVal::Expr(Expr::Name(existing));
                        pc = cap_pc;
                        continue;
                    }

                    let mut new_name = ctx.reg_name(proto, a as u8, pc);
                    // B0.129: if the name resolved to "self", it's carried from
                    // a previous NAMECALL that stored the receiver object in this
                    // register. "self" is never a valid name for a function
                    // closure — replace with a generic register name. This fixes
                    // the `self = function()` pattern (349 instances).
                    if new_name == "self" {
                        new_name = format!("fn{}", a);
                    }
                    let (kind, name) = locals.classify_write(a, &new_name);
                    match kind {
                        WriteKind::FirstDecl | WriteKind::Shadow => {
                            stmts.push(Stat::Local {
                                names: vec![name.clone()],
                                values: vec![func_expr],
                            });
                        }
                        WriteKind::Reassign => {
                            stmts.push(Stat::Assign {
                                targets: vec![Expr::Name(name.clone())],
                                values: vec![func_expr],
                            });
                        }
                    }
                    regs[a] = RegVal::Expr(Expr::Name(name));

                    pc = cap_pc;
                    continue;
                } else {
                    // Proto lookup failed — emit a placeholder closure.
                    // Do NOT skip subsequent instructions: without knowing how many
                    // upvalues the child proto expects, the `a <= 2` heuristic is
                    // too broad and will eat real instructions like NameCall(a=1)
                    // or Return(a=0), producing empty output.  Mapped CAPTURE
                    // instructions are already no-ops in the dispatch loop; unmapped
                    // ones go through the Unknown handler (marks reg Unknown) which
                    // is far better than silently consuming a RETURN.
                    // C10f: emit compact empty-function placeholder. The multi-
                    // line `-- closure D=... (proto lookup failed ...)` comment
                    // was 2+ lines per site × thousands of sites = major noise.
                    // The header block of each .lua file already reports
                    // unresolved counts; per-site diagnostic is redundant.
                    regs[a] = RegVal::Expr(Expr::Function {
                        params: vec![],
                        is_vararg: false,
                        body: vec![],
                    });
                    // Fall through to normal pc += 1 at end of loop (no skip, no continue).
                }
            }

            LuauOpcode::NameCall => {
                // NameCall A B AUX: A+1 = B (self), A = B:method from proto.constants
                // AUX is a 0-based index into proto.constants. The CALL instruction follows.
                let method = aux.map(|ax| get_method_string_from_aux(proto, &ctx.chunk.strings, ax))
                    .unwrap_or_else(|| format!("method_{}", pc));
                // A pending table literal in the receiver register is handed out
                // as a fresh CLONE at every read, so `obj:bump(5)`, `obj:bump(7)`
                // and `obj.total` each built their own private copy and the two
                // mutations landed on different objects. Materialize R(B) itself
                // — not just the R(A+1) self slot below — so every later consumer
                // shares one identity.
                //
                // Gated on `Expr::Table` only: `ensure_lvalue_base_materialized`
                // also materializes Call/MethodCall, which would break the
                // deliberate `a == b` method-chain pass-through below.
                if matches!(regs.get(b), Some(RegVal::Expr(Expr::Table { .. }))) {
                    ensure_lvalue_base_materialized(ctx, proto, regs, locals, stmts, b, pc);
                }
                let obj = method_receiver_expr(regs, b);
                // If the object is a complex expression (call, field chain, etc.),
                // emit a local first so it doesn't get duplicated inside the
                // MethodCall expression and at every later use site.
                // EXCEPTION: MethodCall/Call expressions that are part of a method
                // chain (kept inline by the CALL handler) pass through directly
                // so they nest naturally: obj:M1():M2():M3().
                let obj_named = match &obj {
                    Expr::Name(_) | Expr::Nil | Expr::Bool(_) | Expr::Number(_) => obj,
                    // Method chain: the previous CALL kept its result inline in
                    // this register specifically so we can nest it here.
                    Expr::MethodCall { .. } | Expr::Call { .. } if a == b => obj,
                    _ => {
                        // Phase B0.49: classify_write for shadow-on-rename.
                        let new_oname = ctx.reg_name(proto, (a + 1) as u8, pc);
                        let (kind, oname) = locals.classify_write(a + 1, &new_oname);
                        match kind {
                            WriteKind::FirstDecl | WriteKind::Shadow => {
                                stmts.push(Stat::Local {
                                    names: vec![oname.clone()],
                                    values: vec![obj],
                                });
                            }
                            WriteKind::Reassign => {
                                stmts.push(Stat::Assign {
                                    targets: vec![Expr::Name(oname.clone())],
                                    values: vec![obj],
                                });
                            }
                        }
                        Expr::Name(oname)
                    }
                };
                regs[a + 1] = RegVal::Expr(obj_named.clone());
                regs[a] = RegVal::Expr(Expr::MethodCall {
                    object: Box::new(obj_named),
                    method,
                    args: vec![],
                });
            }

            LuauOpcode::Call => {
                // If the function register contains a Table literal (from
                // DUPTABLE/NEWTABLE + SETTABLEKS), emit it as a local first.
                // Otherwise we'd produce invalid Luau like `{...}(args)`.
                let func = match reg_expr(regs, a) {
                    Expr::Table { .. } => {
                        let tbl_expr = reg_expr(regs, a);
                        // Phase B0.49: classify_write for shadow-on-rename.
                        let new_name = ctx.reg_name(proto, a as u8, pc);
                        let (kind, name) = locals.classify_write(a, &new_name);
                        match kind {
                            WriteKind::FirstDecl | WriteKind::Shadow => {
                                stmts.push(Stat::Local {
                                    names: vec![name.clone()],
                                    values: vec![tbl_expr],
                                });
                            }
                            WriteKind::Reassign => {
                                stmts.push(Stat::Assign {
                                    targets: vec![Expr::Name(name.clone())],
                                    values: vec![tbl_expr],
                                });
                            }
                        }
                        regs[a] = RegVal::Expr(Expr::Name(name.clone()));
                        Expr::Name(name)
                    }
                    // Nil in the function register means LOADNIL was used to
                    // initialize it before the real function was loaded in a
                    // different control flow path. Use a placeholder name.
                    Expr::Nil => {
                        Expr::Name(ctx.reg_name(proto, a as u8, pc))
                    }
                    // String/Bool/Number literals in function position are never valid
                    // Luau calls. This typically happens when NAMECALL's AUX string
                    // ends up in the function register, or from cross-region register
                    // state leaking a constant into the call position.
                    Expr::String(s) => {
                        // If it looks like a method name, it might be a NAMECALL
                        // artifact — use the string as part of the name
                        let name = if s.chars().all(|c| c.is_alphanumeric() || c == '_') && !s.is_empty() {
                            s
                        } else {
                            ctx.reg_name(proto, a as u8, pc)
                        };
                        Expr::Name(name)
                    }
                    Expr::Bool(_) | Expr::Number(_) => {
                        Expr::Name(ctx.reg_name(proto, a as u8, pc))
                    }
                    other => other,
                };
                // Phase B0.114b: detect require() calls — Table arguments passed
                // to require are always decompilation artifacts (require takes a
                // ModuleScript, never a table). Materialize them as locals later.
                let is_require_call = matches!(&func, Expr::Name(n) if n == "require");

                let is_vararg_call = b == 0;
                let nargs = if b == 0 { 0 } else { b - 1 };
                // C encoding: 0 = varargs, 1 = 0 results, 2+ = (C-1) results
                // Clamp to sane maximum — Luau functions almost never return >10 values.
                // Very large C values indicate corrupted/misdetected bytecode.
                // Clamp to 1 (statement, 0 captures) rather than 0 (vararg),
                // because vararg mode triggers aggressive register scanning that
                // can over-collect stale arguments from previous calls.
                let raw_nresults = c;
                let nresults = if raw_nresults > 10 && raw_nresults != 0 { 1 } else { raw_nresults };

                // For method calls (from NAMECALL), skip the implicit self argument at A+1
                let is_method = matches!(&func, Expr::MethodCall { .. });
                let arg_start = if is_method { 2 } else { 1 };
                let mut args = Vec::new();
                if is_vararg_call {
                    // B=0 means args go from A+1 (or A+2 for method) up to the top of the
                    // stack from the previous instruction. Collect non-nil registers until
                    // we hit an Unknown, Nil, or the end of the register file.
                    // Be conservative: stop at the first gap (Unknown or Nil) to avoid
                    // picking up stale values from previous calls that happen to remain
                    // in adjacent registers.
                    let start = a + arg_start;
                    let max_args = (proto.max_stack_size as usize).min(regs.len()).saturating_sub(start);
                    for i in 0..max_args.min(10) {
                        let r = start + i;
                        match regs.get(r) {
                            Some(RegVal::Expr(e)) if !matches!(e, Expr::Nil) => {
                                args.push(e.clone());
                            }
                            // An explicitly LOADNIL'd register is a real argument
                            // (`f(nil, x)`), not a leak — keep collecting.
                            Some(RegVal::Expr(Expr::Nil)) if explicit_nil_regs.contains(&r) => {
                                args.push(Expr::Nil);
                            }
                            // Unknown or a stale Nil marks the boundary of
                            // intentionally-set args. Stop here to prevent stale
                            // register values from leaking in.
                            _ => break,
                        }
                    }
                } else {
                    // Cap non-vararg argument count — Luau functions rarely have >10 args.
                    // Large nargs values indicate corrupted/misdetected bytecode.
                    let capped_nargs = nargs.min(10);
                    for i in arg_start..=capped_nargs {
                        let reg = a + i;
                        let arg_expr = reg_expr(regs, reg);
                        // B0.54B: if the arg resolves to a generic `vN` fallback but
                        // analyze_register_usage installed a hint for this register
                        // at or before `pc`, promote the arg to the hint-derived
                        // name. This attacks the ~32% of residual generic names in
                        // method/function arg position (`obj:Method(v17, v18)`).
                        let is_generic = matches!(&arg_expr, Expr::Name(n)
                            if n.starts_with('v') && n.len() >= 2
                                && n[1..].chars().all(|c| c.is_ascii_digit()));
                        // Never rename a register that is already BOUND to a
                        // local: `reg_name` returns the most recent hint for the
                        // register across the whole proto, so a hint installed by
                        // a LATER instruction would win and `print(count)` became
                        // `print(w)` — a read of an undeclared global.
                        let is_bound_local = locals.declared.contains(&reg)
                            || matches!(&arg_expr, Expr::Name(n) if locals.is_bound_name(n));
                        let arg_expr = if is_generic && !is_bound_local && reg <= u8::MAX as usize {
                            let hinted = ctx.reg_name(proto, reg as u8, pc);
                            // Only promote when the synthesized name is TRULY
                            // semantic. Rejecting other generic families (fn\d+,
                            // call\d+, upval_\d+, arg\d+) avoids lateral swaps
                            // that don't improve readability.
                            if !hinted.is_empty()
                                && !is_generic_placeholder(&hinted)
                            {
                                Expr::Name(hinted)
                            } else {
                                arg_expr
                            }
                        } else {
                            arg_expr
                        };
                        args.push(arg_expr);
                    }
                    // Trim trailing nil args from non-vararg calls too —
                    // these are typically LOADNIL initialization artifacts
                    // from cross-region register state leaking.
                    // Only trim nils that this range did NOT explicitly LOADNIL —
                    // an explicit nil is a real argument the caller wrote.
                    while args.last().map_or(false, |v| matches!(v, Expr::Nil))
                        && !explicit_nil_regs.contains(&(a + arg_start + args.len() - 1))
                    {
                        args.pop();
                    }
                }

                // Phase B0.114b: materialize Table arguments in require() calls.
                // require() takes a ModuleScript, never a table constructor.
                // Table args here come from pending NEWTABLE+SETTABLEKS that
                // should have been a GETIMPORT for the module path. Extracting
                // them as locals produces `local v = {...}; require(v)` which
                // is clearer than `require({...})`.
                //
                // Phase C8: before materializing, detect the single-Named-field
                // wrapper pattern `require({ K = X })` and unwrap directly to
                // `require(X)`. Observed across ClientScript*.lua (~40 instances)
                // and other framework-style files. Because `require(table)` is a
                // Roblox runtime error, any preserved wrapper is either a
                // decompiler artifact or broken code — unwrapping is safe.
                // Only unwraps when the inner value is a clean module-shaped
                // expression (Name / Field / Index / MethodCall / Call) to
                // avoid pulling out primitives or nested tables.
                if is_require_call {
                    for arg in args.iter_mut() {
                        if let Expr::Table { fields } = arg {
                            if fields.len() == 1 {
                                if let TableField::Named(_, inner) = &fields[0] {
                                    if matches!(inner,
                                        Expr::Name(_)
                                        | Expr::Field { .. }
                                        | Expr::Index { .. }
                                        | Expr::MethodCall { .. }
                                        | Expr::Call { .. })
                                    {
                                        *arg = inner.clone();
                                        continue;
                                    }
                                }
                            }
                        }
                        if matches!(arg, Expr::Table { fields } if !fields.is_empty()) {
                            let tbl_expr = arg.clone();
                            let tbl_name = format!("module_{}", pc);
                            stmts.push(Stat::Local {
                                names: vec![tbl_name.clone()],
                                values: vec![tbl_expr],
                            });
                            *arg = Expr::Name(tbl_name);
                        }
                    }
                }

                // B0.124: resolve Number arguments that are really constant indices.
                // When LOADN is used instead of LOADK (opmap LOADN/LOADK confusion),
                // the D field (constant index) is loaded as a raw integer instead of
                // resolving K[D]. For method calls to known string-arg methods
                // (GetService, FindFirstChild, WaitForChild, etc.), replace Number
                // args with the string constant at that index.
                let is_string_arg_method = matches!(&func,
                    Expr::MethodCall { method, .. } if matches!(method.as_str(),
                        "GetService" | "FindFirstChild" | "WaitForChild"
                        | "FindFirstChildOfClass" | "FindFirstChildWhichIsA"
                        | "IsA" | "GetAttribute" | "SetAttribute"
                    ));
                if is_string_arg_method {
                    for arg in args.iter_mut() {
                        if let Expr::Number(n) = arg {
                            let idx = *n as u32;
                            if let Some(Constant::String(s)) = proto.constants.get(idx as usize) {
                                *arg = Expr::String(s.clone());
                            }
                        }
                    }
                }

                // B0.125: strip String/Bool/Table arguments from numeric-only builtins.
                // These functions always take numeric args; String/Bool/Table args come
                // from NAMECALL AUX leakage contaminating nearby registers.
                let is_numeric_only_call = matches!(&func,
                    Expr::Field { object, field, .. }
                        if (matches!(object.as_ref(), Expr::Name(n) if n == "bit32")
                            && matches!(field.as_str(),
                                "band" | "bor" | "bxor" | "bnot" | "btest"
                                | "lshift" | "rshift" | "arshift" | "lrotate" | "rrotate"
                                | "extract" | "replace" | "countlz" | "countrz" | "byteswap"
                            ))
                        || (matches!(object.as_ref(), Expr::Name(n) if n == "math")
                            && matches!(field.as_str(),
                                "abs" | "ceil" | "floor" | "sqrt" | "sin" | "cos" | "tan"
                                | "asin" | "acos" | "atan" | "atan2" | "exp" | "log"
                                | "max" | "min" | "pow" | "fmod" | "clamp" | "round"
                                | "noise" | "sign" | "random"
                            ))
                        // Color3/Vector3/CFrame constructors take only numbers
                        || (matches!(object.as_ref(), Expr::Name(n)
                                if n == "Color3" || n == "Vector3" || n == "CFrame"
                                    || n == "Vector2" || n == "UDim2" || n == "UDim")
                            && matches!(field.as_str(), "new" | "fromRGB" | "fromHSV"
                                | "fromScale" | "fromOffset" | "Angles" | "lookAt"))
                );
                if is_numeric_only_call {
                    for (i, arg) in args.iter_mut().enumerate() {
                        if matches!(arg, Expr::String(_) | Expr::Bool(_) | Expr::Table { .. }) {
                            let reg = a + if is_method { 2 } else { 1 } + i;
                            *arg = Expr::Name(ctx.reg_name(proto, reg as u8, pc));
                        }
                    }
                }

                // B0.125b: table.clear/insert/remove/sort/find first arg must be
                // table-like. String/Bool first args are NAMECALL leakage.
                let is_table_first_arg_fn = matches!(&func,
                    Expr::Field { object, field, .. }
                        if matches!(object.as_ref(), Expr::Name(n) if n == "table")
                        && matches!(field.as_str(),
                            "clear" | "insert" | "remove" | "sort" | "find"
                            | "move" | "concat" | "pack" | "clone" | "freeze"
                        ));
                if is_table_first_arg_fn && !args.is_empty() {
                    if matches!(&args[0], Expr::String(_) | Expr::Bool(_) | Expr::Number(_)) {
                        let reg = a + if is_method { 2 } else { 1 };
                        args[0] = Expr::Name(ctx.reg_name(proto, reg as u8, pc));
                    }
                }

                // B0.128: resolve Name arguments for string-arg methods.
                // When GetService/FindFirstChild/WaitForChild/etc. has a Name
                // argument (e.g., `game:GetService(v2)` instead of
                // `game:GetService("Players")`), the argument register was never
                // properly loaded — the LOADK that should have set it was
                // misidentified by opcode shuffling.
                //
                // Fix: scan backwards through raw instructions to find one whose
                // A field matches the argument register. Extract its D field as a
                // constant index and resolve it to a String if possible.
                if is_string_arg_method {
                    for (i, arg) in args.iter_mut().enumerate() {
                        let is_name_arg = matches!(arg, Expr::Name(n)
                            if !n.is_empty() && !matches!(n.as_str(), "game" | "workspace" | "script"));
                        if !is_name_arg { continue; }
                        let arg_reg = a + (if is_method { 2 } else { 1 }) + i;
                        // Scan up to 6 instructions backwards to find one targeting arg_reg
                        let mut resolved = false;
                        for back in 1..=6 {
                            if pc < back { break; }
                            let prev_pc = pc - back;
                            let prev_insn = code[prev_pc];
                            let prev_a = insn_a(prev_insn) as usize;
                            let prev_d = insn_d(prev_insn);
                            if prev_a == arg_reg {
                                // This instruction targeted our argument register.
                                // Try interpreting its D field as a constant index.
                                let kidx = prev_d as u16 as usize;
                                if let Some(Constant::String(s)) = proto.constants.get(kidx) {
                                    if is_valid_luau_identifier(s) {
                                        *arg = Expr::String(s.clone());
                                        resolved = true;
                                    }
                                }
                                break;
                            }
                        }
                        // Fallback: if scan didn't find a match, try the AUX word
                        // of the NAMECALL instruction (pc-2 for 2-word NAMECALL).
                        // Sometimes the string arg index is adjacent.
                        if !resolved && pc >= 3 {
                            let aux_word = code[pc - 1];
                            if let Some(Constant::String(s)) = proto.constants.get(aux_word as usize) {
                                // Only use if it looks like a Roblox service/instance name
                                if is_valid_luau_identifier(s) && !s.starts_with("__") {
                                    // Don't override — this fallback is too speculative
                                }
                            }
                        }
                    }
                }

                // Remember how many argument registers were used so we can
                // clear them after the call to prevent stale value leakage.
                let _actual_arg_count = if is_vararg_call { args.len() } else { nargs };

                // If we have a pending fastcall, use the builtin name
                let call_expr = if let Some((bfn_id, _target)) = pending_fastcall.take() {
                    // Prefer the function already loaded into register A (from GETIMPORT/GETGLOBAL)
                    // over the hardcoded builtin_name() lookup. The register value is derived from
                    // actual bytecode constants and is always correct; builtin IDs are Roblox-specific
                    // and may differ from standard Luau's ordering.
                    //
                    // Only fall back to the builtin name if register A holds something that isn't
                    // directly callable by name (nil, unknown, a table literal, etc.).
                    match func {
                        Expr::Name(_) | Expr::Field { .. } => {
                            // Register has a named function — use it directly.
                            Expr::Call { func: Box::new(func), args }
                        }
                        Expr::MethodCall { object, method, .. } => {
                            Expr::MethodCall { object, method, args }
                        }
                        _ => {
                            // Register holds something non-callable by name (nil, unknown, literal).
                            // Fall back to the builtin name if available.
                            let bname = builtin_name(bfn_id);
                            if bname != "none" && !bname.is_empty() {
                                Expr::Call { func: Box::new(Expr::Name(bname.to_string())), args }
                            } else {
                                Expr::Call { func: Box::new(func), args }
                            }
                        }
                    }
                } else {
                    match func {
                        Expr::MethodCall { object, method, .. } => {
                            Expr::MethodCall { object, method, args }
                        }
                        _ => Expr::Call { func: Box::new(func), args },
                    }
                };

                if nresults == 1 {
                    // nresults==1 means 0 return values captured → statement
                    stmts.push(Stat::ExprStat(call_expr));
                } else if nresults >= 2 {
                    // nresults==2+ means (n-1) return values captured
                    // Always emit a local for call results to prevent re-inlining
                    // the entire call expression at every use site
                    let mut new_name = ctx.reg_name(proto, a as u8, pc);
                    // Phase C6: require(path) → name result from path tail.
                    // Only override when the default name is a generic placeholder;
                    // real debug-info names always win.
                    if is_require_call && is_generic_placeholder(&new_name) {
                        if let Expr::Call { args: call_args, .. } = &call_expr {
                            if let Some(first) = call_args.first() {
                                if let Some(derived) = require_arg_to_name(first) {
                                    new_name = derived;
                                }
                            }
                        }
                    }
                    if nresults == 2 {
                        // Single return value -- check if the next instruction is
                        // NAMECALL targeting the same register (method chain).
                        // CALL has no AUX, so the next instruction is at pc+1.
                        // Pattern: CALL A _ 2 ; NAMECALL A A AUX
                        let next_is_namecall_chain = if pc + 1 < code.len() {
                            let next_insn = code[pc + 1];
                            let next_op = LuauOpcode::from_u8(insn_op(next_insn));
                            let next_a = insn_a(next_insn) as usize;
                            let next_b = insn_b(next_insn) as usize;
                            next_op == LuauOpcode::NameCall && next_b == a && next_a == a
                        } else {
                            false
                        };

                        if next_is_namecall_chain {
                            // Method chain: keep the call expression inline so
                            // the subsequent NAMECALL picks it up as the object,
                            // producing nested MethodCall expressions instead of
                            // intermediate local variables.
                            regs[a] = RegVal::Expr(call_expr);
                        } else {
                            // Phase B0.49: classify_write for shadow-on-rename.
                            let (kind, name) = locals.classify_write(a, &new_name);
                            match kind {
                                WriteKind::FirstDecl | WriteKind::Shadow => {
                                    stmts.push(Stat::Local {
                                        names: vec![name.clone()],
                                        values: vec![call_expr],
                                    });
                                }
                                WriteKind::Reassign => {
                                    stmts.push(Stat::Assign {
                                        targets: vec![Expr::Name(name.clone())],
                                        values: vec![call_expr],
                                    });
                                }
                            }
                            // Raw C == 2 means the VM keeps exactly ONE result,
                            // i.e. the source wrote `(f())`. Record the temp so
                            // the inliner will not splice it into a slot where
                            // the call would re-expand to all its results.
                            if raw_nresults == 2 {
                                ctx.arity_pinned_temps.insert(name.clone());
                            }
                            regs[a] = RegVal::Expr(Expr::Name(name));
                        }
                    } else {
                        // Multiple return values
                        let mut names = vec![new_name.clone()];
                        for i in 1..(nresults - 1) {
                            if a + i >= regs.len() { break; }
                            names.push(ctx.reg_name(proto, (a + i) as u8, pc));
                        }
                        // Check if ANY result register needs a local.  Luau
                        // multi-assignment must be all-local or all-assign, so
                        // if any target is new we emit `local` for the group.
                        let any_new = (0..names.len()).any(|i| !locals.declared.contains(&(a + i)));
                        if any_new {
                            for i in 0..names.len() {
                                locals.pre_declare(a + i);
                                // Phase B0.49: keep current_names in sync so
                                // future single-reg writes can shadow when the
                                // semantic hint changes.
                                locals.record_name(a + i, &names[i]);
                            }
                            stmts.push(Stat::Local {
                                names: names.clone(),
                                values: vec![call_expr],
                            });
                        } else {
                            let targets: Vec<Expr> = names.iter().map(|n| Expr::Name(n.clone())).collect();
                            stmts.push(Stat::Assign {
                                targets,
                                values: vec![call_expr],
                            });
                        }
                        for (i, n) in names.into_iter().enumerate() {
                            if a + i < regs.len() {
                                regs[a + i] = RegVal::Expr(Expr::Name(n));
                            }
                        }
                    }
                } else {
                    // nresults==0 means variable results (multret) — may be consumed
                    // by RETURN, SETLIST, or another CALL.  Emit a local so the call
                    // expression is not duplicated at every use site.
                    // Phase B0.49: classify_write for shadow-on-rename.
                    let new_name = ctx.reg_name(proto, a as u8, pc);
                    let (kind, name) = locals.classify_write(a, &new_name);
                    match kind {
                        WriteKind::FirstDecl | WriteKind::Shadow => {
                            stmts.push(Stat::Local {
                                names: vec![name.clone()],
                                values: vec![call_expr],
                            });
                            // Only a `local` is foldable back into a RETURN:
                            // a Reassign writes a binding that may be read
                            // elsewhere, so it must stay put.
                            pending_multret = Some((a, stmts.len() - 1));
                        }
                        WriteKind::Reassign => {
                            stmts.push(Stat::Assign {
                                targets: vec![Expr::Name(name.clone())],
                                values: vec![call_expr],
                            });
                        }
                    }
                    regs[a] = RegVal::Expr(Expr::Name(name));
                }

                // Clear argument registers AND the entire stack above them after
                // the call completes. The VM resets stack top to (A + nresults)
                // after every CALL, so any register above that range is invalid.
                //
                // Without this, a subsequent vararg CALL that scans from (nextA+1)
                // upward will pick up stale values from:
                //   1. Argument registers of THIS call (e.g., "Gems", 3000)
                //   2. Stale values left in registers above this call's args from
                //      EARLIER calls (e.g., the self-register of a prior NAMECALL)
                //
                // Example bug:
                //   NAMECALL R6 "SetCost" ; R7 = self
                //   LOADK R8 "Gems" ; LOADN R9 3000
                //   CALL R6 args=3 results=1           -- arg_clear cleans R7..R9
                //   NAMECALL R6 "Build" ; R7 = old R6  -- R7 is re-set here
                //   CALL R6 args=1 results=multret     -- Build() runs
                //   NAMECALL R4 "AddPermanentItem"     -- R4 is overwritten
                //   CALL R4 args=vararg                -- scans R5..
                //                                       Without wide clearing,
                //                                       R6 (Build result) is kept
                //                                       but R7,R8,R9 still leak.
                //
                // The fix: after the Build() CALL completes, clear everything
                // ABOVE its result range. result_count=1 for multret (assume 1
                // return value by convention), so we clear regs[a+1..].
                let result_count = match nresults {
                    0 => 1,             // vararg results: assume 1 result by convention
                    1 => 0,             // statement: no captured results
                    n => n - 1,         // n-1 captured result registers starting at a
                };
                let result_end = a + result_count.max(1); // at least reg `a` itself is written (even as statement we hold the call expr there transiently)
                // For statement calls (nresults==1), reg `a` is NOT written back,
                // so clear from `a` onward.
                let clear_start = if nresults == 1 { a } else { result_end };
                for r in clear_start..regs.len() {
                    regs[r] = RegVal::Unknown;
                    explicit_nil_regs.remove(&r);
                }
            }

            LuauOpcode::Return => {
                let mut values = Vec::new();
                if b == 0 {
                    // B=0: return all values from R(A) to top of stack (multret).
                    // Used for `return f()`, `return ...`, and `return a, f()`.
                    //
                    // The last form is why this is not just `reg_expr(regs, a)`:
                    // the compiler puts the fixed values at R(A).. and the
                    // multret-producing CALL at the top, so returning only R(A)
                    // dropped every value above it AND left the call stranded as
                    // a dead `local result = select("#", ...)`.
                    let folded = match multret_from_prev {
                        Some((m, idx)) if m >= a && idx + 1 == stmts.len() => {
                            match &stmts[idx] {
                                Stat::Local { names, values: lv }
                                    if names.len() == 1 && lv.len() == 1 =>
                                {
                                    // The temp must not be referenced anywhere
                                    // else, or removing it would leave a
                                    // dangling name; and it must not be
                                    // duplicated, or a side-effecting call would
                                    // run twice.
                                    let name = names[0].clone();
                                    let referenced_elsewhere = stmts[..idx]
                                        .iter()
                                        .any(|s| stmt_reads_name(s, &name));
                                    if referenced_elsewhere {
                                        None
                                    } else {
                                        Some(lv[0].clone())
                                    }
                                }
                                _ => None,
                            }
                        }
                        _ => None,
                    };

                    if let Some(call_expr) = folded {
                        let (m, idx) = multret_from_prev.unwrap();
                        stmts.remove(idx);
                        // Fixed values sit at R(A)..R(m-1); the multret call is
                        // the final value so it expands at runtime.
                        for i in a..m {
                            values.push(sanitize_leaked_global_string(reg_expr(regs, i)));
                        }
                        values.push(call_expr);
                    } else {
                        // B0.127: sanitize stdlib-name string leakage in return values.
                        let expr = sanitize_leaked_global_string(reg_expr(regs, a));
                        if !matches!(&expr, Expr::Nil) {
                            values.push(expr);
                        }
                        // `return a, ...` puts the fixed values at R(A).. and the
                        // varargs on top; reading only R(A) silently truncated the
                        // tail. Extend only when the contiguous run above R(A) ends
                        // in `Expr::Varargs` — stale registers above the return base
                        // are common, so a general widening would inject garbage.
                        if !values.is_empty() {
                            let mut tail = Vec::new();
                            for r in (a + 1)..regs.len() {
                                match regs.get(r) {
                                    Some(RegVal::Expr(e)) => tail.push(e.clone()),
                                    _ => break,
                                }
                            }
                            if matches!(tail.last(), Some(Expr::Varargs)) {
                                for e in tail {
                                    values.push(sanitize_leaked_global_string(e));
                                }
                            }
                        }
                    }
                } else {
                    let raw_count = b - 1; // B=1 means 0 values, B=2 means 1
                    // Clamp to sane max; very large B is likely corrupted.
                    // Clamp to 1 (not 0) so we don't silently drop everything.
                    let count = if raw_count > 10 { 1 } else { raw_count };
                    for i in 0..count {
                        // B0.127: sanitize stdlib-name string leakage in return values.
                        values.push(sanitize_leaked_global_string(reg_expr(regs, a + i)));
                    }
                }
                // Strip trailing redundant empty return. Luau always emits
                // RETURN 0 1 as the last instruction but real source omits it.
                let is_trailing_empty = values.is_empty() && pc + 1 >= end;
                if !is_trailing_empty {
                    stmts.push(Stat::Return { values });
                }
                break; // Return terminates the current block
            }

            // ── Jumps: these should mostly be handled by the structuring pass ──
            // But we handle simple patterns here as fallback
            LuauOpcode::Jump | LuauOpcode::JumpBack | LuauOpcode::JumpX => {
                let target = match op {
                    LuauOpcode::JumpX => (pc as i32 + e + 1) as usize,
                    _ => (pc as i32 + d as i32 + 1) as usize,
                };
                if target > pc && target <= end {
                    // Forward jump within our range → skip to target (else branch of previous if)
                    pc = target;
                    continue;
                } else if target > pc {
                    // Forward jump out of range
                    if in_loop {
                        // A forward jump that stays INSIDE the loop skips the
                        // rest of the iteration; it is `continue`, not `break`.
                        if ctx.current_loop_end.map_or(false, |le| target < le) {
                            stmts.push(Stat::Continue);
                        } else {
                            stmts.push(Stat::Break);
                        }
                    }
                    // Unconditional jump — remaining instructions are dead code
                    break;
                } else if target < pc {
                    // Backward jump → continue (only valid inside a loop body)
                    if in_loop {
                        stmts.push(Stat::Continue);
                    }
                    // Unconditional backward jump — remaining instructions are dead code
                    break;
                }
            }

            LuauOpcode::JumpIf | LuauOpcode::JumpIfNot => {
                let cond = reg_expr(regs, a);
                let target = (pc as i32 + d as i32 + 1) as usize;

                // `local x = <cond>` compiles to a branch over a LOADB pair.
                // Store the predicate as a value instead of lowering it to
                // control flow (which loses one half of the pair).
                if let Some(idiom) = recognize_bool_idiom(code, pc) {
                    let taken = if op == LuauOpcode::JumpIf {
                        cond
                    } else {
                        Expr::UnOp { op: UnOp::Not, operand: Box::new(cond) }
                    };
                    let value = if idiom.taken_value {
                        taken
                    } else {
                        Expr::UnOp { op: UnOp::Not, operand: Box::new(taken) }
                    };
                    let value = simplify_expr(&value);
                    store_complex(ctx, proto, regs, locals, stmts, idiom.dest, pc, value);
                    pc = idiom.end_pc;
                    continue;
                }

                // `a or b or c` / `a and b and c` merge their operands in one
                // register. Rebuild the expression rather than lowering it to
                // control flow, which loses every operand but the first.
                if let Some(chain) = recognize_or_and_chain(code, pc) {
                    let mut acc = reg_expr(regs, chain.dest);
                    let mut ok = true;
                    for &(seg_start, seg_end) in &chain.segments {
                        let mut seg_stmts = Vec::new();
                        lift_instruction_range(
                            ctx, proto, proto_index, depth + 1,
                            seg_start, seg_end,
                            regs, locals, &mut seg_stmts, in_loop,
                        );
                        let operand = match seg_stmts.len() {
                            // Pure value computation — read it straight back.
                            0 => reg_expr(regs, chain.dest),
                            // The operand was materialized as a temp; fold the
                            // temp away so the short-circuit stays an expression.
                            1 => match &seg_stmts[0] {
                                Stat::Local { names, values }
                                    if names.len() == 1 && values.len() == 1
                                        && matches!(regs.get(chain.dest),
                                            Some(RegVal::Expr(Expr::Name(n))) if *n == names[0]) =>
                                {
                                    values[0].clone()
                                }
                                _ => { ok = false; break; }
                            },
                            // Anything else has side effects we cannot inline.
                            _ => { ok = false; break; }
                        };
                        acc = Expr::BinOp {
                            left: Box::new(acc),
                            op: if chain.is_or { BinOp::Or } else { BinOp::And },
                            right: Box::new(operand),
                        };
                    }
                    if ok {
                        let acc = simplify_expr(&acc);
                        store_complex(ctx, proto, regs, locals, stmts, chain.dest, pc, acc);
                        pc = chain.end_pc;
                        continue;
                    }
                }

                // A jump that reaches the END of the enclosing loop leaves it,
                // even when it is still inside the current lift range. Treating
                // it as an inline if produced a guard with no else, so a failing
                // guard just fell through and the loop never terminated.
                let exits_loop = ctx.current_loop_end.map_or(false, |le| target >= le);
                if target > pc && target <= end && !exits_loop {
                    // Forward jump within our range → inline if-then block
                    // The condition SKIPS to target, so the "then" body is the code between
                    // JumpIfNot condition means: if NOT cond, skip to target
                    // So the "then" body runs when cond IS true (JumpIfNot) or NOT true (JumpIf)
                    let condition = if op == LuauOpcode::JumpIfNot {
                        cond // JumpIfNot: skip when false, so then-body runs when true
                    } else {
                        Expr::UnOp { op: UnOp::Not, operand: Box::new(cond) } // JumpIf: skip when true
                    };
                    // Lift the guarded block with isolated register state so
                    // assignments inside the then-body don't corrupt the
                    // fall-through path's register view.
                    let regs_snapshot = regs.clone();
                    let jump_next = pc + 1 + if op.has_aux() { 1 } else { 0 };

                    // If/else diamond: see the JumpXEqK* arm below.
                    let (then_end, else_range) =
                        detect_else_skip(code, jump_next, target, end);

                    let mut then_stmts = Vec::new();
                    lift_instruction_range(ctx, proto, proto_index, depth + 1, jump_next, then_end, regs, locals, &mut then_stmts, in_loop);

                    let else_body = if let Some((es, ee)) = else_range {
                        *regs = regs_snapshot.clone();
                        let mut else_stmts = Vec::new();
                        lift_instruction_range(ctx, proto, proto_index, depth + 1, es, ee, regs, locals, &mut else_stmts, in_loop);
                        Some(else_stmts)
                    } else {
                        None
                    };

                    *regs = regs_snapshot;
                    stmts.push(Stat::If {
                        condition,
                        then_body: then_stmts,
                        elseif_clauses: vec![],
                        else_body,
                    });
                    // Skip past the instructions we just lifted
                    pc = else_range.map_or(target, |(_, ee)| ee);
                    continue;
                } else if target > pc {
                    // Forward jump beyond our range (or out of the loop)
                    if in_loop {
                        let condition = if op == LuauOpcode::JumpIf {
                            cond
                        } else {
                            Expr::UnOp { op: UnOp::Not, operand: Box::new(cond) }
                        };
                        stmts.push(Stat::If {
                            condition,
                            then_body: vec![if ctx.current_loop_end.map_or(false, |le| target < le) {
                                Stat::Continue
                            } else {
                                Stat::Break
                            }],
                            elseif_clauses: vec![],
                            else_body: None,
                        });
                    }
                    // In non-loop context, out-of-range forward jumps are skipped
                } else {
                    // Backward conditional jump → if ... then continue (only in loops)
                    let condition = if op == LuauOpcode::JumpIf {
                        cond
                    } else {
                        Expr::UnOp { op: UnOp::Not, operand: Box::new(cond) }
                    };
                    if in_loop {
                        stmts.push(Stat::If {
                            condition,
                            then_body: vec![Stat::Continue],
                            elseif_clauses: vec![],
                            else_body: None,
                        });
                    }
                }
            }

            LuauOpcode::JumpIfEq | LuauOpcode::JumpIfNotEq
            | LuauOpcode::JumpIfLE | LuauOpcode::JumpIfNotLE
            | LuauOpcode::JumpIfLT | LuauOpcode::JumpIfNotLT => {
                let left = reg_expr(regs, a);
                let aux_reg = (aux.unwrap_or(0) & 0xFF) as usize;
                let right = reg_expr(regs, aux_reg);
                // The jump skips when condition IS true, so the "then" body is the negated case
                let cmp = match op {
                    LuauOpcode::JumpIfEq => BinOp::Eq,
                    LuauOpcode::JumpIfNotEq => BinOp::NotEq,
                    LuauOpcode::JumpIfLE => BinOp::LE,
                    LuauOpcode::JumpIfNotLE => BinOp::GT,
                    LuauOpcode::JumpIfLT => BinOp::LT,
                    LuauOpcode::JumpIfNotLT => BinOp::GE,
                    _ => BinOp::Eq,
                };
                let neg_cmp = match cmp {
                    BinOp::Eq => BinOp::NotEq,
                    BinOp::NotEq => BinOp::Eq,
                    BinOp::LT => BinOp::GE,
                    BinOp::GE => BinOp::LT,
                    BinOp::LE => BinOp::GT,
                    BinOp::GT => BinOp::LE,
                    other => other,
                };
                let condition = Expr::BinOp { left: Box::new(left.clone()), op: cmp, right: Box::new(right.clone()) };
                let target = (pc as i32 + d as i32 + 1) as usize;

                // `local x = a < b` compiles to a branch over a LOADB pair —
                // store the comparison as a value, not as control flow.
                if let Some(idiom) = recognize_bool_idiom(code, pc) {
                    let value = if idiom.taken_value {
                        condition
                    } else {
                        Expr::BinOp { left: Box::new(left), op: neg_cmp, right: Box::new(right) }
                    };
                    store_complex(ctx, proto, regs, locals, stmts, idiom.dest, pc, value);
                    pc = idiom.end_pc;
                    continue;
                }

                // See the JumpIf arm: a jump reaching the end of the enclosing
                // loop exits it, even when still inside the current lift range.
                let exits_loop = ctx.current_loop_end.map_or(false, |le| target >= le);
                if target > pc && target <= end && !exits_loop {
                    // Forward jump within range → inline if-then block
                    // Isolate register state so then-body doesn't corrupt fall-through.
                    let guard_cond = Expr::BinOp { left: Box::new(left), op: neg_cmp, right: Box::new(right) };
                    let jump_next = pc + 2; // comparison jumps have AUX word

                    // If/else diamond: see the JumpXEqK* arm below. Without this
                    // the compiler's else-skip JUMP falls through to the
                    // out-of-range handler and becomes a spurious `break`.
                    let (then_end, else_range) =
                        detect_else_skip(code, jump_next, target, end);

                    // Name any parked literal this branch overwrites and later
                    // code reads, BEFORE snapshotting — otherwise the restore
                    // below annihilates the branch's result and the join silently
                    // reads the stale pre-branch literal.
                    let construct_end = else_range.map_or(target, |(_, ee)| ee);
                    premateralize_branch_escapes(
                        ctx, proto, regs, locals, stmts,
                        jump_next, construct_end, construct_end, end, pc,
                    );

                    // Isolate register state so then-body doesn't corrupt fall-through.
                    let regs_snapshot = regs.clone();

                    let mut then_stmts = Vec::new();
                    lift_instruction_range(ctx, proto, proto_index, depth + 1, jump_next, then_end, regs, locals, &mut then_stmts, in_loop);

                    let else_body = if let Some((es, ee)) = else_range {
                        *regs = regs_snapshot.clone();
                        let mut else_stmts = Vec::new();
                        lift_instruction_range(ctx, proto, proto_index, depth + 1, es, ee, regs, locals, &mut else_stmts, in_loop);
                        Some(else_stmts)
                    } else {
                        None
                    };

                    *regs = regs_snapshot;
                    stmts.push(Stat::If {
                        condition: guard_cond,
                        then_body: then_stmts,
                        elseif_clauses: vec![],
                        else_body,
                    });
                    pc = else_range.map_or(target, |(_, ee)| ee);
                    continue;
                } else if target <= pc {
                    if in_loop {
                        stmts.push(Stat::If {
                            condition,
                            then_body: vec![Stat::Continue],
                            elseif_clauses: vec![],
                            else_body: None,
                        });
                    }
                } else {
                    if in_loop {
                        stmts.push(Stat::If {
                            condition,
                            then_body: vec![if ctx.current_loop_end.map_or(false, |le| target < le) {
                                Stat::Continue
                            } else {
                                Stat::Break
                            }],
                            elseif_clauses: vec![],
                            else_body: None,
                        });
                    }
                }
            }

            LuauOpcode::JumpXEqKNil | LuauOpcode::JumpXEqKB
            | LuauOpcode::JumpXEqKN | LuauOpcode::JumpXEqKS => {
                let left = reg_expr(regs, a);
                let aux_val = aux.unwrap_or(0);
                let negated = aux_val & 0x80000000 != 0;

                let right = match op {
                    LuauOpcode::JumpXEqKNil => Expr::Nil,
                    LuauOpcode::JumpXEqKB => Expr::Bool((aux_val & 1) != 0),
                    _ => {
                        let kidx = aux_val & 0x00FFFFFF;
                        get_const_expr(proto, &ctx.chunk.strings, kidx)
                    }
                };

                let cmp = if negated { BinOp::NotEq } else { BinOp::Eq };
                let neg_cmp = if negated { BinOp::Eq } else { BinOp::NotEq };
                let condition = Expr::BinOp { left: Box::new(left.clone()), op: cmp, right: Box::new(right.clone()) };
                let target = (pc as i32 + d as i32 + 1) as usize;

                // `local x = a < b` compiles to a branch over a LOADB pair —
                // store the comparison as a value, not as control flow.
                if let Some(idiom) = recognize_bool_idiom(code, pc) {
                    let value = if idiom.taken_value {
                        condition
                    } else {
                        Expr::BinOp { left: Box::new(left), op: neg_cmp, right: Box::new(right) }
                    };
                    store_complex(ctx, proto, regs, locals, stmts, idiom.dest, pc, value);
                    pc = idiom.end_pc;
                    continue;
                }

                // See the JumpIf arm: a jump reaching the end of the enclosing
                // loop exits it, even when still inside the current lift range.
                let exits_loop = ctx.current_loop_end.map_or(false, |le| target >= le);
                if target > pc && target <= end && !exits_loop {
                    // Forward jump within range → inline if-then block
                    // Isolate register state so then-body doesn't corrupt fall-through.
                    let regs_snapshot = regs.clone();
                    let guard_cond = Expr::BinOp { left: Box::new(left), op: neg_cmp, right: Box::new(right) };
                    let jump_next = pc + 2; // these have AUX word

                    // If/else diamond: when the then-range's LAST instruction is
                    // an unconditional forward JUMP past `target`, that jump is
                    // the else-skip and `[target, jump_target)` is the else body.
                    // Without this the jump falls through to the out-of-range
                    // handler and becomes a spurious `break` inside loops, and
                    // the else body is lifted as if it were unconditional.
                    let (then_end, else_range) =
                        detect_else_skip(code, jump_next, target, end);

                    let mut then_stmts = Vec::new();
                    lift_instruction_range(ctx, proto, proto_index, depth + 1, jump_next, then_end, regs, locals, &mut then_stmts, in_loop);

                    let else_body = if let Some((es, ee)) = else_range {
                        *regs = regs_snapshot.clone();
                        let mut else_stmts = Vec::new();
                        lift_instruction_range(ctx, proto, proto_index, depth + 1, es, ee, regs, locals, &mut else_stmts, in_loop);
                        Some(else_stmts)
                    } else {
                        None
                    };

                    *regs = regs_snapshot;
                    stmts.push(Stat::If {
                        condition: guard_cond,
                        then_body: then_stmts,
                        elseif_clauses: vec![],
                        else_body,
                    });
                    pc = else_range.map_or(target, |(_, ee)| ee);
                    continue;
                } else if target <= pc {
                    if in_loop {
                        stmts.push(Stat::If {
                            condition,
                            then_body: vec![Stat::Continue],
                            elseif_clauses: vec![],
                            else_body: None,
                        });
                    }
                } else {
                    if in_loop {
                        stmts.push(Stat::If {
                            condition,
                            then_body: vec![if ctx.current_loop_end.map_or(false, |le| target < le) {
                                Stat::Continue
                            } else {
                                Stat::Break
                            }],
                            elseif_clauses: vec![],
                            else_body: None,
                        });
                    }
                }
            }

            // For-loop ops are handled at the region level
            LuauOpcode::ForNPrep | LuauOpcode::ForNLoop
            | LuauOpcode::ForGPrep | LuauOpcode::ForGLoop
            | LuauOpcode::ForGPrepINext | LuauOpcode::ForGPrepNext
            | LuauOpcode::Deprecated61 => {}

            // Fastcalls: record the builtin ID so the next CALL uses it
            LuauOpcode::FastCall => {
                pending_fastcall = Some((a as u8, c));
            }
            LuauOpcode::FastCall1 => {
                pending_fastcall = Some((a as u8, c));
            }
            LuauOpcode::FastCall2 => {
                // AUX: second arg register
                pending_fastcall = Some((a as u8, c));
            }
            LuauOpcode::FastCall2K => {
                // AUX: constant index for second arg
                pending_fastcall = Some((a as u8, c));
            }
            LuauOpcode::FastCall3 => {
                // AUX: arg2 + arg3 packed (arg2 in low byte, arg3 in next byte)
                pending_fastcall = Some((a as u8, c));
            }

            LuauOpcode::GetVarargs => {
                // GETVARARGS A B: store B-1 vararg values into R(A)..R(A+B-2)
                // B=0 means store all varargs starting at A
                //
                // LEGALITY GUARD — a non-vararg proto cannot have varargs.
                //
                // `proto.is_vararg` is read from the bytecode header
                // (parser/mod.rs:239), so it is authoritative: it comes from the
                // file rather than from opcode inference. If a proto whose flag
                // is false appears to execute GETVARARGS, the DECODE is wrong —
                // most often a byte assigned by bijection completion rather than
                // pinned by a detector.
                //
                // Emitting `...` anyway turns a recoverable mis-decode into
                // source that cannot compile at all:
                //
                //     local function HasCapacity()            -- params=0, not vararg
                //         local service = game:GetService(...)
                //     SyntaxError: Cannot use '...' outside of a vararg function
                //
                // Measured on a 628-chunk corpus: 53 chunks emitted illegal `...`,
                // SpiritBearInit alone 570 times, and that single class was 40 of
                // the 66 files that failed to compile.
                //
                // Marking the register Unknown keeps the mis-decode visible to the
                // semantic checks instead of laundering it into valid-looking
                // syntax. That is deliberate: a defect the tooling can see beats
                // one it cannot.
                if !proto.is_vararg {
                    regs[a] = RegVal::Unknown;
                    pc += 1;
                    continue;
                }
                let count = if b == 0 { 1 } else { b.saturating_sub(1).max(1) };
                if count == 1 {
                    regs[a] = RegVal::Expr(Expr::Varargs);
                } else {
                    // Multi-value varargs: emit `local v0, v1, v2 = ...`
                    let mut names = Vec::new();
                    for i in 0..count.min(10) {
                        if a + i >= regs.len() { break; }
                        names.push(ctx.reg_name(proto, (a + i) as u8, pc));
                    }
                    // Check if ANY vararg register needs a local declaration
                    let any_new = (0..names.len()).any(|i| !locals.declared.contains(&(a + i)));
                    if any_new {
                        for i in 0..names.len() {
                            locals.pre_declare(a + i);
                            // Phase B0.49: keep current_names in sync so future
                            // single-reg writes to these regs can shadow on
                            // semantic-rename.
                            locals.record_name(a + i, &names[i]);
                        }
                        stmts.push(Stat::Local {
                            names: names.clone(),
                            values: vec![Expr::Varargs],
                        });
                    } else {
                        let targets: Vec<Expr> = names.iter().map(|n| Expr::Name(n.clone())).collect();
                        stmts.push(Stat::Assign { targets, values: vec![Expr::Varargs] });
                    }
                    for (i, n) in names.into_iter().enumerate() {
                        if a + i < regs.len() {
                            regs[a + i] = RegVal::Expr(Expr::Name(n));
                        }
                    }
                }
            }

            // ── Roblox native bitwise operators (canonical 84-91) ──
            // B0.73: self-same passthrough guards for bitwise ops.
            // SHL: 11 corpus hits (Cart_Escape Rotation, PathDisplay, HatchEgg) — all nonsensical.
            // SHR: 18 corpus hits (HatchEgg CFrame/SetVolume, DreamerBlessing) — all nonsensical.
            // BAND/BOR/BXOR: 3/1/0 hits but apply same Roblox passthrough pattern.
            // B0.73: BAND/BOR passthrough when B==C (x & x = x, x | x = x).
            LuauOpcode::Band => {
                if b == c {
                    let val = regs.get(b).cloned().unwrap_or(RegVal::Unknown);
                    if a < regs.len() { regs[a] = val; }
                } else {
                    let e = mk_binop(regs, b, c, BinOp::BAnd); store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
                }
            }
            LuauOpcode::Bor => {
                if b == c {
                    let val = regs.get(b).cloned().unwrap_or(RegVal::Unknown);
                    if a < regs.len() { regs[a] = val; }
                } else {
                    let e = mk_binop(regs, b, c, BinOp::BOr); store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
                }
            }
            LuauOpcode::Bxor => { if a == b && b == c {} else { let e = mk_binop(regs, b, c, BinOp::BXor); store_complex(ctx, proto, regs, locals, stmts, a, pc, e); } }
            // B0.75: SHL/SHR promoted to passthrough like BNOT.
            // Evidence: 601 vN<<vN self-ops + `v1 << v0:GetAttribute(v2)`,
            // `service2 << service2`, `v7 << "task"` — 100% misidentified.
            // Real SHL/SHR never appears in Roblox game bytecode.
            // B==C: MOVE passthrough (same as BAND/BOR).
            // B!=C: passthrough left operand (B) to suppress garbage.
            LuauOpcode::Shl => {
                let val = regs.get(b).cloned().unwrap_or(RegVal::Unknown);
                if a < regs.len() { regs[a] = val; }
            }
            LuauOpcode::Shr => {
                let val = regs.get(b).cloned().unwrap_or(RegVal::Unknown);
                if a < regs.len() { regs[a] = val; }
            }
            LuauOpcode::Bnot => {
                // B0.73: Roblox repurposed standard BNOT as passthrough
                // (same pattern as NOT/B0.70). Evidence (213 corpus hits):
                //   `~v0 > NUM_SECRETS_REQUIRED` — bnot before comparison
                //   `(~v0 - 2).left` — bnot then field access
                //   `(~v1)(v5, true)` — calling bnot result as function
                //   `-(~v0)` — double negation chain
                //   `return ~v1` — returning passthrough
                // No RBX_EXT slot for real BNOT identified — bitwise NOT
                // appears absent from this Roblox game's bytecode.
                let val = regs.get(b as usize).cloned().unwrap_or(RegVal::Unknown);
                if a < regs.len() {
                    regs[a] = val;
                }
            }
            LuauOpcode::Bandk => { let e = mk_binop_k(proto, &ctx.chunk.strings, regs, b, c as u32, BinOp::BAnd); store_complex(ctx, proto, regs, locals, stmts, a, pc, e); }
            LuauOpcode::Bork  => { let e = mk_binop_k(proto, &ctx.chunk.strings, regs, b, c as u32, BinOp::BOr);  store_complex(ctx, proto, regs, locals, stmts, a, pc, e); }

            // Roblox-specific extensions beyond canonical 91 — exact semantics unknown.
            // Emit as placeholder calls so output is at least syntactically valid.

            LuauOpcode::RbxExt93 | LuauOpcode::RbxExt94 | LuauOpcode::RbxExt98 => {
                // RBX_EXT_93/94/98: transparent passthrough / type annotation.
                // B0.69 evidence:
                //  93 — output register frequently immediately overwritten; module
                //       exit `return __rbx93(v0)` is semantically `return v0`.
                //  94 — corpus-wide pattern: always A=B=0, sandwiched between
                //       unrelated BANDK+LOADK pairs in Runes.lua (56 hits).
                //  98 — same A=B=0 pattern between BAND+DUPTABLE (25 hits).
                // Treatment: propagate B's register value to A, emit no code.
                let val = regs.get(b as usize).cloned().unwrap_or(RegVal::Unknown);
                if a < regs.len() {
                    regs[a] = val;
                }
            }

            // B0.63: RBX_EXT_96 decoded as Luau `not` (UnOp::Not) based on
            // ground-truth A/B. Source: `local notx = not x` → bytecode emits
            // RBX_EXT_96 with shape (A=dest, B=src). Previously emitted as
            // `__rbx_unary96(x)` placeholder.
            LuauOpcode::RbxExt96 => {
                // B0.67: route through mk_unop. `Not` accepts any operand.
                // Phase C4: guard against `not <function>` — in Luau `not f`
                // on a function value is always `false`, a compiler would
                // never emit this, so it signals opmap corruption.
                if let Some(RegVal::Expr(Expr::Function { .. })) = regs.get(b as usize) {
                    stmts.push(Stat::Comment(format!(
                        "-- lifter error: NOT on Function (RbxExt96)  raw_opcode=0x{:08x}",
                        insn
                    )));
                }
                let e = mk_unop(regs, b as usize, UnOp::Not);
                store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
            }

            // B0.69: RBX_EXT_97 decoded as duplicate GETVARARGS.
            // Evidence: Signal.lua Fire(self, ...) disassembly shows
            //   PREPVARARGS nfixed=1 → RBX_EXT_97 A=3 B=0 C=0 → NAMECALL → CALL vararg
            // A=target register, B=count+1 (0=all), exactly matching standard GETVARARGS.
            // Proto is vararg=true and there is no standard GETVARARGS in the instruction stream.
            LuauOpcode::RbxExt97 => {
                // Same legality guard as GETVARARGS — see that handler for the
                // full reasoning. Note the comment directly above ASSERTS "Proto
                // is vararg=true" as part of the evidence for this decode, but
                // nothing ever checked it. When the assumption does not hold, the
                // decode is wrong and `...` here cannot compile.
                if !proto.is_vararg {
                    regs[a] = RegVal::Unknown;
                    pc += 1;
                    continue;
                }
                // Reuse exact logic from LuauOpcode::GetVarargs handler.
                let count = if b == 0 { 1 } else { b.saturating_sub(1).max(1) };
                if count == 1 {
                    regs[a] = RegVal::Expr(Expr::Varargs);
                } else {
                    let mut names = Vec::new();
                    for i in 0..count.min(10) {
                        if a + i >= regs.len() { break; }
                        names.push(ctx.reg_name(proto, (a + i) as u8, pc));
                    }
                    let any_new = (0..names.len()).any(|i| !locals.declared.contains(&(a + i)));
                    if any_new {
                        for i in 0..names.len() {
                            locals.pre_declare(a + i);
                            locals.record_name(a + i, &names[i]);
                        }
                        stmts.push(Stat::Local {
                            names: names.clone(),
                            values: vec![Expr::Varargs],
                        });
                    }
                    for i in 0..names.len() {
                        regs[a + i] = RegVal::Expr(Expr::Name(names[i].clone()));
                    }
                }
            }

            LuauOpcode::RbxExt92 => {
                // B0.73: RBX_EXT_92 decoded as passthrough (type annotation).
                // Evidence (7 corpus hits):
                //   VehicleCamera.lua:   self = __rbx_unary92(self) → passthrough
                //   arrow.lua:           __rbx_unary92(#self.rects) → wraps length
                //   EventBoard.lua (×2): __rbx_unary92("game") → wraps string
                //   Egg_Darts/Hyper_Darts: __rbx_unary92(v0) → wraps register
                // All occurrences wrap a single value and return it unchanged.
                let val = regs.get(b as usize).cloned().unwrap_or(RegVal::Unknown);
                if a < regs.len() {
                    regs[a] = val;
                }
            }

            // B0.63: RBX_EXT_99 decoded as Luau `and` (BinOp::And) based on
            // ground-truth A/B. Source: `local landy = x and y` → RBX_EXT_99.
            LuauOpcode::RbxExt99 => {
                if b == c {
                    let val = regs.get(b).cloned().unwrap_or(RegVal::Unknown);
                    if a < regs.len() { regs[a] = val; }
                } else {
                    // B0.126: use mk_binop for And/Or string leakage guard.
                    let e = mk_binop(regs, b, c, BinOp::And);
                    store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
                }
            }

            // B0.63: RBX_EXT_95 decoded as Luau `or` (BinOp::Or) based on
            // ground-truth A/B. Source: `local lory = x or y` → RBX_EXT_95.
            LuauOpcode::RbxExt95 => {
                if b == c {
                    let val = regs.get(b).cloned().unwrap_or(RegVal::Unknown);
                    if a < regs.len() { regs[a] = val; }
                } else {
                    // B0.126: use mk_binop for And/Or string leakage guard.
                    let e = mk_binop(regs, b, c, BinOp::Or);
                    store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
                }
            }

            // B0.69: RBX_EXT_102 decoded as duplicate ORK (R(A) = R(B) or K(C)).
            // Evidence: getPaddingObject.lua (stack=4, C=8 > stack → constant index,
            // K8=0) produces `padding.top or 0`. RoundFloatingPoint.lua (stack=9,
            // C=14 > stack, K14=0) produces `tonumber(...) or 0`. Both confirmed
            // via in-game bytecode disassembly.
            LuauOpcode::RbxExt102 => {
                let e = mk_binop_k(proto, &ctx.chunk.strings, regs, b, c as u32, BinOp::Or);
                store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
            }

            // B0.69b: RBX_EXT_103 decoded as duplicate IDIV (R(A) = R(B) // R(C)).
            // Evidence: PresentRain.lua time formatting —
            //   string.format("%02i:%02i:%02i", __rbx_binary103(result, v8), ...)
            //   where result=seconds, v8=3600 → hours = seconds // 3600.
            // DailyPerkUtil.lua weekday calculation —
            //   __rbx_binary103(timestamp, secondsPerDay) → day number.
            // SpecialEventUtil.lua —
            //   __rbx_binary103(adjusted_time, period) → period index.
            LuauOpcode::RbxExt103 => {
                if a == b && b == c {
                    // B0.71: self-idiv passthrough guard (same as standard IDiv)
                } else {
                    let e = mk_binop(regs, b, c, BinOp::IDiv);
                    store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
                }
            }

            // B0.69b: RBX_EXT_104 decoded as duplicate MOD (R(A) = R(B) % R(C)).
            // Evidence: GlobalIncentive.lua —
            //   timestamp % secondsInHour → seconds within current hour.
            // VirtualGrid.lua —
            //   v0 % v1; if v0 == 0 then — divisibility check for grid layout.
            // QuestHUDTask.lua —
            //   result % totalTasks / totalTasks → normalized progress fraction.
            // Spinner.lua —
            //   self.Offset % period → animation offset wrapping.
            LuauOpcode::RbxExt104 => {
                if a == b && b == c {
                    // B0.71: self-mod passthrough guard (same as standard Mod)
                } else {
                    let e = mk_binop(regs, b, c, BinOp::Mod);
                    store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
                }
            }

            // B0.73: RBX_EXT_100 decoded as duplicate POW (R(A) = R(B) ^ R(C)).
            // Evidence:
            //   GetRepeatableTasks.lua: __rbx_binary100(Seed2, v0.Difficulty.Exponent)
            //     — field named "Exponent" → Seed ^ Exponent.
            //   ExperienceUtil.lua: __rbx_binary100(v3, arg3.Power)
            //     — field named "Power" → base ^ power.
            //   GetValueOnCurve.lua: __rbx_binary100(arg1, arg4)
            //     — curve evaluation commonly uses exponentiation.
            //   CameraUtils.lua: __rbx_binary100(v3, arg2) — camera math.
            LuauOpcode::RbxExt100 => {
                let e = mk_binop(regs, b, c, BinOp::Pow);
                store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
            }

            // B0.73: RBX_EXT_101 decoded as duplicate AND (R(A) = R(B) and R(C)).
            // Evidence:
            //   BaseCamera.lua: __rbx_binary101(Head, new2):IsA("BasePart")
            //     — Head and new2, chained with method → returns Instance if both truthy.
            //   ItemShop.lua: __rbx_binary101(true, true) → true and true = true.
            //   TradingTerminal.lua: __rbx_binary101(v4, true) → v4 and true.
            //   RichText.lua: __rbx_binary101(match1, match2) → first and second match.
            //   Specials.lua: __rbx_binary101(self:GetAttribute(v6), arg3) → attr and arg3.
            LuauOpcode::RbxExt101 => {
                if b == c {
                    let val = regs.get(b).cloned().unwrap_or(RegVal::Unknown);
                    if a < regs.len() { regs[a] = val; }
                } else {
                    let e = Expr::BinOp {
                        left: Box::new(reg_expr(regs, b)),
                        op: BinOp::And,
                        right: Box::new(reg_expr(regs, c)),
                    };
                    store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
                }
            }

            // B0.73: RBX_EXT_105 decoded as duplicate MOD (R(A) = R(B) % R(C)).
            // Evidence:
            //   PresentRainUtil.lua: return __rbx_binary105(now2, SPAWN_EVERY)
            //     — time remaining until next spawn = now % interval.
            //   NewYearsUtil.lua: return __rbx_binary105(now3, BallDropInterval2)
            //     — time within current interval = now % interval.
            //   Leaderboards_CLIENT.lua: __rbx_binary105(UnixTimestamp - offset, Interval)
            //     — time modulo period.
            //   RuneUtil.lua: math.min(v13, __rbx_binary105(result, result2)) — mod result clamped.
            LuauOpcode::RbxExt105 => {
                let e = mk_binop(regs, b, c, BinOp::Mod);
                store_complex(ctx, proto, regs, locals, stmts, a, pc, e);
            }

            LuauOpcode::Unknown => {
                // Unknown opcode — mark register A as unknown without polluting
                // output with noisy variable names. Use RegVal::Unknown so downstream
                // consumers get "nil" rather than "_unk_XX_PC" garbage.
                if a < regs.len() {
                    regs[a] = RegVal::Unknown;
                }

                // Heuristic: skip likely AUX words following this unknown opcode.
                // Many Luau opcodes use AUX words. Signals we use:
                //  - next word looks like a constant index (small value into proto.constants)
                //  - next word looks like a packed import ID (count<<30 | ids)
                //  - next word is a small value plausible as table-size / var-count AUX
                //  - next word matches a comparison AUX shape (low byte < maxstack, rest zero)
                //
                // Prior versions had an aggressive "next word is also Unknown → definitely
                // AUX" branch which consumed adjacent unary ops (consecutive unmapped
                // MINUS/LENGTH/NOT in e.g. ground_truth_module.lua's `logical` function
                // got eaten in pairs). We now refuse to skip when the next word's ABC
                // decoding looks like a real unary instruction (C=0, A<ms, B<ms, A≠B).
                if pc + 1 < code.len() {
                    let next_word = code[pc + 1];

                    // Shape signals (evaluated regardless of next_op mapping).
                    let looks_like_const_idx = (next_word as usize) < proto.constants.len()
                        && matches!(proto.constants.get(next_word as usize),
                            Some(Constant::String(_)) | Some(Constant::Number(_)) | Some(Constant::Import(_)));
                    let looks_like_import = {
                        let count = next_word >> 30;
                        count >= 1 && count <= 3
                            && ((next_word >> 20) & 0x3FF) < proto.constants.len() as u32
                    };
                    let looks_like_small_aux = next_word <= 256;
                    let looks_like_cmp_aux = {
                        let reg = (next_word & 0xFF) as u8;
                        reg < proto.max_stack_size && (next_word >> 8) == 0
                    };

                    // The next word's ABC decode — if it matches a plausible instruction
                    // shape (especially unary), treat it as a real instruction, not AUX.
                    let n_a = ((next_word >> 8) & 0xFF) as u8;
                    let n_b = ((next_word >> 16) & 0xFF) as u8;
                    let n_c = ((next_word >> 24) & 0xFF) as u8;
                    let next_looks_like_unary = n_c == 0
                        && (n_a as u16) < proto.max_stack_size as u16
                        && (n_b as u16) < proto.max_stack_size as u16
                        && n_a != n_b;

                    // Also require the instruction AFTER the suspected AUX to be valid
                    // (or we're at end-of-code). This avoids cascading PC desync.
                    let next_next_valid = if pc + 2 < code.len() {
                        let nn_op = LuauOpcode::from_u8(insn_op(code[pc + 2]));
                        !matches!(nn_op, LuauOpcode::Unknown)
                    } else {
                        looks_like_const_idx || looks_like_import
                    };

                    let should_skip = !next_looks_like_unary
                        && next_next_valid
                        && (looks_like_const_idx || looks_like_import
                            || looks_like_small_aux || looks_like_cmp_aux);

                    if should_skip {
                        pc += 1; // Skip the AUX word
                    }
                }
            }
        }

        pc += 1;
        if op.has_aux() { pc += 1; }
    }
    // Final-tick accounting: catch any statements pushed during the last
    // instruction that we otherwise would not charge against the budget.
    let delta = stmts.len().saturating_sub(last_len);
    if delta > 0 {
        note_stmts_pushed(stmts, delta);
    }
}
