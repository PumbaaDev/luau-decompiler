//! The lifter: converts bytecode into AST statements using CFG-based
//! control flow structuring. This is the core intelligence of the decompiler.

use std::collections::{HashMap, HashSet};

use crate::analysis::cfg::ControlFlowGraph;
use crate::analysis::structuring::{structure_control_flow, Region};
use crate::ast::*;
use crate::decompiler::{analyze_register_usage, constant_to_expr, is_stdlib_shadow_name, DecompileContext};
use crate::parser::opcodes::LuauOpcode;
use crate::parser::types::*;

mod naming;
use naming::{WriteKind, LocalTracker, is_semantic_local_name};
pub(crate) use naming::RegVal;

mod table_reconstruction;
use table_reconstruction::{
    reconstruct_table_constructors,
    coalesce_setlist_sequential,
    is_valid_luau_identifier,
};

mod post_passes;
use post_passes::{
    collapse_elseif_chains,
    collapse_nil_init_conditional,
    collapse_short_circuit_assignments,
    inline_single_use_temps,
    inline_pure_literals,
};

mod opcode_handlers;
use opcode_handlers::lift_instruction_range;

// Re-exported solely for the `#[cfg(test)]` submodules under `lifter/tests/`,
// which reach these helpers via `super::super::<name>`. They are not referenced
// by non-test lifter code, so the re-import is test-gated to keep the regular
// build warning-free.
#[cfg(test)]
use table_reconstruction::{is_pure_two_step_value, two_step_field_absorb};
#[cfg(test)]
use post_passes::{is_inlinable_literal, stmt_writes_name_recursive};

/// Max recursion depth for nested closures to prevent stack overflow.
/// Obfuscated/auto-generated Luau (HUD.lua-class GUI files) nests 20+ deep;
/// the server thread runs with a 256 MB stack so we have headroom.
const MAX_DECOMPILE_DEPTH: usize = 40;

/// Phase C1 stability guard: proto-wide statement budget. Augments the
/// existing per-range safety guards. When a single proto tries to emit
/// more than this many statements we short-circuit the current block with
/// a comment stub instead of continuing to bloat memory. 50,000 statements
/// is well above any legitimate hand-written or compiled Luau proto.
pub(crate) const MAX_STMTS_PER_PROTO: usize = 50_000;

thread_local! {
    /// Running count of statements emitted for the proto currently being
    /// lifted. Reset at the top of every `lift_proto_inner` call. Mutated
    /// through the [`push_stat`] helper (and its in-place twin
    /// [`note_stmts_pushed`]) so the budget is enforced everywhere the
    /// lifter appends AST statements.
    pub(crate) static STMTS_EMITTED: std::cell::Cell<usize> =
        std::cell::Cell::new(0);
    /// True once we have already appended the budget-exceeded sentinel to
    /// the outermost block for this proto. Prevents repeated comments and
    /// marks the lifter as "tripped" so push helpers silently drop further
    /// statements.
    pub(crate) static STMT_BUDGET_TRIPPED: std::cell::Cell<bool> =
        std::cell::Cell::new(false);
}

/// Reset the proto-wide statement counter. Called at the top of every
/// `lift_proto_inner` invocation (including recursive closure lifts — nested
/// closures reuse the main proto's budget intentionally, so if the parent is
/// tripped children short-circuit too).
pub(crate) fn reset_stmt_budget() {
    STMTS_EMITTED.with(|c| c.set(0));
    STMT_BUDGET_TRIPPED.with(|c| c.set(false));
}

/// Returns true if the proto-wide statement budget has been exhausted.
/// Callers can consult this to short-circuit region loops early.
pub(crate) fn stmt_budget_tripped() -> bool {
    STMT_BUDGET_TRIPPED.with(|c| c.get())
}

/// Push a `Stat` into `block`, counting against the proto-wide budget.
/// On the first push that crosses [`MAX_STMTS_PER_PROTO`] the helper
/// substitutes a `Stat::Comment("-- statement budget exceeded")` sentinel
/// so downstream passes can see that truncation occurred. All subsequent
/// calls after tripping are silently dropped, so callers can keep invoking
/// this helper without extra guards — the block simply stops growing.
#[allow(dead_code)]
pub(crate) fn push_stat(block: &mut Vec<Stat>, stat: Stat) {
    if STMT_BUDGET_TRIPPED.with(|c| c.get()) {
        // Already tripped — drop further statements to cap memory.
        return;
    }
    let current = STMTS_EMITTED.with(|c| c.get());
    if current >= MAX_STMTS_PER_PROTO {
        STMT_BUDGET_TRIPPED.with(|c| c.set(true));
        block.push(Stat::Comment("-- statement budget exceeded".to_string()));
        return;
    }
    STMTS_EMITTED.with(|c| c.set(current + 1));
    block.push(stat);
}

/// Record that `count` statements were appended to a block by a path that
/// does not route through [`push_stat`] (e.g., post-passes that extend the
/// vec via `append`). Same trip behaviour as `push_stat`.
pub(crate) fn note_stmts_pushed(block: &mut Vec<Stat>, count: usize) {
    if count == 0 {
        return;
    }
    if STMT_BUDGET_TRIPPED.with(|c| c.get()) {
        return;
    }
    let current = STMTS_EMITTED.with(|c| c.get());
    let new_total = current.saturating_add(count);
    if new_total > MAX_STMTS_PER_PROTO {
        STMT_BUDGET_TRIPPED.with(|c| c.set(true));
        // Truncate block back down to whatever fits, then append sentinel.
        let overshoot = new_total - MAX_STMTS_PER_PROTO;
        let keep = block.len().saturating_sub(overshoot);
        block.truncate(keep);
        block.push(Stat::Comment("-- statement budget exceeded".to_string()));
        STMTS_EMITTED.with(|c| c.set(MAX_STMTS_PER_PROTO));
    } else {
        STMTS_EMITTED.with(|c| c.set(new_total));
    }
}

/// For the main proto (depth==0, no parent), infer upvalue names by scanning
/// bytecode usage patterns. In Roblox scripts, the main proto's upvalues are
/// VM-injected globals: typically `script` (the script instance) and sometimes
/// others. Since there's no parent proto to provide CAPTURE-based inference,
/// we look at how each upvalue is *used* and assign names accordingly.
///
/// Heuristics:
///   - NAMECALL with `:GetService()` on the upval -> "game"
///   - GETTABLEKS with `.Parent`, `.Name`, `.ClassName` -> "script"
///   - GETTABLEKS with `.Client`, `.Shared`, `.Server` (module paths) -> "script"
///   - NAMECALL with Roblox-instance methods (`:WaitForChild`, `:FindFirstChild`,
///     `:Connect`, `:Fire`, `:InvokeServer`, etc.) -> "script" / "signal" / "remote"
///   - `SETGLOBAL R(A), "X"` after `GETUPVAL R(A), U(idx)` -> name upval as "X"
///     (idiom: `_G.MyVar = upval_0` or bare global assignment)
///   - `require(upval)` (where `require` is a known import/global) -> "module"
///   - If none match, leave unnamed (will fall through to `upval_N`)
fn infer_main_proto_upval_names(proto: &Proto, strings: &[String]) -> Vec<String> {
    let num_upvals = proto.num_upvalues as usize;
    if num_upvals == 0 {
        return Vec::new();
    }

    // Collect usage evidence for each upvalue index.
    // We track which field/method names are accessed on each upval,
    // whether it's used as a call target, and whether it has SETTABLEKS writes.
    let mut upval_methods: HashMap<usize, Vec<String>> = HashMap::new();
    let mut upval_fields: HashMap<usize, Vec<String>> = HashMap::new();
    let mut upval_is_called: HashSet<usize> = HashSet::new();
    let mut upval_settable_fields: HashMap<usize, Vec<String>> = HashMap::new();
    // Phase B0.43B additions:
    //   - upval_setglobal_names[i] = list of global names assigned FROM upval i
    //     (from `GETUPVAL R(A), U(i); SETGLOBAL R(A), "name"`)
    //   - upval_is_require_arg[i]  = true if upval i is passed as the sole arg
    //     to a call of a register that was loaded from `require` (GETIMPORT /
    //     GETGLOBAL producing the name "require").
    let mut upval_setglobal_names: HashMap<usize, Vec<String>> = HashMap::new();
    let mut upval_is_require_arg: HashSet<usize> = HashSet::new();

    // Lightweight register-name tracker used for pattern 1 (track which reg
    // currently holds which upval) and pattern 6 (track which reg currently
    // holds the name "require"). `None` = unknown contents.
    //
    // For the upval tracker we store Some(upval_idx) if the register was most
    // recently written by a GETUPVAL; any later overwrite clears it.
    let reg_count = (proto.max_stack_size as usize).max(256);
    let mut reg_holds_upval: Vec<Option<usize>> = vec![None; reg_count];
    // For the `require` tracker we store whether the register currently holds
    // the callable `require` function (as identified by the *name* "require").
    let mut reg_is_require: Vec<bool> = vec![false; reg_count];

    let code = &proto.code;
    let mut pc = 0;
    while pc < code.len() {
        let insn = code[pc];
        let op = LuauOpcode::from_u8(insn_op(insn));
        let a = insn_a(insn) as usize;
        let b = insn_b(insn) as usize;
        let d = insn_d(insn);

        if op == LuauOpcode::GetUpval {
            let dest_reg = a;
            let upval_idx = b;
            if upval_idx < num_upvals {
                // Look ahead at the next non-AUX instruction to see how this upval is used
                let next_pc = pc + 1;
                if next_pc < code.len() {
                    let next_insn = code[next_pc];
                    let next_op = LuauOpcode::from_u8(insn_op(next_insn));

                    match next_op {
                        LuauOpcode::NameCall => {
                            // NAMECALL A B AUX: B is the object register
                            let nc_b = insn_b(next_insn) as usize;
                            if nc_b == dest_reg {
                                // The upval is the object of a method call
                                let aux_pc = next_pc + 1;
                                if aux_pc < code.len() {
                                    let aux = code[aux_pc];
                                    let method = get_method_string_from_aux(proto, strings, aux);
                                    upval_methods.entry(upval_idx).or_default().push(method);
                                }
                            }
                        }
                        LuauOpcode::GetTableKS => {
                            // GETTABLEKS A B AUX: B is the table register
                            let gt_b = insn_b(next_insn) as usize;
                            if gt_b == dest_reg {
                                let aux_pc = next_pc + 1;
                                if aux_pc < code.len() {
                                    let aux = code[aux_pc];
                                    let field = get_table_string_from_aux(proto, strings, aux);
                                    upval_fields.entry(upval_idx).or_default().push(field);
                                }
                            }
                        }
                        LuauOpcode::SetTableKS => {
                            // SETTABLEKS A B AUX: B is the table register
                            let st_b = insn_b(next_insn) as usize;
                            if st_b == dest_reg {
                                let aux_pc = next_pc + 1;
                                if aux_pc < code.len() {
                                    let aux = code[aux_pc];
                                    let field = get_table_string_from_aux(proto, strings, aux);
                                    upval_settable_fields.entry(upval_idx).or_default().push(field);
                                }
                            }
                        }
                        LuauOpcode::Call => {
                            // CALL A B C: A is the function register
                            let call_a = insn_a(next_insn) as usize;
                            if call_a == dest_reg {
                                upval_is_called.insert(upval_idx);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        // ── Pattern 1: SETGLOBAL <- upval ────────────────────────────────
        //
        // Update `reg_holds_upval` so we can recognise
        //   GETUPVAL R(A), U(i)      -- possibly with other ops in between
        //   SETGLOBAL R(A), "MyName" -- still holding upval i
        // and assign "MyName" to upval i.
        //
        // The SETGLOBAL handler also uses `reg_is_require` for pattern 6.
        match op {
            LuauOpcode::GetUpval => {
                if a < reg_count && b < num_upvals {
                    reg_holds_upval[a] = Some(b);
                    reg_is_require[a] = false;
                }
            }
            LuauOpcode::SetGlobal => {
                // SETGLOBAL A D [AUX]: writes R(A) into global K[D].
                // If R(A) was loaded from GETUPVAL with no intervening overwrite,
                // the upval's "natural" name is the global being assigned to.
                if a < reg_count {
                    if let Some(upval_idx) = reg_holds_upval[a] {
                        let aux = code.get(pc + 1).copied();
                        if let Some(name) = resolve_global_name(proto, strings, d, aux) {
                            if upval_idx < num_upvals && is_sane_identifier(&name) {
                                upval_setglobal_names
                                    .entry(upval_idx)
                                    .or_default()
                                    .push(name);
                            }
                        }
                    }
                }
                // SETGLOBAL does NOT write R(A); just note we've seen it (no
                // register invalidation needed).
            }

            // ── Pattern 6: require(upval) ────────────────────────────────
            //
            // Track registers that currently hold the callable `require`.
            // Then watch for `CALL R(f), 2, N` where `R(f+1)` was loaded
            // from GETUPVAL.
            LuauOpcode::GetImport => {
                if a < reg_count {
                    // K[D] is typically Constant::Import; decode id0 to get the
                    // top-level name.
                    let aux_val = code.get(pc + 1).copied();
                    let import_val = aux_val.unwrap_or_else(|| {
                        let d_unsigned = d as u16 as usize;
                        match proto.constants.get(d_unsigned) {
                            Some(Constant::Import(v)) => *v,
                            _ => 0,
                        }
                    });
                    let mut name: Option<String> = None;
                    if import_val != 0 {
                        let ids = decode_import(import_val);
                        if let Some(&id0) = ids.first() {
                            if let Some(Constant::String(s)) = proto.constants.get(id0 as usize) {
                                name = Some(s.clone());
                            } else if let Some(s) = strings.get(id0 as usize) {
                                name = Some(s.clone());
                            }
                        }
                    }
                    reg_is_require[a] = matches!(name.as_deref(), Some("require"));
                    reg_holds_upval[a] = None;
                }
            }
            LuauOpcode::GetGlobal => {
                if a < reg_count {
                    let aux = code.get(pc + 1).copied();
                    let name = resolve_global_name(proto, strings, d, aux);
                    reg_is_require[a] = matches!(name.as_deref(), Some("require"));
                    reg_holds_upval[a] = None;
                }
            }
            LuauOpcode::Call => {
                // CALL A B C: A=func, B=nargs+1 (0=vararg), C=nresults+1
                // Detect `require(R(A+1))` when R(A)=require and B==2 (1 arg).
                let call_a = a;
                if call_a < reg_count
                    && reg_is_require[call_a]
                    && b == 2
                {
                    let arg_reg = call_a + 1;
                    if arg_reg < reg_count {
                        if let Some(upval_idx) = reg_holds_upval[arg_reg] {
                            if upval_idx < num_upvals {
                                upval_is_require_arg.insert(upval_idx);
                            }
                        }
                    }
                }
                // Invalidate result register(s): CALL overwrites R(A)..R(A+nresults-1).
                // We don't need precise tracking here, just a conservative clear
                // so subsequent patterns don't match stale data.
                if call_a < reg_count {
                    reg_holds_upval[call_a] = None;
                    reg_is_require[call_a] = false;
                }
            }

            // Instructions that overwrite R(A) without giving it a meaningful
            // upval/require identity — clear the tracker.
            LuauOpcode::Move
            | LuauOpcode::LoadK | LuauOpcode::LoadN | LuauOpcode::LoadB
            | LuauOpcode::LoadNil | LuauOpcode::LoadKX
            | LuauOpcode::NameCall | LuauOpcode::NewTable | LuauOpcode::NewClosure
            | LuauOpcode::DupClosure | LuauOpcode::DupTable
            | LuauOpcode::Add | LuauOpcode::Sub | LuauOpcode::Mul | LuauOpcode::Div
            | LuauOpcode::Mod | LuauOpcode::Pow | LuauOpcode::Concat
            | LuauOpcode::Length | LuauOpcode::Minus | LuauOpcode::Not
            | LuauOpcode::GetTableKS | LuauOpcode::GetTableN | LuauOpcode::GetTable
            | LuauOpcode::Band | LuauOpcode::Bor | LuauOpcode::Bxor
            | LuauOpcode::Bnot | LuauOpcode::Shl | LuauOpcode::Shr
            | LuauOpcode::Bandk | LuauOpcode::Bork => {
                if a < reg_count {
                    reg_holds_upval[a] = None;
                    reg_is_require[a] = false;
                }
            }

            _ => {}
        }

        // Advance past AUX if needed
        if op.has_aux() {
            pc += 2;
        } else {
            pc += 1;
        }
    }

    // Now decide names based on collected evidence
    let mut names = vec![String::new(); num_upvals];
    for idx in 0..num_upvals {
        let methods = upval_methods.get(&idx);
        let fields = upval_fields.get(&idx);

        // Check for "game" pattern: :GetService() is the definitive signal
        if let Some(m) = methods {
            if m.iter().any(|name| name == "GetService" || name == "FindService") {
                names[idx] = "game".to_string();
                continue;
            }
        }

        // Check for "script" pattern: .Parent, .Name, .ClassName, or module
        // hierarchy fields (.Client, .Shared, .Server)
        if let Some(f) = fields {
            let script_fields = ["Parent", "Name", "ClassName", "Client", "Shared", "Server"];
            if f.iter().any(|name| script_fields.contains(&name.as_str())) {
                names[idx] = "script".to_string();
                continue;
            }
        }

        // Check for Roblox event / remote method NAMECALLs (pattern 5).
        // These are strong signals even without field-access context because
        // these method names are rare outside the Roblox instance API.
        if let Some(m) = methods {
            // Remote-like methods: upval is a RemoteEvent / RemoteFunction
            let remote_methods = [
                "FireServer", "FireClient", "FireAllClients",
                "InvokeServer", "InvokeClient",
            ];
            if m.iter().any(|name| remote_methods.contains(&name.as_str())) {
                names[idx] = "remote".to_string();
                continue;
            }
            // Signal-like methods: upval is a RBXScriptSignal / BindableEvent
            // Note: `Connect`/`Once`/`Wait` also appear on many non-signal
            // objects, but in practice NAMECALL sites for these names are
            // overwhelmingly on signals in real Roblox code.
            let signal_methods = ["Connect", "Once", "Wait", "Fire", "ConnectParallel", "DisconnectAll"];
            if m.iter().any(|name| signal_methods.contains(&name.as_str())) {
                names[idx] = "signal".to_string();
                continue;
            }
        }

        // Check for method patterns suggesting a service or instance
        if let Some(m) = methods {
            // Phase B0.43B: expanded set includes FindFirstAncestor + friends.
            let instance_methods = [
                "WaitForChild", "FindFirstChild", "FindFirstChildOfClass",
                "FindFirstChildWhichIsA", "FindFirstAncestor",
                "FindFirstAncestorOfClass", "FindFirstAncestorWhichIsA",
                "FindFirstDescendant",
                "GetChildren", "GetDescendants", "GetAttribute", "SetAttribute",
                "GetAttributes", "GetAttributeChangedSignal",
                "GetPropertyChangedSignal",
                "Clone", "Destroy", "IsA", "IsDescendantOf", "IsAncestorOf",
            ];
            if m.iter().any(|name| instance_methods.contains(&name.as_str())) {
                // Generic instance -- could be script or something else.
                // If it also has field access, lean toward "script".
                if fields.map(|f| !f.is_empty()).unwrap_or(false) {
                    names[idx] = "script".to_string();
                } else {
                    // Unknown instance with method calls -- name it "instance"
                    // rather than leaving as upval_N
                    names[idx] = "instance".to_string();
                }
                continue;
            }
        }

        // Check for Roblox event/signal patterns via field access.
        if let Some(f) = fields {
            if !f.is_empty() {
                let event_fields = [
                    "OnServerEvent", "OnClientEvent", "OnServerInvoke",
                    "OnClientInvoke", "FireServer", "FireClient", "FireAllClients",
                    "InvokeServer", "InvokeClient",
                ];
                let conn_fields = ["Connect", "Wait", "Once", "DisconnectAll"];
                if f.iter().any(|name| event_fields.contains(&name.as_str())) {
                    names[idx] = "remote".to_string();
                    continue;
                }
                if f.iter().any(|name| conn_fields.contains(&name.as_str())) {
                    names[idx] = "signal".to_string();
                    continue;
                }
            }
        }

        // Phase B0.43B — pattern 1: SETGLOBAL from upval.
        // The first meaningful global name assigned from this upval wins.
        // Example: `_G.Config = upval_0` or `MyThing = upval_0`.
        if let Some(ns) = upval_setglobal_names.get(&idx) {
            if let Some(first) = ns.first() {
                names[idx] = first.clone();
                continue;
            }
        }

        // Phase B0.43B — pattern 6: require(upval) means upval is likely a
        // ModuleScript instance.  Name it "module" unless something stronger
        // was inferred above.
        if upval_is_require_arg.contains(&idx) && names[idx].is_empty() {
            names[idx] = "module".to_string();
            continue;
        }

        // Check for upvals that are SETTABLEKS targets (written-to tables).
        // Common for module tables: upval.foo = bar
        if let Some(sf) = upval_settable_fields.get(&idx) {
            if !sf.is_empty() && names[idx].is_empty() {
                names[idx] = "module".to_string();
                continue;
            }
        }

        // Check for upvals used as call targets (functions captured from parent)
        if upval_is_called.contains(&idx) && names[idx].is_empty() {
            names[idx] = "func".to_string();
            continue;
        }
    }

    names
}

/// Returns true if `s` looks like a plain Luau identifier:
///   - non-empty, starts with a letter or `_`, only contains
///     alphanumerics or `_`.
///
/// Used by pattern 1 (SETGLOBAL <- upval) to reject AUX-resolved strings
/// that happen to not be legal identifiers (hashes, paths, etc).
fn is_sane_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return false,
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Infer upvalue names from SETUPVAL instructions by tracing what value is
/// being stored.  When `SETUPVAL upval[B] = R(A)` and R(A) was loaded by a
/// preceding instruction that gives it a meaningful name (GETIMPORT, GETGLOBAL,
/// LOADK with a string, or another GETUPVAL with a known name), we can
/// retroactively name that upvalue.
///
/// This runs as a lightweight bytecode scan *before* lifting, so the inferred
/// names are available when GETUPVAL/SETUPVAL are processed into AST nodes.
///
/// Returns a vec of length `num_upvalues` where non-empty entries are the
/// inferred names.  Empty entries mean no SETUPVAL-based name was found.
fn infer_upval_names_from_setupval(
    proto: &Proto,
    strings: &[String],
    existing_names: Option<&Vec<String>>,
) -> Vec<String> {
    let num_upvals = proto.num_upvalues as usize;
    if num_upvals == 0 {
        return Vec::new();
    }

    let code = &proto.code;
    let reg_count = (proto.max_stack_size as usize).max(256);

    // Lightweight register tracker: we only care about *names* that were loaded
    // into registers, not full expressions.  None = unknown / not a simple name.
    let mut reg_names: Vec<Option<String>> = vec![None; reg_count];

    // For each upvalue index, collect candidate names from SETUPVAL sites.
    // We keep only the *first* meaningful name per upvalue (the initial store
    // is the most reliable; later stores may be mutations).
    let mut upval_names: Vec<String> = vec![String::new(); num_upvals];

    let mut pc = 0;
    while pc < code.len() {
        let insn = code[pc];
        let op = LuauOpcode::from_u8(insn_op(insn));
        let a = insn_a(insn) as usize;
        let b = insn_b(insn) as usize;
        let d = insn_d(insn);

        match op {
            // Track instructions that give a register a meaningful name.

            LuauOpcode::GetImport => {
                // GETIMPORT A D [AUX]: imports a global like `game`, `workspace`
                // For multi-segment imports like `game.X.Module`, the register
                // holds the *final* value — the module, not "game". Use the
                // LAST id for name inference so SETUPVAL / require() arg pick
                // up the module identity. Single-segment imports still use
                // that one id.  (C6 follow-up.)
                let aux_val = code.get(pc + 1).copied();
                let import_val = aux_val.unwrap_or_else(|| {
                    let d_unsigned = d as u16 as usize;
                    match proto.constants.get(d_unsigned) {
                        Some(Constant::Import(v)) => *v,
                        _ => 0,
                    }
                });
                if import_val != 0 {
                    let ids = decode_import(import_val);
                    if let Some(&id_last) = ids.last() {
                        let name = if let Some(Constant::String(s)) = proto.constants.get(id_last as usize) {
                            Some(s.clone())
                        } else {
                            strings.get(id_last as usize).cloned()
                        };
                        if a < reg_count {
                            reg_names[a] = name;
                        }
                    }
                }
            }

            LuauOpcode::GetGlobal => {
                // GETGLOBAL A D [AUX]: K[D] is the global name, AUX is hash/index
                let aux_word = code.get(pc + 1).copied();
                let name = resolve_global_name(proto, strings, d, aux_word);
                if a < reg_count {
                    reg_names[a] = name;
                }
            }

            LuauOpcode::GetUpval => {
                // GETUPVAL A B: loads upvalue B into R(A)
                if a < reg_count {
                    let upval_idx = b;
                    let known = existing_names
                        .and_then(|names| names.get(upval_idx))
                        .filter(|n| !n.is_empty())
                        .cloned()
                        .or_else(|| {
                            let n = upval_names.get(upval_idx)?;
                            if n.is_empty() { None } else { Some(n.clone()) }
                        });
                    reg_names[a] = known;
                }
            }

            LuauOpcode::LoadK => {
                // LOADK A D: loads constant K[D] into R(A)
                let d_unsigned = d as u16 as usize;
                if a < reg_count {
                    if let Some(Constant::String(s)) = proto.constants.get(d_unsigned) {
                        reg_names[a] = Some(s.clone());
                    } else {
                        reg_names[a] = None;
                    }
                }
            }

            LuauOpcode::Move => {
                // MOVE A B: R(A) = R(B), propagate name
                if a < reg_count && b < reg_count {
                    reg_names[a] = reg_names[b].clone();
                }
            }

            LuauOpcode::SetUpval => {
                // SETUPVAL A B: upval[B] = R(A)
                let upval_idx = b;
                if upval_idx < num_upvals && upval_names[upval_idx].is_empty() {
                    if let Some(name) = reg_names.get(a).cloned().flatten() {
                        let is_generic_vreg = name.starts_with('v')
                            && name.len() > 1
                            && name[1..].chars().all(|c| c.is_ascii_digit());
                        if !name.is_empty() && !is_generic_vreg {
                            upval_names[upval_idx] = name;
                        }
                    }
                }
            }

            // B0.61: GetTableKS loads R(A) = R(B)[AUX_string]. The FIELD name
            // is a much better hint for R(A) than nothing — e.g. `local Keypress = M.Keypress`
            // turns into the field being tracked, so a subsequent `upval_N = Keypress`
            // setupval can inherit the name.
            LuauOpcode::GetTableKS => {
                if a < reg_count {
                    let aux = code.get(pc + 1).copied().unwrap_or(0);
                    if let Some(field) = resolve_aux_string(proto, strings, aux) {
                        reg_names[a] = Some(field);
                    } else {
                        reg_names[a] = None;
                    }
                }
            }

            // B0.61: NameCall prepares a method call — R(A) = method function,
            // R(A+1) = object. The method name (from AUX) is the right hint for A.
            LuauOpcode::NameCall => {
                if a < reg_count {
                    let aux = code.get(pc + 1).copied().unwrap_or(0);
                    if let Some(method) = resolve_aux_string(proto, strings, aux) {
                        reg_names[a] = Some(method);
                    } else {
                        reg_names[a] = None;
                    }
                }
            }

            // B0.131: CALL result naming for upval inference.
            // Instead of blanket-clearing, try to derive a name from the
            // call pattern:
            // - NAMECALL :GetService("X") / :FindFirstChild("X") → "X"
            // - require(script.X) → "X"
            // - require(Name) → use Name
            // Falls back to None when no pattern matches.
            LuauOpcode::Call => {
                if a < reg_count {
                    let mut call_name: Option<String> = None;
                    // CALL A B C: function is R(A), first arg is R(A+1)
                    // For NAMECALL-preceded calls, R(A) held the method name
                    let func_name = reg_names.get(a).cloned().flatten();
                    if let Some(ref method) = func_name {
                        let is_naming_method = matches!(method.as_str(),
                            "GetService" | "FindFirstChild" | "FindFirstChildOfClass"
                            | "FindFirstChildWhichIsA" | "WaitForChild"
                            | "FindFirstAncestor" | "FindFirstAncestorOfClass"
                            | "FindFirstAncestorWhichIsA"
                        );
                        if is_naming_method {
                            // First arg is at R(A+2) for NAMECALL calls (A+1 is self)
                            let arg_reg = a + 2;
                            if let Some(Some(arg_name)) = reg_names.get(arg_reg) {
                                if is_valid_luau_identifier(arg_name)
                                    && !is_stdlib_shadow_name(arg_name)
                                {
                                    call_name = Some(arg_name.clone());
                                }
                            }
                        }
                    }
                    // Check for require() pattern: R(A) = require, arg at R(A+1)
                    if call_name.is_none() {
                        if let Some(Some(fname)) = reg_names.get(a) {
                            if fname == "require" {
                                // First arg at R(A+1), could be Name or Field
                                let arg_reg = a + 1;
                                if let Some(Some(arg_name)) = reg_names.get(arg_reg) {
                                    // Use the argument name as module name
                                    if is_valid_luau_identifier(arg_name)
                                        && !is_stdlib_shadow_name(arg_name)
                                    {
                                        call_name = Some(arg_name.clone());
                                    }
                                }
                            }
                        }
                    }
                    reg_names[a] = call_name;
                }
            }

            // Instructions that overwrite a register without a meaningful name
            LuauOpcode::NewTable | LuauOpcode::NewClosure | LuauOpcode::DupClosure
            | LuauOpcode::Add | LuauOpcode::Sub | LuauOpcode::Mul | LuauOpcode::Div
            | LuauOpcode::Mod | LuauOpcode::Pow | LuauOpcode::Concat
            | LuauOpcode::Length | LuauOpcode::Minus | LuauOpcode::Not
            | LuauOpcode::GetTableN | LuauOpcode::GetTable
            | LuauOpcode::Band | LuauOpcode::Bor | LuauOpcode::Bxor
            | LuauOpcode::Bnot | LuauOpcode::Shl | LuauOpcode::Shr
            | LuauOpcode::Bandk | LuauOpcode::Bork
            | LuauOpcode::RbxExt92 | LuauOpcode::RbxExt93 | LuauOpcode::RbxExt94
            | LuauOpcode::RbxExt95 | LuauOpcode::RbxExt96 | LuauOpcode::RbxExt97
            | LuauOpcode::RbxExt98 | LuauOpcode::RbxExt99 | LuauOpcode::RbxExt100
            | LuauOpcode::RbxExt101 | LuauOpcode::RbxExt102 | LuauOpcode::RbxExt103
            | LuauOpcode::RbxExt104 | LuauOpcode::RbxExt105 => {
                if a < reg_count {
                    reg_names[a] = None;
                }
            }

            _ => {}
        }

        if op.has_aux() {
            pc += 2;
        } else {
            pc += 1;
        }
    }

    upval_names
}

/// Lift a proto's bytecode into AST statements using structured control flow
pub fn lift_proto(ctx: &mut DecompileContext, proto: &Proto, proto_index: usize) -> Vec<Stat> {
    // Phase C1: reset the proto-wide statement budget at the top-level entry
    // so every fresh proto gets the full allowance. Recursive closure lifts
    // (via `lift_proto_inner` from `NewClosure`) intentionally share this
    // same counter with the main proto — the budget is a whole-decompilation
    // cap, not a per-closure cap.
    reset_stmt_budget();
    lift_proto_inner(ctx, proto, proto_index, 0)
}

pub(super) fn lift_proto_inner(ctx: &mut DecompileContext, proto: &Proto, proto_index: usize, depth: usize) -> Vec<Stat> {
    if depth >= MAX_DECOMPILE_DEPTH {
        return vec![Stat::Comment("-- max decompile depth reached".to_string())];
    }
    // If the proto-wide statement budget has already been exhausted by a
    // sibling / ancestor proto on this decompilation, short-circuit here so
    // we don't do more lifting work than we can possibly emit.
    if stmt_budget_tripped() {
        return vec![Stat::Comment("-- statement budget exceeded".to_string())];
    }

    // Infer upvalue names from usage patterns in the bytecode.
    //
    // For the main proto (depth==0), there's no parent to provide CAPTURE-based
    // inference, so this is the only source of upvalue names.
    //
    // For child protos (depth>0), CAPTURE-based inference from the parent is
    // preferred. However, if CAPTURE inference failed or was incomplete (e.g.,
    // un-remapped CAPTUREs that didn't pass structural validation), we fill in
    // the gaps with usage-based inference as a fallback.
    //
    // IMPORTANT: this block runs *before* analyze_register_usage so that the
    // pre-pass can hint GETUPVAL destinations with real upvalue names instead
    // of generic v{reg} fallbacks.
    if proto.num_upvalues > 0 {
        let existing = ctx.inferred_upvalue_names.get(&proto_index);
        let num_upvals = proto.num_upvalues as usize;
        let has_gaps = match existing {
            None => true,
            Some(names) => names.len() < num_upvals || names.iter().any(|n| n.is_empty()),
        };
        if has_gaps {
            let usage_names = infer_main_proto_upval_names(proto, &ctx.chunk.strings);
            if usage_names.iter().any(|n| !n.is_empty()) {
                // Merge: keep CAPTURE-inferred names where available, fill gaps with usage names
                let mut merged = existing.cloned().unwrap_or_else(|| vec![String::new(); num_upvals]);
                // Pad to full length if needed
                merged.resize(num_upvals, String::new());
                for (i, usage) in usage_names.iter().enumerate() {
                    if i < merged.len() && merged[i].is_empty() && !usage.is_empty() {
                        merged[i] = usage.clone();
                    }
                }
                ctx.inferred_upvalue_names.insert(proto_index, merged);
            }
        }
    }


    // SETUPVAL-based upvalue name inference: scan the bytecode for SETUPVAL
    // instructions and trace backwards to see what meaningful name R(A) held.
    // This catches cases where CAPTURE inference failed but the proto's own
    // code stores a known value (from GETIMPORT/GETGLOBAL/LOADK) into an upvalue.
    if proto.num_upvalues > 0 {
        let existing = ctx.inferred_upvalue_names.get(&proto_index);
        let num_upvals = proto.num_upvalues as usize;
        let has_gaps = match existing {
            None => true,
            Some(names) => names.len() < num_upvals || names.iter().any(|n| n.is_empty()),
        };
        if has_gaps {
            let setupval_names = infer_upval_names_from_setupval(
                proto,
                &ctx.chunk.strings,
                ctx.inferred_upvalue_names.get(&proto_index),
            );
            if setupval_names.iter().any(|n| !n.is_empty()) {
                let mut merged = ctx.inferred_upvalue_names
                    .get(&proto_index)
                    .cloned()
                    .unwrap_or_else(|| vec![String::new(); num_upvals]);
                merged.resize(num_upvals, String::new());
                for (i, name) in setupval_names.iter().enumerate() {
                    if i < merged.len() && merged[i].is_empty() && !name.is_empty() {
                        merged[i] = name.clone();
                    }
                }
                ctx.inferred_upvalue_names.insert(proto_index, merged);
            }
        }
    }

    // Run pre-pass to analyze register usage and generate naming hints.
    // This must happen AFTER upvalue inference (so GETUPVAL hints get real
    // upvalue names) and BEFORE any reg_name calls for this proto.
    let upval_names_clone = ctx.inferred_upvalue_names.get(&proto_index).cloned();
    let hints = analyze_register_usage(
        proto,
        &ctx.chunk.strings,
        upval_names_clone.as_deref(),
        Some(&ctx.chunk.protos),
    );
    ctx.init_proto_naming(proto_index, hints);
    let prev_proto = ctx.current_proto_index;
    ctx.current_proto_index = Some(proto_index);
    // B0.134b: push this proto onto the decompilation stack so child
    // NEWCLOSURE instructions can detect (and prevent) recursion into
    // any ancestor proto. Previously proto_stack was only maintained
    // in the NEWCLOSURE handler, leaving the main proto untracked.
    ctx.proto_stack.push(proto_index);

    let cfg = ControlFlowGraph::build(proto);
    let regions = structure_control_flow(&cfg, proto);
    // Use a generous register count — max_stack_size can be too small for some operands
    let reg_count = (proto.max_stack_size as usize).max(256);
    let mut regs = vec![RegVal::Unknown; reg_count];
    let mut locals = LocalTracker::new(proto.num_params as usize);

    // Initialize parameters
    for i in 0..proto.num_params {
        let name = ctx.reg_name(proto, i, 0);
        regs[i as usize] = RegVal::Expr(Expr::Name(name.clone()));
        // Phase B0.49: record param name.  Parameter registers never
        // re-declare (classify_write always returns Reassign for reg <
        // param_count), but keeping current_names populated guards any
        // future code path that might read the current name by proxy.
        locals.record_name(i as usize, &name);
    }

    let mut stmts = Vec::new();
    for region in &regions {
        if stmt_budget_tripped() {
            break;
        }
        let before_len = stmts.len();
        lift_region(ctx, proto, proto_index, &cfg, region, &mut regs, &mut locals, depth, &mut stmts);
        // Account for statements appended by push-sites that do not route
        // through `push_stat` (the lifter has ~56 direct `.push(Stat::…)`
        // call sites). `note_stmts_pushed` trims back + stamps the comment
        // if this particular region tipped us over the edge.
        let delta = stmts.len().saturating_sub(before_len);
        note_stmts_pushed(&mut stmts, delta);
    }

    // Restore parent proto context for nested decompilation
    ctx.proto_stack.pop(); // B0.134b: match the push at entry
    ctx.current_proto_index = prev_proto;

    // Post-processing: eliminate dead stores and collapse control flow
    simplify_stmts(&mut stmts);
    eliminate_dead_stores(&mut stmts);
    eliminate_dead_code(&mut stmts);
    // C10b: drop `local v_N = { K = "K" }` artifact patterns when unused.
    // C10j: also drop `local v_N = <pure_rhs>` for bare literals/Name/Field
    // chains when never read/written downstream.
    // Second dead-code sweep because these drops can leave empty-if shells.
    eliminate_dead_key_eq_value_locals(&mut stmts);
    eliminate_dead_code(&mut stmts);
    // Phase B0.46A: post-AST repeat-until detection. Catches `repeat ... until`
    // loops that the bytecode-level structuring pass emitted as
    // `while true do <body>; if cond then break end end`. Must run BEFORE
    // `convert_single_pass_loops` so the trailing `if cond then break end`
    // is still present (single-pass collapse would rewrite it into an if/else).
    convert_while_true_break_to_repeat(&mut stmts);
    convert_single_pass_loops(&mut stmts);
    collapse_elseif_chains(&mut stmts);

    // Phase B0.95: collapse `if cond then X = true else X = false end`
    // into `X = cond`. MUST run BEFORE collapse_short_circuit_assignments
    // to prevent the short-circuit pass from converting boolean-assignment
    // patterns into `X = cond and true or false` (which doesn't simplify).
    post_passes::collapse_if_assign_bool(&mut stmts);

    collapse_short_circuit_assignments(&mut stmts);

    // Phase B0.89: merge `local x = nil; x = expr` into `local x = expr`.
    // Must run BEFORE B0.87/88 so that `local x = nil; x = expr; if ...`
    // patterns are simplified first (the nil→expr merge may expose a new
    // ternary pattern for B0.87/88).
    post_passes::merge_dead_init_with_assignment(&mut stmts);

    // Phase B0.87/88: collapse `local x = <init>; if cond then x = a [else x = b] end`
    // into `local x = if cond then a else b` (or `... else <init>` without else).
    // Must run AFTER collapse_short_circuit_assignments (which handles the
    // self-referencing `if x then x = a end` shape separately) and BEFORE
    // inline_single_use_temps (which benefits from the reduced read-count).
    collapse_nil_init_conditional(&mut stmts);

    rename_upvals(&mut stmts);

    // Phase C2 pass #2: recursive upvalue name propagation (bounded fixpoint).
    //
    // rename_upvals just resolved this proto's upvalue names from AST usage.
    // Children captured by this proto may still have `upval_N` placeholders
    // because their parent's upvalue was named AFTER the child was visited.
    // Grandchildren are an even deeper case: P1 → P2 → P3 where P3's upval_N
    // depends on P2's upval_M depending on P1's resolved name.
    //
    // We walk `upval_parent_links` for ALL protos (not just direct children of
    // the current one) and propagate named parents to unnamed children. Each
    // iteration may resolve a new link, feeding the next iteration. We bound
    // the loop at 5 iterations to guarantee termination even if the parent
    // link graph contains a cycle (which would be malformed but must not hang).
    //
    // After any update we re-run `rename_upvals` on the current proto's AST
    // so that newly-resolved names flow into the emitted output.
    for _ in 0..PROPAGATE_UPVAL_MAX_ITERATIONS {
        let changed = propagate_upval_names_once(
            &ctx.chunk.protos,
            &mut ctx.inferred_upvalue_names,
            &ctx.upval_parent_links,
        );
        if !changed { break; }
        // Re-walk this proto's AST — new parent→child names may have been
        // propagated that affect `upval_N` references emitted during the
        // lifting phase. `rename_upvals` uses AST-usage heuristics and
        // descends into nested closure bodies (via apply_renames_to_stmts),
        // so any newly-resolved context propagates throughout the tree.
        rename_upvals(&mut stmts);
    }

    // Clean up decompiler artifacts
    cleanup_stmts(&mut stmts);

    // Collapse method chains: `local v0 = obj:M1()` + `v0 = v0:M2(args)` → `local v0 = obj:M1():M2(args)`
    // Also collapses different-name chains: `call = X()` + `call2 = call:M()` → `call2 = X():M()`
    collapse_method_chains(&mut stmts);

    // Phase B0.47: Reconstruct module-style table constructors from the
    // `local M = {}; M.foo = ...; M.bar = ...` pattern.  Must run BEFORE
    // `inline_single_use_temps` because removing the intermediate field
    // assignments changes the read-count of `M`, which `inline_single_use_temps`
    // uses to decide whether to inline the table value.
    reconstruct_table_constructors(&mut stmts);

    // Phase C2: SETLIST / sequential-integer-index coalesce.  Converts
    // `local t = {[1] = a, [2] = b, [3] = c}` (produced by
    // `reconstruct_table_constructors` when the keys are integers and
    // therefore not valid identifiers) into `local t = {a, b, c}`.  Runs
    // AFTER `reconstruct_table_constructors` so it operates on the already-
    // folded Table constructor.  Purely cosmetic: no name-read-count side
    // effects, so order relative to `inline_single_use_temps` is free.
    coalesce_setlist_sequential(&mut stmts);

    // Phase C10O: unwrap `local R = {K = inner}; require(R)` → `require(inner)`.
    // Must run BEFORE inline_single_use_temps — that pass has a B0.114 guard
    // that refuses to inline tables into require() args, so the wrapper local
    // would otherwise persist. C10O handles the pre-materialized case
    // directly; C8 (CALL-time) already covers the inline form.
    post_passes::unwrap_require_wrapper_locals(&mut stmts);

    // Phase C10P: rename `local serviceN = game:GetService("X")` locals to X.
    // Upstream LITERAL_NAMING_METHODS logic (mod.rs) is supposed to propagate
    // the string arg into the register hint, but ~1112 corpus locals still
    // surface as generic `serviceN`. This post-pass catches the survivors.
    post_passes::rename_service_locals(&mut stmts);

    // Inline single-use call/method temps into their use sites:
    // `call7 = chain:Build()` + `call8 = Y:Add(call7)` → `call8 = Y:Add(chain:Build())`
    inline_single_use_temps(&mut stmts);

    // Phase B0.51B: inline pure literal locals at ALL read sites
    // (regardless of read count) up to the next reassignment.  Targets
    // the Roblox pattern where a register is reused with multiple
    // LOADK loads, e.g. `local v3 = "Players"; game:GetService(v3); ...`
    // becomes `game:GetService("Players")` etc.
    inline_pure_literals(&mut stmts);

    // Phase C2: fold multi-return call unpack pattern
    //   `local v1,v2,v3 = f()` + `x.a=v1; x.b=v2; x.c=v3`
    //   →  `x.a, x.b, x.c = f()`
    // Runs AFTER inline_single_use_temps so the simple one-to-one single-use
    // case is already collapsed; any surviving scattered-unpack cluster is
    // the N-ary pattern this fold targets.
    post_passes::fold_multireturn_unpack(&mut stmts);

    // Run chain collapse again — inlining may create new consecutive chain opportunities
    collapse_method_chains(&mut stmts);

    // Phase B0.60: reconstruct Luau method-function syntax from the
    // two-step `local F = function(...) end; Base.X = F` pattern that
    // the lifter produces for every Roblox module field-closure write.
    // Converts to `Stat::MethodFunction` with `is_method=true` when the
    // first param is used as an object receiver in the body — emit.rs
    // then renders as `function Base:X(...) end` (proper Luau method
    // syntax). Runs AFTER inline_single_use_temps so any folded temps
    // are cleaned up first.
    post_passes::reconstruct_method_assignments(&mut stmts);

    // Phase C10S: drop `setmetatable.X = ...` / `function pcall.Y() end`
    // and similar stdlib-function-lvalue artifacts. These only appear
    // when a register that should hold a local-binding name got
    // corrupted upstream into `Name("setmetatable")` (or any other
    // stdlib function). Real source never writes to stdlib functions,
    // so the resulting statement is pure decompiler noise. Runs AFTER
    // reconstruct_method_assignments so both the raw `Stat::Assign`
    // artifact AND any `Stat::MethodFunction` form get swept.
    post_passes::drop_stdlib_function_lvalue_artifacts(&mut stmts);

    // B0.119: Convert `local fn = function(...) ... end` to the idiomatic
    // `local function fn(...) ... end` form. Runs AFTER method-function
    // reconstruction (which may absorb some of these into Base:Method style)
    // and AFTER inline_single_use_temps (which may inline closures).
    convert_local_function_sugar(&mut stmts);

    // Phase C2 pass #5: convert `T.m = function(self, ...)` to idiomatic
    // `function T:m(...)` when the body uses `self.x` or `self:y()` at
    // least twice. Runs AFTER reconstruct_method_assignments (which
    // handles the two-step local-then-assign pattern) so this pass picks
    // up the remaining direct-assign shapes. Must run AFTER all temp-
    // inlining and naming so the self-count is accurate.
    post_passes::convert_dot_to_method_function(&mut stmts);

    // Fold constant expressions (3 + 4 → 7, "a" .. "b" → "ab", etc.)
    fold_constants_in_stmts(&mut stmts);

    // C10h: after fold, `if "utf8" == "utf8" then X end` becomes
    // `if true then X end`. Splice into parent. Always-false becomes
    // `if false then X else Y end` → Y.
    collapse_constant_ifs(&mut stmts);

    // Phase B0.93c: collapse `if cond then return true else return false end`
    // into `return cond`. Must run AFTER fold_constants (which may simplify
    // conditions) and AFTER collapse_nil_init_conditional (which may convert
    // if/assign patterns that look similar but aren't return-based).
    post_passes::collapse_if_return_bool(&mut stmts);

    // Phase B0.97: collapse `if cond then return a else return b end`
    // into `return if cond then a else b`. Also handles the fallthrough
    // pattern `if cond then return a end; return b`. Must run AFTER
    // collapse_if_return_bool so bool-specific `return cond` fires first.
    post_passes::collapse_if_return_ternary(&mut stmts);

    stmts
}

/// Lift a single region into statements
fn lift_region(
    ctx: &mut DecompileContext,
    proto: &Proto,
    proto_index: usize,
    cfg: &ControlFlowGraph,
    region: &Region,
    regs: &mut Vec<RegVal>,
    locals: &mut LocalTracker,
    depth: usize,
    stmts: &mut Vec<Stat>,
) {
    match region {
        Region::Linear { start, end } => {
            lift_instruction_range(ctx, proto, proto_index, depth, *start, *end, regs, locals, stmts, false);
        }

        Region::IfThenElse {
            cond_pc,
            then_region,
            else_region,
            merge_pc,
        } => {
            // Lift the condition block up to (not including) the branch
            let block = &cfg.blocks[cond_pc];
            lift_instruction_range(ctx, proto, proto_index, depth, block.start, find_branch_pc(proto, block.end), regs, locals, stmts, false);

            // Extract the condition expression from the branch instruction
            let condition = extract_branch_condition(ctx, proto, find_branch_pc(proto, block.end), regs);

            // Snapshot register state before branches so then/else don't
            // corrupt each other's view of the registers.
            let regs_before = regs.clone();

            // Lift then-body (sorted by start PC for correct instruction order)
            let mut then_sorted = then_region.clone();
            then_sorted.sort_unstable();
            let mut then_stmts = Vec::new();
            for &block_id in &then_sorted {
                if let Some(b) = cfg.blocks.get(&block_id) {
                    lift_instruction_range(ctx, proto, proto_index, depth, b.start, b.end, regs, locals, &mut then_stmts, false);
                }
            }
            let regs_after_then = regs.clone();

            // Restore pre-branch state for the else-branch. Clone so we can
            // still reference `regs_before` afterward in the B0.57 hoist.
            *regs = regs_before.clone();

            // Lift else-body (sorted by start PC for correct instruction order)
            let mut else_sorted = else_region.clone();
            else_sorted.sort_unstable();
            let else_body = if !else_sorted.is_empty() {
                let mut else_stmts = Vec::new();
                for &block_id in &else_sorted {
                    if let Some(b) = cfg.blocks.get(&block_id) {
                        lift_instruction_range(ctx, proto, proto_index, depth, b.start, b.end, regs, locals, &mut else_stmts, false);
                    }
                }
                Some(else_stmts)
            } else {
                None
            };

            // Save else-path register state before merge for B0.116.
            let regs_after_else = regs.clone();

            // Merge register state: keep values both branches agree on,
            // reset to Unknown where they diverge. This is conservative
            // but correct — after an if/else, only values that are the
            // same on both paths are guaranteed to hold.
            merge_regs(regs, &regs_after_then);

            // B0.116: Import-guard MethodCall propagation.
            // Roblox bytecode uses GETIMPORT + guard + NAMECALL + CALL for
            // service imports. The NAMECALL lives in one branch (then or
            // else, depending on guard type — JUMPIFNOT vs DEPRECATED_61)
            // while CALL sits at the merge point. merge_regs (B0.56 Name
            // rule) discards the MethodCall and keeps the Name from the
            // other path's GETIMPORT, producing `game(game, ...)` instead
            // of `game:GetService(...)`. Fix: when the merge block starts
            // with CALL and EITHER branch wrote a MethodCall to the func
            // register, propagate it through the merge so CALL sees it.
            if let Some(mpc) = merge_pc {
                if let Some(&insn) = proto.code.get(*mpc) {
                    let mop = LuauOpcode::from_u8(insn_op(insn));
                    if mop == LuauOpcode::Call {
                        let ca = insn_a(insn) as usize;
                        let cur_not_method = ca < regs.len()
                            && !matches!(&regs[ca], RegVal::Expr(Expr::MethodCall { .. }));
                        if cur_not_method {
                            // Check then-path first, then else-path
                            let source = if ca < regs_after_then.len()
                                && matches!(&regs_after_then[ca], RegVal::Expr(Expr::MethodCall { .. }))
                            {
                                Some(&regs_after_then)
                            } else if ca < regs_after_else.len()
                                && matches!(&regs_after_else[ca], RegVal::Expr(Expr::MethodCall { .. }))
                            {
                                Some(&regs_after_else)
                            } else {
                                None
                            };
                            if let Some(src) = source {
                                regs[ca] = src[ca].clone();
                                if ca + 1 < regs.len() && ca + 1 < src.len() {
                                    regs[ca + 1] = src[ca + 1].clone();
                                }

                                // B0.118 (future): service name recovery for
                                // GetService arguments needs batch import guard
                                // analysis — the LOADK for the service name is
                                // inside a PREVIOUS IfThenElse's branch, and its
                                // value gets lost through cascading merge_regs.
                            }
                        }
                    }
                }
            }

            // B0.57: hoist `Stat::Local` declarations out of branch bodies
            // when the register escapes (i.e. post-merge `regs` still holds
            // an `Expr::Name(n)` matching the declared local). Without this,
            // BaseCamera-style modules emit `local M = {}` INSIDE an if
            // block and then use `M.field = X` outside, which Luau parses
            // as a bare global write because the local's scope ended at
            // the block's `end`. Pair fix for the B0.56 merge_regs
            // Name-preservation: B0.56 keeps the Name across the merge,
            // B0.57 makes sure the declaration survives alongside it.
            let mut hoisted_names: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut hoisted: Vec<Stat> = Vec::new();
            hoist_escaping_locals(&mut then_stmts, regs, &regs_before, &mut hoisted, &mut hoisted_names);
            let else_body = if let Some(mut es) = else_body {
                hoist_escaping_locals(&mut es, regs, &regs_before, &mut hoisted, &mut hoisted_names);
                Some(es)
            } else {
                None
            };
            stmts.extend(hoisted);

            // If condition is always true (unknown branch opcode), inline the body directly
            if matches!(&condition, Expr::Bool(true)) {
                stmts.extend(then_stmts);
                if let Some(else_stmts) = else_body {
                    stmts.extend(else_stmts);
                }
            } else {
                stmts.push(Stat::If {
                    condition,
                    then_body: then_stmts,
                    elseif_clauses: vec![],
                    else_body,
                });
            }
        }

        Region::WhileDo { header, body_blocks } => {
            let block = &cfg.blocks[header];
            let branch_pc = find_branch_pc(proto, block.end);

            // Lift the header block's pre-branch instructions (e.g., GETTABLEKS,
            // NAMECALL+CALL, etc.) so registers are populated before we extract
            // the condition. Without this, the condition expression may reference
            // stale or Unknown registers.
            // Emit these BEFORE the loop -- they run once on initial entry.
            if block.start < branch_pc {
                lift_instruction_range(ctx, proto, proto_index, depth, block.start, branch_pc, regs, locals, stmts, false);
            }

            let condition = extract_branch_condition(ctx, proto, branch_pc, regs);

            let snap = locals.snapshot();
            let mut sorted_body = body_blocks.clone();
            sorted_body.sort_unstable();
            let mut body_stmts = Vec::new();
            for &block_id in &sorted_body {
                if block_id == *header { continue; }
                if let Some(b) = cfg.blocks.get(&block_id) {
                    lift_instruction_range(ctx, proto, proto_index, depth, b.start, b.end, regs, locals, &mut body_stmts, true);
                }
            }
            // Remove trailing JUMPBACK if present
            remove_trailing_jump(&mut body_stmts);

            // Re-lift the header's pre-branch instructions at the end of the
            // loop body. In the original bytecode, JUMPBACK returns control to
            // the header, which re-executes these instructions every iteration
            // to re-evaluate the condition. We place them at the bottom of the
            // body so the condition is re-computed each time around.
            if block.start < branch_pc {
                lift_instruction_range(ctx, proto, proto_index, depth, block.start, branch_pc, regs, locals, &mut body_stmts, true);
            }

            // Hoist locals first declared inside the loop body
            hoist_loop_locals(locals, &snap, stmts, &mut body_stmts);

            stmts.push(Stat::While {
                condition,
                body: body_stmts,
            });
        }

        Region::WhileTrue { header: _, body_blocks } => {
            let snap = locals.snapshot();
            let mut sorted_body = body_blocks.clone();
            sorted_body.sort_unstable();
            let mut body_stmts = Vec::new();
            for &block_id in &sorted_body {
                if let Some(b) = cfg.blocks.get(&block_id) {
                    lift_instruction_range(ctx, proto, proto_index, depth, b.start, b.end, regs, locals, &mut body_stmts, true);
                }
            }
            remove_trailing_jump(&mut body_stmts);

            // Hoist locals first declared inside the loop body
            hoist_loop_locals(locals, &snap, stmts, &mut body_stmts);

            stmts.push(Stat::While {
                condition: Expr::Bool(true),
                body: body_stmts,
            });
        }

        Region::RepeatUntil { header: _, body_blocks, cond_pc } => {
            let snap = locals.snapshot();
            let mut sorted_body = body_blocks.clone();
            sorted_body.sort_unstable();
            let mut body_stmts = Vec::new();
            for &block_id in &sorted_body {
                if block_id == *cond_pc { continue; } // condition is separate
                if let Some(b) = cfg.blocks.get(&block_id) {
                    lift_instruction_range(ctx, proto, proto_index, depth, b.start, b.end, regs, locals, &mut body_stmts, true);
                }
            }

            // Lift the condition block's pre-branch instructions so that
            // registers used in the condition are populated. For example,
            // the cond block may contain GETTABLEKS or CALL instructions
            // that produce the value compared by the branch instruction.
            let cond_block = &cfg.blocks[cond_pc];
            let cond_branch_pc = find_branch_pc(proto, cond_block.end);
            if cond_block.start < cond_branch_pc {
                lift_instruction_range(ctx, proto, proto_index, depth, cond_block.start, cond_branch_pc, regs, locals, &mut body_stmts, true);
            }

            let condition = extract_branch_condition(ctx, proto, cond_branch_pc, regs);

            // Hoist locals first declared inside the loop body
            hoist_loop_locals(locals, &snap, stmts, &mut body_stmts);

            stmts.push(Stat::Repeat {
                body: body_stmts,
                condition,
            });
        }

        Region::NumericFor { prep_pc, loop_pc: _, body_start, body_end, body: nested_body } => {
            let code = &proto.code;
            let a = insn_a(code[*prep_pc]) as usize;

            // Phase B0.3 fix: Luau v6 FORNPREP register layout is:
            //   R(A+0) = limit
            //   R(A+1) = step
            //   R(A+2) = initial index (also the loop variable `i` during the body)
            //
            // Verified empirically against ModuleScript.luac:
            //   - Proto 9 numeric_for_simple  (for i = 1, n do sum = sum + i)
            //   - Proto 11 nested_for outer   (for i = 1, n do ...)
            //   - Proto 11 nested_for inner   (for j = 1, n do sum = sum + i*j)
            // In all three, the body uses R(A+2) as the loop variable, while
            // R(A+0) holds the (runtime) limit and R(A+1) holds the step.
            //
            // Note: this is the MODERN Luau layout and differs from the
            // Lua 5.1 / classic layout (start=A, stop=A+1, step=A+2, var=A+3).
            // The project's earlier docs/code used the 5.1 layout, which broke
            // numeric-for reconstruction (e.g. emitted `for i = arg1, 1 do end`
            // instead of `for i = 1, n do ... end`).
            //
            // Try to absorb the for-loop setup (limit/step/start assignments)
            // from the preceding statements. Absorb in reverse order because
            // the start expression (highest register) is usually the most
            // recently emitted statement on top of the stmts stack.
            let start_expr = absorb_numeric_for_setup(stmts, regs, a + 2);
            let step_expr  = absorb_numeric_for_setup(stmts, regs, a + 1);
            let stop_expr  = absorb_numeric_for_setup(stmts, regs, a);
            let var_name   = ctx.reg_name(proto, ((a + 2) & 0xFF) as u8, *prep_pc);

            // Phase B0.3 fix: pre-materialize any register that (a) currently
            // holds a live inlinable value and (b) is written somewhere in the
            // loop body. Without this, a pattern like `local sum = 0; for i =
            // 1, n do sum = sum + i end` never materializes `sum` as a local —
            // LOADN R1, 0 inlines `Number(0)` into regs[1], and the body's
            // `ADD R1, R1, R4` then reads `Number(0) + Name(i)`, folds it into
            // `regs[1]` silently, and emits an empty body. Force-materializing
            // R1 before the loop turns it into `local v1 = 0`, after which the
            // ADD reads `Name(v1) + Name(i)` and (via self-mutation detection
            // in `store_complex`) emits the expected `v1 = v1 + i`.
            //
            // We guard against re-materializing the loop-control registers
            // themselves (R(A), R(A+1), R(A+2)) because those are either
            // already absorbed (start/limit/step) or about to be rebound to
            // the loop variable name below.
            {
                let body_writes = collect_body_writes(&proto.code, *body_start, *body_end);
                for reg in body_writes {
                    if reg == a || reg == a + 1 || reg == a + 2 {
                        continue;
                    }
                    let is_live_literal = matches!(
                        regs.get(reg),
                        Some(RegVal::Expr(e)) if !matches!(e, Expr::Name(_))
                    );
                    if !is_live_literal {
                        continue;
                    }
                    // Snapshot the pending value and force-emit it as a local.
                    // Phase B0.49: classify_write for shadow-on-rename.
                    let pending = match &regs[reg] {
                        RegVal::Expr(e) => e.clone(),
                        _ => continue,
                    };
                    // C10d: stdlib-shadow sanitize at force-materialize path.
                    let pending = sanitize_leaked_global_string(pending);
                    let new_name = ctx.reg_name(proto, reg as u8, *prep_pc);
                    let (kind, name) = locals.classify_write(reg, &new_name);
                    match kind {
                        WriteKind::FirstDecl | WriteKind::Shadow => {
                            stmts.push(Stat::Local {
                                names: vec![name.clone()],
                                values: vec![pending],
                            });
                        }
                        WriteKind::Reassign => {
                            stmts.push(Stat::Assign {
                                targets: vec![Expr::Name(name.clone())],
                                values: vec![pending],
                            });
                        }
                    }
                    regs[reg] = RegVal::Expr(Expr::Name(name));
                }
            }

            // Phase B0.3 fix: bind the loop variable register to its symbolic
            // name (`i`, `j`, etc.) so body instructions that read R(A+2) see
            // the loop variable rather than the stale initial-value literal.
            // Without this, `MUL R8, R4, R7` inside a nested loop would read
            // the initial `1`s stored by LOADN during setup and emit
            // `local v8 = 1 * 1`.
            if (a + 2) < regs.len() {
                regs[a + 2] = RegVal::Expr(Expr::Name(var_name.clone()));
                locals.pre_declare(a + 2);
                // Phase B0.49: record the loop-var name so body writes to
                // this register treat the loop variable as the existing
                // binding and don't accidentally shadow it.
                locals.record_name(a + 2, &var_name);
            }

            let mut body_stmts = Vec::new();
            // Phase B0.4: iterate the structured nested body if the
            // structurer produced one. This is what makes inner for-loops
            // render as real `Stat::NumericFor` sub-nodes instead of raw
            // opcodes inside a linear body range.
            //
            // The nested body is a Vec<Region> where each entry is either a
            // Region::Linear (straight-line body code) or a nested
            // Region::NumericFor. When the vector is empty we fall back to
            // the pre-B0.4 linear lift — covers degenerate bodies.
            //
            // CRITICAL: Linear sub-regions MUST be lifted with `in_loop:
            // true` so that forward-jumps-beyond-body-end emit `break`
            // statements (the for-loop break semantics). Calling
            // `lift_region` on a `Region::Linear` uses `in_loop: false` and
            // silently drops those breaks. For Linear children of a
            // Region::NumericFor we therefore bypass `lift_region` and call
            // `lift_instruction_range` directly with in_loop=true — which
            // exactly mirrors the pre-B0.4 body-lift semantics. For nested
            // Region::NumericFor children we delegate to `lift_region`,
            // which re-enters this same arm and establishes its own body
            // scope (including its own in_loop semantics).
            if nested_body.is_empty() {
                lift_instruction_range(
                    ctx, proto, proto_index, depth,
                    *body_start, *body_end,
                    regs, locals, &mut body_stmts, true,
                );
            } else {
                for sub_region in nested_body {
                    match sub_region {
                        Region::Linear { start, end } => {
                            lift_instruction_range(
                                ctx, proto, proto_index, depth,
                                *start, *end,
                                regs, locals, &mut body_stmts, true,
                            );
                        }
                        _ => {
                            lift_region(
                                ctx, proto, proto_index, cfg, sub_region,
                                regs, locals, depth + 1, &mut body_stmts,
                            );
                        }
                    }
                }
            }
            // Remove the FORNLOOP at the end
            remove_trailing_jump(&mut body_stmts);

            // Omit step if it's 1
            let step = match &step_expr {
                Expr::Number(n) if *n == 1.0 => None,
                _ => Some(step_expr),
            };

            // C10c: same stdlib-shadow leak guard as materialized assignments.
            // `for i = "os", v1, v2 do` is always a decompiler artifact — the
            // start value came from an unresolved LOADK of a stdlib name.
            let start_expr = sanitize_leaked_global_string(start_expr);
            let stop_expr  = sanitize_leaked_global_string(stop_expr);
            let step = step.map(sanitize_leaked_global_string);

            // C10L: FORNPREP bound that resolves to a stdlib Name (e.g. `os`,
            // `math`, `game`) is always a corruption artifact — you cannot
            // iterate from a library table. Replace the corrupt bound with 0
            // so the output parses. (C10W: previously emitted a diagnostic
            // comment here; dropped because the 0-replacement is evidence
            // enough and the comment was noise in ~30 hot files.)
            let start_corrupt = is_stdlib_name_corruption(&start_expr);
            let stop_corrupt  = is_stdlib_name_corruption(&stop_expr);
            let step_corrupt  = step.as_ref().map_or(false, is_stdlib_name_corruption);
            let start_expr = if start_corrupt { Expr::Number(0.0) } else { start_expr };
            let stop_expr  = if stop_corrupt  { Expr::Number(0.0) } else { stop_expr };
            let step = if step_corrupt { Some(Expr::Number(1.0)) } else { step };

            stmts.push(Stat::NumericFor {
                var: var_name,
                start: start_expr,
                stop: stop_expr,
                step,
                body: body_stmts,
            });
        }

        Region::GenericFor { prep_pc, loop_pc, body_start, body_end, body: nested_body } => {
            let code = &proto.code;
            let a = insn_a(code[*prep_pc]) as usize;

            // The iterator state is at registers a, a+1, a+2
            // Loop variables start at a+3 (or a+2 depending on encoding)
            //
            // Try to absorb the iterator setup from the preceding statement.
            // The typical pattern is:
            //   local v5, v6, v7 = pairs(t)   -- CALL result for iterator
            //   FORGPREP r5 D
            // We want to emit `for k, v in pairs(t)` rather than `for k, v in v5`.
            //
            // Phase B0.7: `absorb_iterator_setup` now returns a `Vec<Expr>`.
            // When the absorption succeeds, the vec contains exactly one call
            // expression (the common shape). When it fails, the vec contains
            // between 1 and 3 expressions drawn directly from
            // `regs[a..=a+2]` — this recovers `for k, v in next, t do` style
            // iterators where the compiler never emits a CALL because the
            // three-value iterator triple is already in the right registers.
            let iter_exprs = absorb_iterator_setup(stmts, regs, a);
            // First iterator is the generator expression used by name-inference
            // below; the whole vec is what we emit into `Stat::GenericFor`.
            let primary_iter = iter_exprs
                .first()
                .cloned()
                .unwrap_or_else(|| reg_expr(regs, a));

            // Determine if the loop-back instruction is FORGLOOPINEXT (Deprecated61),
            // which has no AUX word and always iterates exactly 2 variables (integer
            // index + value) for ipairs-style loops.
            let loop_back_is_inext = *loop_pc < code.len()
                && LuauOpcode::from_u8(insn_op(code[*loop_pc])) == LuauOpcode::Deprecated61;

            // Try to get variable names from debug info
            let mut var_names = Vec::new();
            if *loop_pc < code.len() {
                let nresults = if loop_back_is_inext {
                    // FORGLOOPINEXT: no AUX, always 2 vars (integer key + value)
                    2u32
                } else {
                    // FORGLOOP: AUX encodes the loop variable count in low bits;
                    // bit 31 is the "inext" flag (ipairs-style). Mask off bit 31 first.
                    // Cap at 5 to guard against corrupted AUX, require at least 1.
                    let loop_aux = if *loop_pc + 1 < code.len() { code[*loop_pc + 1] } else { 0 };
                    (loop_aux & 0x7FFFFFFF).clamp(1, 5)
                };
                for i in 0..nresults {
                    var_names.push(ctx.reg_name(proto, ((a + 3 + i as usize) & 0xFF) as u8, *body_start));
                }
            }
            if var_names.is_empty() {
                // Infer conventional names from the iterator expression.
                // pairs(t) → k, v   ipairs(t) / next → i, v   unknown → k, v
                let (first, second) = if loop_back_is_inext {
                    // FORGLOOPINEXT is always ipairs-style: integer index + value
                    ("i", "v")
                } else {
                    match &primary_iter {
                        Expr::Call { func, .. } | Expr::MethodCall { method: _, object: func, .. } => {
                            match func.as_ref() {
                                Expr::Name(n) if n == "ipairs" => ("i", "v"),
                                Expr::Name(n) if n == "pairs" || n == "next" => ("k", "v"),
                                _ => ("k", "v"),
                            }
                        }
                        Expr::Name(n) if n == "ipairs" => ("i", "v"),
                        Expr::Name(n) if n == "pairs" || n == "next" => ("k", "v"),
                        _ => ("k", "v"),
                    }
                };
                // Use plain names when they are not yet taken; fall back to gen_var.
                let first_name = if ctx.is_name_used(first) {
                    ctx.gen_var(first)
                } else {
                    ctx.reserve_name(first)
                };
                var_names.push(first_name);
                let second_name = if ctx.is_name_used(second) {
                    ctx.gen_var(second)
                } else {
                    ctx.reserve_name(second)
                };
                var_names.push(second_name);
                // If FORGLOOP AUX specifies more than 2 variables, add extra vars.
                // FORGLOOPINEXT always has exactly 2, so skip this for inext loops.
                if !loop_back_is_inext && *loop_pc < code.len() && *loop_pc + 1 < code.len() {
                    let loop_aux = code[*loop_pc + 1];
                    // Mask off bit 31 (inext flag) before reading variable count
                    let nresults = (loop_aux & 0x7FFFFFFF).clamp(1, 5) as usize;
                    for extra in 2..nresults {
                        var_names.push(ctx.gen_var(&format!("v{}", extra)));
                    }
                }
            }

            // Phase B0.10: seed loop variable registers so body read-ops see
            // the correct names.  Without this, `reg_expr(regs, a+3)` returns
            // `v{a+3}` (the Unknown fallback) while `Stat::GenericFor.vars`
            // already contains the correct names — creating a disconnect between
            // the for-loop header and its body.  This mirrors the NumericFor
            // treatment of its loop variable at line ~862:
            //   `regs[a + 2] = RegVal::Expr(Expr::Name(var_name.clone()))`.
            //
            // When debug info is present, `var_names[i]` already holds the
            // original source name (e.g. "player") because `ctx.reg_name`
            // consulted `proto.debug_info.locals` when building var_names above.
            // Seeding `regs` here propagates those original names into the body
            // without any additional infrastructure.
            //
            // IMPORTANT: We use `RegVal::LoopVar(name)` instead of
            // `RegVal::Expr(Expr::Name(name))`.  Both produce `Expr::Name(name)`
            // when read by `reg_expr`.  The difference is that the CALL vararg
            // boundary scanner (B=0 path) uses `_ => break` for non-`Expr`
            // variants, so `LoopVar` registers are NOT absorbed as trailing call
            // arguments — fixing the regression where B0.10 caused inner vararg
            // CALLs to pick up loop variable registers as extra args.
            {
                let mut reg = a + 3;
                for name in &var_names {
                    if reg < regs.len() {
                        regs[reg] = RegVal::LoopVar(name.clone());
                        locals.pre_declare(reg);
                        // Phase B0.49: keep current_names in sync with loop
                        // variable bindings so body writes correctly classify.
                        locals.record_name(reg, name);
                    }
                    reg += 1;
                }
            }

            // Phase B0.6: iterate the structured nested body if the
            // structurer produced one. Mirrors the Phase B0.4 NumericFor
            // mechanism for the generic-for case. Each sub-region is either
            // a Region::Linear (straight-line body code) or a nested
            // Region::NumericFor / Region::GenericFor / Region::InlineIfThenInLoop.
            // When the vector is empty we fall back to the pre-B0.6 linear lift.
            //
            // CRITICAL: Linear children must be lifted with `in_loop: true` so
            // forward-jumps-beyond-body-end translate into `break` statements.
            // `lift_region` passes `false` for Linear regions, so we call
            // `lift_instruction_range` directly for Linear children here.
            let mut body_stmts = Vec::new();
            if nested_body.is_empty() {
                lift_instruction_range(
                    ctx, proto, proto_index, depth,
                    *body_start, *body_end,
                    regs, locals, &mut body_stmts, true,
                );
            } else {
                for sub_region in nested_body {
                    match sub_region {
                        Region::Linear { start, end } => {
                            lift_instruction_range(
                                ctx, proto, proto_index, depth,
                                *start, *end,
                                regs, locals, &mut body_stmts, true,
                            );
                        }
                        _ => {
                            lift_region(
                                ctx, proto, proto_index, cfg, sub_region,
                                regs, locals, depth + 1, &mut body_stmts,
                            );
                        }
                    }
                }
            }
            remove_trailing_jump(&mut body_stmts);

            // Phase C4: lifter corruption guard — reject GenericFor whose
            // iterator is provably non-iterable.  Two checks:
            //  (1) the absorbed/built iterator expression (post-filter) is
            //      a known non-callable path like `math.huge` / `math.pi`;
            //  (2) the pre-absorption register at `a` holds a literal
            //      Number/Bool/Nil.  absorb_iterator_setup replaces such
            //      literals with `Name("v{a}")` for downstream safety, but
            //      the presence of the literal in the iterator slot is
            //      itself evidence of bad opmap detection, so surface it
            //      as a Comment.
            //
            // When either fires, emit a `Stat::Comment` instead of the
            // `Stat::GenericFor`.  This keeps the rest of the file
            // parseable by full_moon while making the corruption visible.
            let primary = iter_exprs.first();
            let mut non_iter_reason: Option<String> = match primary {
                Some(Expr::Number(n)) => Some(format!("non-iterable number literal {}", n)),
                Some(Expr::Bool(b)) => Some(format!("non-iterable bool literal {}", b)),
                Some(Expr::Nil) => Some("non-iterable nil literal".to_string()),
                Some(Expr::Field { object, field }) => {
                    if let Expr::Name(obj_name) = object.as_ref() {
                        if obj_name == "math" && matches!(
                            field.as_str(),
                            "huge" | "pi" | "maxinteger" | "mininteger"
                        ) {
                            Some(format!("non-iterable non-callable {}.{}", obj_name, field))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                _ => None,
            };
            // Pre-absorption register inspection — catches literals the
            // absorb-fallback filter would have papered over.
            if non_iter_reason.is_none() {
                if let Some(RegVal::Expr(e)) = regs.get(a) {
                    non_iter_reason = match e {
                        Expr::Number(n) => Some(format!("non-iterable number literal {}", n)),
                        Expr::Bool(b) => Some(format!("non-iterable bool literal {}", b)),
                        Expr::Nil => Some("non-iterable nil literal".to_string()),
                        _ => None,
                    };
                }
            }
            if let Some(reason) = non_iter_reason {
                let raw = if *prep_pc < code.len() {
                    code[*prep_pc]
                } else {
                    0
                };
                stmts.push(Stat::Comment(format!(
                    "-- lifter error: GenericFor iterator is {}  raw_opcode=0x{:08x}",
                    reason, raw
                )));
            } else {
                stmts.push(Stat::GenericFor {
                    vars: var_names,
                    iterators: iter_exprs,
                    body: body_stmts,
                });
            }
        }

        // Phase B0.5, Shape B: an `if <cond> then <for-loop> end` inside a
        // numeric-for body. Emitted by `structure_numeric_for_body` when a
        // forward conditional jump's target range contains a nested for-loop.
        //
        // Without this arm, the nested for-loop would be extracted correctly
        // (Shape A / B0.4 path), but the Linear segment preceding the inner
        // FORNPREP/FORGPREP would contain the JumpIf* whose D target lands
        // past the Linear segment's end (because the structurer already
        // split the range at the inner prep). `lift_instruction_range` would
        // then fire its "forward jump beyond range" fallback at that PC and
        // emit a spurious `if cond then break end` that doesn't exist in the
        // source. This arm bypasses that by:
        //   1) extracting the branch condition from `cond_pc` directly,
        //   2) iterating `body` in structured form (Linear → in-loop lift,
        //      non-Linear → recursive `lift_region`),
        //   3) emitting a single `Stat::If` with the nested body.
        //
        // `in_loop: true` is preserved on the Linear children so any real
        // `break`s inside the then-body still translate into Stat::Break.
        //
        // If `extract_branch_condition` cannot decode the condition it
        // returns `Expr::Bool(true)` (the always-true fallback) — in that
        // case we inline the body directly without an `if` wrapper, matching
        // the IfThenElse handler's behavior.
        Region::InlineIfThenInLoop { cond_pc, body } => {
            let condition = extract_branch_condition(ctx, proto, *cond_pc, regs);

            let mut then_stmts = Vec::new();
            for sub_region in body {
                match sub_region {
                    Region::Linear { start, end } => {
                        lift_instruction_range(
                            ctx, proto, proto_index, depth,
                            *start, *end,
                            regs, locals, &mut then_stmts, true,
                        );
                    }
                    _ => {
                        lift_region(
                            ctx, proto, proto_index, cfg, sub_region,
                            regs, locals, depth + 1, &mut then_stmts,
                        );
                    }
                }
            }

            if matches!(&condition, Expr::Bool(true)) {
                stmts.extend(then_stmts);
            } else {
                stmts.push(Stat::If {
                    condition,
                    then_body: then_stmts,
                    elseif_clauses: vec![],
                    else_body: None,
                });
            }
        }
    }
}

/// B0.57 — hoist `Stat::Local { names: [n], values: [init] }` statements out
/// of a branch body when register `n` survives the post-branch merge (its
/// `Expr::Name(n)` appears in `regs_after_merge`). Replaces the Local inside
/// the branch with a plain `Stat::Assign` so the inside-body semantics stay.
/// Conservative: only hoists when the RHS init value references only
/// expressions that were already available pre-branch (so the hoist doesn't
/// move code past its dependencies).
///
/// `hoisted_names` accumulates across calls (for the else-branch pass) so we
/// don't emit duplicate `local` declarations for the same name.
fn hoist_escaping_locals(
    body: &mut Vec<Stat>,
    regs_after_merge: &[RegVal],
    regs_before: &[RegVal],
    hoisted_out: &mut Vec<Stat>,
    hoisted_names: &mut std::collections::HashSet<String>,
) {
    // Collect the set of names that post-merge still resolve to themselves
    // (i.e. the register escapes the branch).
    let mut escaping: std::collections::HashSet<String> = std::collections::HashSet::new();
    for slot in regs_after_merge.iter() {
        if let RegVal::Expr(Expr::Name(n)) = slot {
            escaping.insert(n.clone());
        }
    }
    if escaping.is_empty() {
        return;
    }

    // Pre-branch names: what was already bound before the if. Used to
    // decide whether an init RHS is "safe" to hoist (references only
    // pre-existing values).
    let mut pre_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for slot in regs_before.iter() {
        if let RegVal::Expr(Expr::Name(n)) = slot {
            pre_names.insert(n.clone());
        } else if let RegVal::LoopVar(n) = slot {
            pre_names.insert(n.clone());
        }
    }

    for stmt in body.iter_mut() {
        let (name_opt, init_opt) = match stmt {
            Stat::Local { names, values } if names.len() == 1 && values.len() == 1 => {
                (Some(names[0].clone()), Some(values[0].clone()))
            }
            _ => (None, None),
        };
        let Some(name) = name_opt else { continue; };
        let Some(init) = init_opt else { continue; };
        if !escaping.contains(&name) {
            continue;
        }
        if hoisted_names.contains(&name) {
            // Already hoisted by the other branch — convert to Assign here
            // so we don't shadow-redeclare.
            *stmt = Stat::Assign {
                targets: vec![Expr::Name(name.clone())],
                values: vec![init],
            };
            continue;
        }
        if !is_safe_hoist_init(&init, &pre_names) {
            continue;
        }
        hoisted_out.push(Stat::Local {
            names: vec![name.clone()],
            values: vec![init.clone()],
        });
        hoisted_names.insert(name.clone());
        *stmt = Stat::Assign {
            targets: vec![Expr::Name(name)],
            values: vec![init],
        };
    }
}

/// B0.57 helper — is `init` safe to hoist out of a branch? Safe means the
/// expression uses only literals or identifiers that existed pre-branch.
/// Branch-local temps or compound expressions with register-reads are NOT
/// safe (hoisting would reference something not yet computed).
fn is_safe_hoist_init(init: &Expr, pre_names: &std::collections::HashSet<String>) -> bool {
    match init {
        Expr::Nil | Expr::Bool(_) | Expr::Number(_) | Expr::String(_) | Expr::Varargs => true,
        Expr::Name(n) => pre_names.contains(n),
        // An empty table literal is always safe — this covers the common
        // `local M = {}` module-table seed case that's the whole point of
        // the hoist. Non-empty constructors reference arbitrary expressions
        // so we bail on those.
        Expr::Table { fields } => fields.is_empty(),
        _ => false,
    }
}

/// Merge register state after an if/else branch: keep values both branches
/// agree on, reset to Unknown where they diverge. `regs` holds the else-path
/// state, `other` holds the then-path state.
///
/// Phase B0.56: if ONE side is `Expr::Name(n)` and the other side is Unknown
/// or a compound expression, preserve the Name. Rationale: the Name almost
/// always refers to a local that was declared on one of the two paths (e.g.
/// `local v16 = {}` emitted inside an `if` branch); resetting to Unknown
/// makes the lifter forget the declaration and emit `v16.field = X` as a
/// bare global write downstream. In real Roblox modules this is the root
/// cause of missing `local v16 = {}` declarations in BaseCamera-style
/// scripts. Preserving the Name allows subsequent SetTableKS handlers to
/// continue treating the register as a known local.
/// Phase B0.80/B0.85: compare two expressions for structural equality.
/// Used by `merge_regs` to preserve identical register values across
/// branch merges. Handles all expression types recursively so that
/// registers holding call results, arithmetic, unary ops, etc. that
/// were NOT modified in either branch are correctly preserved.
///
/// This is purely a structural AST comparison — it does NOT imply that
/// two structurally equal call expressions return the same value. The
/// correctness guarantee comes from the fact that merge_regs compares
/// CLONES of the same pre-branch register state: if neither branch
/// modified the register, both clones hold the exact same expression.
fn exprs_structurally_equal(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (Expr::Nil, Expr::Nil) => true,
        (Expr::Bool(x), Expr::Bool(y)) => x == y,
        (Expr::Number(x), Expr::Number(y)) => x.to_bits() == y.to_bits(),
        (Expr::String(x), Expr::String(y)) => x == y,
        (Expr::Varargs, Expr::Varargs) => true,
        (Expr::Name(x), Expr::Name(y)) => x == y,
        (Expr::Field { object: o1, field: f1 }, Expr::Field { object: o2, field: f2 }) => {
            f1 == f2 && exprs_structurally_equal(o1, o2)
        }
        // Phase B0.85: extend to compound expressions. These are safe
        // because merge_regs compares cloned pre-branch state — if
        // neither branch modified the register, both sides are
        // structurally identical clones of the same expression.
        (Expr::Index { object: o1, key: k1 }, Expr::Index { object: o2, key: k2 }) => {
            exprs_structurally_equal(o1, o2) && exprs_structurally_equal(k1, k2)
        }
        (Expr::BinOp { op: op1, left: l1, right: r1 },
         Expr::BinOp { op: op2, left: l2, right: r2 }) => {
            op1 == op2 && exprs_structurally_equal(l1, l2) && exprs_structurally_equal(r1, r2)
        }
        (Expr::UnOp { op: op1, operand: o1 },
         Expr::UnOp { op: op2, operand: o2 }) => {
            op1 == op2 && exprs_structurally_equal(o1, o2)
        }
        (Expr::Call { func: f1, args: a1 }, Expr::Call { func: f2, args: a2 }) => {
            a1.len() == a2.len()
                && exprs_structurally_equal(f1, f2)
                && a1.iter().zip(a2.iter()).all(|(x, y)| exprs_structurally_equal(x, y))
        }
        (Expr::MethodCall { object: o1, method: m1, args: a1 },
         Expr::MethodCall { object: o2, method: m2, args: a2 }) => {
            m1 == m2
                && a1.len() == a2.len()
                && exprs_structurally_equal(o1, o2)
                && a1.iter().zip(a2.iter()).all(|(x, y)| exprs_structurally_equal(x, y))
        }
        (Expr::Table { fields: f1 }, Expr::Table { fields: f2 }) => {
            f1.len() == f2.len()
                && f1.iter().zip(f2.iter()).all(|(a, b)| table_fields_equal(a, b))
        }
        // Function expressions: skip — full body comparison is expensive
        // and function literals are rarely in registers across branches.
        // Vector: compare all three components by bits.
        (Expr::Vector(x1, y1, z1), Expr::Vector(x2, y2, z2)) => {
            x1.to_bits() == x2.to_bits()
                && y1.to_bits() == y2.to_bits()
                && z1.to_bits() == z2.to_bits()
        }
        _ => false,
    }
}

/// Helper for table field structural comparison.
fn table_fields_equal(a: &crate::ast::TableField, b: &crate::ast::TableField) -> bool {
    use crate::ast::TableField;
    match (a, b) {
        (TableField::Sequential(e1), TableField::Sequential(e2)) => {
            exprs_structurally_equal(e1, e2)
        }
        (TableField::Named(k1, v1), TableField::Named(k2, v2)) => {
            k1 == k2 && exprs_structurally_equal(v1, v2)
        }
        (TableField::Indexed(k1, v1), TableField::Indexed(k2, v2)) => {
            exprs_structurally_equal(k1, k2) && exprs_structurally_equal(v1, v2)
        }
        _ => false,
    }
}

fn merge_regs(regs: &mut Vec<RegVal>, other: &[RegVal]) {
    for (i, slot) in regs.iter_mut().enumerate() {
        if i >= other.len() {
            // Stay with current slot — caller's side may have a valid Name
            // that we shouldn't drop just because the other side is short.
            continue;
        }
        // Phase B0.80: compare register values using structural equality
        // for expressions.  Before this fix, only Name/LoopVar/Unknown
        // matched — identical constant values (Number, String, Bool, Nil,
        // Field chains) from untouched registers were reset to Unknown
        // after any if/else merge, losing the value for all subsequent reads.
        let same = match (&slot, &other[i]) {
            (RegVal::Expr(a), RegVal::Expr(b)) => exprs_structurally_equal(a, b),
            (RegVal::LoopVar(a), RegVal::LoopVar(b)) => a == b,
            (RegVal::Unknown, RegVal::Unknown) => true,
            _ => false,
        };
        if same {
            continue;
        }
        // B0.56: preserve a Name on either side rather than resetting to
        // Unknown. A declared local is valid on whichever path declared it,
        // and keeping the Name lets downstream opcode handlers continue
        // emitting valid `name.field = X` statements.
        let self_name = matches!(&slot, RegVal::Expr(Expr::Name(_)));
        let other_name = matches!(&other[i], RegVal::Expr(Expr::Name(_)));
        if self_name && !other_name {
            // Keep self's Name.
            continue;
        }
        if other_name && !self_name {
            *slot = other[i].clone();
            continue;
        }
        // Neither side is a simple Name (both compound or one side is
        // Unknown without a Name counterpart) — safe to reset.
        *slot = RegVal::Unknown;
    }
}

/// Extract a condition expression from a branch instruction at the given PC.
///
/// Returns the FALL-THROUGH condition, i.e., the condition under which the jump
/// is NOT taken and execution continues to the next instruction. This is because
/// callers (IfThenElse, WhileDo, RepeatUntil) treat the fall-through path as
/// the "then" body / loop body / exit condition respectively.
fn extract_branch_condition(
    ctx: &mut DecompileContext,
    proto: &Proto,
    pc: usize,
    regs: &[RegVal],
) -> Expr {
    let code = &proto.code;
    if pc >= code.len() {
        return Expr::Bool(true);
    }

    let insn = code[pc];
    let op = LuauOpcode::from_u8(insn_op(insn));
    let a = insn_a(insn) as usize;
    let aux = if op.has_aux() && pc + 1 < code.len() { Some(code[pc + 1]) } else { None };

    match op {
        LuauOpcode::JumpIf => {
            // JumpIf jumps when A is truthy; fall-through when A is falsy
            Expr::UnOp { op: UnOp::Not, operand: Box::new(reg_expr(regs, a)) }
        }
        LuauOpcode::JumpIfNot => {
            // JumpIfNot jumps when A is falsy; fall-through when A is truthy
            reg_expr(regs, a)
        }
        LuauOpcode::JumpIfEq => {
            // JumpIfEq jumps when A == AUX; fall-through when A ~= AUX
            let right = reg_expr(regs, (aux.unwrap_or(0) & 0xFF) as usize);
            Expr::BinOp { left: Box::new(reg_expr(regs, a)), op: BinOp::NotEq, right: Box::new(right) }
        }
        LuauOpcode::JumpIfNotEq => {
            // JumpIfNotEq jumps when A ~= AUX; fall-through when A == AUX
            let right = reg_expr(regs, (aux.unwrap_or(0) & 0xFF) as usize);
            Expr::BinOp { left: Box::new(reg_expr(regs, a)), op: BinOp::Eq, right: Box::new(right) }
        }
        LuauOpcode::JumpIfLT => {
            // JumpIfLT jumps when A < AUX; fall-through when A >= AUX
            let right = reg_expr(regs, (aux.unwrap_or(0) & 0xFF) as usize);
            Expr::BinOp { left: Box::new(reg_expr(regs, a)), op: BinOp::GE, right: Box::new(right) }
        }
        LuauOpcode::JumpIfLE => {
            // JumpIfLE jumps when A <= AUX; fall-through when A > AUX
            let right = reg_expr(regs, (aux.unwrap_or(0) & 0xFF) as usize);
            Expr::BinOp { left: Box::new(reg_expr(regs, a)), op: BinOp::GT, right: Box::new(right) }
        }
        LuauOpcode::JumpIfNotLT => {
            // JumpIfNotLT jumps when A >= AUX; fall-through when A < AUX
            let right = reg_expr(regs, (aux.unwrap_or(0) & 0xFF) as usize);
            Expr::BinOp { left: Box::new(reg_expr(regs, a)), op: BinOp::LT, right: Box::new(right) }
        }
        LuauOpcode::JumpIfNotLE => {
            // JumpIfNotLE jumps when A > AUX; fall-through when A <= AUX
            let right = reg_expr(regs, (aux.unwrap_or(0) & 0xFF) as usize);
            Expr::BinOp { left: Box::new(reg_expr(regs, a)), op: BinOp::LE, right: Box::new(right) }
        }
        LuauOpcode::JumpXEqKNil => {
            let negated = aux.unwrap_or(0) & 0x80000000 != 0;
            // Negate: jump-taken has cmp, fall-through has the opposite
            let cmp = if negated { BinOp::Eq } else { BinOp::NotEq };
            Expr::BinOp { left: Box::new(reg_expr(regs, a)), op: cmp, right: Box::new(Expr::Nil) }
        }
        LuauOpcode::JumpXEqKB => {
            let aux_val = aux.unwrap_or(0);
            let negated = aux_val & 0x80000000 != 0;
            let val = (aux_val & 1) != 0;
            // Negate: jump-taken has cmp, fall-through has the opposite
            let cmp = if negated { BinOp::Eq } else { BinOp::NotEq };
            Expr::BinOp { left: Box::new(reg_expr(regs, a)), op: cmp, right: Box::new(Expr::Bool(val)) }
        }
        LuauOpcode::JumpXEqKN | LuauOpcode::JumpXEqKS => {
            let aux_val = aux.unwrap_or(0);
            let negated = aux_val & 0x80000000 != 0;
            let kidx = aux_val & 0x00FFFFFF;
            let right = get_const_expr(proto, &ctx.chunk.strings, kidx);
            // Negate: jump-taken has cmp, fall-through has the opposite
            let cmp = if negated { BinOp::Eq } else { BinOp::NotEq };
            Expr::BinOp { left: Box::new(reg_expr(regs, a)), op: cmp, right: Box::new(right) }
        }
        // For-loop preps with conditional (skip loop)
        LuauOpcode::ForNPrep | LuauOpcode::ForGPrep
        | LuauOpcode::ForGPrepINext | LuauOpcode::ForGPrepNext => {
            Expr::Bool(true) // Condition is implicit in the for-loop
        }
        _ => Expr::Bool(true),
    }
}

/// Find the PC of the branch instruction at the end of a block.
/// `block_end` is exclusive — the first PC NOT in the block.
/// If the last real instruction has an AUX word, block_end-1 is the AUX
/// and block_end-2 is the branch. Otherwise block_end-1 is the branch.
fn find_branch_pc(proto: &Proto, block_end: usize) -> usize {
    if block_end < 2 || block_end > proto.code.len() + 1 {
        return block_end.saturating_sub(1);
    }
    // Check if the instruction at block_end-2 has AUX (making block_end-1 the AUX word)
    let candidate = block_end - 2;
    if candidate < proto.code.len() {
        let insn = proto.code[candidate];
        let op = LuauOpcode::from_u8(insn_op(insn));
        if op.has_aux() {
            return candidate; // branch is at end-2, AUX at end-1
        }
    }
    // No AUX — branch is the last word
    block_end - 1
}

fn remove_trailing_jump(stmts: &mut Vec<Stat>) {
    // Remove trailing comments first (these are often opcode annotations)
    while matches!(stmts.last(), Some(Stat::Comment(_))) {
        stmts.pop();
    }
    // Remove exactly ONE trailing break or continue — the synthetic one from
    // JUMPBACK/FORNLOOP handling. Don't strip more, as additional break/continue
    // statements before it may be legitimate user code.
    if matches!(stmts.last(), Some(Stat::Break) | Some(Stat::Continue)) {
        stmts.pop();
    }
}

/// Phase B0.51 — materialize an implicit `local vN = {}` seed for a register
/// that is about to be used as a SET*/GET* table target but was never written.
///
/// Root cause this fixes:  when the Roblox compiler emits a module-style
/// `local M = {}; M.foo = ...; return M` pattern but the NEWTABLE opcode is
/// missed by our opmap detector (or permuted onto a byte we don't yet
/// recognise), the lifter sees a sequence of SETTABLEKS targeting an
/// undeclared register.  `reg_expr`/`table_expr` synthesize a `v0` name as
/// a fallback, producing orphaned `v0.foo = value` lines with no preceding
/// `local v0 = {}` declaration.  Downstream, `reconstruct_table_constructors`
/// (B0.47) can't fire because there is no empty-table seed statement to
/// absorb the field assigns into.
///
/// This helper runs BEFORE the SET/GET handler uses `table_expr(regs, reg)`.
/// If and only if:
///   * `regs[reg]` is `RegVal::Unknown` (never written in THIS control path),
///     AND
///   * `reg` has never been declared as a local (never classified), AND
///   * `reg` is not a parameter slot
/// then we emit `local vN = {}` via `classify_write` (which also marks the
/// register declared) and seed `regs[reg] = Name(vN)` so the subsequent SET*
/// handler treats the table as "already materialized" and emits the classic
/// `vN.foo = value` assignment shape.  B0.47/B0.48 then collapse these into
/// a proper constructor at end-of-pipeline.
///
/// The guard is intentionally narrow: when the register has any other state
/// (pending Table literal, Name, loop var, etc.) we leave the existing flow
/// untouched so B0.49's shadow-local detection, the B0.3 self-mutation path,
/// and all other accumulated invariants remain unchanged.
pub(super) fn ensure_table_reg_declared(
    ctx: &mut DecompileContext,
    proto: &Proto,
    regs: &mut Vec<RegVal>,
    locals: &mut LocalTracker,
    stmts: &mut Vec<Stat>,
    reg: usize,
    pc: usize,
) {
    if reg >= regs.len() {
        return;
    }
    // Seeding predicate: we materialize a `local vN = {}` when the register
    // contains either
    //   (a) `Unknown` — never written in this proto's control-flow path, OR
    //   (b) a non-table primitive (Bool / Number / String / Nil) that would
    //       be safely ignored by `table_expr` (it falls back to
    //       `Name(v{reg})`) but leaves the register state stale for
    //       subsequent reads.
    // Any NAME / TABLE / FUNCTION / CALL / FIELD / INDEX / BINOP / UNOP /
    // METHODCALL / VECTOR / VARARGS or `LoopVar` reg value means the
    // register is already meaningful and must NOT be clobbered.
    let needs_seed = match &regs[reg] {
        RegVal::Unknown => true,
        RegVal::Expr(Expr::Bool(_))
        | RegVal::Expr(Expr::Number(_))
        | RegVal::Expr(Expr::String(_))
        | RegVal::Expr(Expr::Nil) => true,
        _ => false,
    };
    if !needs_seed {
        return;
    }
    if !locals.is_undeclared_non_param(reg) {
        return;
    }
    if (reg as usize) >= (proto.max_stack_size as usize).max(256) {
        return;
    }

    let seed_name = ctx.reg_name(proto, reg as u8, pc);
    let (kind, name) = locals.classify_write(reg, &seed_name);
    let empty = Expr::Table { fields: vec![] };
    match kind {
        WriteKind::FirstDecl | WriteKind::Shadow => {
            stmts.push(Stat::Local {
                names: vec![name.clone()],
                values: vec![empty],
            });
        }
        WriteKind::Reassign => {
            // Defensive: classify_write shouldn't return Reassign here since
            // the is_undeclared_non_param guard above proves the reg was
            // never declared.  Handle it anyway for completeness.
            stmts.push(Stat::Assign {
                targets: vec![Expr::Name(name.clone())],
                values: vec![empty],
            });
        }
    }
    regs[reg] = RegVal::Expr(Expr::Name(name));
}

/// B0.117: Materialize a register's complex expression as a local when it would
/// produce an invalid lvalue root for a table-write (SETTABLEKS / SETTABLE).
/// Expressions like MethodCall, Call, Table, Function, Vector are valid *values*
/// but not valid *lvalue bases* in Luau — `obj:Method().field = x` is a syntax
/// error. This function checks whether `regs[reg]` would be invalid as a table
/// base for assignment, and if so, emits `local vN = <expr>` and replaces
/// `regs[reg]` with `Name(vN)`.
pub(super) fn ensure_lvalue_base_materialized(
    ctx: &mut DecompileContext,
    proto: &Proto,
    regs: &mut Vec<RegVal>,
    locals: &mut LocalTracker,
    stmts: &mut Vec<Stat>,
    reg: usize,
    pc: usize,
) {
    if reg >= regs.len() {
        return;
    }
    let expr = reg_expr(regs, reg);
    // If the expression is already a valid lvalue root (Name, or Field/Index
    // chain rooted in a Name), no materialization needed.
    if is_lvalue_root(&expr) {
        return;
    }
    // If the expression is Unknown or a primitive that table_expr would replace
    // with a fallback Name anyway, skip — ensure_table_reg_declared handles those.
    if matches!(&regs[reg], RegVal::Unknown) || is_impossible_as_table(&expr) {
        return;
    }
    // The expression is complex (MethodCall, Call, Table, Function, Vector,
    // BinOp, etc.) — materialize it as a local.
    emit_local_or_assign(ctx, proto, regs, locals, stmts, reg, pc, expr);
}

/// Emit a local declaration if this is the first write to a register,
/// otherwise emit a plain assignment. Always emits a statement so the
/// value gets a name and isn't re-inlined at every use site.
///
/// Phase B0.49: uses `classify_write` so that a reassignment to a
/// register with a NEW semantic name shadows the old local with a fresh
/// `local` declaration — preventing emission of a global write to an
/// undeclared name (e.g., `reverse_k_arith = function()...end`).
pub(super) fn emit_local_or_assign(
    ctx: &mut DecompileContext,
    proto: &Proto,
    regs: &mut Vec<RegVal>,
    locals: &mut LocalTracker,
    stmts: &mut Vec<Stat>,
    reg: usize,
    pc: usize,
    value: Expr,
) {
    let new_name = ctx.reg_name(proto, reg as u8, pc);
    let (kind, name) = locals.classify_write(reg, &new_name);
    regs[reg] = RegVal::Expr(Expr::Name(name.clone()));

    match kind {
        WriteKind::FirstDecl | WriteKind::Shadow => {
            stmts.push(Stat::Local {
                names: vec![name],
                values: vec![value],
            });
        }
        WriteKind::Reassign => {
            stmts.push(Stat::Assign {
                targets: vec![Expr::Name(name)],
                values: vec![value],
            });
        }
    }
}

/// Store a computed expression in a register. For simple values (names, literals)
/// and side-effect-free expressions (field chains, index lookups, unary/binary ops
/// with simple operands, empty tables, vectors), just stores in the register for
/// later inlining. For expressions with side effects or unbounded size (calls,
/// method calls, functions, deeply nested ops), emits a local declaration to
/// prevent the expression from being duplicated at every use site.
pub(super) fn store_complex(
    ctx: &mut DecompileContext,
    proto: &Proto,
    regs: &mut Vec<RegVal>,
    locals: &mut LocalTracker,
    stmts: &mut Vec<Stat>,
    reg: usize,
    pc: usize,
    value: Expr,
) {
    // Phase B0.3 fix: self-mutation detection.
    //
    // Pattern: `R1 = R1 + R4` inside a loop body, where R1 is a carried local
    // (e.g. `sum = sum + i`). With lazy inlining, the value `BinOp(Name(v1),
    // Add, Name(i))` looks like a leaf-leaf BinOp → `expr_is_inlinable` → true
    // → store in `regs[1]` without emitting a statement. Result: the body is
    // silently empty and the emitted loop is `for i = 1, n do end` with no
    // update to `sum`.
    //
    // Detect this by checking whether `value` transitively references the
    // destination register's current Name. If so, we MUST emit a statement
    // because inlining would lose the write semantics. This is both narrow
    // (only fires when the destination is already a named local AND the new
    // value reads from it) and safe (fold-only patterns like `R2 = R0 + 1`
    // are unaffected because `value` doesn't reference `R2`).
    let is_self_mutation = match regs.get(reg) {
        Some(RegVal::Expr(Expr::Name(n))) | Some(RegVal::LoopVar(n)) => {
            expr_references_name(&value, n)
        }
        _ => false,
    };

    if !is_self_mutation && expr_is_inlinable(&value) {
        regs[reg] = RegVal::Expr(value);
    } else {
        // B0.127b: sanitize stdlib-name strings when emitting as a statement.
        // This does NOT apply in the inlining branch above because call
        // arguments like `print("game")` must preserve the string literal.
        // Only materialized assignments (`local v4 = "os"`) need conversion.
        let value = sanitize_leaked_global_string(value);
        // Complex expressions with side effects or unbounded size, or
        // self-mutation patterns that must be materialized: emit a statement.
        //
        // Phase B0.3: if the register already holds a `Name(n)` (e.g. because
        // it was pre-materialized before a loop), reuse that name instead of
        // asking `ctx.reg_name` for a new one. `ctx.reg_name` is PC-scoped and
        // can return a DIFFERENT name at different PCs (e.g. `v1` before the
        // loop, `v12` inside the loop), which would break the `sum = sum + i`
        // pattern by renaming the LHS between iterations.
        //
        // Phase B0.49: when there IS no existing Name to reuse (fresh write
        // to this register), run the new name through `classify_write` so
        // shadowing fires when a semantic-rename arrives (e.g., a subsequent
        // arithmetic or CALL write changing the register's meaning).  When
        // we CAN reuse the existing name, we skip the classifier: arithmetic
        // self-mutation (`count = count + 1`) must NOT re-declare `count`.
        //
        // Phase B0.65: before blindly reusing the carried name, peek at the
        // FRESH `Named` hint for this register at the current PC.  Pattern:
        // GETGLOBAL R0 installs `Named("script")` and seeds R0 with
        // `Expr::Name("script")`; the subsequent GETTABLEKS R0, R0 writes
        // `Field(Name("script"), "Parent")` to the same register — old code
        // reused "script" and emitted `local script = script.Parent` even
        // though GETTABLEKS just installed a newer `Named("Parent")` hint at
        // its own PC.  Blind-test corpus had 559 such "local X = X.Y" bugs.
        //
        // When the hint at `pc` yields a DIFFERENT semantic name than the
        // carried one, route through `classify_write` so the newer name
        // wins via Shadow or FirstDecl.  When the hint matches the carried
        // name (the benign `count = count + 1` case — B0.43C's arithmetic
        // name-propagation keeps `Named("count")` current), skip the
        // classifier and reuse the existing name so arithmetic loops never
        // re-declare `count`.
        //
        // Skip the fresh-hint peek when:
        //   * the carried source is `RegVal::LoopVar` (loop-var names are
        //     stable by construction — never shadow them),
        //   * the carried name is a generic `v\d+` fallback (mirrors
        //     `classify_write`'s shadow gate — avoids churn from
        //     counter-bumped generic names).
        //
        // We DO peek on both declared and undeclared registers: the
        // GETGLOBAL/GETTABLEKS pattern sets `regs[reg] = Expr::Name("script")`
        // WITHOUT calling `needs_local` (the global name isn't materialized
        // as a local), so the subsequent store_complex at GETTABLEKS sees
        // an undeclared register with a semantic carried name.  That is
        // exactly where the "local script = script.Parent" bug fires, and
        // we need the peek to catch it.
        let existing_name = match &regs[reg] {
            RegVal::Expr(Expr::Name(n)) | RegVal::LoopVar(n) => Some(n.clone()),
            _ => None,
        };
        let carried_is_loopvar = matches!(&regs[reg], RegVal::LoopVar(_));
        // B0.130b: "self" is a NAMECALL artifact — never preserve it as a
        // reassignment target for non-self values.  Without this,
        // LoadKX/DupClosure closures stored via store_complex emit
        // `self = function()...end` (59 remaining instances after B0.130).
        // Force through the no-carried-name classifier path with a generic
        // replacement name.
        let existing_name = if matches!(existing_name.as_deref(), Some("self"))
            && matches!(&value, Expr::Function { .. })
        {
            None
        } else {
            existing_name
        };
        if let Some(name) = existing_name {
            // Consult the fresh hint when the carried source is a plain
            // `Expr::Name` (not LoopVar).
            //
            // B0.72: also peek when the carried name is GENERIC (`v0`, `v12`).
            // Previously, the peek was gated on `is_semantic_local_name(&name)`,
            // which blocked generic-to-semantic transitions. This caused
            // self-field-access chains (`v0 = v0.Title; v0 = v0.Timer`) to
            // stay as `v0` instead of `local Title = v0.Title; local Timer =
            // Title.Timer`. The inner check `is_semantic_local_name(&fresh)`
            // still prevents generic-to-generic churn.
            //
            // `ctx.reg_name` is idempotent at the same (reg, pc); calling
            // it here caches the synthesized name for downstream lookups
            // at the same PC.
            let do_peek = !carried_is_loopvar;
            let rebind_name: Option<String> = if do_peek {
                let fresh = ctx.reg_name(proto, reg as u8, pc);
                if fresh != name && is_semantic_local_name(&fresh) {
                    Some(fresh)
                } else {
                    None
                }
            } else {
                None
            };

            if let Some(new_name) = rebind_name {
                // Fresh semantic hint differs from carried name → treat as
                // a semantic rebind.  `classify_write` returns Shadow or
                // Reassign depending on `current_names` state; either way
                // the emitted code uses the new name.
                let (kind, final_name) = locals.classify_write(reg, &new_name);
                match kind {
                    WriteKind::FirstDecl | WriteKind::Shadow => {
                        stmts.push(Stat::Local {
                            names: vec![final_name.clone()],
                            values: vec![value],
                        });
                    }
                    WriteKind::Reassign => {
                        stmts.push(Stat::Assign {
                            targets: vec![Expr::Name(final_name.clone())],
                            values: vec![value],
                        });
                    }
                }
                regs[reg] = RegVal::Expr(Expr::Name(final_name));
            } else if locals.needs_local(reg) {
                // Pre-declare path (reg wasn't yet declared but has a carried
                // name — happens for loop-var pre-declare).  Emit Local.
                locals.record_name(reg, &name);
                stmts.push(Stat::Local {
                    names: vec![name.clone()],
                    values: vec![value],
                });
                regs[reg] = RegVal::Expr(Expr::Name(name));
            } else {
                // Already declared, hint matches (or peek was skipped).
                // Keep current name to avoid clobbering the self-mutation
                // pattern (see Phase B0.3 comment above).  Do not overwrite
                // current_names here — the existing binding stands.
                stmts.push(Stat::Assign {
                    targets: vec![Expr::Name(name.clone())],
                    values: vec![value],
                });
                regs[reg] = RegVal::Expr(Expr::Name(name));
            }
        } else {
            // No carried name — freshly compute and go through classifier.
            let mut new_name = ctx.reg_name(proto, reg as u8, pc);
            // B0.130b: "self" escape — same logic as NewClosure handler.
            if new_name == "self" && matches!(&value, Expr::Function { .. }) {
                new_name = format!("fn{}", reg);
            }
            let (kind, name) = locals.classify_write(reg, &new_name);
            match kind {
                WriteKind::FirstDecl | WriteKind::Shadow => {
                    stmts.push(Stat::Local {
                        names: vec![name.clone()],
                        values: vec![value],
                    });
                }
                WriteKind::Reassign => {
                    stmts.push(Stat::Assign {
                        targets: vec![Expr::Name(name.clone())],
                        values: vec![value],
                    });
                }
            }
            regs[reg] = RegVal::Expr(Expr::Name(name));
        }
    }
}

/// Does `expr` transitively reference the local variable `name`?
///
/// Used by `store_complex` to detect self-mutation (`R1 = R1 + X`) inside
/// loops. Recursively walks every sub-expression so that deeply-nested reads
/// still count (e.g. `R1 = (a + R1) * 2`).
pub(super) fn expr_references_name(expr: &Expr, name: &str) -> bool {
    match expr {
        Expr::Name(n) => n == name,
        Expr::Field { object, .. } => expr_references_name(object, name),
        Expr::Index { object, key } => {
            expr_references_name(object, name) || expr_references_name(key, name)
        }
        Expr::UnOp { operand, .. } => expr_references_name(operand, name),
        Expr::BinOp { left, right, .. } => {
            expr_references_name(left, name) || expr_references_name(right, name)
        }
        Expr::Call { func, args } => {
            expr_references_name(func, name)
                || args.iter().any(|a| expr_references_name(a, name))
        }
        Expr::MethodCall { object, args, .. } => {
            expr_references_name(object, name)
                || args.iter().any(|a| expr_references_name(a, name))
        }
        Expr::Table { fields } => fields.iter().any(|f| match f {
            TableField::Sequential(e) => expr_references_name(e, name),
            TableField::Named(_, e) => expr_references_name(e, name),
            TableField::Indexed(k, e) => {
                expr_references_name(k, name) || expr_references_name(e, name)
            }
        }),
        // Vectors carry f32 literals, not nested expressions, so they can
        // never reference a named register.
        Expr::Vector(_, _, _) => false,
        // Phase B0.52P10: ternary references a name iff any sub-expression does.
        Expr::Ternary { cond, then_expr, else_expr } => {
            expr_references_name(cond, name)
                || expr_references_name(then_expr, name)
                || expr_references_name(else_expr, name)
        }
        Expr::Nil
        | Expr::Bool(_)
        | Expr::Number(_)
        | Expr::String(_)
        | Expr::Varargs
        | Expr::Function { .. } => false,
    }
}
/// After lifting a loop body, hoist any `local` declarations that were first
/// introduced inside the loop.  Bytecode registers are function-scoped, so
/// variables assigned inside a loop must be visible after the loop exits.
/// We emit bare `local vN` declarations *before* the loop and rewrite the
/// corresponding `Stat::Local` nodes inside the body to plain `Stat::Assign`.
fn hoist_loop_locals(
    locals: &LocalTracker,
    snap: &HashSet<usize>,
    stmts: &mut Vec<Stat>,
    body: &mut Vec<Stat>,
) {
    let new_regs = locals.new_since(snap);
    if new_regs.is_empty() {
        return;
    }

    // Collect names from Local statements in the body that correspond to
    // newly-declared registers and rewrite them to Assign.
    let mut hoisted_names: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for stat in body.iter() {
        if let Stat::Local { names, .. } = stat {
            for name in names {
                if seen.insert(name.clone()) {
                    hoisted_names.push(name.clone());
                }
            }
        }
    }

    if hoisted_names.is_empty() {
        return;
    }

    // Emit a single hoisted `local` declaration before the loop
    stmts.push(Stat::Local {
        names: hoisted_names.clone(),
        values: vec![],
    });

    // Rewrite matching Local nodes in the body to plain Assign
    let rewrite_set: HashSet<String> = hoisted_names.into_iter().collect();
    rewrite_locals_to_assigns(body, &rewrite_set);
}

/// Rewrite `Stat::Local { names, values }` into `Stat::Assign` when any name
/// is in `rewrite_set`.  Only rewrites top-level statements.
fn rewrite_locals_to_assigns(body: &mut Vec<Stat>, rewrite_set: &HashSet<String>) {
    for stat in body.iter_mut() {
        let should_rewrite = if let Stat::Local { names, .. } = stat {
            names.iter().any(|n| rewrite_set.contains(n))
        } else {
            false
        };
        if should_rewrite {
            if let Stat::Local { names, values } =
                std::mem::replace(stat, Stat::Comment(String::new()))
            {
                let targets: Vec<Expr> = names.into_iter().map(Expr::Name).collect();
                *stat = Stat::Assign { targets, values };
            }
        }
    }
}


/// Determine whether an expression is safe and cheap to inline into every use
/// site rather than being emitted as a local variable.
///
/// An expression is inlinable if it is:
/// 1. Side-effect-free (no function calls)
/// 2. Small enough that duplicating it doesn't bloat the output
///
/// This avoids generating unnecessary locals like:
///   local v5 = game.Players   ->  game.Players.LocalPlayer  (inlined)
///   local v3 = not v2          ->  not v2  (inlined)
///   local v4 = a + b           ->  a + b   (inlined)
fn expr_is_inlinable(expr: &Expr) -> bool {
    match expr {
        // Tier 0: literals and names -- always inlinable
        Expr::Name(_) | Expr::Nil | Expr::Bool(_) | Expr::Number(_)
        | Expr::Varargs | Expr::String(_) | Expr::Vector(..) => true,

        // Tier 1: field chains -- side-effect-free property access.
        // game.Players.LocalPlayer reads better inlined than as a temp local.
        // Only inline if the base object is itself inlinable.
        Expr::Field { object, .. } => expr_is_inlinable(object),

        // Tier 2: index lookups with simple keys -- tbl[1], tbl["key"], tbl[name].
        // Only inline if both object and key are inlinable.
        Expr::Index { object, key } => expr_is_inlinable(object) && expr_is_inlinable(key),

        // Tier 3: unary ops -- `not x`, `-x`, `#x` are tiny and side-effect-free.
        // Only inline if the operand is inlinable.
        Expr::UnOp { operand, .. } => expr_is_inlinable(operand),

        // Tier 4: binary ops -- `a + b`, `x == 5`, `a .. b`.
        // Only inline when BOTH operands are leaf-level (names/literals/field chains)
        // to prevent exponential expression growth from nested inlining.
        Expr::BinOp { left, right, .. } => expr_is_leaf(left) && expr_is_leaf(right),

        // Tier 5: tables -- keep pending so SETTABLEKS/SETLIST can fill them
        // in-place. DUPTABLE creates tables with nil-initialized fields (template
        // keys) that are meant to be overwritten — those must stay pending too.
        Expr::Table { fields } => {
            fields.is_empty()
                || fields.iter().all(|f| matches!(f, TableField::Named(_, Expr::Nil)))
        },

        // Tier 6: ternary -- `if c then a else b` is side-effect-free when
        // all sub-expressions are leaf-level (prevents nested ternary blowup).
        Expr::Ternary { cond, then_expr, else_expr } => {
            expr_is_leaf(cond) && expr_is_leaf(then_expr) && expr_is_leaf(else_expr)
        }

        // Everything else (Call, MethodCall, Function, non-empty Table) has side
        // effects or is too large -- must be emitted as a local.
        _ => false,
    }
}

/// Check if an expression is a "leaf" -- small enough to appear as a BinOp
/// operand without risk of blowup. This is intentionally more restrictive
/// than `expr_is_inlinable` to prevent nested BinOps from being inlined
/// recursively (which would cause exponential duplication).
fn expr_is_leaf(expr: &Expr) -> bool {
    match expr {
        Expr::Name(_) | Expr::Nil | Expr::Bool(_) | Expr::Number(_)
        | Expr::Varargs | Expr::String(_) | Expr::Vector(..) => true,
        Expr::Field { object, .. } => expr_is_leaf(object),
        Expr::UnOp { operand, .. } => expr_is_leaf(operand),
        _ => false,
    }
}

/// Check if an expression references a given variable name anywhere.
fn expr_uses_name(expr: &Expr, name: &str) -> bool {
    match expr {
        Expr::Name(n) => n == name,
        Expr::Field { object, .. } => expr_uses_name(object, name),
        Expr::Index { object, key } => expr_uses_name(object, name) || expr_uses_name(key, name),
        Expr::BinOp { left, right, .. } => expr_uses_name(left, name) || expr_uses_name(right, name),
        Expr::UnOp { operand, .. } => expr_uses_name(operand, name),
        Expr::Call { func, args } => {
            expr_uses_name(func, name) || args.iter().any(|a| expr_uses_name(a, name))
        }
        Expr::MethodCall { object, args, .. } => {
            expr_uses_name(object, name) || args.iter().any(|a| expr_uses_name(a, name))
        }
        Expr::Table { fields } => fields.iter().any(|f| match f {
            TableField::Sequential(e) => expr_uses_name(e, name),
            TableField::Named(_, e) => expr_uses_name(e, name),
            TableField::Indexed(k, v) => expr_uses_name(k, name) || expr_uses_name(v, name),
        }),
        Expr::Function { .. } => false, // closures capture by upvalue, not name
        // Phase B0.92: recurse into Ternary sub-expressions.
        Expr::Ternary { cond, then_expr, else_expr } => {
            expr_uses_name(cond, name) || expr_uses_name(then_expr, name) || expr_uses_name(else_expr, name)
        }
        _ => false,
    }
}

/// Eliminate dead stores: remove assignments that are immediately overwritten
/// by a later assignment to the same register (same variable name).
// ---------------------------------------------------------------------------
// Expression simplification pass
// ---------------------------------------------------------------------------

/// Returns true for constant literals (Nil, Bool, Number, String).
/// Used by boolean-comparison simplifications to avoid folding
/// `"str" == true` into `"str"` (which changes the value).
fn is_literal(expr: &Expr) -> bool {
    matches!(expr, Expr::Nil | Expr::Bool(_) | Expr::Number(_) | Expr::String(_))
}

/// Recursively simplify a single expression.
/// Returns a fully simplified clone — does not mutate in place.
fn simplify_expr(expr: &Expr) -> Expr {
    match expr {
        // ── UnOp simplifications ────────────────────────────────────────────
        Expr::UnOp { op, operand } => {
            let inner = simplify_expr(operand);
            match op {
                // not not x  →  x
                UnOp::Not => {
                    if let Expr::UnOp { op: UnOp::Not, operand: inner2 } = &inner {
                        return simplify_expr(inner2);
                    }
                    // Phase B0.92+B0.97b: constant-fold `not` on known values.
                    // In Lua/Luau, only nil and false are falsy; everything else
                    // (including 0, "", empty tables) is truthy.
                    match &inner {
                        Expr::Nil | Expr::Bool(false) => return Expr::Bool(true),
                        Expr::Bool(true) => return Expr::Bool(false),
                        Expr::Number(_) | Expr::String(_) | Expr::Table { .. } => {
                            return Expr::Bool(false);
                        }
                        _ => {}
                    }
                    // not (a == b)  →  a ~= b
                    // not (a ~= b)  →  a == b
                    // not (a < b)   →  a >= b
                    // not (a <= b)  →  a > b
                    // not (a > b)   →  a <= b
                    // not (a >= b)  →  a < b
                    if let Expr::BinOp { left, op: bop, right } = &inner {
                        let flipped = match bop {
                            BinOp::Eq    => Some(BinOp::NotEq),
                            BinOp::NotEq => Some(BinOp::Eq),
                            BinOp::LT    => Some(BinOp::GE),
                            BinOp::LE    => Some(BinOp::GT),
                            BinOp::GT    => Some(BinOp::LE),
                            BinOp::GE    => Some(BinOp::LT),
                            _ => None,
                        };
                        if let Some(new_op) = flipped {
                            return Expr::BinOp {
                                left: left.clone(),
                                op: new_op,
                                right: right.clone(),
                            };
                        }
                    }
                    Expr::UnOp { op: UnOp::Not, operand: Box::new(inner) }
                }
                // -(-x)  →  x
                UnOp::Negate => {
                    if let Expr::UnOp { op: UnOp::Negate, operand: inner2 } = &inner {
                        return simplify_expr(inner2);
                    }
                    Expr::UnOp { op: UnOp::Negate, operand: Box::new(inner) }
                }
                // #{} → 0
                UnOp::Length => {
                    if let Expr::Table { fields } = &inner {
                        if fields.is_empty() {
                            return Expr::Number(0.0);
                        }
                    }
                    // #nil, #true, #false, #number are runtime errors — replace
                    // with a placeholder since these are decompiler artifacts
                    match &inner {
                        Expr::Nil | Expr::Bool(_) | Expr::Number(_) => {
                            return Expr::Number(0.0);
                        }
                        // #"string" can be folded to a constant
                        Expr::String(s) => {
                            return Expr::Number(s.len() as f64);
                        }
                        _ => {}
                    }
                    Expr::UnOp { op: UnOp::Length, operand: Box::new(inner) }
                }
                // ~x — no simplification rules for bitwise NOT
                UnOp::BNot => Expr::UnOp { op: UnOp::BNot, operand: Box::new(inner) },
            }
        }

        // ── BinOp simplifications ────────────────────────────────────────────
        Expr::BinOp { left, op, right } => {
            let l = simplify_expr(left);
            let r = simplify_expr(right);

            // -- Constant folding: Number op Number --------------------------
            if let (Expr::Number(a), Expr::Number(b)) = (&l, &r) {
                let folded = match op {
                    BinOp::Add  => Some(a + b),
                    BinOp::Sub  => Some(a - b),
                    BinOp::Mul  => Some(a * b),
                    BinOp::Div  if *b != 0.0 => Some(a / b),
                    BinOp::IDiv if *b != 0.0 => Some((a / b).floor()),
                    BinOp::Mod  if *b != 0.0 => Some(a % b),
                    BinOp::Pow  => Some(a.powf(*b)),
                    _ => None,
                };
                if let Some(val) = folded {
                    if val.is_finite() {
                        return Expr::Number(val);
                    }
                }
            }

            // -- Constant folding: String .. String --------------------------
            if matches!(op, BinOp::Concat) {
                if let (Expr::String(a), Expr::String(b)) = (&l, &r) {
                    return Expr::String(format!("{}{}", a, b));
                }
            }

            match op {
                // x + 0  →  x
                BinOp::Add => {
                    if matches!(r, Expr::Number(n) if n == 0.0) { return l; }
                    if matches!(l, Expr::Number(n) if n == 0.0) { return r; }
                }
                // x - 0  →  x
                // x - (-y)  →  x + y  (subtraction of negation)
                BinOp::Sub => {
                    if matches!(r, Expr::Number(n) if n == 0.0) { return l; }
                    if let Expr::UnOp { op: UnOp::Negate, operand } = r {
                        return Expr::BinOp { left: Box::new(l), op: BinOp::Add, right: operand };
                    }
                }
                // x * 1  →  x,  x * 0  →  0,  x * -1  →  -x
                BinOp::Mul => {
                    if matches!(r, Expr::Number(n) if n == 1.0) { return l; }
                    if matches!(l, Expr::Number(n) if n == 1.0) { return r; }
                    if matches!(r, Expr::Number(n) if n == 0.0) { return Expr::Number(0.0); }
                    if matches!(l, Expr::Number(n) if n == 0.0) { return Expr::Number(0.0); }
                    if matches!(r, Expr::Number(n) if n == -1.0) {
                        return Expr::UnOp { op: UnOp::Negate, operand: Box::new(l) };
                    }
                    if matches!(l, Expr::Number(n) if n == -1.0) {
                        return Expr::UnOp { op: UnOp::Negate, operand: Box::new(r) };
                    }
                }
                // x / 1  →  x
                BinOp::Div => {
                    if matches!(r, Expr::Number(n) if n == 1.0) { return l; }
                }
                // x // 1  ->  x
                BinOp::IDiv => {
                    if matches!(r, Expr::Number(n) if n == 1.0) { return l; }
                }
                // x ^ 1  →  x,  x ^ 0  →  1
                BinOp::Pow => {
                    if matches!(r, Expr::Number(n) if n == 1.0) { return l; }
                    if matches!(r, Expr::Number(n) if n == 0.0) { return Expr::Number(1.0); }
                }
                // x .. ""  →  x  (only safe when x is already a string-producing expr,
                //                 but in practice the bytecode only emits CONCAT when
                //                 TOSTRING has already been applied; emit as-is otherwise)
                BinOp::Concat => {
                    if matches!(&r, Expr::String(s) if s.is_empty()) { return l; }
                    if matches!(&l, Expr::String(s) if s.is_empty()) { return r; }
                }
                // true and x   ->  x,   false and x  ->  false
                // nil and x    ->  nil   (nil is falsy, short-circuits)
                // x and x      ->  x     (idempotent)
                BinOp::And => {
                    if matches!(l, Expr::Bool(true))  { return r; }
                    if matches!(l, Expr::Bool(false)) { return Expr::Bool(false); }
                    if matches!(l, Expr::Nil)         { return Expr::Nil; }
                    // Phase B0.97b: x and x → x (idempotent)
                    if exprs_structurally_equal(&l, &r) { return l; }
                }
                // false or x   ->  x,   true or x  ->  true
                // nil or x     ->  x    (nil is falsy, falls through to RHS)
                // x or nil     ->  x    (nil is falsy, or-nil is always identity)
                // x or x       ->  x    (idempotent)
                BinOp::Or => {
                    if matches!(l, Expr::Bool(false)) { return r; }
                    if matches!(l, Expr::Bool(true))  { return Expr::Bool(true); }
                    if matches!(l, Expr::Nil)         { return r; }
                    if matches!(r, Expr::Nil)         { return l; }
                    // Phase B0.97b: x or x → x (idempotent)
                    if exprs_structurally_equal(&l, &r) { return l; }
                }
                // nil == x     ->  x == nil  (convention: nil always on right)
                // x == true    ->  x         (only for non-literal x)
                // x == false   ->  not x     (only for non-literal x)
                BinOp::Eq => {
                    if matches!(l, Expr::Nil) {
                        return Expr::BinOp { left: Box::new(r), op: BinOp::Eq, right: Box::new(Expr::Nil) };
                    }
                    if matches!(r, Expr::Bool(true)) && !is_literal(&l) {
                        return l;
                    }
                    if matches!(r, Expr::Bool(false)) && !is_literal(&l) {
                        return Expr::UnOp { op: UnOp::Not, operand: Box::new(l) };
                    }
                }
                // nil ~= x     ->  x ~= nil
                // x ~= true    ->  not x    (only for non-literal x)
                // x ~= false   ->  x        (only for non-literal x)
                BinOp::NotEq => {
                    if matches!(l, Expr::Nil) {
                        return Expr::BinOp { left: Box::new(r), op: BinOp::NotEq, right: Box::new(Expr::Nil) };
                    }
                    if matches!(r, Expr::Bool(true)) && !is_literal(&l) {
                        return Expr::UnOp { op: UnOp::Not, operand: Box::new(l) };
                    }
                    if matches!(r, Expr::Bool(false)) && !is_literal(&l) {
                        return l;
                    }
                }
                _ => {}
            }
            Expr::BinOp { left: Box::new(l), op: *op, right: Box::new(r) }
        }

        // ── Recurse into sub-expressions that contain Expr children ─────────
        Expr::Field { object, field } => Expr::Field {
            object: Box::new(simplify_expr(object)),
            field: field.clone(),
        },
        Expr::Index { object, key } => Expr::Index {
            object: Box::new(simplify_expr(object)),
            key: Box::new(simplify_expr(key)),
        },
        Expr::Call { func, args } => Expr::Call {
            func: Box::new(simplify_expr(func)),
            args: args.iter().map(simplify_expr).collect(),
        },
        Expr::MethodCall { object, method, args } => Expr::MethodCall {
            object: Box::new(simplify_expr(object)),
            method: method.clone(),
            args: args.iter().map(simplify_expr).collect(),
        },
        Expr::Table { fields } => Expr::Table {
            fields: fields.iter().filter_map(|f| match f {
                TableField::Sequential(e)    => Some(TableField::Sequential(simplify_expr(e))),
                TableField::Named(k, e)      => {
                    let simplified = simplify_expr(e);
                    // Strip named fields with nil values — {x = nil} ≡ {} in Lua
                    if matches!(simplified, Expr::Nil) {
                        None
                    } else {
                        Some(TableField::Named(k.clone(), simplified))
                    }
                }
                TableField::Indexed(k, v)    => Some(TableField::Indexed(simplify_expr(k), simplify_expr(v))),
            }).collect(),
        },
        Expr::Function { params, is_vararg, body } => {
            let mut b = body.clone();
            simplify_stmts(&mut b);
            Expr::Function { params: params.clone(), is_vararg: *is_vararg, body: b }
        }
        // Phase B0.92: recurse into Ternary sub-expressions.
        Expr::Ternary { cond, then_expr, else_expr } => {
            let c = simplify_expr(cond);
            let t = simplify_expr(then_expr);
            let e = simplify_expr(else_expr);
            // Constant-condition fold
            match &c {
                Expr::Bool(true) => return t,
                Expr::Bool(false) | Expr::Nil => return e,
                _ => {}
            }
            // Identical branches: `if c then X else X` → `X`
            if exprs_structurally_equal(&t, &e) {
                return t;
            }
            // Phase B0.94: `if not cond then a else b` → `if cond then b else a`
            // Removes unnecessary negation by swapping branches.
            if let Expr::UnOp { op: UnOp::Not, operand } = c {
                return Expr::Ternary {
                    cond: operand,
                    then_expr: Box::new(e),
                    else_expr: Box::new(t),
                };
            }
            Expr::Ternary {
                cond: Box::new(c),
                then_expr: Box::new(t),
                else_expr: Box::new(e),
            }
        }
        // Leaves — return as-is
        other => other.clone(),
    }
}

/// B0.119: Convert `local fn = function(...) end` to `local function fn(...)`.
/// Recurses into nested blocks. The `local function` form is idiomatic Luau
/// and puts the name in scope during the body (enabling recursion).
fn convert_local_function_sugar(stmts: &mut Vec<Stat>) {
    for stmt in stmts.iter_mut() {
        // Recurse into nested blocks first
        match stmt {
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                convert_local_function_sugar(then_body);
                for (_, body) in elseif_clauses {
                    convert_local_function_sugar(body);
                }
                if let Some(eb) = else_body { convert_local_function_sugar(eb); }
            }
            Stat::While { body, .. } | Stat::Repeat { body, .. }
            | Stat::NumericFor { body, .. } | Stat::GenericFor { body, .. }
            | Stat::DoBlock { body } => convert_local_function_sugar(body),
            Stat::LocalFunction { func, .. } | Stat::MethodFunction { func, .. } => {
                if let Expr::Function { body, .. } = func {
                    convert_local_function_sugar(body);
                }
            }
            _ => {}
        }

        // Convert: `local NAME = function(...) ... end`
        // → `local function NAME(...) ... end`
        if let Stat::Local { names, values } = stmt {
            if names.len() == 1 && values.len() == 1 {
                if matches!(&values[0], Expr::Function { .. }) {
                    let name = names[0].clone();
                    let func = values[0].clone();
                    *stmt = Stat::LocalFunction { name, func };
                }
            }
        }
    }
}

/// Recursively simplify all expressions inside a statement list, and fold
/// constant-condition control flow (`if true`, `if false`, empty while).
fn simplify_stmts(stmts: &mut Vec<Stat>) {
    let mut i = 0;
    while i < stmts.len() {
        // First, simplify expressions within the current statement in place.
        match &mut stmts[i] {
            Stat::Local { values, .. } => {
                for v in values.iter_mut() { *v = simplify_expr(v); }
            }
            Stat::Assign { targets, values } => {
                for t in targets.iter_mut() { *t = simplify_expr(t); }
                for v in values.iter_mut()  { *v = simplify_expr(v); }
            }
            Stat::Return { values } => {
                for v in values.iter_mut() { *v = simplify_expr(v); }
            }
            Stat::ExprStat(e) => { *e = simplify_expr(e); }
            Stat::While { condition, body } => {
                *condition = simplify_expr(condition);
                simplify_stmts(body);
            }
            Stat::Repeat { body, condition } => {
                simplify_stmts(body);
                *condition = simplify_expr(condition);
            }
            Stat::NumericFor { start, stop, step, body, .. } => {
                *start = simplify_expr(start);
                *stop  = simplify_expr(stop);
                if let Some(s) = step { *s = simplify_expr(s); }
                simplify_stmts(body);
            }
            Stat::GenericFor { iterators, body, .. } => {
                for it in iterators.iter_mut() { *it = simplify_expr(it); }
                simplify_stmts(body);
            }
            Stat::DoBlock { body } => { simplify_stmts(body); }
            Stat::If { condition, then_body, elseif_clauses, else_body } => {
                *condition = simplify_expr(condition);
                simplify_stmts(then_body);
                for (cond, body) in elseif_clauses.iter_mut() {
                    *cond = simplify_expr(cond);
                    simplify_stmts(body);
                }
                if let Some(ref mut eb) = else_body { simplify_stmts(eb); }
                // Phase B0.94: `if not cond then A else B end` → `if cond then B else A end`
                // Only when no elseif clauses and else_body exists.
                if elseif_clauses.is_empty() && else_body.is_some() {
                    if let Expr::UnOp { op: UnOp::Not, operand } = condition {
                        *condition = *operand.clone();
                        std::mem::swap(then_body, else_body.as_mut().unwrap());
                    }
                }
            }
            // Phase B0.92: recurse into LocalFunction/MethodFunction bodies.
            Stat::LocalFunction { func, .. } | Stat::MethodFunction { func, .. } => {
                *func = simplify_expr(func);
            }
            _ => {}
        }

        // Now fold constant-condition If statements.
        let replacement: Option<Vec<Stat>> = match &stmts[i] {
            // while false do ... end  →  nothing
            Stat::While { condition: Expr::Bool(false), .. } => Some(vec![]),
            // if true then body [elseif/else] end  →  body
            // (drop the elseif/else branches — `true` short-circuits)
            Stat::If { condition: Expr::Bool(true), then_body, .. } => {
                Some(then_body.clone())
            }
            // if false then _ [elseif/else] end  →  else_body (or nothing)
            Stat::If { condition: Expr::Bool(false), else_body, .. } => {
                Some(else_body.clone().unwrap_or_default())
            }
            _ => None,
        };

        if let Some(mut replacement_stmts) = replacement {
            // Recursively simplify the inlined body before inserting.
            simplify_stmts(&mut replacement_stmts);
            stmts.splice(i..=i, replacement_stmts);
            // Don't advance i — the spliced statements need to be checked too.
        } else {
            i += 1;
        }
    }
}

// ---------------------------------------------------------------------------
/// Also recursively processes nested statements in control flow.
fn eliminate_dead_stores(stmts: &mut Vec<Stat>) {
    let mut i = 0;
    while i < stmts.len() {
        let should_remove = {
            // Look for assignments that have another assignment to the same target shortly after
            if let Stat::Assign { targets, .. } = &stmts[i] {
                if targets.len() == 1 {
                    if let Expr::Name(var_name) = &targets[0] {
                        // Scan forward for the next write to this variable
                        let mut found_overwrite = false;
                        for j in (i + 1)..stmts.len() {
                            match &stmts[j] {
                                // Found an assignment to the same variable - this is a dead store
                                Stat::Assign { targets: t2, values: v2 } if t2.len() == 1 => {
                                    if let Expr::Name(n2) = &t2[0] {
                                        if n2 == var_name {
                                            // Only a dead store if the overwriting RHS
                                            // does NOT read the variable being assigned.
                                            // e.g. `x = 5; x = x + 3` — first store is NOT dead.
                                            let rhs_uses_var = v2.iter().any(|v| expr_uses_name(v, var_name));
                                            if !rhs_uses_var {
                                                found_overwrite = true;
                                            }
                                            break;
                                        }
                                    }
                                }
                                    // Phase B0.92: use stmt_reads_name to precisely check
                                // whether intervening statements reference the variable.
                                // Stops on any read (including through control flow).
                                other => {
                                    if stmt_reads_name(other, var_name) {
                                        break;
                                    }
                                    // Statement doesn't read the variable — safe to continue.
                                }
                            }
                        }
                        found_overwrite
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            }
        };

        if should_remove {
            stmts.remove(i);
            // Don't increment i, check the same position again (it now has the next statement)
        } else {
            // Recursively process nested statements
            match &mut stmts[i] {
                Stat::If { then_body, elseif_clauses, else_body, .. } => {
                    eliminate_dead_stores(then_body);
                    for (_, body) in elseif_clauses.iter_mut() {
                        eliminate_dead_stores(body);
                    }
                    if let Some(ref mut eb) = else_body {
                        eliminate_dead_stores(eb);
                    }
                }
                Stat::While { body, .. } | Stat::Repeat { body, .. } | Stat::DoBlock { body } => {
                    eliminate_dead_stores(body);
                }
                Stat::NumericFor { body, .. } | Stat::GenericFor { body, .. } => {
                    eliminate_dead_stores(body);
                }
                _ => {}
            }
            i += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// C10b: dead `local v_N = { K = "K" }` artifact elimination
// ---------------------------------------------------------------------------

fn is_generic_vn(name: &str) -> bool {
    // Phase C10V: extend to other decompiler-generated generic prefixes.
    // Each prefix is produced by name_from_call_result / RegisterHint fallbacks
    // (mod.rs:436+). Matching them lets the pure-RHS drop in
    // eliminate_dead_key_eq_value_locals sweep dead `local result\d+ = {}` etc.
    // Constrained to exact prefix + all-digit suffix so user names like `result`
    // or `tbl_a` survive unchanged. Corpus counts pre-C10V:
    //   local result\d+ = {}: 651, local fn\d+ = {}: 771, local tbl\d+ = {}: 997.
    const PREFIXES: &[&str] = &["v", "result", "fn", "tbl", "arg"];
    for p in PREFIXES {
        if let Some(rest) = name.strip_prefix(p) {
            if !rest.is_empty() && rest.bytes().all(|c| c.is_ascii_digit()) {
                return true;
            }
        }
    }
    false
}

fn is_key_eq_value_table(expr: &Expr) -> bool {
    if let Expr::Table { fields } = expr {
        if fields.is_empty() { return false; }
        fields.iter().all(|f| match f {
            TableField::Named(k, Expr::String(v)) => k == v,
            _ => false,
        })
    } else {
        false
    }
}

/// C10R: `Expr::Function` with zero params, no vararg, empty body.
/// Emitted by C10f as a placeholder when a child proto failed to lift
/// (opcode_handlers.rs:1518). Dead if the enclosing `local` name is
/// never referenced downstream.
fn is_empty_function_literal(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Function { params, is_vararg: false, body }
            if params.is_empty() && body.is_empty()
    )
}

fn expr_uses_name_deep(expr: &Expr, name: &str) -> bool {
    match expr {
        Expr::Name(n) => n == name,
        Expr::Field { object, .. } => expr_uses_name_deep(object, name),
        Expr::Index { object, key } => expr_uses_name_deep(object, name) || expr_uses_name_deep(key, name),
        Expr::BinOp { left, right, .. } => expr_uses_name_deep(left, name) || expr_uses_name_deep(right, name),
        Expr::UnOp { operand, .. } => expr_uses_name_deep(operand, name),
        Expr::Call { func, args } =>
            expr_uses_name_deep(func, name) || args.iter().any(|a| expr_uses_name_deep(a, name)),
        Expr::MethodCall { object, args, .. } =>
            expr_uses_name_deep(object, name) || args.iter().any(|a| expr_uses_name_deep(a, name)),
        Expr::Table { fields } => fields.iter().any(|f| match f {
            TableField::Sequential(e) => expr_uses_name_deep(e, name),
            TableField::Named(_, e) => expr_uses_name_deep(e, name),
            TableField::Indexed(k, v) => expr_uses_name_deep(k, name) || expr_uses_name_deep(v, name),
        }),
        Expr::Function { body, .. } => body.iter().any(|s| stmt_reads_name_deep(s, name)),
        Expr::Ternary { cond, then_expr, else_expr } =>
            expr_uses_name_deep(cond, name) || expr_uses_name_deep(then_expr, name) || expr_uses_name_deep(else_expr, name),
        _ => false,
    }
}

fn stmt_reads_name_deep(stmt: &Stat, name: &str) -> bool {
    match stmt {
        Stat::Local { values, .. } => values.iter().any(|v| expr_uses_name_deep(v, name)),
        Stat::Assign { targets, values } => {
            values.iter().any(|v| expr_uses_name_deep(v, name))
            || targets.iter().any(|t| match t {
                Expr::Name(_) => false,
                other => expr_uses_name_deep(other, name),
            })
        }
        Stat::ExprStat(e) => expr_uses_name_deep(e, name),
        Stat::Return { values } => values.iter().any(|v| expr_uses_name_deep(v, name)),
        Stat::If { condition, then_body, elseif_clauses, else_body } => {
            expr_uses_name_deep(condition, name)
            || then_body.iter().any(|s| stmt_reads_name_deep(s, name))
            || elseif_clauses.iter().any(|(c, b)|
                expr_uses_name_deep(c, name) || b.iter().any(|s| stmt_reads_name_deep(s, name)))
            || else_body.as_ref().map_or(false, |eb| eb.iter().any(|s| stmt_reads_name_deep(s, name)))
        }
        Stat::While { condition, body } =>
            expr_uses_name_deep(condition, name) || body.iter().any(|s| stmt_reads_name_deep(s, name)),
        Stat::Repeat { body, condition } =>
            body.iter().any(|s| stmt_reads_name_deep(s, name)) || expr_uses_name_deep(condition, name),
        Stat::NumericFor { start, stop, step, body, .. } =>
            expr_uses_name_deep(start, name) || expr_uses_name_deep(stop, name)
            || step.as_ref().map_or(false, |s| expr_uses_name_deep(s, name))
            || body.iter().any(|s| stmt_reads_name_deep(s, name)),
        Stat::GenericFor { iterators, body, .. } =>
            iterators.iter().any(|it| expr_uses_name_deep(it, name))
            || body.iter().any(|s| stmt_reads_name_deep(s, name)),
        Stat::DoBlock { body } => body.iter().any(|s| stmt_reads_name_deep(s, name)),
        _ => false,
    }
}

/// Drop `local v_N = { K = "K", ... }` statements whose RHS is a table of
/// Named(k, String(v)) fields with every k == v, and whose name is never
/// read afterwards in the current scope (including inside nested closures).
///
/// This is a decompiler artifact pattern — real source never writes tables
/// whose field keys literally equal their string values. ~1,650 instances
/// observed across 63 files in the the reference corpus.
fn eliminate_dead_key_eq_value_locals(stmts: &mut Vec<Stat>) {
    // Recurse first so inner scopes are cleaned before we check siblings.
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                eliminate_dead_key_eq_value_locals(then_body);
                for (_, body) in elseif_clauses.iter_mut() {
                    eliminate_dead_key_eq_value_locals(body);
                }
                if let Some(ref mut eb) = else_body {
                    eliminate_dead_key_eq_value_locals(eb);
                }
            }
            Stat::While { body, .. }
            | Stat::Repeat { body, .. }
            | Stat::DoBlock { body }
            | Stat::NumericFor { body, .. }
            | Stat::GenericFor { body, .. } => {
                eliminate_dead_key_eq_value_locals(body);
            }
            Stat::Local { values, .. } | Stat::Assign { values, .. } => {
                for v in values.iter_mut() {
                    if let Expr::Function { body, .. } = v {
                        eliminate_dead_key_eq_value_locals(body);
                    }
                }
            }
            _ => {}
        }
    }

    let mut i = 0;
    while i < stmts.len() {
        let drop = if let Stat::Local { names, values } = &stmts[i] {
            let no_downstream_read = !stmts[i+1..]
                .iter()
                .any(|s| stmt_reads_name_deep(s, &names[0]));
            let no_downstream_write = !stmts[i+1..]
                .iter()
                .any(|s| stmt_writes_name(s, &names[0]));
            // C10Q: drop `local X = { K = "K", ... }` regardless of the
            // local name as long as it is never read OR reassigned in the
            // remainder of this scope. Every field must be Named(k, "k")
            // — this is a distinctive decompiler artifact shape (most
            // common: `local X = { Get = "Get" }` inside deeply-nested
            // closure stubs, ~1611 occurrences in HUD alone). Also covers
            // the original C10b v_N case. The write-check guards against
            // dropping a local whose later reassignment would otherwise
            // silently become a global write.
            let qualifies_key_eq_value = names.len() == 1
                && values.len() == 1
                && is_key_eq_value_table(&values[0])
                && no_downstream_write;
            // C10b original: pure-literal / field-chain RHS at v_N names
            // only (arbitrary names too risky — `local MAX = 100` in user
            // code would incorrectly vanish).
            let qualifies_pure_rhs = names.len() == 1
                && values.len() == 1
                && is_generic_vn(&names[0])
                && is_generic_vn_drop_candidate(&values[0])
                && no_downstream_write;
            // C10N: drop dead method-reference shadows regardless of local
            // name. Artifact shape: `local GetService = game.GetService`
            // where the local is never read afterwards. Emitted when
            // GETTABLEKS method-prep doesn't fuse with the subsequent
            // NAMECALL/CALL. Root object must be a known Roblox/stdlib
            // global so we don't accidentally drop a user's cached
            // reference to a hand-rolled object method.
            let qualifies_method_ref = names.len() == 1
                && values.len() == 1
                && is_dead_method_reference_rhs(&values[0])
                && no_downstream_write;
            // C10R: drop dead empty function stubs — `local X = function() end`
            // with zero params and empty body. These are C10f placeholders for
            // child protos that failed to lift; when the name is never read
            // or reassigned afterwards they carry zero diagnostic value
            // (the file-header aggregate unresolved count already reports
            // them). 5169 corpus occurrences (4152 in HUD alone).
            let qualifies_empty_fn = names.len() == 1
                && values.len() == 1
                && is_empty_function_literal(&values[0])
                && no_downstream_write;
            (qualifies_key_eq_value
                || qualifies_pure_rhs
                || qualifies_method_ref
                || qualifies_empty_fn)
                && no_downstream_read
        } else {
            false
        };
        if drop {
            stmts.remove(i);
        } else {
            i += 1;
        }
    }
}

/// C10N: detect RHS of shape `<root>.<method>` or chains rooted at a known
/// Roblox/stdlib global. Used to drop dead method-reference locals.
fn is_dead_method_reference_rhs(expr: &Expr) -> bool {
    match expr {
        Expr::Field { object, .. } => is_known_global_root(object),
        _ => false,
    }
}

fn is_known_global_root(expr: &Expr) -> bool {
    match expr {
        Expr::Name(n) => matches!(
            n.as_str(),
            "game" | "script" | "workspace" | "shared" | "plugin"
                | "UserSettings" | "UserInputService" | "DebuggerManager"
        ) || is_stdlib_shadow_name(n),
        Expr::Field { object, .. } => is_known_global_root(object),
        Expr::MethodCall { object, .. } => is_known_global_root(object),
        Expr::Call { func, .. } => is_known_global_root(func),
        _ => false,
    }
}

/// C10j: Pure RHS classes we consider safe to drop when the LHS generic `v_N`
/// is never read and never re-assigned downstream. We purposely limit this to
/// shapes that decompilers produce as register artifacts: empty tables, nil,
/// bool/number/string literals, and bare Name/Field/Index expressions that
/// don't touch globals whose evaluation could throw.
fn is_generic_vn_drop_candidate(expr: &Expr) -> bool {
    match expr {
        Expr::Nil | Expr::Bool(_) | Expr::Number(_) | Expr::String(_) => true,
        Expr::Table { fields } if fields.is_empty() => true,
        // `self.X`, `self.X.Y`, `v3.Row` — pure-looking field chains. Evaluating
        // a field access never runs user code in Luau (no metatables on tables
        // without __index), but could error if object is nil. Accept the risk
        // for decompiler artifacts.
        Expr::Field { object, .. } => is_generic_vn_drop_candidate(object),
        Expr::Name(_) => true,
        _ => false,
    }
}

fn stmt_writes_name(stmt: &Stat, name: &str) -> bool {
    match stmt {
        Stat::Assign { targets, .. } => targets.iter().any(|t| matches!(t, Expr::Name(n) if n == name)),
        Stat::Local { names, .. } => names.iter().any(|n| n == name),
        Stat::If { then_body, elseif_clauses, else_body, .. } => {
            then_body.iter().any(|s| stmt_writes_name(s, name))
                || elseif_clauses.iter().any(|(_, b)| b.iter().any(|s| stmt_writes_name(s, name)))
                || else_body.as_ref().map_or(false, |eb| eb.iter().any(|s| stmt_writes_name(s, name)))
        }
        Stat::While { body, .. } | Stat::Repeat { body, .. } | Stat::DoBlock { body } => {
            body.iter().any(|s| stmt_writes_name(s, name))
        }
        Stat::NumericFor { body, .. } | Stat::GenericFor { body, .. } => {
            body.iter().any(|s| stmt_writes_name(s, name))
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Dead code elimination pass
// ---------------------------------------------------------------------------

/// Returns true if a statement unconditionally terminates the current block
/// (return, break, or continue).
fn is_terminator(stmt: &Stat) -> bool {
    matches!(stmt, Stat::Return { .. } | Stat::Break | Stat::Continue)
}

/// Returns true if a statement has side effects that must be preserved even
/// when the result is unused. Function calls may mutate state; assignments
/// to non-local targets (fields, indexing) may be observable.
/// C10g: artifact method calls on non-object globals. Calling `:METHOD()` on
/// things like `setmetatable`, `pairs`, `type` is always decompiler garbage
/// (these are standalone functions, not objects with methods). Treating them
/// as side-effect-free lets the dead-code pass drop empty-if wrappers around
/// them, eliminating hundreds of noise lines like
/// `if setmetatable:_connections() ~= "_connections" then end`.
pub(super) fn is_artifact_method_call(expr: &Expr) -> bool {
    if let Expr::MethodCall { object, .. } = expr {
        if let Expr::Name(n) = object.as_ref() {
            return matches!(
                n.as_str(),
                "setmetatable"
                    | "getmetatable"
                    | "pairs"
                    | "ipairs"
                    | "next"
                    | "tostring"
                    | "tonumber"
                    | "type"
                    | "typeof"
                    | "rawequal"
                    | "rawget"
                    | "rawset"
                    | "rawlen"
                    | "select"
                    | "unpack"
                    | "assert"
                    | "error"
                    | "pcall"
                    | "xpcall"
                    | "loadstring"
                    | "require"
                    | "print"
                    | "warn"
            );
        }
    }
    false
}

fn has_side_effects(expr: &Expr) -> bool {
    match expr {
        // C10g: calls on known non-object globals are artifacts, treat as no-op.
        _ if is_artifact_method_call(expr) => false,
        Expr::Call { .. } | Expr::MethodCall { .. } => true,
        Expr::Field { object, .. } => has_side_effects(object),
        Expr::Index { object, key } => has_side_effects(object) || has_side_effects(key),
        Expr::BinOp { left, right, .. } => has_side_effects(left) || has_side_effects(right),
        Expr::UnOp { operand, .. } => has_side_effects(operand),
        Expr::Table { fields } => fields.iter().any(|f| match f {
            TableField::Sequential(e) => has_side_effects(e),
            TableField::Named(_, e) => has_side_effects(e),
            TableField::Indexed(k, v) => has_side_effects(k) || has_side_effects(v),
        }),
        // Phase B0.92: recurse into Ternary sub-expressions.
        Expr::Ternary { cond, then_expr, else_expr } => {
            has_side_effects(cond) || has_side_effects(then_expr) || has_side_effects(else_expr)
        }
        _ => false,
    }
}

/// Returns true if an if-statement is entirely empty (all branches have no
/// statements) and the condition has no side effects.
fn is_empty_if(stmt: &Stat) -> bool {
    if let Stat::If { condition, then_body, elseif_clauses, else_body } = stmt {
        then_body.is_empty()
            && elseif_clauses.iter().all(|(_, body)| body.is_empty())
            && else_body.as_ref().map_or(true, |b| b.is_empty())
            && !has_side_effects(condition)
    } else {
        false
    }
}

/// C10i: extract the side-effect calls from an expression in evaluation order.
/// Used to turn `if not obj:method() then end` into just `obj:method()`.
/// Only extracts direct Call / MethodCall nodes (and recurses through
/// short-circuit-safe wrappers). For OR (lazy) we only take the LEFT side,
/// since RIGHT may not execute. For AND both run if LEFT is truthy — we take
/// both and accept a slight semantic drift in rare artifact conditions.
fn extract_side_effect_stmts(expr: &Expr) -> Vec<Stat> {
    let mut out = Vec::new();
    extract_into(expr, &mut out);
    out
}

fn extract_into(expr: &Expr, out: &mut Vec<Stat>) {
    match expr {
        Expr::Call { .. } | Expr::MethodCall { .. } => {
            out.push(Stat::ExprStat(expr.clone()));
        }
        Expr::UnOp { operand, .. } => extract_into(operand, out),
        Expr::Field { object, .. } => extract_into(object, out),
        Expr::Index { object, key } => {
            extract_into(object, out);
            extract_into(key, out);
        }
        Expr::BinOp { op, left, right } => {
            extract_into(left, out);
            // `or` is lazy — skip right when left has a definite call (we just
            // extracted it). For `and`, right runs when left is truthy; we
            // conservatively extract both. For relational/arith ops both run.
            if !matches!(op, BinOp::Or) || out.is_empty() {
                extract_into(right, out);
            }
        }
        Expr::Table { fields } => {
            for f in fields {
                match f {
                    TableField::Sequential(e) => extract_into(e, out),
                    TableField::Named(_, e) => extract_into(e, out),
                    TableField::Indexed(k, v) => {
                        extract_into(k, out);
                        extract_into(v, out);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Eliminate unreachable code and empty blocks.
///
/// This pass performs three cleanups on a statement list:
///
/// 1. **Unreachable code after terminators**: After a `return`, `break`, or
///    `continue`, all subsequent statements at the same nesting level are
///    unreachable and can be removed.
///
/// 2. **Empty if-blocks**: `if v9 then end` (no then-body, no else) is a
///    no-op when the condition has no side effects. Remove it entirely.
///    If only some branches are empty, prune those branches.
///
/// 3. **Empty do-blocks**: `do end` with no body is removed.
///
/// The pass recurses into all nested bodies (if/while/repeat/for/do/function).
fn eliminate_dead_code(stmts: &mut Vec<Stat>) {
    // --- Phase 1: Recurse into nested bodies first (bottom-up) ---
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                eliminate_dead_code(then_body);
                for (_, body) in elseif_clauses.iter_mut() {
                    eliminate_dead_code(body);
                }
                if let Some(ref mut eb) = else_body {
                    eliminate_dead_code(eb);
                }
            }
            Stat::While { body, .. } | Stat::Repeat { body, .. } | Stat::DoBlock { body } => {
                eliminate_dead_code(body);
            }
            Stat::NumericFor { body, .. } | Stat::GenericFor { body, .. } => {
                eliminate_dead_code(body);
            }
            _ => {}
        }
    }

    // --- Phase 2: Truncate after terminators ---
    // Find the first unconditional terminator at the top level.
    let mut truncate_after = None;
    for (i, stmt) in stmts.iter().enumerate() {
        if is_terminator(stmt) {
            truncate_after = Some(i);
            break;
        }
        // An if/elseif/else where ALL branches terminate also terminates.
        if let Stat::If { then_body, elseif_clauses, else_body, .. } = stmt {
            if else_body.is_some() {
                let then_exits = exits_on_all_paths(then_body);
                let all_elseif_exit = elseif_clauses.iter().all(|(_, b)| exits_on_all_paths(b));
                let else_exits = else_body.as_ref().map_or(false, |b| exits_on_all_paths(b));
                if then_exits && all_elseif_exit && else_exits {
                    truncate_after = Some(i);
                    break;
                }
            }
        }
    }
    if let Some(idx) = truncate_after {
        stmts.truncate(idx + 1);
    }

    // --- Phase 3: Remove empty if-blocks and empty do-blocks ---
    let mut i = 0;
    while i < stmts.len() {
        // C10i: `if X:method() then end` — keep the call, drop the if-wrap.
        // Only applies when all branches are empty (so the if is pure wrapping).
        if let Stat::If { condition, then_body, elseif_clauses, else_body } = &stmts[i] {
            let all_empty = then_body.is_empty()
                && elseif_clauses.iter().all(|(_, b)| b.is_empty())
                && else_body.as_ref().map_or(true, |b| b.is_empty());
            if all_empty && has_side_effects(condition) {
                let side = extract_side_effect_stmts(condition);
                if !side.is_empty() {
                    stmts.splice(i..=i, side);
                    continue;
                }
            }
        }
        let should_remove = match &stmts[i] {
            // Empty do-block: `do end`
            Stat::DoBlock { body } if body.is_empty() => true,
            // Entirely empty if: `if cond then end` (no side effects in cond)
            stmt if is_empty_if(stmt) => true,
            _ => false,
        };
        if should_remove {
            stmts.remove(i);
        } else {
            i += 1;
        }
    }

    // --- Phase 4: Prune empty branches from if-statements ---
    // If the then-body is empty but there's an else or elseif, we can negate
    // the condition and swap. If an else-body is empty, drop it.
    for stmt in stmts.iter_mut() {
        if let Stat::If { condition, then_body, elseif_clauses, else_body } = stmt {
            // If then-body is empty and there are no elseif clauses, but there
            // IS an else body, negate the condition and swap.
            if then_body.is_empty() && elseif_clauses.is_empty() && !has_side_effects(condition) {
                if let Some(eb) = else_body.take() {
                    if !eb.is_empty() {
                        *condition = negate_condition(condition);
                        *then_body = eb;
                    }
                }
            }
            // Drop empty else body: `if x then ... else end` -> `if x then ... end`
            if let Some(ref eb) = else_body {
                if eb.is_empty() {
                    *else_body = None;
                }
            }
        }
    }
}

/// Negate a boolean condition, trying to produce clean output.
/// `not (a == b)` -> `a ~= b`, `not (not x)` -> `x`, etc.
fn negate_condition(cond: &Expr) -> Expr {
    match cond {
        // not x  ->  x
        Expr::UnOp { op: UnOp::Not, operand } => (**operand).clone(),
        // a == b  ->  a ~= b, etc.
        Expr::BinOp { left, op, right } => {
            let flipped = match op {
                BinOp::Eq    => Some(BinOp::NotEq),
                BinOp::NotEq => Some(BinOp::Eq),
                BinOp::LT    => Some(BinOp::GE),
                BinOp::LE    => Some(BinOp::GT),
                BinOp::GT    => Some(BinOp::LE),
                BinOp::GE    => Some(BinOp::LT),
                _ => None,
            };
            if let Some(new_op) = flipped {
                Expr::BinOp { left: left.clone(), op: new_op, right: right.clone() }
            } else {
                Expr::UnOp { op: UnOp::Not, operand: Box::new(cond.clone()) }
            }
        }
        // true -> false, false -> true
        Expr::Bool(b) => Expr::Bool(!b),
        // General case: wrap in `not`
        _ => Expr::UnOp { op: UnOp::Not, operand: Box::new(cond.clone()) },
    }
}

/// Phase B0.46A: post-AST conversion of `while true do <body>; if cond then break end end`
/// into `repeat <body> until cond`.
///
/// The bytecode-level structuring pass already emits `Region::RepeatUntil`
/// when it can detect a 2-successor back-edge source. But many real
/// `repeat ... until` loops still leak through as `while true do ... break end`
/// because the structurer's CFG-level pattern match misses some shapes
/// (e.g. when the back-edge predecessor block has been split, or when the
/// CFG was lifted before the structural pattern was firmly recognised).
///
/// This is a syntactic safety net that runs after the lifter has produced
/// statements but before `convert_single_pass_loops` rewrites `if cond then
/// break end` chains into nested if/else. We require an EXACT shape so
/// false positives are impossible:
///
///   while true do
///       <body...>          // one or more statements (anything)
///       if <cond> then     // condition is unconstrained
///           break          // EXACTLY one statement: a bare break
///       end                // no elseif, no else
///   end
///
///   →  repeat
///         <body...>
///       until <cond>
///
/// The condition is preserved verbatim — `if not cond then break` becomes
/// `until not cond`, since `repeat ... until X` exits when X is true,
/// which matches the `if X then break` semantics exactly.
///
/// Negative shapes that must NOT convert:
///   - Bare `break` at the end (no wrapping if). The body has no condition.
///   - The if-then has an else-clause or elseif-clauses.
///   - The if-then's body is anything other than exactly one bare `break`.
///   - Statements after the if-then (the if isn't the LAST stmt of the body).
///   - Empty body (no stmts before the if-cond-break) — converting yields
///     `repeat until cond` which is structurally legal but more confusing
///     than the original `while true do if X then break end end`.
fn convert_while_true_break_to_repeat(stmts: &mut Vec<Stat>) {
    // Recurse into every nested block first so inner loops convert before
    // their parents are inspected.
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::While { body, .. }
            | Stat::Repeat { body, .. }
            | Stat::DoBlock { body }
            | Stat::NumericFor { body, .. }
            | Stat::GenericFor { body, .. } => {
                convert_while_true_break_to_repeat(body);
            }
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                convert_while_true_break_to_repeat(then_body);
                for (_, body) in elseif_clauses.iter_mut() {
                    convert_while_true_break_to_repeat(body);
                }
                if let Some(eb) = else_body {
                    convert_while_true_break_to_repeat(eb);
                }
            }
            _ => {}
        }
    }

    // Walk this level and rewrite matching `while true do ... end` nodes.
    for i in 0..stmts.len() {
        let convert = matches!(&stmts[i], Stat::While { condition: Expr::Bool(true), body }
            if matches_repeat_until_shape(body));
        if convert {
            // Replace in place. Take the existing While so we own its body.
            let owned = std::mem::replace(&mut stmts[i], Stat::Break);
            if let Stat::While { body, .. } = owned {
                let mut body = body;
                // The trailing `if cond then break end` was validated by
                // matches_repeat_until_shape above. Pop it off and pull cond.
                let last = body.pop().expect("matches_repeat_until_shape requires non-empty body");
                let cond = match last {
                    Stat::If { condition, .. } => condition,
                    _ => unreachable!("matches_repeat_until_shape ensures last is If"),
                };
                stmts[i] = Stat::Repeat { body, condition: cond };
            } else {
                // Should never happen — `convert` implied While. Restore the
                // sentinel so the slot has something coherent.
                stmts[i] = owned;
            }
        }
    }
}

/// Returns true if `body` ends with `if <cond> then break end` (no elseif,
/// no else, then-body is exactly `[Stat::Break]`) AND there is at least one
/// statement before the trailing if-cond-break (so the converted Repeat has
/// a non-empty body — see `convert_while_true_break_to_repeat` rationale).
fn matches_repeat_until_shape(body: &[Stat]) -> bool {
    if body.len() < 2 {
        return false;
    }
    match body.last() {
        Some(Stat::If { then_body, elseif_clauses, else_body, .. }) => {
            then_body.len() == 1
                && matches!(&then_body[0], Stat::Break)
                && elseif_clauses.is_empty()
                && else_body.is_none()
        }
        _ => false,
    }
}

/// Detect `while true do ... end` blocks that execute at most once (every
/// path through the body exits via `break` or `return`) and convert them
/// into proper `if/else` chains or `do ... end` blocks.
///
/// The most common pattern produced by the structurer:
///   while true do
///       if cond1 then break end
///       ... body1 ...
///       if cond2 then break end
///       ... body2 ...
///       break
///   end
///
/// This is equivalent to:
///   if not cond1 then
///       ... body1 ...
///       if not cond2 then
///           ... body2 ...
///       end
///   end
///
/// We also handle:
///   while true do <body with no back-jumps> end  →  do <body> end
fn convert_single_pass_loops(stmts: &mut Vec<Stat>) {
    // First recurse into sub-blocks so inner loops are converted first
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::While { body, .. } | Stat::Repeat { body, .. }
            | Stat::DoBlock { body }
            | Stat::NumericFor { body, .. } | Stat::GenericFor { body, .. } => {
                convert_single_pass_loops(body);
            }
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                convert_single_pass_loops(then_body);
                for (_, body) in elseif_clauses.iter_mut() {
                    convert_single_pass_loops(body);
                }
                if let Some(ref mut eb) = else_body {
                    convert_single_pass_loops(eb);
                }
            }
            _ => {}
        }
    }

    let mut i = 0;
    while i < stmts.len() {
        let should_convert = match &stmts[i] {
            Stat::While { condition: Expr::Bool(true), body } => {
                is_single_pass_body(body)
            }
            _ => false,
        };

        if should_convert {
            if let Stat::While { body, .. } = stmts.remove(i) {
                let converted = convert_break_chain_to_if_else(body);
                // Splice the converted statements in place
                let count = converted.len();
                for (j, s) in converted.into_iter().enumerate() {
                    stmts.insert(i + j, s);
                }
                // Don't advance i past all inserted stmts — they might need
                // further processing, but since we recursed first they should
                // be clean. Skip past them.
                i += count;
            }
        } else {
            i += 1;
        }
    }
}

/// Check if a loop body always exits (every execution path ends with
/// `break`, `return`, or `continue` — i.e., no path falls through the
/// bottom without an exit statement). This means the "loop" executes at
/// most once.
fn is_single_pass_body(body: &[Stat]) -> bool {
    if body.is_empty() {
        return true; // empty body trivially exits (while true do end → nothing)
    }
    // Check if the body has any `continue` — if so, it really loops
    if body_contains_continue(body) {
        return false;
    }
    // Check if every path through the body exits
    exits_on_all_paths(body)
}

/// Returns true if every execution path through `stmts` terminates with
/// a `break` or `return` (at the current nesting level).
fn exits_on_all_paths(stmts: &[Stat]) -> bool {
    if stmts.is_empty() {
        return false;
    }
    // Check if there's a top-level break or return anywhere
    for (idx, stmt) in stmts.iter().enumerate() {
        match stmt {
            Stat::Break | Stat::Return { .. } => return true,
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                // If we have if/elseif/else where ALL branches exit, the whole thing exits
                let then_exits = exits_on_all_paths(then_body);
                let elseifs_exit = elseif_clauses.iter().all(|(_, b)| exits_on_all_paths(b));
                let else_exits = else_body.as_ref().map_or(false, |eb| exits_on_all_paths(eb));

                if then_exits && elseifs_exit && else_exits {
                    return true;
                }

                // If this if has a `break` in its then-body and NO else, check if
                // remaining stmts after this if also exit. This handles:
                //   if C then break end
                //   ... more code ...
                //   break
                if then_exits && else_body.is_none() && elseif_clauses.is_empty() {
                    // The "else" path falls through to stmts[idx+1..]
                    if exits_on_all_paths(&stmts[idx + 1..]) {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

/// Check if the body contains a `continue` statement at the current nesting
/// level (not inside a nested loop). `continue` means the loop actually iterates.
fn body_contains_continue(stmts: &[Stat]) -> bool {
    for stmt in stmts {
        match stmt {
            Stat::Continue => return true,
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                if body_contains_continue(then_body) { return true; }
                for (_, b) in elseif_clauses {
                    if body_contains_continue(b) { return true; }
                }
                if let Some(eb) = else_body {
                    if body_contains_continue(eb) { return true; }
                }
            }
            // Don't recurse into nested loops — `continue` there refers to the inner loop
            Stat::While { .. } | Stat::Repeat { .. }
            | Stat::NumericFor { .. } | Stat::GenericFor { .. } => {}
            Stat::DoBlock { body } => {
                if body_contains_continue(body) { return true; }
            }
            _ => {}
        }
    }
    false
}

/// Structural equality for `Expr` values.
///
/// `Expr` does not derive `PartialEq` (and shouldn't — floats and deep
/// trees make a blanket Eq semantically tricky), but we need a shallow
/// comparison to detect self-referential assignments like
/// `pairs.GetService = pairs` produced by misidentified SETTABLEKS
/// instructions.
fn expr_structurally_eq(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (Expr::Nil, Expr::Nil) => true,
        (Expr::Bool(x), Expr::Bool(y)) => x == y,
        (Expr::Number(x), Expr::Number(y)) => x.to_bits() == y.to_bits(),
        (Expr::String(x), Expr::String(y)) => x == y,
        (Expr::Varargs, Expr::Varargs) => true,
        (Expr::Name(x), Expr::Name(y)) => x == y,
        (Expr::Field { object: o1, field: f1 }, Expr::Field { object: o2, field: f2 }) => {
            f1 == f2 && expr_structurally_eq(o1, o2)
        }
        (Expr::Index { object: o1, key: k1 }, Expr::Index { object: o2, key: k2 }) => {
            expr_structurally_eq(o1, o2) && expr_structurally_eq(k1, k2)
        }
        _ => false,
    }
}

/// Detect self-referential field assignment: `tbl.field = tbl`.
///
/// This pattern is produced when a NAMECALL instruction is misidentified
/// as SETTABLEKS.  NAMECALL does `R(A+1) = R(B); R(A) = R(B):method`,
/// but SETTABLEKS does `R(B)[K(AUX)] = R(A)`.  When both A and B resolve
/// to the same register value the emitted code becomes the self-referential
/// nonsense `pairs.GetService = pairs` or `v34.connect = v34`.
///
/// Returns true when the assignment target is `tbl.field` or `tbl[key]`
/// and the value expression is structurally equal to the base table
/// expression, indicating a spurious self-assignment that should be
/// suppressed.
pub(super) fn is_self_referential_field_assign(target: &Expr, value: &Expr) -> bool {
    match target {
        Expr::Field { object, .. } => expr_structurally_eq(object, value),
        Expr::Index { object, .. } => expr_structurally_eq(object, value),
        _ => false,
    }
}

/// C10e: detect `x.FindFirstChild = y` and similar patterns where the field
/// is a well-known Roblox Instance method name. These are always decompiler
/// artifacts (normal Luau code never mutates inherited method references)
/// and drop cleanly without altering program semantics.
pub(super) fn is_roblox_method_lvalue_artifact(target: &Expr) -> bool {
    let field_name = match target {
        Expr::Field { field, .. } => field.as_str(),
        _ => return false,
    };
    matches!(
        field_name,
        // Instance core API
        "FindFirstChild"
            | "FindFirstChildOfClass"
            | "FindFirstChildWhichIsA"
            | "FindFirstAncestor"
            | "FindFirstAncestorOfClass"
            | "FindFirstAncestorWhichIsA"
            | "FindFirstDescendant"
            | "WaitForChild"
            | "GetChildren"
            | "GetDescendants"
            | "GetFullName"
            | "GetAttribute"
            | "SetAttribute"
            | "GetAttributes"
            | "GetAttributeChangedSignal"
            | "IsA"
            | "IsAncestorOf"
            | "IsDescendantOf"
            | "Clone"
            | "Destroy"
            | "ClearAllChildren"
            // DataModel / service access
            | "GetService"
            // Signal / RemoteEvent / BindableEvent
            | "Connect"
            | "ConnectParallel"
            | "Once"
            | "Wait"
            | "Fire"
            | "FireServer"
            | "FireClient"
            | "FireAllClients"
            | "Invoke"
            | "InvokeServer"
            | "InvokeClient"
    ) || is_stdlib_constructor_lvalue_artifact(target)
}

/// C10k: drop LHS of the form `<RobloxType>.<constructor>.<field>` — e.g.
/// `UDim2.new.X = v13`, `Instance.new.Parent = x`. These are always
/// decompiler artifacts (can't meaningfully assign to a field of a
/// constructor function) and have been observed in ~200 instances.
///
/// We only match when the DOUBLY-nested object chain looks like
/// `Name("Type").<ConstructorMethod>` and the outer field is anything.
fn is_stdlib_constructor_lvalue_artifact(target: &Expr) -> bool {
    let inner = match target {
        Expr::Field { object, .. } => object.as_ref(),
        _ => return false,
    };
    let (type_name, method_name) = match inner {
        Expr::Field { object, field } => match object.as_ref() {
            Expr::Name(n) => (n.as_str(), field.as_str()),
            _ => return false,
        },
        _ => return false,
    };
    let type_ok = matches!(
        type_name,
        "UDim" | "UDim2" | "Vector2" | "Vector3" | "Vector2int16" | "Vector3int16"
            | "CFrame" | "Color3" | "BrickColor" | "ColorSequence" | "ColorSequenceKeypoint"
            | "NumberSequence" | "NumberSequenceKeypoint" | "NumberRange"
            | "Rect" | "Ray" | "Region3" | "Region3int16"
            | "Axes" | "Faces" | "TweenInfo" | "Random"
            | "Instance" | "Enum" | "DateTime" | "PathWaypoint"
            | "PhysicalProperties" | "OverlapParams" | "RaycastParams"
            | "FloatCurveKey" | "Path2DControlPoint"
    );
    if !type_ok { return false; }
    matches!(
        method_name,
        "new" | "fromScale" | "fromOffset" | "fromRGB" | "fromHSV" | "fromHex"
            | "fromName" | "fromWedgeAngles" | "fromOrientation" | "fromEulerAnglesXYZ"
            | "fromEulerAnglesYXZ" | "fromAxisAngle" | "fromMatrix" | "fromUnit"
            | "lookAt" | "Angles" | "now" | "fromUnixTimestamp" | "fromUnixTimestampMillis"
            | "fromIsoDate" | "fromUniversalTime" | "fromLocalTime"
            | "random" | "palette"
    )
}

/// Convert a break-chain body into proper if/else statements.
///
/// Input (body of a `while true do`):
///   stmt1; stmt2; if C1 then break end; stmt3; if C2 then break end; stmt4; break
///
/// Output:
///   stmt1; stmt2; if not C1 then stmt3; if not C2 then stmt4 end end
fn convert_break_chain_to_if_else(body: Vec<Stat>) -> Vec<Stat> {
    let mut result = Vec::new();
    let mut remaining = body;

    loop {
        if remaining.is_empty() {
            break;
        }

        // Find the first guard-break: `if <cond> then break end` (no else, no elseif)
        let guard_pos = remaining.iter().position(|s| match s {
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                then_body.len() == 1
                    && matches!(&then_body[0], Stat::Break)
                    && elseif_clauses.is_empty()
                    && else_body.is_none()
            }
            _ => false,
        });

        // Also check for a bare `break` at the end
        let bare_break = remaining.last().map_or(false, |s| matches!(s, Stat::Break));

        match guard_pos {
            Some(pos) => {
                // Emit everything before the guard
                for s in remaining.drain(..pos) {
                    if !matches!(&s, Stat::Break) {
                        result.push(s);
                    }
                }
                // Extract the guard condition
                let guard = remaining.remove(0);
                if let Stat::If { condition, .. } = guard {
                    // Everything after the guard becomes the "else" path
                    // (the body that runs when condition is false)
                    let rest = std::mem::take(&mut remaining);

                    // Strip trailing bare break from rest
                    let inner_body = strip_trailing_breaks(rest);

                    if inner_body.is_empty() {
                        // Nothing after the guard — just skip
                        break;
                    }

                    // Recursively convert the inner body
                    let converted_inner = convert_break_chain_to_if_else(inner_body);

                    if converted_inner.is_empty() {
                        break;
                    }

                    let inverted = negate_condition(&condition);
                    result.push(Stat::If {
                        condition: inverted,
                        then_body: converted_inner,
                        elseif_clauses: vec![],
                        else_body: None,
                    });
                }
                break;
            }
            None => {
                // No guard-break found. Check if it's just a plain body with a trailing break.
                if bare_break && remaining.len() > 1 {
                    // Emit everything except the trailing break
                    remaining.pop(); // remove the break
                    result.append(&mut remaining);
                } else if bare_break {
                    // Just a `break` — skip it
                    remaining.pop();
                    result.append(&mut remaining);
                } else {
                    // No break pattern — emit as-is (shouldn't normally happen
                    // since is_single_pass_body verified all paths exit)
                    result.append(&mut remaining);
                }
                break;
            }
        }
    }

    result
}

/// Remove trailing `break` statements from a statement list.
fn strip_trailing_breaks(mut stmts: Vec<Stat>) -> Vec<Stat> {
    while stmts.last().map_or(false, |s| matches!(s, Stat::Break)) {
        stmts.pop();
    }
    stmts
}

// ── Helpers ──

/// Try to absorb a single for-loop setup assignment from the preceding
/// statement for a numeric for-loop.  When LOADN/LOADK/MOVE instructions
/// precede FORNPREP, they appear as `local v5 = 1` etc.  We fold these into
/// the `for i = START, STOP, STEP` header.
///
/// If the last statement is a `local NAME = VALUE` or `NAME = VALUE` where
/// NAME matches the register, remove the statement and return VALUE directly.
/// Otherwise, fall back to the current register contents.
/// Walk a `[start, end)` instruction range and return the set of destination
/// registers (field A) of every instruction whose opcode writes to R(A).
///
/// Used by `Region::NumericFor` to pre-materialize registers that carry live
/// inlinable values across the loop body — without this, LOADN-inlined
/// literals get silently re-folded into self-referential BinOps that never
/// emit a body statement. See the Phase B0.3 comment at the NumericFor call
/// site for the full rationale.
///
/// This is intentionally a lightweight best-effort scan: it doesn't descend
/// into nested regions or interpret AUX words, so the returned set is a
/// SUPERset of the true write set (safe to over-materialize, unsafe to
/// under-materialize). Aux-bearing opcodes advance PC by 2 to avoid
/// mis-reading the AUX word as a new instruction's opcode.
fn collect_body_writes(code: &[u32], start: usize, end: usize) -> Vec<usize> {
    use std::collections::BTreeSet;
    let mut out: BTreeSet<usize> = BTreeSet::new();
    let mut pc = start;
    while pc < end && pc < code.len() {
        let insn = code[pc];
        let op = LuauOpcode::from_u8(insn_op(insn));
        let a = insn_a(insn) as usize;
        // Most AD/ABC opcodes write R(A). The exceptions we care about below
        // are explicitly listed. For unknown/control-flow opcodes we skip the
        // register (safe: they don't produce data that needs to be live).
        match op {
            // Pure register writers: R(A) receives the result.
            LuauOpcode::LoadNil
            | LuauOpcode::LoadB
            | LuauOpcode::LoadN
            | LuauOpcode::LoadK
            | LuauOpcode::LoadKX
            | LuauOpcode::Move
            | LuauOpcode::GetGlobal
            | LuauOpcode::GetUpval
            | LuauOpcode::GetImport
            | LuauOpcode::GetTable
            | LuauOpcode::GetTableKS
            | LuauOpcode::GetTableN
            | LuauOpcode::NewClosure
            | LuauOpcode::DupClosure
            | LuauOpcode::NewTable
            | LuauOpcode::DupTable
            | LuauOpcode::Add
            | LuauOpcode::Sub
            | LuauOpcode::Mul
            | LuauOpcode::Div
            | LuauOpcode::Mod
            | LuauOpcode::Pow
            | LuauOpcode::IDiv
            | LuauOpcode::AddK
            | LuauOpcode::SubK
            | LuauOpcode::MulK
            | LuauOpcode::DivK
            | LuauOpcode::ModK
            | LuauOpcode::PowK
            | LuauOpcode::IDivK
            | LuauOpcode::And
            | LuauOpcode::Or
            | LuauOpcode::AndK
            | LuauOpcode::OrK
            | LuauOpcode::Concat
            | LuauOpcode::Not
            | LuauOpcode::Minus
            | LuauOpcode::Length
            | LuauOpcode::SubRK
            | LuauOpcode::DivRK
            | LuauOpcode::NameCall
            | LuauOpcode::Call
            | LuauOpcode::Band
            | LuauOpcode::Bor
            | LuauOpcode::Bxor
            | LuauOpcode::Bnot
            | LuauOpcode::Shl
            | LuauOpcode::Shr
            | LuauOpcode::Bandk
            | LuauOpcode::Bork
            | LuauOpcode::RbxExt92
            | LuauOpcode::RbxExt93
            | LuauOpcode::RbxExt94
            | LuauOpcode::RbxExt95
            | LuauOpcode::RbxExt96
            | LuauOpcode::RbxExt97
            | LuauOpcode::RbxExt98
            | LuauOpcode::RbxExt99
            | LuauOpcode::RbxExt100
            | LuauOpcode::RbxExt101
            | LuauOpcode::RbxExt102
            | LuauOpcode::RbxExt103
            | LuauOpcode::RbxExt104
            | LuauOpcode::RbxExt105
            | LuauOpcode::FastCall1
            | LuauOpcode::FastCall2
            | LuauOpcode::FastCall2K
            | LuauOpcode::GetVarargs => {
                out.insert(a);
            }
            _ => {}
        }
        // Advance PC: skip AUX word for opcodes that carry one.
        if op.has_aux() {
            pc += 2;
        } else {
            pc += 1;
        }
    }
    out.into_iter().collect()
}

fn absorb_numeric_for_setup(stmts: &mut Vec<Stat>, regs: &[RegVal], reg: usize) -> Expr {
    let reg_name = match regs.get(reg) {
        Some(RegVal::Expr(Expr::Name(n))) => n.as_str(),
        _ => return reg_expr(regs, reg),
    };

    if let Some(last) = stmts.last() {
        let (names, values) = match last {
            Stat::Local { names, values } if names.len() == 1 && values.len() == 1 => {
                (names.as_slice(), values.as_slice())
            }
            Stat::Assign { targets, values } if targets.len() == 1 && values.len() == 1 => {
                if let Expr::Name(n) = &targets[0] {
                    if n == reg_name {
                        let val = values[0].clone();
                        stmts.pop();
                        return val;
                    }
                }
                return reg_expr(regs, reg);
            }
            _ => return reg_expr(regs, reg),
        };

        if names[0] == reg_name {
            let val = values[0].clone();
            stmts.pop();
            return val;
        }
    }
    reg_expr(regs, reg)
}

/// Try to absorb the iterator setup from the preceding statement for a generic
/// for-loop.  When a CALL immediately precedes FORGPREP, the emitted code
/// typically looks like `local v5, v6, v7 = pairs(t)`.  We want to fold that
/// into `for k, v in pairs(t)` instead of `for k, v in v5`.
///
/// Returns the list of iterator expressions to pass to `Stat::GenericFor`:
///   - `[call_expr]` when a matching preceding `local v = pairs(t)` / `v = pairs(t)`
///     assignment is absorbed (the common `for k, v in pairs(t) do` shape).
///   - `[regs[a], regs[a+1], regs[a+2]]` fallback when no absorption is
///     possible — trailing `Nil`/`Unknown` registers are trimmed, so the
///     result has between 1 and 3 elements.
///
/// The 3-element fallback is essential for `for k, v in next, t do` and
/// `for k, v in next, t, nil do` style code, which the Luau compiler emits as:
///   ```text
///   GETIMPORT r_a next
///   MOVE      r_{a+1} t
///   [LOADNIL  r_{a+2}]            -- optional, defaults to nil
///   FORGPREP_NEXT r_a -> D
///   ```
/// There is *no* `CALL` in this shape — the compiler has already set up the
/// three-value iterator triple in registers — so the absorb path fails and
/// pre-Phase-B0.7 would render it as `for k, v in next do`, losing the table.
fn absorb_iterator_setup(stmts: &mut Vec<Stat>, regs: &[RegVal], a: usize) -> Vec<Expr> {
    if let Some(RegVal::Expr(Expr::Name(reg_name))) = regs.get(a) {
        let reg_name = reg_name.clone();
        if let Some(last) = stmts.last() {
            // Extract the first assigned name and the value expression
            let matched = match last {
                Stat::Local { names, values } if !names.is_empty() && values.len() == 1 => {
                    Some((names[0].as_str(), values))
                }
                Stat::Assign { targets, values } if !targets.is_empty() && values.len() == 1 => {
                    if let Expr::Name(n) = &targets[0] {
                        Some((n.as_str(), values))
                    } else {
                        None
                    }
                }
                _ => None,
            };

            if let Some((first_name, values)) = matched {
                // Check if the assigned name matches register a and value is a call
                if first_name == reg_name {
                    let value = &values[0];
                    if matches!(value, Expr::Call { .. } | Expr::MethodCall { .. }) {
                        let expr = value.clone();
                        stmts.pop();
                        return vec![expr];
                    }
                }
            }
        }
    }

    // Fallback: build an explicit iterator tuple from regs[a..a+3].
    // This recovers `for k, v in next, t do` style source that the Luau
    // compiler emits without a preceding `CALL`.
    //
    // Only whitelisted Expr variants are accepted for the state/control
    // slots — everything else is treated as noise and elided. The whitelist
    // is narrow on purpose: real iterator state/control are almost always
    // a Name, Field, Index, or a Call/MethodCall result (when absorption
    // couldn't fold the preceding CALL). Literals (Nil/Bool/Number/String),
    // arithmetic (BinOp/UnOp), tables, vectors, and — crucially — Function
    // placeholders (including the lifter's "unresolved closure" comment
    // shape) are rejected because surfacing them inside the `for ... in`
    // header is strictly worse than omitting the slot. See Phase B0.7
    // sweep: file `06039b020c557365_33314b` had a FORGPREP where regs[a+2]
    // held an unresolved NEWCLOSURE, which we initially rendered as a
    // multi-line `function() -- unresolved closure end` inside the
    // `for ... in` clause — completely unreadable.
    fn is_valid_iterator_slot(e: &Expr) -> bool {
        matches!(
            e,
            Expr::Name(_)
                | Expr::Field { .. }
                | Expr::Index { .. }
                | Expr::Call { .. }
                | Expr::MethodCall { .. }
                | Expr::Varargs
        )
    }

    let generator = {
        let raw = reg_expr(regs, a);
        if is_valid_iterator_slot(&raw) {
            raw
        } else {
            // Phase B0.119: generator register can hold a leaked NAMECALL
            // method-name string (e.g. Expr::String) which produces invalid
            // `for k, v in "MethodName" do`.  Fall back to a register name.
            Expr::Name(format!("v{}", a))
        }
    };
    let second = regs.get(a + 1).and_then(|r| match r {
        RegVal::Expr(e) if is_valid_iterator_slot(e) => Some(e.clone()),
        _ => None,
    });
    // Third iterator (initial control). Include only if it's a real non-nil
    // expression — Luau's `for k, v in f, s do` is equivalent to `... f, s, nil do`,
    // and LOADNIL on r_{a+2} produces the canonical nil default we should elide.
    let third = regs.get(a + 2).and_then(|r| match r {
        RegVal::Expr(e) if is_valid_iterator_slot(e) => Some(e.clone()),
        _ => None,
    });

    // Phase B0.8: deduplicate same-name iterators.
    //
    // Root cause: both GETIMPORT R[A] and GETGLOBAL R[A+1] can resolve to the
    // same constant string — observed as `for k, v in pairs, pairs do` on
    // `06038f010d557365_10830b.luac` where GETIMPORT R0 and GETGLOBAL R1 both
    // yielded Name("pairs") from the same K0="pairs" constant slot. The B0.7
    // fallback faithfully surfaced both registers, producing the duplicate.
    //
    // Dedup rule (narrow): if the state candidate is `Expr::Name(s)` and the
    // generator is also `Expr::Name(g)` where s == g, drop the state. This is
    // strictly limited to exact-same-Name pairs — it does NOT collapse
    //   • `Name("next"), Name("t")`           (different names → preserved ✓)
    //   • `Field { .. }, Field { .. }`        (Fields not compared → preserved ✓)
    //   • `Name("x"), Name("y")`              (different names → preserved ✓)
    // Only `Name("pairs"), Name("pairs")` and analogous identical-Name pairs
    // are collapsed, which is always an artifact of duplicate GETIMPORT/GETGLOBAL
    // resolution, not a user-intentional self-referential iterator triple.
    fn same_name(gen: &Expr, state: &Expr) -> bool {
        match (gen, state) {
            (Expr::Name(g), Expr::Name(s)) => g == s,
            _ => false,
        }
    }

    let mut iterators = vec![generator.clone()];
    if let Some(s) = second {
        if !same_name(&generator, &s) {
            iterators.push(s.clone());
            // Third only makes sense if the second was also present (a `nil, x`
            // initial control without a state is invalid Luau).
            if let Some(t) = third {
                if !same_name(&generator, &t) && !same_name(&s, &t) {
                    iterators.push(t);
                }
            }
        }
    }

    // Phase B0.9: generator-call folding for known iterator names.
    //
    // When the fallback path produces exactly [Name("pairs"), state] or
    // [Name("ipairs"), state], fold into [pairs(state)] / [ipairs(state)].
    // This recovers the common `for k, v in pairs(t) do` source form.
    //
    // Background: Luau's FORGPREP_NEXT / FORGPREP_INEXT optimizations inline
    // `pairs(t)` as a direct register triple — GETIMPORT generator + MOVE state
    // + LOADNIL control — without a preceding CALL.  The B0.7 fallback
    // faithfully surfaces those registers; B0.9 then refolds the known-iterator
    // names back into call syntax, recovering the original source.
    //
    // Whitelist is intentionally narrow:
    //   • "pairs"  → `pairs(state)`  ✓
    //   • "ipairs" → `ipairs(state)` ✓
    //   • "next"   → NOT folded.  `for k, v in next, t do` is valid Luau that
    //     appears in real source (direct use of the `next` iterator function)
    //     and must not be silently rewritten as `next(t)`, which has different
    //     semantics: `next(t)` returns the first key/value pair, not an
    //     iterator triple.
    //   • Arbitrary functions — NOT folded.  We never emit spurious `f(s)`.
    //
    // B0.8 dedup runs first, so by the time we reach this check the vec is
    // already free of duplicate-name artifacts.
    // The absorption path returns early (vec![call_expr]), so this only fires
    // on the register-triple fallback.
    const FOLDABLE_ITERATORS: &[&str] = &["pairs", "ipairs"];
    let should_fold = iterators.len() == 2
        && matches!(&iterators[0], Expr::Name(n) if FOLDABLE_ITERATORS.contains(&n.as_str()));
    if should_fold {
        if let Expr::Name(gen_name) = iterators.remove(0) {
            let state = iterators.remove(0);
            iterators.push(Expr::Call {
                func: Box::new(Expr::Name(gen_name)),
                args: vec![state],
            });
        }
    }

    iterators
}

pub(super) fn reg_expr(regs: &[RegVal], idx: usize) -> Expr {
    match regs.get(idx) {
        Some(RegVal::Expr(e)) => e.clone(),
        Some(RegVal::LoopVar(s)) => Expr::Name(s.clone()),
        _ => Expr::Name(format!("v{}", idx)),
    }
}

/// Like `reg_expr`, but guards against bare string/number/bool literals being
/// used as a table base.  In valid Luau bytecode the table operand of
/// GET/SETTABLE(KS/N) and NAMECALL is always a register holding an object, but
/// when a preceding LOADK loads a string constant (e.g. "game") into the same
/// register, `reg_expr` returns `Expr::String("game")`.  Using that directly
/// produces garbage like `"game".field = val`.  Instead, fall back to the
/// variable name for the register so the output is `vN.field = val`.
///
/// B0.59: also reject compound expressions whose result type CAN'T be a table
/// (arithmetic BinOp → number, comparison BinOp → bool, Concat → string,
/// UnOp Length/Negate/BNot → number, UnOp Not → bool). Without this, the
/// corpus had ~200 garbage emissions like `(#v32).lastUpdate = X`,
/// `(-self).Visible = ...`, `(Instance + self).Magnitude = X` — all invalid
/// Luau. And/Or ops are KEPT: `a or b` can legitimately yield a table
/// in Lua's short-circuit semantics (common `t or default_t` pattern).
pub(super) fn table_expr(regs: &[RegVal], idx: usize) -> Expr {
    let e = reg_expr(regs, idx);
    if is_impossible_as_table(&e) {
        return Expr::Name(format!("v{}", idx));
    }
    e
}

/// B0.59 — is this expression's runtime type known to be NOT a table?
/// Used by `table_expr` to reject invalid table bases that would emit
/// `(number).field = X` or similar. Conservative: expressions with
/// ambiguous/polymorphic result types (And, Or, Call, MethodCall,
/// Field, Index, Name, Varargs) are allowed through.
pub(super) fn is_impossible_as_table(e: &Expr) -> bool {
    match e {
        Expr::String(_) | Expr::Number(_) | Expr::Bool(_) | Expr::Nil => true,
        Expr::UnOp { op, .. } => matches!(
            op,
            UnOp::Negate | UnOp::Length | UnOp::BNot | UnOp::Not
        ),
        Expr::BinOp { op, .. } => matches!(
            op,
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div
            | BinOp::Mod | BinOp::Pow | BinOp::IDiv | BinOp::Concat
            | BinOp::Eq | BinOp::NotEq
            | BinOp::LT | BinOp::LE | BinOp::GT | BinOp::GE
            | BinOp::BAnd | BinOp::BOr | BinOp::BXor
            | BinOp::Shl | BinOp::Shr
            // And / Or intentionally NOT rejected — short-circuit semantics
            // mean `a or b` can legitimately yield a table operand.
        ),
        _ => false,
    }
}

pub(super) fn mk_binop(regs: &[RegVal], left: usize, right: usize, op: BinOp) -> Expr {
    let left_expr = reg_expr(regs, left);
    let right_expr = reg_expr(regs, right);
    // B0.58: arithmetic ops on non-numeric literals are always a misfire
    // (the instruction was misidentified, or upstream register state was
    // corrupted). Returning `Expr::BinOp(Mod, Bool(false), Bool(false))`
    // produces `false % false` in the output, a runtime error in real
    // Luau and visibly garbage in the corpus (62 occurrences before this
    // guard). Reject when EITHER operand is a non-numeric literal and
    // return the left operand as the salvage value.
    //
    // Previously: only guarded against Expr::String (from B0.43-era). Now
    // includes Bool and Nil (the common forms seen in practice).
    let is_numeric_op = matches!(op,
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div
        | BinOp::Mod | BinOp::Pow | BinOp::IDiv
        // B0.121: bitwise ops also require numeric operands; strings from
        // NAMECALL method name leaking produce `bit32.band("PrimaryPart", ...)`.
        | BinOp::BAnd | BinOp::BOr | BinOp::BXor);
    if is_numeric_op {
        let bad = |e: &Expr| matches!(e,
            Expr::String(_) | Expr::Bool(_) | Expr::Nil);
        if bad(&left_expr) || bad(&right_expr) {
            // Prefer returning a side that isn't itself a non-numeric
            // literal; if both are, fall back to left (matches B0.43
            // behavior for pure-string operands).
            if !matches!(left_expr, Expr::String(_) | Expr::Bool(_) | Expr::Nil) {
                return left_expr;
            }
            if !matches!(right_expr, Expr::String(_) | Expr::Bool(_) | Expr::Nil) {
                return right_expr;
            }
            return left_expr;
        }
    }
    // B0.126: And/Or string leakage guard. NAMECALL/GETTABLEKS AUX strings
    // leak into registers and get picked up as And/Or operands, producing
    // garbage like `v3 and "GetPlayers"`, `workspace or "workspace"`.
    // Real Luau `x and "literal"` / `x or "default"` patterns always have
    // user-visible strings (error messages, defaults with spaces, etc.).
    // Identifier-shaped strings (valid Luau identifiers matching method/
    // property names) are always leakage. Guard: if either operand is
    // Expr::String(s) where s is a valid identifier, replace it with the
    // register name fallback.
    if matches!(op, BinOp::And | BinOp::Or) {
        let is_ident_string = |e: &Expr| -> bool {
            if let Expr::String(s) = e {
                !s.is_empty()
                    && s.chars().next().map_or(false, |c| c.is_ascii_alphabetic() || c == '_')
                    && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            } else {
                false
            }
        };
        let left_leaked = is_ident_string(&left_expr);
        let right_leaked = is_ident_string(&right_expr);
        if left_leaked && right_leaked {
            // Both are leaked strings — return left register name.
            return Expr::Name(format!("v{}", left));
        }
        if right_leaked {
            // Right is leaked — return left (the meaningful operand).
            return left_expr;
        }
        if left_leaked {
            // Left is leaked — return right (the meaningful operand).
            return right_expr;
        }
    }
    Expr::BinOp {
        left: Box::new(left_expr),
        op,
        right: Box::new(right_expr),
    }
}

pub(super) fn mk_binop_k(proto: &Proto, strings: &[String], regs: &[RegVal], left: usize, kidx: u32, op: BinOp) -> Expr {
    // For arithmetic ops, the constant should be a Number. If it's not (e.g.,
    // Import/String constant in dead code), return just the constant expression
    // to avoid garbage like `v0 % "game"`.
    let right_expr = get_const_expr(proto, strings, kidx);
    let left_expr = reg_expr(regs, left);
    if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Pow | BinOp::IDiv) {
        if !matches!(right_expr, Expr::Number(_)) {
            return right_expr;
        }
        // B0.73: If left operand is non-numeric (String, Table, Bool, Function),
        // this is a Roblox passthrough K-variant — return just the left register.
        // Evidence: `"Pulse" + 100`, `table.create("Name" + 2)`.
        if matches!(left_expr, Expr::String(_) | Expr::Table { .. } | Expr::Bool(_)
                           | Expr::Function { .. }) {
            return left_expr;
        }
    }
    // B0.121: bitwise K-ops (BANDK, BORK) also require numeric constant.
    // String constants (from NAMECALL method name leaking into the constant
    // index) produce garbage like `bit32.band("PrimaryPart", "PrimaryPart")`.
    if matches!(op, BinOp::BAnd | BinOp::BOr | BinOp::BXor) {
        if matches!(right_expr, Expr::String(_)) {
            return left_expr;
        }
        if matches!(left_expr, Expr::String(_) | Expr::Table { .. } | Expr::Bool(_)
                           | Expr::Function { .. }) {
            return left_expr;
        }
    }
    // B0.126: And/Or K-constant string leakage guard. Same logic as mk_binop:
    // identifier-shaped strings are NAMECALL leakage, not real operands.
    if matches!(op, BinOp::And | BinOp::Or) {
        let is_ident_string = |e: &Expr| -> bool {
            if let Expr::String(s) = e {
                !s.is_empty()
                    && s.chars().next().map_or(false, |c| c.is_ascii_alphabetic() || c == '_')
                    && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            } else {
                false
            }
        };
        let left_leaked = is_ident_string(&left_expr);
        let right_leaked = is_ident_string(&right_expr);
        if left_leaked && right_leaked {
            return Expr::Name(format!("v{}", left));
        }
        if right_leaked {
            return left_expr;
        }
        if left_leaked {
            return right_expr;
        }
    }
    Expr::BinOp {
        left: Box::new(left_expr),
        op,
        right: Box::new(right_expr),
    }
}

/// Phase B0.67 — construct a unary op while rejecting operand types that
/// make the op trivially invalid. Mirrors the pattern from `mk_binop`
/// (B0.58): when a misidentified opcode reads a register holding an
/// Instance/Name/Field expression, the lifter would previously emit
/// `-ReplicatedStorage`, `#game`, `~"string"`, etc. — syntactically valid
/// Luau but runtime errors and visibly garbage in the corpus.
///
/// Per-op rejection rules:
///   * `Negate` (unary minus): reject Bool, Nil, String, Table, Function,
///     Varargs, and bare `Name(n)` where `is_stdlib_shadow_name(n)` (those
///     name Roblox services/instances exclusively: `game`, `workspace`,
///     `script`, `task`, `pcall`, etc.).
///   * `Not`: no operand is rejected. Luau's `not x` is defined for all
///     types.
///   * `Length` (`#x`): reject Bool, Nil, Number, Function, Varargs, and
///     `Name(n)` with `is_stdlib_shadow_name(n)`. (Strings and tables are
///     the only legitimate targets; strings not listed here because
///     `#"literal"` is rare but legal.)
///   * `BNot` (bitwise `~`): reject Bool, Nil, String, Table, Function,
///     Varargs, and `Name(n)` with `is_stdlib_shadow_name(n)`.
///
/// Salvage: return `Expr::Name(format!("v{}", src))` — same shape as
/// `reg_expr`'s fallback, matching the spirit of B0.58's mk_binop
/// salvage (which returns a non-literal operand).
pub(crate) fn mk_unop(regs: &[RegVal], src: usize, op: UnOp) -> Expr {
    let operand = reg_expr(regs, src);
    let is_stdlib_name = |e: &Expr| matches!(e, Expr::Name(n) if is_stdlib_shadow_name(n));
    let rejected = match op {
        UnOp::Not => false, // `not x` accepts any operand type in Luau
        UnOp::Negate => matches!(
            &operand,
            Expr::Bool(_) | Expr::Nil | Expr::String(_)
                | Expr::Table { .. } | Expr::Function { .. } | Expr::Varargs
        ) || is_stdlib_name(&operand),
        UnOp::Length => matches!(
            &operand,
            Expr::Bool(_) | Expr::Nil | Expr::Number(_)
                | Expr::Function { .. } | Expr::Varargs
        ) || is_stdlib_name(&operand),
        UnOp::BNot => matches!(
            &operand,
            Expr::Bool(_) | Expr::Nil | Expr::String(_)
                | Expr::Table { .. } | Expr::Function { .. } | Expr::Varargs
        ) || is_stdlib_name(&operand),
    };
    if rejected {
        // Phase B0.99: return the source register's expression as-is.
        // Previously returned `v{src}`, losing the real name/value.
        // For Roblox passthrough opcodes (type annotations), this
        // preserves the source register's expression through the no-op.
        return operand;
    }
    Expr::UnOp { op, operand: Box::new(operand) }
}

/// B0.68 — is this expression an impossible operand for the Luau `..` concat
/// operator? Luau's concat only accepts strings and numbers at runtime;
/// anything else (bool, nil, table, function, vararg) is a type error.
///
/// When the lifter sees one of these on either side of a concat it means
/// either (a) the opcode was misidentified as CONCAT, or (b) upstream
/// register state was corrupted. Either way, emitting
/// `Expr::BinOp { op: Concat, .. }` against an impossible operand produces
/// visibly broken output like `((v1 .. false) .. v3) .. false` in the
/// corpus (observed dozens of times pre-B0.68).
///
/// The `is_stdlib_shadow_name` branch catches the common misfire where an
/// opcode stream produces a concat whose operand resolved to `workspace`,
/// `script`, `game`, `Players` etc. These globals are never legitimate
/// concat operands — any such emission is always a bug.
///
/// Kept as-is:
///   - `Expr::String`, `Expr::Number`       — valid primitives
///   - `Expr::Name(non-stdlib)`              — could be any typed var
///   - `Expr::Field`, `Expr::Index`          — table access, may be string
///   - `Expr::Call`, `Expr::MethodCall`      — may return string/number
///   - `Expr::BinOp`                          — allows chained `a..b..c`
///   - `Expr::UnOp`                           — allows tostring-style ops
pub(super) fn is_invalid_concat_operand(e: &Expr) -> bool {
    match e {
        Expr::Bool(_) | Expr::Nil | Expr::Function { .. }
        | Expr::Table { .. } | Expr::Varargs => true,
        Expr::Name(n) => is_stdlib_shadow_name(n),
        _ => false,
    }
}

/// B0.68 — build a CONCAT BinOp node, guarding against operand types that
/// Luau's `..` operator cannot consume at runtime. When either side fails
/// `is_invalid_concat_operand`, salvage by returning the other (valid)
/// operand directly rather than emitting a poisoned BinOp. If both are
/// invalid, fall through to `left` — matches the B0.58 `mk_binop` salvage
/// shape.
///
/// This is the CONCAT analogue of the B0.58 arithmetic-guard patch on
/// `mk_binop` and the B0.43 string-rejection patch that preceded it.
/// Before B0.68 the CONCAT handler built the chained BinOp inline with no
/// validation, so misidentified opcodes whose operands resolved to `Bool`,
/// `Nil`, or a global name like `workspace` produced garbage like
/// `((v1 .. false) .. v3) .. false` in 746-script corpus runs.
pub(crate) fn mk_concat(left: Expr, right: Expr) -> Expr {
    let left_bad = is_invalid_concat_operand(&left);
    let right_bad = is_invalid_concat_operand(&right);
    if left_bad || right_bad {
        if !left_bad {
            return left;
        }
        if !right_bad {
            return right;
        }
        return left;
    }
    Expr::BinOp {
        left: Box::new(left),
        op: BinOp::Concat,
        right: Box::new(right),
    }
}

/// Phase B0.127: convert `Expr::String(s)` to `Expr::Name(s)` when `s` is a
/// known stdlib / Roblox global name (per `is_stdlib_shadow_name`).
///
/// In Luau bytecode, global names like `math`, `game`, `workspace`, `task`,
/// etc. are always accessed via GETIMPORT, which produces `Expr::Name(s)`.
/// The Luau compiler never emits LOADK to load these names as string literals.
/// When an `Expr::String("math")` appears as an *assignment value* (right-hand
/// side of SETTABLEKS, SETTABLEN, SETTABLE, SETGLOBAL, SETUPVAL), it is
/// virtually always NAMECALL/AUX leakage or a misidentified opcode reading
/// the constant table at the wrong index.
///
/// Applying this guard in call-argument positions would be wrong: `print("game")`
/// is legitimate Luau. So this function is called ONLY in assignment-value
/// contexts by the per-opcode handlers.
pub(super) fn sanitize_leaked_global_string(val: Expr) -> Expr {
    if let Expr::String(ref s) = val {
        if is_stdlib_shadow_name(s) {
            return Expr::Name(s.clone());
        }
    }
    val
}

/// C10L: detect a NumericFor bound that resolved to a stdlib name reference.
/// `for i = os, v1, v2 do` is always a decompiler artifact from a deep-proto
/// register leak — you cannot iterate from a library table.
pub(super) fn is_stdlib_name_corruption(val: &Expr) -> bool {
    matches!(val, Expr::Name(s) if is_stdlib_shadow_name(s))
}

pub(super) fn emit_assign(stmts: &mut Vec<Stat>, target: Expr, value: Expr) {
    // B0.111: validate that the assignment target is a valid Luau lvalue.
    // Luau only allows assignment to: Name, Name.field, Name[key], and
    // deeper chains rooted in a Name. Expressions like ({})[k] = v,
    // Instance.new(x).field = v, or (function()end).f = v are syntactically
    // invalid and cause parse failures. When the root is not a Name, emit
    // as a comment instead of a broken assignment.
    if !is_valid_lvalue(&target) {
        // Phase B0.115: include the invalid target in the comment for diagnostics.
        let target_dbg = format!("{:?}", target);
        let value_dbg = format!("{:?}", value);
        let trunc_t = if target_dbg.len() > 80 { &target_dbg[..80] } else { &target_dbg };
        let trunc_v = if value_dbg.len() > 80 { &value_dbg[..80] } else { &value_dbg };
        stmts.push(Stat::Comment(format!(
            "invalid lvalue: {} = {}", trunc_t, trunc_v
        )));
        return;
    }
    stmts.push(Stat::Assign {
        targets: vec![target],
        values: vec![value],
    });
}

/// Check whether an expression is a valid Luau assignment target (lvalue).
/// Valid lvalues must be rooted in a Name that is a valid identifier:
/// `x`, `x.f`, `x[k]`, `x.f.g[k]`, etc. Names that are not valid identifiers
/// (e.g., "AtomicBinding:BindRoot") get emitted as string literals by the
/// emitter, making them invalid lvalue roots.
fn is_valid_lvalue(expr: &Expr) -> bool {
    match expr {
        Expr::Name(n) => is_identifier(n),
        Expr::Field { object, .. } | Expr::Index { object, .. } => {
            is_lvalue_root(object)
        }
        _ => false,
    }
}

/// Walk down field/index chains to find the root expression.
/// Returns true only if the root is a Name with a valid identifier.
fn is_lvalue_root(expr: &Expr) -> bool {
    match expr {
        Expr::Name(n) => is_identifier(n),
        Expr::Field { object, .. } | Expr::Index { object, .. } => {
            is_lvalue_root(object)
        }
        _ => false,
    }
}

/// Quick identifier check: starts with [a-zA-Z_], rest [a-zA-Z0-9_], non-empty.
fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Extract a string from a constant, regardless of its type.
/// Returns the string for String constants, or resolves Import constants
/// to their dot-separated name. Returns None for non-string-like constants.
fn const_to_string(k: &Constant, strings: &[String], proto_constants: &[Constant]) -> Option<String> {
    match k {
        Constant::String(s) => Some(s.clone()),
        Constant::Import(val) => {
            let ids = decode_import(*val);
            let parts: Vec<String> = ids.iter().filter_map(|&id| {
                if let Some(Constant::String(s)) = proto_constants.get(id as usize) {
                    Some(s.clone())
                } else {
                    strings.get(id as usize).cloned()
                }
            }).collect();
            if !parts.is_empty() { Some(parts.join(".")) } else { None }
        }
        _ => None,
    }
}

/// Compute the Luau string hash (matches `luaS_hash` in the Luau VM).
///
/// This is a variant of FNV-1a used by Luau's string table and is the value
/// stored in the AUX word of GETGLOBAL/SETGLOBAL instructions. We use it to
/// build a reverse-lookup table: hash -> string name.
///
/// Algorithm (from Luau `lstring.cpp`):
///   seed = len
///   step = (len >> 5) + 1
///   for i in (len..=1).step_by(step):  seed ^= (seed<<5) + (seed>>2) + byte[i-1]
fn luau_hash(s: &[u8]) -> u32 {
    let len = s.len();
    let mut h = len as u32;
    let step = (len >> 5) + 1;
    let mut i = len;
    while i >= step {
        h ^= h.wrapping_shl(5)
            .wrapping_add(h.wrapping_shr(2))
            .wrapping_add(s[i - 1] as u32);
        i -= step;
    }
    h
}

/// Try to find a string in `candidates` whose Luau hash equals `hash_val`.
/// Returns the first match found. Used as a last-resort lookup when index-based
/// strategies fail — the AUX is likely a hash, not an index.
fn reverse_hash_lookup(candidates: &[String], hash_val: u32) -> Option<String> {
    for s in candidates {
        if luau_hash(s.as_bytes()) == hash_val {
            return Some(s.clone());
        }
    }
    None
}

/// Resolve an AUX value to a string name, trying multiple lookup strategies.
/// Used by GETGLOBAL/SETGLOBAL/GETTABLEKS/SETTABLEKS/NAMECALL.
///
/// Strategy tiers (each tier is tried with successively transformed index values):
///
/// **Tier A — Direct index lookups (raw AUX):**
///   1. proto.constants[aux] as String/Import (0-based, matches Luau VM: VM_KV(aux))
///   2. chunk.strings[aux] (0-based)
///   3. proto.constants[aux-1] (1-based)
///   4. chunk.strings[aux-1] (1-based, matches read_string_ref convention)
///
/// **Tier B — Lower-16-bit masked index** (upper bits may contain hash/flags):
///   5-8. Same four lookups with `aux & 0xFFFF`
///
/// **Tier C — Upper-16-bit index** (some encodings pack index in high half):
///   9-12. Same four lookups with `aux >> 16`
///
/// **Tier D — Lower-10-bit index** (import-style 10-bit field packing):
///   13-16. Same four lookups with `aux & 0x3FF`
///
/// **Tier E — Hash reverse lookup** (AUX is a Luau string hash):
///   17. Compute `luau_hash(s)` for every string in proto.constants and
///       chunk.strings; return the first match.
///   18. Same with `aux & 0xFFFF` as hash (masked variant).
///
/// Returns None if all lookups fail.
fn resolve_aux_string(proto: &Proto, strings: &[String], aux: u32) -> Option<String> {
    // Tier A: raw AUX as index
    if let Some(s) = resolve_aux_string_at(proto, strings, aux) {
        return Some(s);
    }

    // Tier B: lower 16 bits as index (upper bits may be hash/flags)
    let masked16 = aux & 0xFFFF;
    if masked16 != aux {
        if let Some(s) = resolve_aux_string_at(proto, strings, masked16) {
            return Some(s);
        }
    }

    // Tier C: upper 16 bits as index
    let upper16 = aux >> 16;
    if upper16 > 0 && upper16 != aux {
        if let Some(s) = resolve_aux_string_at(proto, strings, upper16) {
            return Some(s);
        }
    }

    // Tier D: lower 10 bits as index (import-style 10-bit packing)
    let masked10 = aux & 0x3FF;
    if masked10 != aux && masked10 != masked16 {
        if let Some(s) = resolve_aux_string_at(proto, strings, masked10) {
            return Some(s);
        }
    }

    // Tier E: Luau hash reverse-lookup.
    // The AUX word for GETGLOBAL/SETGLOBAL is canonically a string hash.
    // For GETTABLEKS/SETTABLEKS/NAMECALL it is normally an index, but when
    // index lookups all fail the AUX may have been misinterpreted and is
    // actually a hash too (or the constant table is incomplete).
    // We search proto string constants first (smaller, more likely), then
    // chunk.strings.

    // Collect all candidate strings from proto.constants (string constants only)
    let proto_strings: Vec<&String> = proto.constants.iter().filter_map(|k| {
        if let Constant::String(s) = k { Some(s) } else { None }
    }).collect();

    // Try hash match against proto string constants
    for s in &proto_strings {
        if luau_hash(s.as_bytes()) == aux {
            return Some((*s).clone());
        }
    }
    // Try hash match against chunk.strings
    if let Some(s) = reverse_hash_lookup(strings, aux) {
        return Some(s);
    }
    // Try masked hash variant
    if masked16 != aux {
        for s in &proto_strings {
            if luau_hash(s.as_bytes()) == masked16 {
                return Some((*s).clone());
            }
        }
        if let Some(s) = reverse_hash_lookup(strings, masked16) {
            return Some(s);
        }
    }

    None
}

/// Try to resolve a single index value to a string via proto.constants and
/// chunk.strings, with 0-based, 1-based, and +1 offset indexing.
fn resolve_aux_string_at(proto: &Proto, strings: &[String], idx: u32) -> Option<String> {
    // Strategy 1: proto.constants with 0-based indexing (primary path)
    if let Some(k) = proto.constants.get(idx as usize) {
        if let Some(s) = const_to_string(k, strings, &proto.constants) {
            return Some(s);
        }
    }
    // Strategy 2: chunk.strings with 0-based indexing
    if let Some(s) = strings.get(idx as usize) {
        return Some(s.clone());
    }
    // Strategy 3: proto.constants with 1-based indexing (idx-1)
    if idx > 0 {
        if let Some(k) = proto.constants.get((idx as usize) - 1) {
            if let Some(s) = const_to_string(k, strings, &proto.constants) {
                return Some(s);
            }
        }
    }
    // Strategy 4: chunk.strings with 1-based indexing (idx-1)
    // The bytecode string table uses 1-based refs (read_string_ref subtracts 1),
    // so AUX values from some opcodes may follow that convention.
    if idx > 0 {
        if let Some(s) = strings.get((idx as usize) - 1) {
            return Some(s.clone());
        }
    }
    // Strategy 5: proto.constants with idx+1 (off-by-one in the other direction)
    if let Some(k) = proto.constants.get((idx as usize) + 1) {
        if let Some(s) = const_to_string(k, strings, &proto.constants) {
            return Some(s);
        }
    }
    // Strategy 6: chunk.strings with idx+1
    if let Some(s) = strings.get((idx as usize) + 1) {
        return Some(s.clone());
    }
    None
}

/// Resolve a global name for GETGLOBAL/SETGLOBAL using both the D field
/// (constant index K[D]) and the AUX word (which may be a hash or index).
///
/// In the Luau VM, GETGLOBAL is `A D [AUX]` where:
///   - K[D] is the String constant holding the global's name
///   - AUX is typically a hash of that string (used for fast table lookup)
///
/// However, in Roblox's shuffled bytecode, AUX sometimes doubles as a
/// constant index. We try K[D] first (the canonical source), then fall back
/// to AUX-based resolution, then finally to known-globals inference.
pub(super) fn resolve_global_name(proto: &Proto, strings: &[String], d: i16, aux: Option<u32>) -> Option<String> {
    // Primary: K[D] — the D field is a signed 16-bit constant index
    let d_unsigned = d as u16 as usize;
    if let Some(k) = proto.constants.get(d_unsigned) {
        if let Some(s) = const_to_string(k, strings, &proto.constants) {
            return Some(s);
        }
    }

    // Also try chunk.strings[D] directly (0-based)
    if let Some(s) = strings.get(d_unsigned) {
        return Some(s.clone());
    }

    // Try 1-based K[D-1]
    if d_unsigned > 0 {
        if let Some(k) = proto.constants.get(d_unsigned - 1) {
            if let Some(s) = const_to_string(k, strings, &proto.constants) {
                return Some(s);
            }
        }
        if let Some(s) = strings.get(d_unsigned - 1) {
            return Some(s.clone());
        }
    }

    // Try K[D+1] (off-by-one other direction)
    if let Some(k) = proto.constants.get(d_unsigned + 1) {
        if let Some(s) = const_to_string(k, strings, &proto.constants) {
            return Some(s);
        }
    }

    // Fallback: AUX-based resolution (multi-strategy including hash reverse lookup)
    if let Some(ax) = aux {
        if let Some(s) = resolve_aux_string(proto, strings, ax) {
            return Some(s);
        }
    }

    None
}

/// Get a string from AUX value for GETTABLEKS/SETTABLEKS.
pub(super) fn get_table_string_from_aux(proto: &Proto, strings: &[String], aux: u32) -> String {
    resolve_aux_string(proto, strings, aux)
        .filter(|s| is_plausible_field_name(s))
        .unwrap_or_else(|| {
            log::warn!(
                "UNRESOLVED field AUX: aux=0x{:08X} ({}) | proto.constants.len()={} | chunk.strings.len()={} | aux_as_insn: op=0x{:02X} A=0x{:02X} B=0x{:02X} C=0x{:02X}",
                aux, aux, proto.constants.len(), strings.len(),
                aux & 0xFF, (aux >> 8) & 0xFF, (aux >> 16) & 0xFF, (aux >> 24) & 0xFF
            );
            format!("field_{}", aux & 0xFFFF)
        })
}

/// Phase B0.113: reject garbage strings resolved by the fallback tiers.
/// Real field/method names never contain control characters, newlines, or
/// null bytes. Pattern strings like `%*\n%*` (format strings or error
/// messages) that get matched via hash reverse-lookup are caught here.
fn is_plausible_field_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && !s.bytes().any(|b| b < 0x20 || b == 0x7F)
}

/// Get a string from AUX value for NAMECALL (method name).
/// Same resolution as get_table_string_from_aux but with "method" fallback prefix
/// so unresolved method names don't masquerade as field accesses.
pub(super) fn get_method_string_from_aux(proto: &Proto, strings: &[String], aux: u32) -> String {
    resolve_aux_string(proto, strings, aux)
        .filter(|s| is_plausible_field_name(s))
        .unwrap_or_else(|| {
            log::warn!(
                "UNRESOLVED method AUX: aux=0x{:08X} ({}) | proto.constants.len()={} | chunk.strings.len()={}",
                aux, aux, proto.constants.len(), strings.len()
            );
            format!("method_{}", aux & 0xFFFF)
        })
}

#[allow(dead_code)]
fn const_type_name(k: &Constant) -> &'static str {
    match k {
        Constant::Nil => "Nil",
        Constant::Boolean(_) => "Bool",
        Constant::Number(_) => "Number",
        Constant::String(_) => "String",
        Constant::Import(_) => "Import",
        Constant::Table(_) => "Table",
        Constant::Closure(_) => "Closure",
        Constant::Vector(..) => "Vector",
    }
}

pub(super) fn get_const_expr(proto: &Proto, strings: &[String], idx: u32) -> Expr {
    // Primary: proto.constants with 0-based indexing (bounds-checked via .get())
    if let Some(k) = proto.constants.get(idx as usize) {
        return constant_to_expr(k, strings, &proto.constants);
    }
    // Fallback: chunk.strings with 0-based indexing
    if let Some(s) = strings.get(idx as usize) {
        return Expr::String(s.clone());
    }
    // AUX is wrong — emit nil instead of a visible placeholder
    Expr::Nil
}

// ---------------------------------------------------------------------------
// Upvalue renaming pass
// ---------------------------------------------------------------------------

/// Returns true if `name` matches the pattern `upval_N` (an unresolved upvalue).
fn is_upval_name(name: &str) -> bool {
    name.starts_with("upval_") && name.len() > 6 && name[6..].chars().all(|c| c.is_ascii_digit())
}

/// Scan a statement list (and all nested bodies) for usage patterns of
/// `upval_N` names. Returns a map from each upval name to the best
/// inferred replacement name (a Roblox global like `game`, `script`, etc.).
///
/// Evidence collection is exhaustive: every expression and statement is walked
/// recursively (including into nested closures). The decision tree at the end
/// is ordered from most-specific to least-specific so that ambiguous cases
/// resolve to the most useful name.
fn infer_upval_names(stmts: &[Stat]) -> HashMap<String, String> {
    #[derive(Default)]
    struct Evidence {
        // --- game ---
        get_service: bool,
        // --- Players (service) ---
        local_player: bool,
        // --- script ---
        find_first_child: bool,
        wait_for_child: bool,
        parent_access: bool,            // .Parent, .Name, .ClassName
        shared_client_server: bool,     // .Shared, .Client, .Server (module hierarchy)
        // --- workspace ---
        workspace_methods: bool,        // :Raycast, :Blockcast, :FindPartOnRay, etc.
        // --- CFrame / Vector3 ---
        cframe_methods: bool,
        has_new_field: bool,            // .new (constructor access -- CFrame.new, etc.)
        // --- Roblox datatype disambiguation for .new ---
        vector3_evidence: bool,         // .Magnitude, .Unit, :Cross, :Dot
        udim2_evidence: bool,           // .fromScale, .fromOffset
        color3_evidence: bool,          // .R, .G, .B, :ToHSV, .fromRGB, .fromHSV
        // --- Instance methods ---
        clone_destroy: bool,            // :Clone, :Destroy, :IsA, etc.
        // --- TweenService ---
        create_method: bool,            // :Create
        // --- Remote events/functions ---
        remote_fire: bool,              // :FireServer, :InvokeServer
        remote_listen: bool,            // .OnServerEvent, .OnClientEvent
        connect_method: bool,           // :Connect, :Once
        // --- Assignment evidence ---
        assigned_from_pairs: bool,      // upval_N = pairs
        assigned_from_ipairs: bool,     // upval_N = ipairs
        assigned_from_require: bool,    // upval_N = require(...)
        // Phase B0.83: direct assignment name from RHS expression
        // (e.g., upval_N = game:GetService("Players") → "Players")
        assigned_name: Option<String>,
        // --- Every field and method accessed (for library heuristics) ---
        all_fields: Vec<String>,
        all_methods: Vec<String>,
    }

    let mut evidence: HashMap<String, Evidence> = HashMap::new();

    fn scan_expr(expr: &Expr, evidence: &mut HashMap<String, Evidence>) {
        match expr {
            Expr::MethodCall { object, method, args, .. } => {
                if let Expr::Name(name) = object.as_ref() {
                    if is_upval_name(name) {
                        let ev = evidence.entry(name.clone()).or_default();
                        ev.all_methods.push(method.clone());
                        match method.as_str() {
                            "GetService" | "FindService" => ev.get_service = true,
                            "FindFirstChild" | "FindFirstChildOfClass"
                            | "FindFirstChildWhichIsA" => ev.find_first_child = true,
                            "WaitForChild" => ev.wait_for_child = true,
                            "Clone" | "Destroy" | "GetChildren" | "GetDescendants"
                            | "IsA" | "GetAttribute" | "SetAttribute"
                            | "GetPropertyChangedSignal" => ev.clone_destroy = true,
                            "Create" => ev.create_method = true,
                            "Connect" | "connect" | "Once" | "once"
                            | "Wait" | "wait" => ev.connect_method = true,
                            // CFrame/rotation methods
                            "toEulerAnglesYXZ" | "ToEulerAnglesYXZ"
                            | "Inverse" | "inverse" | "Lerp" | "lerp"
                            | "ToWorldSpace" | "ToObjectSpace"
                            | "toWorldSpace" | "toObjectSpace"
                            | "PointToWorldSpace" | "PointToObjectSpace"
                            | "VectorToWorldSpace" | "VectorToObjectSpace"
                            | "components" => ev.cframe_methods = true,
                            // Workspace raycasting
                            "Raycast" | "Blockcast" | "Spherecast" | "Shapecast"
                            | "FindPartOnRay" | "FindPartOnRayWithIgnoreList"
                            | "FindPartOnRayWithWhitelist"
                            | "FindPartsInRegion3" | "FindPartsInRegion3WithIgnoreList" => ev.workspace_methods = true,
                            // Remote events/functions
                            "FireServer" | "InvokeServer"
                            | "FireClient" | "InvokeClient"
                            | "FireAllClients" => ev.remote_fire = true,
                            // Vector3 methods
                            "Cross" | "Dot" | "FuzzyEq" => ev.vector3_evidence = true,
                            // Color3 methods
                            "ToHSV" => ev.color3_evidence = true,
                            _ => {}
                        }
                    }
                }
                scan_expr(object, evidence);
                for arg in args { scan_expr(arg, evidence); }
            }
            Expr::Field { object, field, .. } => {
                if let Expr::Name(name) = object.as_ref() {
                    if is_upval_name(name) {
                        let ev = evidence.entry(name.clone()).or_default();
                        ev.all_fields.push(field.clone());
                        match field.as_str() {
                            "LocalPlayer" => ev.local_player = true,
                            "Parent" | "Name" | "ClassName" => ev.parent_access = true,
                            "Shared" | "Client" | "Server" => ev.shared_client_server = true,
                            "new" => ev.has_new_field = true,
                            // Remote event/function signals
                            "OnServerEvent" | "OnClientEvent"
                            | "OnServerInvoke" | "OnClientInvoke" => ev.remote_listen = true,
                            // Vector3-specific fields
                            "Magnitude" | "Unit" => ev.vector3_evidence = true,
                            // Color3-specific fields
                            "R" | "G" | "B" | "fromRGB" | "fromHSV" => ev.color3_evidence = true,
                            // UDim2 hints
                            "fromScale" | "fromOffset" => ev.udim2_evidence = true,
                            _ => {}
                        }
                    }
                }
                scan_expr(object, evidence);
            }
            Expr::Call { func, args } => {
                // Detect upval used as direct call target: upval_N(...)
                if let Expr::Name(name) = func.as_ref() {
                    if is_upval_name(name) {
                        let ev = evidence.entry(name.clone()).or_default();
                        ev.all_methods.push("__call__".to_string());
                    }
                }
                scan_expr(func, evidence);
                for arg in args { scan_expr(arg, evidence); }
            }
            Expr::BinOp { left, right, .. } => {
                scan_expr(left, evidence);
                scan_expr(right, evidence);
            }
            Expr::UnOp { operand, .. } => scan_expr(operand, evidence),
            Expr::Index { object, key } => {
                scan_expr(object, evidence);
                scan_expr(key, evidence);
            }
            Expr::Table { fields } => {
                for f in fields {
                    match f {
                        TableField::Sequential(e) => scan_expr(e, evidence),
                        TableField::Named(_, e) => scan_expr(e, evidence),
                        TableField::Indexed(k, v) => {
                            scan_expr(k, evidence);
                            scan_expr(v, evidence);
                        }
                    }
                }
            }
            Expr::Function { body, .. } => scan_stmts(body, evidence),
            _ => {}
        }
    }

    /// Check assignment RHS for known value patterns.
    ///
    /// Phase B0.83: extended to extract direct names from assignment RHS:
    /// - `upval_N = X:GetService("Players")` → assigned_name = "Players"
    /// - `upval_N = X:FindFirstChild("Module")` → assigned_name = "Module"
    /// - `upval_N = X.FieldName` → assigned_name = "FieldName"
    fn check_assign_value(upval_name: &str, value: &Expr, evidence: &mut HashMap<String, Evidence>) {
        match value {
            Expr::Name(rhs) => {
                match rhs.as_str() {
                    "pairs" => evidence.entry(upval_name.to_string()).or_default().assigned_from_pairs = true,
                    "ipairs" => evidence.entry(upval_name.to_string()).or_default().assigned_from_ipairs = true,
                    _ => {
                        // B0.131b: upval_N = LocalName → use LocalName as
                        // assigned_name fallback (only when the RHS is a
                        // non-generic, non-stdlib semantic identifier).
                        if is_valid_luau_identifier(rhs)
                            && !is_stdlib_shadow_name(rhs)
                            && !is_upval_name(rhs)
                            && !(rhs.starts_with('v') && rhs.len() > 1 && rhs[1..].chars().all(|c| c.is_ascii_digit()))
                            && !(rhs.starts_with("arg") && rhs.len() > 3 && rhs[3..].chars().all(|c| c.is_ascii_digit()))
                            && !(rhs.starts_with("fn") && rhs.len() > 2 && rhs[2..].chars().all(|c| c.is_ascii_digit()))
                        {
                            let ev = evidence.entry(upval_name.to_string()).or_default();
                            if ev.assigned_name.is_none() {
                                ev.assigned_name = Some(rhs.clone());
                            }
                        }
                    }
                }
            }
            Expr::Call { func, .. } => {
                if let Expr::Name(fname) = func.as_ref() {
                    if fname == "require" {
                        evidence.entry(upval_name.to_string()).or_default().assigned_from_require = true;
                    }
                }
            }
            // Phase B0.83: extract name from method call patterns
            // upval_N = X:GetService("Players") → "Players"
            Expr::MethodCall { method, args, .. } => {
                const NAMING_METHODS: &[&str] = &[
                    "GetService", "FindFirstChild", "FindFirstChildOfClass",
                    "FindFirstChildWhichIsA", "FindFirstAncestor",
                    "FindFirstAncestorOfClass", "FindFirstAncestorWhichIsA",
                    "WaitForChild",
                ];
                if NAMING_METHODS.contains(&method.as_str()) {
                    if let Some(Expr::String(s)) = args.first() {
                        if is_valid_luau_identifier(s) && !is_stdlib_shadow_name(s) {
                            let ev = evidence.entry(upval_name.to_string()).or_default();
                            if ev.assigned_name.is_none() {
                                ev.assigned_name = Some(s.clone());
                            }
                        }
                    }
                }
            }
            // Phase B0.83: extract name from field access
            // upval_N = X.FieldName → "FieldName"
            Expr::Field { field, .. } => {
                if is_valid_luau_identifier(field) && !is_stdlib_shadow_name(field) {
                    let ev = evidence.entry(upval_name.to_string()).or_default();
                    if ev.assigned_name.is_none() {
                        ev.assigned_name = Some(field.clone());
                    }
                }
            }
            _ => {}
        }
    }

    fn scan_stmts(stmts: &[Stat], evidence: &mut HashMap<String, Evidence>) {
        for stmt in stmts {
            match stmt {
                Stat::Local { values, .. } => {
                    for v in values { scan_expr(v, evidence); }
                }
                Stat::Assign { targets, values } => {
                    // Check for assignment TO an upval_N (upval_N = <value>)
                    for (i, t) in targets.iter().enumerate() {
                        if let Expr::Name(name) = t {
                            if is_upval_name(name) {
                                if let Some(val) = values.get(i) {
                                    check_assign_value(name, val, evidence);
                                }
                            }
                        }
                        scan_expr(t, evidence);
                    }
                    for v in values { scan_expr(v, evidence); }
                }
                Stat::ExprStat(e) => scan_expr(e, evidence),
                Stat::Return { values } => {
                    for v in values { scan_expr(v, evidence); }
                }
                Stat::If { condition, then_body, elseif_clauses, else_body } => {
                    scan_expr(condition, evidence);
                    scan_stmts(then_body, evidence);
                    for (cond, body) in elseif_clauses {
                        scan_expr(cond, evidence);
                        scan_stmts(body, evidence);
                    }
                    if let Some(eb) = else_body { scan_stmts(eb, evidence); }
                }
                Stat::While { condition, body } => {
                    scan_expr(condition, evidence);
                    scan_stmts(body, evidence);
                }
                Stat::Repeat { body, condition } => {
                    scan_stmts(body, evidence);
                    scan_expr(condition, evidence);
                }
                Stat::NumericFor { start, stop, step, body, .. } => {
                    scan_expr(start, evidence);
                    scan_expr(stop, evidence);
                    if let Some(s) = step { scan_expr(s, evidence); }
                    scan_stmts(body, evidence);
                }
                Stat::GenericFor { iterators, body, .. } => {
                    for it in iterators { scan_expr(it, evidence); }
                    scan_stmts(body, evidence);
                }
                Stat::DoBlock { body } => scan_stmts(body, evidence),
                _ => {}
            }
        }
    }

    scan_stmts(stmts, &mut evidence);

    let mut renames: HashMap<String, String> = HashMap::new();
    let mut used_replacements: HashSet<String> = HashSet::new();

    let mut upvals: Vec<(String, Evidence)> = evidence.into_iter().collect();
    upvals.sort_by(|a, b| {
        let a_idx: usize = a.0[6..].parse().unwrap_or(usize::MAX);
        let b_idx: usize = b.0[6..].parse().unwrap_or(usize::MAX);
        a_idx.cmp(&b_idx)
    });

    for (upval_name, ev) in upvals {
        // Phase B0.83: direct assignment name takes highest priority
        // when the upval has no strong usage-based evidence that
        // would contradict it (e.g., GetService usage still wins).
        let replacement = if ev.get_service {
            // Definitive: :GetService() is unique to `game`
            "game"
        } else if ev.workspace_methods {
            // Raycasting methods are unique to workspace
            "workspace"
        } else if ev.remote_fire || ev.remote_listen {
            // :FireServer / .OnServerEvent -- remote event/function
            "remoteEvent"
        } else if ev.local_player && !ev.shared_client_server {
            // .LocalPlayer without module-path fields -- Players service
            "Players"
        } else if ev.shared_client_server {
            // .Shared, .Client, .Server -- module hierarchy on script
            "script"
        } else if ev.find_first_child || ev.wait_for_child || ev.parent_access {
            // Instance navigation -- usually script in main closures
            "script"
        } else if ev.cframe_methods && !ev.has_new_field {
            // CFrame instance (not the CFrame constructor)
            "cframe"
        } else if ev.has_new_field {
            // .new constructor -- try to distinguish which datatype
            if ev.color3_evidence || has_any_field(&ev.all_fields, &["fromRGB", "fromHSV"]) {
                "Color3"
            } else if ev.udim2_evidence || has_any_field(&ev.all_fields, &["fromScale", "fromOffset"]) {
                "UDim2"
            } else if ev.vector3_evidence || has_any_field(&ev.all_fields, &["Magnitude", "Unit"]) {
                "Vector3"
            } else if ev.cframe_methods
                || has_any_field(&ev.all_fields, &["Angles", "fromEulerAnglesXYZ",
                   "fromEulerAnglesYXZ", "lookAt", "identity"])
            {
                "CFrame"
            } else if has_any_field(&ev.all_fields, &["xAxis", "yAxis", "zAxis", "one", "zero"]) {
                "Vector3"
            } else {
                // Generic .new -- default to Instance
                "Instance"
            }
        } else if ev.create_method {
            "TweenService"
        } else if ev.clone_destroy {
            "instance"
        } else if ev.connect_method {
            "event"
        } else if ev.assigned_from_pairs || ev.assigned_from_ipairs {
            // upval_N = pairs/ipairs -- stdlib capture, skip renaming
            continue;
        } else if ev.assigned_from_require {
            "module"
        } else if is_math_library(&ev.all_fields) {
            "math"
        } else if is_string_library(&ev.all_methods) {
            "string"
        } else if is_table_library(&ev.all_fields) {
            "table"
        } else if is_bit32_library(&ev.all_fields, &ev.all_methods) {
            "bit32"
        } else if is_coroutine_library(&ev.all_methods) {
            "coroutine"
        } else if is_debug_library(&ev.all_methods) {
            "debug"
        } else if is_os_library(&ev.all_fields) {
            "os"
        } else if !ev.all_methods.is_empty() && ev.all_methods.iter().all(|m| m == "__call__") {
            // Only used as a direct call — it's a function
            "func"
        } else if let Some(ref name) = ev.assigned_name {
            // Phase B0.83: direct assignment name as fallback when
            // usage-based heuristics don't give a strong signal.
            // e.g., `upval_N = game:GetService("Players")` → "Players"
            name.as_str()
        } else if !ev.all_fields.is_empty() || !ev.all_methods.is_empty() {
            // Has some usage but no strong signal — generic module/lib
            "lib"
        } else {
            // No usage evidence at all — leave as upval_N
            continue;
        };

        let final_name = if used_replacements.contains(replacement) {
            let mut suffix = 2;
            loop {
                let candidate = format!("{}_{}", replacement, suffix);
                if !used_replacements.contains(&candidate) {
                    break candidate;
                }
                suffix += 1;
            }
        } else {
            replacement.to_string()
        };

        used_replacements.insert(final_name.clone());
        renames.insert(upval_name, final_name);
    }

    renames
}

/// Check if any of `needles` appear in a list of field names.
fn has_any_field(fields: &[String], needles: &[&str]) -> bool {
    fields.iter().any(|f| needles.contains(&f.as_str()))
}

/// Heuristic: does the set of accessed fields look like the `math` library?
fn is_math_library(fields: &[String]) -> bool {
    const MATH_FIELDS: &[&str] = &[
        "floor", "ceil", "abs", "sqrt", "sin", "cos", "tan", "asin", "acos",
        "atan", "atan2", "exp", "log", "log10", "max", "min", "pow", "random",
        "randomseed", "huge", "pi", "clamp", "sign", "round", "noise",
        "rad", "deg", "fmod", "modf", "frexp", "ldexp",
    ];
    let matches = fields.iter().filter(|f| MATH_FIELDS.contains(&f.as_str())).count();
    // Lower threshold to 1 — single math field access is sufficient signal
    matches >= 1
}

/// Heuristic: does the set of called methods look like the `string` library?
fn is_string_library(methods: &[String]) -> bool {
    const STRING_METHODS: &[&str] = &[
        "format", "find", "match", "gmatch", "gsub", "sub", "rep",
        "reverse", "upper", "lower", "byte", "char", "len", "split",
    ];
    let matches = methods.iter().filter(|m| STRING_METHODS.contains(&m.as_str())).count();
    matches >= 1
}

/// Heuristic: does the set of accessed fields look like the `table` library?
fn is_table_library(fields: &[String]) -> bool {
    const TABLE_FIELDS: &[&str] = &[
        "insert", "remove", "sort", "concat", "move", "create",
        "find", "pack", "unpack", "freeze", "isfrozen", "clone",
        "clear", "getn", "foreach", "foreachi",
    ];
    let matches = fields.iter().filter(|f| TABLE_FIELDS.contains(&f.as_str())).count();
    matches >= 1
}


fn rename_expr(expr: &mut Expr, renames: &HashMap<String, String>) {
    match expr {
        Expr::Name(name) => {
            if let Some(replacement) = renames.get(name.as_str()) {
                *name = replacement.clone();
            }
        }
        Expr::Field { object, .. } => rename_expr(object, renames),
        Expr::Index { object, key } => {
            rename_expr(object, renames);
            rename_expr(key, renames);
        }
        Expr::BinOp { left, right, .. } => {
            rename_expr(left, renames);
            rename_expr(right, renames);
        }
        Expr::UnOp { operand, .. } => rename_expr(operand, renames),
        Expr::Call { func, args } => {
            rename_expr(func, renames);
            for arg in args { rename_expr(arg, renames); }
        }
        Expr::MethodCall { object, args, .. } => {
            rename_expr(object, renames);
            for arg in args { rename_expr(arg, renames); }
        }
        Expr::Function { body, .. } => {
            apply_renames_to_stmts(body, renames);
        }
        Expr::Table { fields } => {
            for f in fields {
                match f {
                    TableField::Sequential(e) => rename_expr(e, renames),
                    TableField::Named(_, e) => rename_expr(e, renames),
                    TableField::Indexed(k, v) => {
                        rename_expr(k, renames);
                        rename_expr(v, renames);
                    }
                }
            }
        }
        _ => {}
    }
}

fn apply_renames_to_stmts(stmts: &mut [Stat], renames: &HashMap<String, String>) {
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::Local { names, values } => {
                for name in names.iter_mut() {
                    if let Some(replacement) = renames.get(name.as_str()) {
                        *name = replacement.clone();
                    }
                }
                for v in values { rename_expr(v, renames); }
            }
            Stat::Assign { targets, values } => {
                for t in targets { rename_expr(t, renames); }
                for v in values { rename_expr(v, renames); }
            }
            Stat::ExprStat(e) => rename_expr(e, renames),
            Stat::Return { values } => {
                for v in values { rename_expr(v, renames); }
            }
            Stat::If { condition, then_body, elseif_clauses, else_body } => {
                rename_expr(condition, renames);
                apply_renames_to_stmts(then_body, renames);
                for (cond, body) in elseif_clauses {
                    rename_expr(cond, renames);
                    apply_renames_to_stmts(body, renames);
                }
                if let Some(eb) = else_body { apply_renames_to_stmts(eb, renames); }
            }
            Stat::While { condition, body } => {
                rename_expr(condition, renames);
                apply_renames_to_stmts(body, renames);
            }
            Stat::Repeat { body, condition } => {
                apply_renames_to_stmts(body, renames);
                rename_expr(condition, renames);
            }
            Stat::NumericFor { start, stop, step, body, .. } => {
                rename_expr(start, renames);
                rename_expr(stop, renames);
                if let Some(s) = step { rename_expr(s, renames); }
                apply_renames_to_stmts(body, renames);
            }
            Stat::GenericFor { iterators, body, .. } => {
                for it in iterators { rename_expr(it, renames); }
                apply_renames_to_stmts(body, renames);
            }
            Stat::DoBlock { body } => apply_renames_to_stmts(body, renames),
            _ => {}
        }
    }
}

/// Post-processing pass: scan the AST for `upval_N` usage patterns and
/// rename them to likely Roblox globals based on how they are used.
fn rename_upvals(stmts: &mut Vec<Stat>) {
    let renames = infer_upval_names(stmts);
    if !renames.is_empty() {
        apply_renames_to_stmts(stmts, &renames);
    }
}

// Phase C2 pass #2: recursive upvalue name propagation.
//
// Iteration cap for the bounded fixpoint. Deep closure nests in Roblox scripts
// rarely exceed 3 levels; 5 is a generous ceiling that still guarantees fast
// termination on malformed (cyclic) parent-link maps.
pub(crate) const PROPAGATE_UPVAL_MAX_ITERATIONS: usize = 5;

/// Single pass of parent→child upvalue-name propagation.
///
/// For every entry in `links` (maps `child_proto_index` → list of
/// `(child_upval_slot, parent_proto_index, parent_upval_slot)`), if the parent's
/// upvalue slot already has a resolved name in `inferred` (or in the parent's
/// debug info via lookup) AND the child's slot is still an empty/`upval_N`
/// placeholder, copy the parent name into the child slot.
///
/// Returns `true` when at least one slot was updated. Invoke inside a bounded
/// loop to propagate multi-level chains (grandchild → grandparent).
///
/// Defensive against malformed `links` maps: bogus proto indices or out-of-range
/// upvalue slots are skipped rather than panicking, and the caller's iteration
/// cap prevents infinite loops on genuinely cyclic links.
fn propagate_upval_names_once(
    protos: &[Proto],
    inferred: &mut HashMap<usize, Vec<String>>,
    links: &HashMap<usize, Vec<(usize, usize, u8)>>,
) -> bool {
    let num_protos = protos.len();
    let mut changed = false;

    // Snapshot the links to keep the borrow of `links` read-only while we mutate
    // `inferred`. The clone cost is trivial (at most a few entries per proto).
    let snapshot: Vec<(usize, Vec<(usize, usize, u8)>)> = links
        .iter()
        .map(|(child_idx, l)| (*child_idx, l.clone()))
        .collect();

    for (child_idx, child_links) in snapshot {
        if child_idx >= num_protos { continue; }
        for (child_slot, parent_pi, parent_upval) in child_links {
            if parent_pi >= num_protos { continue; }

            // Resolve parent name. Priority: debug info > inferred map. Bail
            // early if no resolved name is available yet (still `upval_N`).
            let parent_proto = &protos[parent_pi];
            let parent_name = parent_upvalue_resolved_name(
                parent_proto,
                inferred.get(&parent_pi).map(|v| v.as_slice()),
                parent_upval,
            );
            let parent_name = match parent_name {
                Some(n) => n,
                None => continue,
            };

            // Propagate into child's slot. Grow the vec to the full upvalue
            // count if necessary — early lifting may have left a shorter vec.
            let num_upvals_child = protos[child_idx].num_upvalues as usize;
            let entry = inferred
                .entry(child_idx)
                .or_insert_with(|| vec![String::new(); num_upvals_child]);
            if entry.len() < num_upvals_child {
                entry.resize(num_upvals_child, String::new());
            }
            if let Some(slot) = entry.get_mut(child_slot) {
                if slot.is_empty() || slot.starts_with("upval_") {
                    *slot = parent_name;
                    changed = true;
                }
            }
        }
    }

    changed
}

/// Resolve the real name for `parent_proto`'s `upval_slot`, using debug info
/// first (if present and valid), then the inferred-names map. Returns `None`
/// when still unresolved (i.e., the caller should leave the child alone).
fn parent_upvalue_resolved_name(
    parent_proto: &Proto,
    inferred_for_parent: Option<&[String]>,
    upval_slot: u8,
) -> Option<String> {
    let idx = upval_slot as usize;
    // Debug info (non-stripped bytecode).
    if let Some(ref debug) = parent_proto.debug_info {
        if let Some(name) = debug.upvalue_names.get(idx) {
            if !name.is_empty()
                && is_valid_luau_identifier(name)
                && !is_stdlib_shadow_name(name)
            {
                return Some(name.clone());
            }
        }
    }
    // Inferred names.
    if let Some(names) = inferred_for_parent {
        if let Some(name) = names.get(idx) {
            if !name.is_empty() && !name.starts_with("upval_") {
                return Some(name.clone());
            }
        }
    }
    None
}

/// Heuristic: does the set of fields look like the `bit32` library?
fn is_bit32_library(fields: &[String], methods: &[String]) -> bool {
    const BIT32: &[&str] = &[
        "band", "bor", "bxor", "bnot", "lshift", "rshift", "arshift",
        "lrotate", "rrotate", "extract", "replace", "countlz", "countrz",
        "btest", "byteswap",
    ];
    let f = fields.iter().filter(|f| BIT32.contains(&f.as_str())).count();
    let m = methods.iter().filter(|m| BIT32.contains(&m.as_str())).count();
    (f + m) >= 1
}

/// Heuristic: does the set of methods look like the `coroutine` library?
fn is_coroutine_library(methods: &[String]) -> bool {
    const COROUTINE: &[&str] = &[
        "create", "resume", "yield", "wrap", "status", "running",
        "isyieldable", "close",
    ];
    methods.iter().filter(|m| COROUTINE.contains(&m.as_str())).count() >= 1
}

/// Heuristic: does the set of methods look like the `debug` library?
fn is_debug_library(methods: &[String]) -> bool {
    const DEBUG: &[&str] = &[
        "traceback", "info", "profilebegin", "profileend",
        "getinfo", "getlocal", "setlocal", "getupvalue", "setupvalue",
        "getmetatable", "setmetatable",
    ];
    methods.iter().filter(|m| DEBUG.contains(&m.as_str())).count() >= 1
}

/// Heuristic: does the set of fields look like the `os` library?
fn is_os_library(fields: &[String]) -> bool {
    const OS: &[&str] = &["time", "clock", "difftime", "date"];
    fields.iter().filter(|f| OS.contains(&f.as_str())).count() >= 1
}

// ═══════════════════════════════════════════════════════════════════
// METHOD CHAIN COLLAPSE — combines sequential method calls into chains
// ═══════════════════════════════════════════════════════════════════

/// Post-processing pass: collapse patterns like:
///   local v0 = obj:Method1(a1)
///   v0 = v0:Method2(a2)
///   v0 = v0:Method3(a3)
/// Into:
///   local v0 = obj:Method1(a1):Method2(a2):Method3(a3)
fn collapse_method_chains(stmts: &mut Vec<Stat>) {
    // Recurse into nested blocks first
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                collapse_method_chains(then_body);
                for (_, body) in elseif_clauses {
                    collapse_method_chains(body);
                }
                if let Some(eb) = else_body { collapse_method_chains(eb); }
            }
            Stat::While { body, .. } => collapse_method_chains(body),
            Stat::Repeat { body, .. } => collapse_method_chains(body),
            Stat::NumericFor { body, .. } => collapse_method_chains(body),
            Stat::GenericFor { body, .. } => collapse_method_chains(body),
            Stat::DoBlock { body } => collapse_method_chains(body),
            _ => {}
        }
    }

    // Collapse consecutive call/method chains.
    // Handles two patterns:
    // 1. Same-name:  `local v0 = obj:M1()` + `v0 = v0:M2()` → `local v0 = obj:M1():M2()`
    // 2. Diff-name:  `local call = obj:M1()` + `call2 = call:M2()` → `call2 = obj:M1():M2()`
    //    (removes the intermediate temp and inlines the expression)
    let mut i = 0;
    while i + 1 < stmts.len() {
        // Extract definition: variable name from a call/method expression
        let prev_name = match &stmts[i] {
            Stat::Local { names, values } if names.len() == 1 && values.len() == 1 => {
                match &values[0] {
                    Expr::Call { .. } | Expr::MethodCall { .. } => names[0].clone(),
                    _ => { i += 1; continue; }
                }
            }
            Stat::Assign { targets, values } if targets.len() == 1 && values.len() == 1 => {
                match (&targets[0], &values[0]) {
                    (Expr::Name(n), Expr::Call { .. } | Expr::MethodCall { .. }) => n.clone(),
                    _ => { i += 1; continue; }
                }
            }
            _ => { i += 1; continue; }
        };

        // Check if stmt[i+1] uses prev_name as the method object or call function
        let chain_info = extract_chain_info(&stmts[i + 1], &prev_name);

        if let Some(is_same_name) = chain_info {
            if !is_same_name {
                // For different-name chains, verify prev_name isn't read later
                let used_later = stmts[i + 2..].iter().any(|s| stmt_reads_name(s, &prev_name));
                if used_later {
                    i += 1;
                    continue;
                }
                // Also verify prev_name isn't used in stmt[i+1]'s call args
                let used_in_args = match &stmts[i + 1] {
                    Stat::Assign { values, .. } if values.len() == 1 => {
                        match &values[0] {
                            Expr::MethodCall { args, .. } | Expr::Call { args, .. } => {
                                args.iter().any(|a| expr_uses_name(a, &prev_name))
                            }
                            _ => false,
                        }
                    }
                    Stat::Local { values, .. } if values.len() == 1 => {
                        match &values[0] {
                            Expr::MethodCall { args, .. } | Expr::Call { args, .. } => {
                                args.iter().any(|a| expr_uses_name(a, &prev_name))
                            }
                            _ => false,
                        }
                    }
                    _ => false,
                };
                if used_in_args {
                    i += 1;
                    continue;
                }
            }

            // Get the original expression from stmt[i]
            let orig_expr = match &stmts[i] {
                Stat::Local { values, .. } => values[0].clone(),
                Stat::Assign { values, .. } => values[0].clone(),
                _ => unreachable!(),
            };

            // Get the chain expression from stmt[i+1] and replace the object
            let chain_val = match &stmts[i + 1] {
                Stat::Assign { values, .. } => values[0].clone(),
                Stat::Local { values, .. } => values[0].clone(),
                _ => unreachable!(),
            };

            let new_expr = match chain_val {
                Expr::MethodCall { method, args, .. } => {
                    Expr::MethodCall {
                        object: Box::new(orig_expr),
                        method,
                        args,
                    }
                }
                Expr::Call { args, .. } => {
                    Expr::Call {
                        func: Box::new(orig_expr),
                        args,
                    }
                }
                _ => { i += 1; continue; }
            };

            if is_same_name {
                // Same-name: update stmt[i]'s value, remove stmt[i+1]
                match &mut stmts[i] {
                    Stat::Local { values, .. } => values[0] = new_expr,
                    Stat::Assign { values, .. } => values[0] = new_expr,
                    _ => unreachable!(),
                }
                stmts.remove(i + 1);
            } else {
                // Diff-name: update stmt[i+1]'s value, remove stmt[i]
                match &mut stmts[i + 1] {
                    Stat::Assign { values, .. } => values[0] = new_expr,
                    Stat::Local { values, .. } => values[0] = new_expr,
                    _ => unreachable!(),
                }
                stmts.remove(i);
            }
            // Don't advance i — check if we can chain more
        } else {
            i += 1;
        }
    }
}

/// Extract chain continuation info from a statement.
/// Returns Some(is_same_name) if the statement uses `prev_name` as the
/// method call object or call function.
/// `is_same_name` is true when the assignment target equals prev_name.
fn extract_chain_info(stmt: &Stat, prev_name: &str) -> Option<bool> {
    match stmt {
        Stat::Assign { targets, values } if targets.len() == 1 && values.len() == 1 => {
            if let Expr::Name(target) = &targets[0] {
                let obj_is_prev = match &values[0] {
                    Expr::MethodCall { object, .. } => {
                        matches!(object.as_ref(), Expr::Name(n) if n == prev_name)
                    }
                    Expr::Call { func, .. } => {
                        matches!(func.as_ref(), Expr::Name(n) if n == prev_name)
                    }
                    _ => false,
                };
                if obj_is_prev {
                    Some(target == prev_name)
                } else {
                    None
                }
            } else {
                None
            }
        }
        Stat::Local { names, values } if names.len() == 1 && values.len() == 1 => {
            let obj_is_prev = match &values[0] {
                Expr::MethodCall { object, .. } => {
                    matches!(object.as_ref(), Expr::Name(n) if n == prev_name)
                }
                Expr::Call { func, .. } => {
                    matches!(func.as_ref(), Expr::Name(n) if n == prev_name)
                }
                _ => false,
            };
            if obj_is_prev { Some(false) } else { None }
        }
        _ => None,
    }
}

/// Check if a name is read (not just defined/assigned-to) in a statement.
/// Used for safety checks in chain collapse and inlining.
pub(super) fn stmt_reads_name(stmt: &Stat, name: &str) -> bool {
    match stmt {
        Stat::Local { values, .. } => values.iter().any(|v| expr_uses_name(v, name)),
        Stat::Assign { targets, values } => {
            let in_values = values.iter().any(|v| expr_uses_name(v, name));
            // Name(x) as target is a write, not a read. But Field{obj=x}/Index{obj=x}
            // targets DO read x (to look up the object for field/index assignment).
            let in_targets = targets.iter().any(|t| match t {
                Expr::Name(_) => false,
                other => expr_uses_name(other, name),
            });
            in_values || in_targets
        }
        Stat::ExprStat(e) => expr_uses_name(e, name),
        Stat::Return { values } => values.iter().any(|v| expr_uses_name(v, name)),
        Stat::If { condition, then_body, elseif_clauses, else_body } => {
            expr_uses_name(condition, name)
            || then_body.iter().any(|s| stmt_reads_name(s, name))
            || elseif_clauses.iter().any(|(c, b)|
                expr_uses_name(c, name) || b.iter().any(|s| stmt_reads_name(s, name)))
            || else_body.as_ref().map_or(false, |eb| eb.iter().any(|s| stmt_reads_name(s, name)))
        }
        Stat::While { condition, body } => {
            expr_uses_name(condition, name) || body.iter().any(|s| stmt_reads_name(s, name))
        }
        Stat::Repeat { body, condition } => {
            body.iter().any(|s| stmt_reads_name(s, name)) || expr_uses_name(condition, name)
        }
        Stat::NumericFor { start, stop, step, body, .. } => {
            expr_uses_name(start, name) || expr_uses_name(stop, name)
            || step.as_ref().map_or(false, |s| expr_uses_name(s, name))
            || body.iter().any(|s| stmt_reads_name(s, name))
        }
        Stat::GenericFor { iterators, body, .. } => {
            iterators.iter().any(|it| expr_uses_name(it, name))
            || body.iter().any(|s| stmt_reads_name(s, name))
        }
        Stat::DoBlock { body } => body.iter().any(|s| stmt_reads_name(s, name)),
        _ => false,
    }
}

// ═══════════════════════════════════════════════════════════════════
// INLINE SINGLE-USE TEMPS — fold intermediate variables into use sites
// ═══════════════════════════════════════════════════════════════════

/// Post-processing pass: inline temporary variables that are defined by a
/// side-effect-free expression and used exactly once later. Handles Call,
/// MethodCall, Field, Index, Name, BinOp, UnOp, and literal expressions.
/// Does NOT inline Function literals (closure state) or Table constructors
/// (may be mutated). Examples:
///   local call7 = chain:Build()
///   call8 = call3:AddPermanentItem(call7)
/// becomes:
///   call8 = call3:AddPermanentItem(chain:Build())
/// and:
///   local v0 = game.Players
///   someFunc(v0)
/// becomes:
///   someFunc(game.Players)
/// Return true if the expression is a call whose function name indicates
/// it has observable side effects or reads external state (require, pcall,
/// spawn, etc.). Inlining these changes semantic timing.
pub(super) fn is_side_effect_call(expr: &Expr) -> bool {
    match expr {
        Expr::Call { func, .. } => {
            match &**func {
                Expr::Name(n) => matches!(n.as_str(),
                    "require" | "pcall" | "xpcall" | "spawn" | "delay"
                    | "coroutine" | "loadstring" | "load" | "loadfile"
                    | "dofile" | "print" | "warn" | "error" | "assert"
                    | "next" | "pairs" | "ipairs" | "setmetatable" | "getmetatable"
                    | "rawset" | "rawget" | "rawequal" | "rawlen"
                    | "getfenv" | "setfenv" | "newproxy"
                ),
                // `game:GetService(...)` — don't inline, it's a Roblox API call
                _ => false,
            }
        }
        Expr::MethodCall { method, .. } => {
            matches!(method.as_str(),
                "GetService" | "WaitForChild" | "FindFirstChild"
                | "FindFirstChildOfClass" | "FindFirstChildWhichIsA"
                | "Clone" | "Destroy" | "Fire" | "Invoke"
                | "Connect" | "Once" | "connect" | "once"
            )
        }
        _ => false,
    }
}

/// Phase B0.45A: Return true if an expression is "pure" — contains no
/// function/method calls and reads no external state that intervening
/// statements could alter. Pure expressions are safe to inline across
/// intervening side-effect statements.
///
/// Pure:
///   - Literals: Nil, Bool, Number, String, Varargs, Vector
///   - Name (reads a local/global; separately gated by is_name_reassigned_between)
///   - Field / Index with pure object and key
///   - BinOp / UnOp whose operands are pure
///   - Table with only pure-expression fields
///
/// Impure:
///   - Call / MethodCall (can have observable effects, can re-evaluate)
///   - Function literals (closure capture semantics)
pub(super) fn is_pure_expr(expr: &Expr) -> bool {
    match expr {
        Expr::Nil | Expr::Bool(_) | Expr::Number(_) | Expr::String(_)
        | Expr::Varargs | Expr::Vector(_, _, _) | Expr::Name(_) => true,
        Expr::Field { object, .. } => is_pure_expr(object),
        Expr::Index { object, key } => is_pure_expr(object) && is_pure_expr(key),
        Expr::BinOp { left, right, .. } => is_pure_expr(left) && is_pure_expr(right),
        Expr::UnOp { operand, .. } => is_pure_expr(operand),
        Expr::Table { fields } => fields.iter().all(|f| match f {
            TableField::Sequential(e) => is_pure_expr(e),
            TableField::Named(_, e) => is_pure_expr(e),
            TableField::Indexed(k, v) => is_pure_expr(k) && is_pure_expr(v),
        }),
        // Phase B0.52P10: ternary is pure iff every sub-expression is pure.
        Expr::Ternary { cond, then_expr, else_expr } => {
            is_pure_expr(cond) && is_pure_expr(then_expr) && is_pure_expr(else_expr)
        }
        // Calls, method calls, and function literals are never pure for the
        // purpose of reorder safety.
        Expr::Call { .. } | Expr::MethodCall { .. } | Expr::Function { .. } => false,
    }
}

/// Phase B0.45A: return true if an expression contains any call or
/// method-call node. Used to detect RHS that must not be duplicated
/// by inlining into a loop body (where it would re-evaluate).
pub(super) fn expr_contains_call(expr: &Expr) -> bool {
    match expr {
        Expr::Call { .. } | Expr::MethodCall { .. } => true,
        Expr::Field { object, .. } => expr_contains_call(object),
        Expr::Index { object, key } => expr_contains_call(object) || expr_contains_call(key),
        Expr::BinOp { left, right, .. } => expr_contains_call(left) || expr_contains_call(right),
        Expr::UnOp { operand, .. } => expr_contains_call(operand),
        Expr::Table { fields } => fields.iter().any(|f| match f {
            TableField::Sequential(e) => expr_contains_call(e),
            TableField::Named(_, e) => expr_contains_call(e),
            TableField::Indexed(k, v) => expr_contains_call(k) || expr_contains_call(v),
        }),
        // Treat any sub-function as "may contain call" for conservative dupe check
        Expr::Function { .. } => true,
        // Phase B0.92: recurse into Ternary sub-expressions.
        Expr::Ternary { cond, then_expr, else_expr } => {
            expr_contains_call(cond) || expr_contains_call(then_expr) || expr_contains_call(else_expr)
        }
        _ => false,
    }
}

/// Phase B0.45A: return true if a statement might have observable
/// side effects that could change the value of a later expression
/// re-evaluating the same RHS. Conservatively treats any call or
/// any assignment/loop/if/etc. as a side effect (since assignments
/// to Field/Index or globals can mutate shared state).
///
/// Returns false for statements that only bind a local (Stat::Local)
/// to an RHS that itself has no side effects (no call, no sub-function).
/// Assignments to simple names are treated as side effects because
/// the target might be a global or an upvalue.
pub(super) fn stmt_has_observable_side_effect(stmt: &Stat) -> bool {
    match stmt {
        // Local-to-pure: does not mutate shared state.
        Stat::Local { values, .. } => {
            values.iter().any(|v| expr_contains_call(v))
        }
        // Any assignment potentially mutates external state
        // (global, upvalue, or field on an object shared with later code).
        Stat::Assign { .. } => true,
        // Bare expression statements (call statements) are side effects.
        Stat::ExprStat(e) => expr_contains_call(e),
        // Returns / break / continue do not execute after the current point.
        Stat::Return { .. } | Stat::Break | Stat::Continue | Stat::Comment(_) => false,
        // Control-flow blocks are conservatively treated as side-effectful
        // because their bodies may contain calls / assigns.
        Stat::If { .. } | Stat::While { .. } | Stat::Repeat { .. }
        | Stat::NumericFor { .. } | Stat::GenericFor { .. }
        | Stat::DoBlock { .. } => true,
        // Phase B0.52P10: LocalFunction binds a fresh local whose RHS is a
        // function literal — function literals do not execute at the decl
        // site, so this is not an observable side effect by itself.
        Stat::LocalFunction { .. } => false,
        // `function obj:method() ... end` assigns to a field on `obj`,
        // which is externally observable (just like Stat::Assign).
        Stat::MethodFunction { .. } => true,
    }
}

/// Phase B0.45A: return true if any Stat in the slice writes to a
/// local/global named `name` (at the current block level, not
/// recursing into nested blocks — those are already handled by the
/// recursive pre-pass and have their own inlining pass).
pub(super) fn stmts_reassign_name(stmts: &[Stat], name: &str) -> bool {
    for s in stmts {
        match s {
            Stat::Local { names, .. } => {
                if names.iter().any(|n| n == name) { return true; }
            }
            Stat::Assign { targets, .. } => {
                if targets.iter().any(|t| matches!(t, Expr::Name(n) if n == name)) {
                    return true;
                }
            }
            // Deep control-flow blocks are treated conservatively elsewhere —
            // this helper is only used on a pre-filtered segment (already
            // guaranteed no side effects by caller), so control-flow bodies
            // cannot occur in that segment.
            Stat::If { .. } | Stat::While { .. } | Stat::Repeat { .. }
            | Stat::NumericFor { .. } | Stat::GenericFor { .. }
            | Stat::DoBlock { .. } => return true,
            _ => {}
        }
    }
    false
}

/// Phase B0.45A: return true if the first statement at or below the
/// given statement that reads `name` is nested inside a loop body
/// (While, Repeat, NumericFor, GenericFor). If the read is inside a
/// loop, inlining an RHS with side effects or calls would change
/// evaluation count semantics (executed N times instead of once).
///
/// Returns Some(true) if read is inside a loop, Some(false) if read
/// is not inside a loop, None if stmt doesn't read the name at all.
pub(super) fn read_is_inside_loop(stmt: &Stat, name: &str) -> Option<bool> {
    // Helper: does any statement in body read the name?
    fn body_reads(body: &[Stat], name: &str) -> bool {
        body.iter().any(|s| stmt_reads_name(s, name))
    }
    match stmt {
        // Direct non-loop reads: not inside a loop.
        Stat::Local { values, .. } => {
            if values.iter().any(|v| expr_uses_name(v, name)) { Some(false) } else { None }
        }
        Stat::Assign { targets, values } => {
            let in_values = values.iter().any(|v| expr_uses_name(v, name));
            let in_targets = targets.iter().any(|t| match t {
                Expr::Name(_) => false,
                other => expr_uses_name(other, name),
            });
            if in_values || in_targets { Some(false) } else { None }
        }
        Stat::ExprStat(e) => {
            if expr_uses_name(e, name) { Some(false) } else { None }
        }
        Stat::Return { values } => {
            if values.iter().any(|v| expr_uses_name(v, name)) { Some(false) } else { None }
        }
        // For NumericFor / GenericFor / While / Repeat: if name is read in
        // the body, report "inside a loop". If read only in header
        // (start/stop/step/iterators/condition), that evaluates once.
        Stat::NumericFor { start, stop, step, body, .. } => {
            let in_header = expr_uses_name(start, name)
                || expr_uses_name(stop, name)
                || step.as_ref().map_or(false, |s| expr_uses_name(s, name));
            let in_body = body_reads(body, name);
            if in_body { Some(true) }
            else if in_header { Some(false) }
            else { None }
        }
        Stat::GenericFor { iterators, body, .. } => {
            let in_header = iterators.iter().any(|it| expr_uses_name(it, name));
            let in_body = body_reads(body, name);
            if in_body { Some(true) }
            else if in_header { Some(false) }
            else { None }
        }
        Stat::While { condition, body } => {
            // While condition evaluates each iteration — still a loop-execution position.
            let in_cond = expr_uses_name(condition, name);
            let in_body = body_reads(body, name);
            if in_cond || in_body { Some(true) } else { None }
        }
        Stat::Repeat { body, condition } => {
            let in_cond = expr_uses_name(condition, name);
            let in_body = body_reads(body, name);
            if in_cond || in_body { Some(true) } else { None }
        }
        // If / DoBlock: not loops; reads inside are evaluated at most once.
        Stat::If { condition, then_body, elseif_clauses, else_body } => {
            let in_cond = expr_uses_name(condition, name);
            let in_then = body_reads(then_body, name);
            let in_elif = elseif_clauses.iter().any(|(c, b)|
                expr_uses_name(c, name) || body_reads(b, name));
            let in_else = else_body.as_ref().map_or(false, |eb| body_reads(eb, name));
            if in_cond || in_then || in_elif || in_else { Some(false) } else { None }
        }
        Stat::DoBlock { body } => {
            if body_reads(body, name) { Some(false) } else { None }
        }
        _ => None,
    }
}

/// Phase B0.45A: collect all `Expr::Name` identifiers inside an
/// expression tree into `out`.  Used to determine which names in a
/// pure RHS could be invalidated by intervening reassignments.
pub(super) fn collect_names_in_expr(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Name(n) => out.push(n.clone()),
        Expr::Field { object, .. } => collect_names_in_expr(object, out),
        Expr::Index { object, key } => {
            collect_names_in_expr(object, out);
            collect_names_in_expr(key, out);
        }
        Expr::BinOp { left, right, .. } => {
            collect_names_in_expr(left, out);
            collect_names_in_expr(right, out);
        }
        Expr::UnOp { operand, .. } => collect_names_in_expr(operand, out),
        Expr::Call { func, args } => {
            collect_names_in_expr(func, out);
            for a in args { collect_names_in_expr(a, out); }
        }
        Expr::MethodCall { object, args, .. } => {
            collect_names_in_expr(object, out);
            for a in args { collect_names_in_expr(a, out); }
        }
        Expr::Table { fields } => {
            for f in fields {
                match f {
                    TableField::Sequential(e) => collect_names_in_expr(e, out),
                    TableField::Named(_, e) => collect_names_in_expr(e, out),
                    TableField::Indexed(k, v) => {
                        collect_names_in_expr(k, out);
                        collect_names_in_expr(v, out);
                    }
                }
            }
        }
        Expr::Function { .. } => {} // closures don't propagate name equivalence
        // Phase B0.92: recurse into Ternary sub-expressions.
        Expr::Ternary { cond, then_expr, else_expr } => {
            collect_names_in_expr(cond, out);
            collect_names_in_expr(then_expr, out);
            collect_names_in_expr(else_expr, out);
        }
        _ => {}
    }
}

/// Count how many times a name is READ (not defined) in a statement.
pub(super) fn count_name_reads_in_stmt(stmt: &Stat, name: &str) -> usize {
    match stmt {
        Stat::Local { values, .. } => {
            values.iter().map(|v| count_name_reads_in_expr(v, name)).sum()
        }
        Stat::Assign { targets, values } => {
            let in_values: usize = values.iter().map(|v| count_name_reads_in_expr(v, name)).sum();
            let in_targets: usize = targets.iter().map(|t| match t {
                Expr::Name(_) => 0, // writing to name, not reading
                other => count_name_reads_in_expr(other, name),
            }).sum();
            in_values + in_targets
        }
        Stat::ExprStat(e) => count_name_reads_in_expr(e, name),
        Stat::Return { values } => values.iter().map(|v| count_name_reads_in_expr(v, name)).sum(),
        Stat::If { condition, then_body, elseif_clauses, else_body } => {
            let mut c = count_name_reads_in_expr(condition, name);
            c += then_body.iter().map(|s| count_name_reads_in_stmt(s, name)).sum::<usize>();
            for (cond, body) in elseif_clauses {
                c += count_name_reads_in_expr(cond, name);
                c += body.iter().map(|s| count_name_reads_in_stmt(s, name)).sum::<usize>();
            }
            if let Some(eb) = else_body {
                c += eb.iter().map(|s| count_name_reads_in_stmt(s, name)).sum::<usize>();
            }
            c
        }
        Stat::While { condition, body } => {
            count_name_reads_in_expr(condition, name)
            + body.iter().map(|s| count_name_reads_in_stmt(s, name)).sum::<usize>()
        }
        Stat::Repeat { body, condition } => {
            body.iter().map(|s| count_name_reads_in_stmt(s, name)).sum::<usize>()
            + count_name_reads_in_expr(condition, name)
        }
        Stat::NumericFor { start, stop, step, body, .. } => {
            count_name_reads_in_expr(start, name) + count_name_reads_in_expr(stop, name)
            + step.as_ref().map_or(0, |s| count_name_reads_in_expr(s, name))
            + body.iter().map(|s| count_name_reads_in_stmt(s, name)).sum::<usize>()
        }
        Stat::GenericFor { iterators, body, .. } => {
            iterators.iter().map(|it| count_name_reads_in_expr(it, name)).sum::<usize>()
            + body.iter().map(|s| count_name_reads_in_stmt(s, name)).sum::<usize>()
        }
        Stat::DoBlock { body } => body.iter().map(|s| count_name_reads_in_stmt(s, name)).sum(),
        // Phase B0.92: recurse into LocalFunction/MethodFunction bodies.
        Stat::LocalFunction { func, .. } | Stat::MethodFunction { func, .. } => {
            count_name_reads_in_expr(func, name)
        }
        _ => 0,
    }
}

/// Count how many times a name appears in an expression.
fn count_name_reads_in_expr(expr: &Expr, name: &str) -> usize {
    match expr {
        Expr::Name(n) => if n == name { 1 } else { 0 },
        Expr::Field { object, .. } => count_name_reads_in_expr(object, name),
        Expr::Index { object, key } => {
            count_name_reads_in_expr(object, name) + count_name_reads_in_expr(key, name)
        }
        Expr::BinOp { left, right, .. } => {
            count_name_reads_in_expr(left, name) + count_name_reads_in_expr(right, name)
        }
        Expr::UnOp { operand, .. } => count_name_reads_in_expr(operand, name),
        Expr::Call { func, args } => {
            count_name_reads_in_expr(func, name)
            + args.iter().map(|a| count_name_reads_in_expr(a, name)).sum::<usize>()
        }
        Expr::MethodCall { object, args, .. } => {
            count_name_reads_in_expr(object, name)
            + args.iter().map(|a| count_name_reads_in_expr(a, name)).sum::<usize>()
        }
        Expr::Table { fields } => {
            fields.iter().map(|f| match f {
                TableField::Sequential(e) => count_name_reads_in_expr(e, name),
                TableField::Named(_, e) => count_name_reads_in_expr(e, name),
                TableField::Indexed(k, v) => count_name_reads_in_expr(k, name) + count_name_reads_in_expr(v, name),
            }).sum()
        }
        Expr::Function { .. } => 0, // closures capture by upvalue, not name
        // Phase B0.92: recurse into Ternary sub-expressions.
        Expr::Ternary { cond, then_expr, else_expr } => {
            count_name_reads_in_expr(cond, name)
            + count_name_reads_in_expr(then_expr, name)
            + count_name_reads_in_expr(else_expr, name)
        }
        _ => 0,
    }
}

/// Replace all occurrences of Name(name) in a statement with the given expression.
pub(super) fn replace_name_in_stmt(stmt: &mut Stat, name: &str, replacement: &Expr) {
    match stmt {
        Stat::Local { values, .. } => {
            for v in values { replace_name_in_expr(v, name, replacement); }
        }
        Stat::Assign { targets, values } => {
            for t in targets {
                // Don't replace simple Name targets (those are assignment destinations),
                // but DO replace names inside Field/Index targets
                match t {
                    Expr::Name(_) => {}
                    other => replace_name_in_expr(other, name, replacement),
                }
            }
            for v in values { replace_name_in_expr(v, name, replacement); }
        }
        Stat::ExprStat(e) => replace_name_in_expr(e, name, replacement),
        Stat::Return { values } => {
            for v in values { replace_name_in_expr(v, name, replacement); }
        }
        Stat::If { condition, then_body, elseif_clauses, else_body } => {
            replace_name_in_expr(condition, name, replacement);
            for s in then_body { replace_name_in_stmt(s, name, replacement); }
            for (c, body) in elseif_clauses {
                replace_name_in_expr(c, name, replacement);
                for s in body { replace_name_in_stmt(s, name, replacement); }
            }
            if let Some(eb) = else_body {
                for s in eb { replace_name_in_stmt(s, name, replacement); }
            }
        }
        Stat::While { condition, body } => {
            replace_name_in_expr(condition, name, replacement);
            for s in body { replace_name_in_stmt(s, name, replacement); }
        }
        Stat::Repeat { body, condition } => {
            for s in body { replace_name_in_stmt(s, name, replacement); }
            replace_name_in_expr(condition, name, replacement);
        }
        Stat::NumericFor { start, stop, step, body, .. } => {
            replace_name_in_expr(start, name, replacement);
            replace_name_in_expr(stop, name, replacement);
            if let Some(s) = step { replace_name_in_expr(s, name, replacement); }
            for s in body { replace_name_in_stmt(s, name, replacement); }
        }
        Stat::GenericFor { iterators, body, .. } => {
            for it in iterators { replace_name_in_expr(it, name, replacement); }
            for s in body { replace_name_in_stmt(s, name, replacement); }
        }
        Stat::DoBlock { body } => {
            for s in body { replace_name_in_stmt(s, name, replacement); }
        }
        // Phase B0.92: recurse into LocalFunction/MethodFunction bodies.
        Stat::LocalFunction { func, .. } | Stat::MethodFunction { func, .. } => {
            replace_name_in_expr(func, name, replacement);
        }
        _ => {}
    }
}

/// Replace all occurrences of Name(name) in an expression with the replacement.
fn replace_name_in_expr(expr: &mut Expr, name: &str, replacement: &Expr) {
    match expr {
        Expr::Name(n) if n == name => {
            *expr = replacement.clone();
        }
        Expr::Field { object, .. } => replace_name_in_expr(object, name, replacement),
        Expr::Index { object, key } => {
            replace_name_in_expr(object, name, replacement);
            replace_name_in_expr(key, name, replacement);
        }
        Expr::BinOp { left, right, .. } => {
            replace_name_in_expr(left, name, replacement);
            replace_name_in_expr(right, name, replacement);
        }
        Expr::UnOp { operand, .. } => replace_name_in_expr(operand, name, replacement),
        Expr::Call { func, args } => {
            replace_name_in_expr(func, name, replacement);
            for a in args { replace_name_in_expr(a, name, replacement); }
        }
        Expr::MethodCall { object, args, .. } => {
            replace_name_in_expr(object, name, replacement);
            for a in args { replace_name_in_expr(a, name, replacement); }
        }
        Expr::Table { fields } => {
            for f in fields {
                match f {
                    TableField::Sequential(e) => replace_name_in_expr(e, name, replacement),
                    TableField::Named(_, e) => replace_name_in_expr(e, name, replacement),
                    TableField::Indexed(k, v) => {
                        replace_name_in_expr(k, name, replacement);
                        replace_name_in_expr(v, name, replacement);
                    }
                }
            }
        }
        Expr::Function { body, .. } => {
            for s in body { replace_name_in_stmt(s, name, replacement); }
        }
        // Phase B0.92: recurse into Ternary sub-expressions.
        Expr::Ternary { cond, then_expr, else_expr } => {
            replace_name_in_expr(cond, name, replacement);
            replace_name_in_expr(then_expr, name, replacement);
            replace_name_in_expr(else_expr, name, replacement);
        }
        _ => {}
    }
}

// ═══════════════════════════════════════════════════════════════════
// CLEANUP PASS — eliminates common decompiler artifacts
// ═══════════════════════════════════════════════════════════════════

/// Post-processing pass: remove decompiler artifacts from the AST.
fn cleanup_stmts(stmts: &mut Vec<Stat>) {
    // Remove self-assignments, dead if-nil blocks, empty do-blocks, etc.
    stmts.retain(|stmt| !is_dead_stmt(stmt));

    // Clean nil/artifact expressions in all statements
    for stmt in stmts.iter_mut() {
        cleanup_exprs_in_stmt(stmt);
    }

    // Recurse into nested blocks
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::If { then_body, elseif_clauses, else_body, condition } => {
                // if nil then X else Y end → just Y (nil is always falsy)
                if matches!(condition, Expr::Nil) {
                    if let Some(eb) = else_body {
                        cleanup_stmts(eb);
                    }
                    // The if-nil itself gets cleaned in a second pass below
                }
                cleanup_stmts(then_body);
                for (_, body) in elseif_clauses.iter_mut() {
                    cleanup_stmts(body);
                }
                if let Some(eb) = else_body {
                    cleanup_stmts(eb);
                }
            }
            Stat::While { body, .. } => cleanup_stmts(body),
            Stat::Repeat { body, .. } => cleanup_stmts(body),
            Stat::NumericFor { body, .. } => cleanup_stmts(body),
            Stat::GenericFor { body, .. } => cleanup_stmts(body),
            Stat::DoBlock { body } => cleanup_stmts(body),
            _ => {}
        }
    }

    // Second pass: replace `if nil then X else Y end` with just Y
    let mut i = 0;
    while i < stmts.len() {
        let replace = if let Stat::If { condition, .. } = &stmts[i] {
            matches!(condition, Expr::Nil) || matches!(condition, Expr::Bool(false))
        } else {
            false
        };
        if replace {
            if let Stat::If { else_body, .. } = stmts.remove(i) {
                if let Some(eb) = else_body {
                    for (j, s) in eb.into_iter().enumerate() {
                        stmts.insert(i + j, s);
                    }
                }
                // Don't increment i — we need to re-check what we just inserted
            }
        } else {
            i += 1;
        }
    }

    // Remove empty do-blocks that may have resulted from cleanup
    stmts.retain(|stmt| {
        !matches!(stmt, Stat::DoBlock { body } if body.is_empty())
    });
}

/// Clean nil and other artifact expressions within a statement.
fn cleanup_exprs_in_stmt(stmt: &mut Stat) {
    match stmt {
        Stat::Local { values, .. } => {
            for v in values { cleanup_expr(v); }
        }
        Stat::Assign { targets, values } => {
            for t in targets { cleanup_expr(t); }
            for v in values { cleanup_expr(v); }
        }
        Stat::ExprStat(e) => cleanup_expr(e),
        Stat::Return { values } => {
            for v in values { cleanup_expr(v); }
        }
        Stat::If { condition, then_body, elseif_clauses, else_body } => {
            cleanup_expr(condition);
            for s in then_body { cleanup_exprs_in_stmt(s); }
            for (cond, body) in elseif_clauses {
                cleanup_expr(cond);
                for s in body { cleanup_exprs_in_stmt(s); }
            }
            if let Some(eb) = else_body {
                for s in eb { cleanup_exprs_in_stmt(s); }
            }
        }
        Stat::While { condition, body } => {
            cleanup_expr(condition);
            for s in body { cleanup_exprs_in_stmt(s); }
        }
        Stat::Repeat { body, condition } => {
            for s in body { cleanup_exprs_in_stmt(s); }
            cleanup_expr(condition);
        }
        Stat::NumericFor { start, stop, step, body, .. } => {
            cleanup_expr(start);
            cleanup_expr(stop);
            if let Some(s) = step { cleanup_expr(s); }
            for s in body { cleanup_exprs_in_stmt(s); }
        }
        Stat::GenericFor { iterators, body, .. } => {
            for it in iterators { cleanup_expr(it); }
            for s in body { cleanup_exprs_in_stmt(s); }
        }
        Stat::DoBlock { body } => {
            for s in body { cleanup_exprs_in_stmt(s); }
        }
        // Phase B0.92: recurse into LocalFunction/MethodFunction bodies.
        Stat::LocalFunction { func, .. } | Stat::MethodFunction { func, .. } => {
            cleanup_expr(func);
        }
        _ => {}
    }
}

/// Clean artifact patterns in expressions recursively.
fn cleanup_expr(expr: &mut Expr) {
    // Recurse first (bottom-up)
    match expr {
        Expr::BinOp { left, right, .. } => {
            cleanup_expr(left);
            cleanup_expr(right);
        }
        Expr::UnOp { operand, .. } => cleanup_expr(operand),
        Expr::Call { func, args } => {
            cleanup_expr(func);
            for a in args { cleanup_expr(a); }
        }
        Expr::MethodCall { object, args, .. } => {
            cleanup_expr(object);
            for a in args { cleanup_expr(a); }
        }
        Expr::Field { object, .. } => cleanup_expr(object),
        Expr::Index { object, key } => {
            cleanup_expr(object);
            cleanup_expr(key);
        }
        Expr::Table { fields } => {
            for f in fields {
                match f {
                    TableField::Sequential(e) => cleanup_expr(e),
                    TableField::Named(_, e) => cleanup_expr(e),
                    TableField::Indexed(k, v) => { cleanup_expr(k); cleanup_expr(v); }
                }
            }
        }
        Expr::Function { body, .. } => {
            for s in body { cleanup_exprs_in_stmt(s); }
        }
        // Phase B0.92: recurse into Ternary sub-expressions.
        Expr::Ternary { cond, then_expr, else_expr } => {
            cleanup_expr(cond);
            cleanup_expr(then_expr);
            cleanup_expr(else_expr);
        }
        _ => {}
    }

    // Now clean this node
    let replacement = match expr {
        // nil[key] → key (table indexed by nil is always an artifact)
        Expr::Index { object, key } if matches!(object.as_ref(), Expr::Nil) => {
            Some(key.as_ref().clone())
        }
        // obj[nil] → obj (nil key is always an artifact)
        Expr::Index { object, key } if matches!(key.as_ref(), Expr::Nil) => {
            Some(object.as_ref().clone())
        }
        // nil.field → Name(field) (field access on nil is an artifact)
        Expr::Field { field, .. } if matches!(expr, Expr::Field { object, .. } if matches!(object.as_ref(), Expr::Nil)) => {
            None // handled below due to borrow issues
        }
        // nil op X or X op nil where op is arithmetic → just the other side
        Expr::BinOp { op, left, right } => {
            let left_nil = matches!(left.as_ref(), Expr::Nil);
            let right_nil = matches!(right.as_ref(), Expr::Nil);
            match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div |
                BinOp::IDiv | BinOp::Mod | BinOp::Pow => {
                    if left_nil && right_nil {
                        Some(Expr::Number(0.0))
                    } else if left_nil {
                        Some(right.as_ref().clone())
                    } else if right_nil {
                        Some(left.as_ref().clone())
                    } else {
                        None
                    }
                }
                BinOp::Concat => {
                    // nil .. X → X, X .. nil → X
                    if left_nil && right_nil {
                        Some(Expr::String(String::new()))
                    } else if left_nil {
                        Some(right.as_ref().clone())
                    } else if right_nil {
                        Some(left.as_ref().clone())
                    } else {
                        None
                    }
                }
                // "str" and "str" → "str" (common artifact in conditionals)
                BinOp::And => {
                    if let (Expr::String(a), Expr::String(b)) = (left.as_ref(), right.as_ref()) {
                        if a == b {
                            Some(Expr::String(a.clone()))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                // "str" or "str" → "str"
                BinOp::Or => {
                    if let (Expr::String(a), Expr::String(b)) = (left.as_ref(), right.as_ref()) {
                        if a == b {
                            Some(Expr::String(a.clone()))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }
        _ => None,
    };

    if let Some(r) = replacement {
        *expr = r;
    }

    // Handle nil.field separately (couldn't do it above due to borrow rules)
    if let Expr::Field { object, field } = expr {
        if matches!(object.as_ref(), Expr::Nil) {
            *expr = Expr::Name(field.clone());
        }
    }
}

/// Check if a statement is a decompiler artifact that should be removed.
fn is_dead_stmt(stmt: &Stat) -> bool {
    match stmt {
        // Self-assignment: x = x, or compound self-assignment: a.b = a.b
        // Phase B0.92: use exprs_structurally_equal for comprehensive comparison
        Stat::Assign { targets, values } => {
            if targets.len() == values.len() {
                targets.iter().zip(values.iter()).all(|(t, v)| exprs_structurally_equal(t, v))
            } else {
                false
            }
        }
        // Redundant local: local x = x
        Stat::Local { names, values } => {
            if names.len() == values.len() && !values.is_empty() {
                names.iter().zip(values.iter()).all(|(n, v)| {
                    matches!(v, Expr::Name(vn) if vn == n)
                })
            } else {
                false
            }
        }
        // Empty do-end block
        Stat::DoBlock { body } => body.is_empty(),
        // Phase B0.95b: expression statement with a trivially pure value
        // (name, literal) is dead code — a lifter artifact from unused register reads.
        // Keep field/index accesses (may have __index metamethods in Roblox).
        Stat::ExprStat(e) => matches!(e,
            Expr::Name(_) | Expr::Nil | Expr::Bool(_) | Expr::Number(_)
            | Expr::String(_) | Expr::Varargs),
        _ => false,
    }
}

// ═══════════════════════════════════════════════════════════════════
// CONSTANT FOLDING — simplify constant expressions at compile time
// ═══════════════════════════════════════════════════════════════════

/// Post-processing pass: fold constant expressions.
/// C10h: collapse `if Bool(true) then X else Y end` → X,
/// `if Bool(false) then X else Y end` → Y. Runs after fold_constants
/// so literal-compare folds into a concrete Bool first.
fn collapse_constant_ifs(stmts: &mut Vec<Stat>) {
    // Recurse into nested bodies first (bottom-up).
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::If { then_body, elseif_clauses, else_body, .. } => {
                collapse_constant_ifs(then_body);
                for (_, body) in elseif_clauses.iter_mut() {
                    collapse_constant_ifs(body);
                }
                if let Some(eb) = else_body.as_mut() {
                    collapse_constant_ifs(eb);
                }
            }
            Stat::While { body, .. }
            | Stat::Repeat { body, .. }
            | Stat::DoBlock { body }
            | Stat::NumericFor { body, .. }
            | Stat::GenericFor { body, .. } => {
                collapse_constant_ifs(body);
            }
            Stat::LocalFunction { func, .. } | Stat::MethodFunction { func, .. } => {
                if let Expr::Function { body, .. } = func {
                    collapse_constant_ifs(body);
                }
            }
            Stat::Local { values, .. } | Stat::Assign { values, .. } => {
                for v in values.iter_mut() {
                    if let Expr::Function { body, .. } = v {
                        collapse_constant_ifs(body);
                    }
                }
            }
            _ => {}
        }
    }

    // Second pass: replace constant-condition Ifs with their taken branch.
    let mut i = 0;
    while i < stmts.len() {
        let splice = match &stmts[i] {
            Stat::If { condition, then_body, elseif_clauses, else_body } => {
                match condition {
                    Expr::Bool(true) => Some(then_body.clone()),
                    Expr::Bool(false) => {
                        // Try first elseif whose condition is true-ish; else else_body; else nothing.
                        let mut taken: Option<Vec<Stat>> = None;
                        for (c, b) in elseif_clauses {
                            if matches!(c, Expr::Bool(true)) {
                                taken = Some(b.clone());
                                break;
                            }
                            if matches!(c, Expr::Bool(false)) {
                                continue;
                            }
                            // Unknown elseif condition — can't collapse whole if.
                            taken = None;
                            break;
                        }
                        if taken.is_none() && elseif_clauses.iter().all(|(c,_)| matches!(c, Expr::Bool(false))) {
                            taken = Some(else_body.clone().unwrap_or_default());
                        }
                        taken
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(body) = splice {
            stmts.splice(i..=i, body);
            // Don't advance i — recheck the newly-spliced content.
        } else {
            i += 1;
        }
    }
}

fn fold_constants_in_stmts(stmts: &mut Vec<Stat>) {
    for stmt in stmts.iter_mut() {
        match stmt {
            Stat::Local { values, .. } => {
                for v in values { fold_expr(v); }
            }
            Stat::Assign { targets, values } => {
                for t in targets { fold_expr(t); }
                for v in values { fold_expr(v); }
            }
            Stat::ExprStat(e) => fold_expr(e),
            Stat::Return { values } => {
                for v in values { fold_expr(v); }
            }
            Stat::If { condition, then_body, elseif_clauses, else_body } => {
                fold_expr(condition);
                fold_constants_in_stmts(then_body);
                for (cond, body) in elseif_clauses {
                    fold_expr(cond);
                    fold_constants_in_stmts(body);
                }
                if let Some(eb) = else_body { fold_constants_in_stmts(eb); }
            }
            Stat::While { condition, body } => {
                fold_expr(condition);
                fold_constants_in_stmts(body);
            }
            Stat::Repeat { body, condition } => {
                fold_constants_in_stmts(body);
                fold_expr(condition);
            }
            Stat::NumericFor { start, stop, step, body, .. } => {
                fold_expr(start);
                fold_expr(stop);
                if let Some(s) = step { fold_expr(s); }
                fold_constants_in_stmts(body);
            }
            Stat::GenericFor { iterators, body, .. } => {
                for it in iterators { fold_expr(it); }
                fold_constants_in_stmts(body);
            }
            Stat::DoBlock { body } => fold_constants_in_stmts(body),
            Stat::LocalFunction { func, .. } | Stat::MethodFunction { func, .. } => {
                fold_expr(func);
            }
            _ => {}
        }
    }
}

/// Recursively fold constant sub-expressions bottom-up.
fn fold_expr(expr: &mut Expr) {
    // First recurse into sub-expressions
    match expr {
        Expr::BinOp { left, right, .. } => {
            fold_expr(left);
            fold_expr(right);
        }
        Expr::UnOp { operand, .. } => fold_expr(operand),
        Expr::Call { func, args } => {
            fold_expr(func);
            for a in args { fold_expr(a); }
        }
        Expr::MethodCall { object, args, .. } => {
            fold_expr(object);
            for a in args { fold_expr(a); }
        }
        Expr::Field { object, .. } => fold_expr(object),
        Expr::Index { object, key } => {
            fold_expr(object);
            fold_expr(key);
        }
        Expr::Table { fields } => {
            for f in fields {
                match f {
                    TableField::Sequential(e) => fold_expr(e),
                    TableField::Named(_, e) => fold_expr(e),
                    TableField::Indexed(k, v) => { fold_expr(k); fold_expr(v); }
                }
            }
        }
        Expr::Function { body, .. } => fold_constants_in_stmts(body),
        // Phase B0.90: recurse into Ternary sub-expressions.
        Expr::Ternary { cond, then_expr, else_expr } => {
            fold_expr(cond);
            fold_expr(then_expr);
            fold_expr(else_expr);
        }
        _ => {}
    }

    // Now try to fold this node
    let replacement = match expr {
        Expr::BinOp { op, left, right } => {
            match (left.as_ref(), right.as_ref()) {
                // Number op Number
                (Expr::Number(a), Expr::Number(b)) => {
                    match op {
                        BinOp::Add => Some(Expr::Number(a + b)),
                        BinOp::Sub => Some(Expr::Number(a - b)),
                        BinOp::Mul => Some(Expr::Number(a * b)),
                        BinOp::Div if *b != 0.0 => Some(Expr::Number(a / b)),
                        BinOp::Mod if *b != 0.0 => Some(Expr::Number(a % b)),
                        BinOp::Pow => Some(Expr::Number(a.powf(*b))),
                        _ => None,
                    }
                }
                // String .. String and equality comparisons
                (Expr::String(a), Expr::String(b)) => {
                    match op {
                        BinOp::Concat => Some(Expr::String(format!("{}{}", a, b))),
                        // C10h: fold `"X" == "Y"` and `"X" ~= "Y"` to Bool.
                        // Covers `if "utf8" == "utf8"` artifacts and similar.
                        BinOp::Eq => Some(Expr::Bool(a == b)),
                        BinOp::NotEq => Some(Expr::Bool(a != b)),
                        _ => None,
                    }
                }
                // C10h: Bool==/~=Bool folds. Number==/~=Number is already
                // covered by the unguarded Number/Number arm above (which
                // returns None for Eq/NotEq); guarded Number arms here would be
                // unreachable, so they are intentionally omitted to preserve
                // existing behavior.
                (Expr::Bool(a), Expr::Bool(b)) if *op == BinOp::Eq => {
                    Some(Expr::Bool(a == b))
                }
                (Expr::Bool(a), Expr::Bool(b)) if *op == BinOp::NotEq => {
                    Some(Expr::Bool(a != b))
                }
                (Expr::Nil, Expr::Nil) if *op == BinOp::Eq => Some(Expr::Bool(true)),
                (Expr::Nil, Expr::Nil) if *op == BinOp::NotEq => Some(Expr::Bool(false)),
                // Boolean logic
                (Expr::Bool(true), _) if *op == BinOp::And => Some(right.as_ref().clone()),
                (Expr::Bool(false), _) if *op == BinOp::And => Some(Expr::Bool(false)),
                (Expr::Bool(true), _) if *op == BinOp::Or => Some(Expr::Bool(true)),
                (Expr::Bool(false), _) if *op == BinOp::Or => Some(right.as_ref().clone()),
                // Phase B0.92: nil short-circuits — nil is falsy.
                (Expr::Nil, _) if *op == BinOp::And => Some(Expr::Nil),
                (Expr::Nil, _) if *op == BinOp::Or => Some(right.as_ref().clone()),
                _ => None,
            }
        }
        Expr::UnOp { op, operand } => {
            match (op, operand.as_ref()) {
                (UnOp::Not, Expr::Bool(b)) => Some(Expr::Bool(!b)),
                (UnOp::Not, Expr::Nil) => Some(Expr::Bool(true)),
                (UnOp::Negate, Expr::Number(n)) => Some(Expr::Number(-n)),
                (UnOp::Length, Expr::String(s)) => Some(Expr::Number(s.len() as f64)),
                // Phase B0.92: `not <truthy-literal>` → false.
                // Numbers and strings are always truthy in Lua.
                (UnOp::Not, Expr::Number(_)) | (UnOp::Not, Expr::String(_)) => Some(Expr::Bool(false)),
                // Phase B0.91: `not (a == b)` → `a ~= b`, etc.
                // Invert comparison operators under `not` for cleaner output.
                (UnOp::Not, Expr::BinOp { left, op: cmp_op, right }) => {
                    let inverted = match cmp_op {
                        BinOp::Eq    => Some(BinOp::NotEq),
                        BinOp::NotEq => Some(BinOp::Eq),
                        BinOp::LT    => Some(BinOp::GE),
                        BinOp::LE    => Some(BinOp::GT),
                        BinOp::GT    => Some(BinOp::LE),
                        BinOp::GE    => Some(BinOp::LT),
                        _ => None,
                    };
                    inverted.map(|new_op| Expr::BinOp {
                        left: left.clone(),
                        op: new_op,
                        right: right.clone(),
                    })
                }
                _ => None,
            }
        }
        // Phase B0.90: fold Ternary with constant condition.
        // Phase B0.92: also fold identical branches.
        Expr::Ternary { cond, then_expr, else_expr } => {
            match cond.as_ref() {
                // `if true then a else b` → a
                Expr::Bool(true) => Some(then_expr.as_ref().clone()),
                // `if false then a else b` → b
                Expr::Bool(false) => Some(else_expr.as_ref().clone()),
                // `if nil then a else b` → b  (nil is falsy)
                Expr::Nil => Some(else_expr.as_ref().clone()),
                _ => {
                    // `if c then X else X` → X  (identical branches)
                    if exprs_structurally_equal(then_expr, else_expr) {
                        Some(then_expr.as_ref().clone())
                    } else {
                        None
                    }
                }
            }
        }
        _ => None,
    };

    if let Some(r) = replacement {
        *expr = r;
    }
}

// ============================================================================
// Tests (split into per-phase files under `lifter/tests/` in Phase B0.52P6).
// ============================================================================

#[cfg(test)]
mod tests;
