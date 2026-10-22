//! Non-loop OUT-OF-RANGE conditional jumps (`lifter/oor_guard.rs`).
//!
//! A forward conditional jump whose target lies past the end of the range
//! being lifted used to be silently DISCARDED outside a loop, so the guarded
//! remainder ran on both paths. Two exact rewrites recover it:
//!
//! * S1 exit alias: the target is where the range itself exits to (its last
//!   instruction is `JUMP -> t`, or it falls through into `code[end] = JUMP ->
//!   t`), so the jump is a jump to the range end;
//! * S2 terminal return: the target starts a terminal `RETURN` block, so the
//!   jump is `if <fire> then return .. end`.
//!
//! The canonical fixture is `fixtures/oor_nonloop_o1.lua` compiled by upstream
//! `luau-compile --fflags=LuauEmitCallFeedback=false --binary -O1 -g1`. At -O1
//! `foldJumps` retargets a jump whose natural target was an arm's exit JUMP to
//! the OUTER merge, which is past the arm's lift range: before this fix `Nest`
//! lost its inner `else` (print(2) ran unconditionally), `NestNoElse` printed
//! 1 unconditionally, `Pop` lost `type(p) == "number"`, and `b` (`c and A or
//! B`) always returned B.

use crate::ast::Expr;
use crate::decompiler::lifter::oor_guard::{range_exit_alias, terminal_return_at, RetVal};
use crate::decompiler::lifter::RegVal;
use crate::decompiler::DecompileContext;
use crate::parser::types::{Chunk, Constant, Proto};

const FIXTURE_O1: &[u8] = include_bytes!("fixtures/oor_nonloop_o1.luac");

fn norm(s: &str) -> String {
    s.lines().map(|l| l.trim_end()).collect::<Vec<_>>().join("\n")
}

fn decompiled() -> String {
    norm(&crate::decompile(FIXTURE_O1).expect("fixture decompiles"))
}

fn assert_has(out: &str, want: &str) {
    let want = norm(want);
    assert!(out.contains(&want), "missing:\n{want}\n--- in output ---\n{out}");
}

#[test]
fn s1_alias_keeps_nested_guard_without_else() {
    // `if p then if p.a then print(1) end else print(3) end print(4)`.
    let out = decompiled();
    assert_has(
        &out,
        "            if self.a then
                print(1)
            end
        else
            print(3)
        end
        print(4)",
    );
}

#[test]
fn s1_else_alias_keeps_inner_else() {
    // `if p then if p.a then print(1) else print(2) end else print(3) end`.
    let out = decompiled();
    assert_has(
        &out,
        "            if self.a then
                print(1)
            else
                print(2)
            end
        else
            print(3)
        end
        print(4)",
    );
}

#[test]
fn s1_alias_keeps_type_guard() {
    // `if p then if type(p)=="number" then p = UDim2.new(p,0,p,0) end else ..`
    let out = decompiled();
    assert_has(
        &out,
        "        if arg2 then
            if type(arg2) == \"number\" then
                local new = UDim2.new(arg2, 0, arg2, 0)
                arg2 = new
            end",
    );
}

#[test]
fn s2_value_carrying_return_guard() {
    // `return #s % 3 == 0 and s:sub(2) or s:upper()`: the `JUMPIF` of the
    // `and` value goes straight to the terminal `RETURN`.
    let out = decompiled();
    assert_has(
        &out,
        "        if #self % 3 == 0 then
            local sub = self:sub(2)
            if sub then
                return sub
            end
        end
        local upper = self:upper()
        return upper",
    );
}

#[test]
fn in_range_guards_are_unchanged() {
    // Shapes the region structurer already handled must not move.
    let out = decompiled();
    assert_has(
        &out,
        "    a = function(arg1, arg2, arg3)
        if arg1 then
            if not arg2 then
                return
            end
            print(1)
        else
            print(2)
        end
        print(3)
    end,",
    );
    assert_has(
        &out,
        "    c = function(self, arg2)
        if self.Frame ~= nil then
            self.X = 1
            if arg2 then
                self.Frame.P = 2
                return
            end
            self.Frame:T(3)
        end
    end,",
    );
}

// ---- helper-level tests -------------------------------------------------

const OP_LOADNIL: u32 = 2;
const OP_LOADB: u32 = 3;
const OP_LOADK: u32 = 5;
const OP_MOVE: u32 = 6;
const OP_CALL: u32 = 21;
const OP_RETURN: u32 = 22;
const OP_JUMP: u32 = 23;
const OP_JUMPIFNOT: u32 = 26;

fn abc(op: u32, a: u32, b: u32, c: u32) -> u32 {
    op | (a << 8) | (b << 16) | (c << 24)
}
fn ad(op: u32, a: u32, d: i16) -> u32 {
    op | (a << 8) | ((d as u16 as u32) << 16)
}

fn chunk_with(code: Vec<u32>, constants: Vec<Constant>) -> Chunk {
    let proto = Proto {
        max_stack_size: 8,
        num_params: 2,
        num_upvalues: 0,
        is_vararg: false,
        flags: 0,
        typeinfo: None,
        code,
        constants,
        child_protos: Vec::new(),
        line_defined: 1,
        debug_name: Some("t".to_string()),
        line_info: None,
        debug_info: None,
    };
    Chunk { version: 6, types_version: 0, strings: Vec::new(), protos: vec![proto], main_proto: 0 }
}

fn name(n: &str) -> RegVal {
    RegVal::Expr(Expr::Name(n.to_string()))
}

fn known(v: &[RetVal]) -> Vec<Expr> {
    v.iter()
        .map(|r| match r {
            RetVal::Known(e) => e.clone(),
            RetVal::Operand(r) => panic!("unexpected operand bind of R{r}"),
        })
        .collect()
}

#[test]
fn terminal_return_bare_and_literal_blocks() {
    // 0: RETURN R0 1          (bare return)
    // 1: LOADB R2 false ; 2: RETURN R2 2
    let chunk = chunk_with(
        vec![abc(OP_RETURN, 0, 1, 0), abc(OP_LOADB, 2, 0, 0), abc(OP_RETURN, 2, 2, 0)],
        vec![],
    );
    let ctx = DecompileContext::new(&chunk);
    let p = &chunk.protos[0];
    let regs = vec![name("a"), name("b"), RegVal::Unknown];
    let bare = terminal_return_at(&ctx, p, &regs, 0, &[1], false).expect("bare return");
    assert!(bare.is_empty());
    let lit = terminal_return_at(&ctx, p, &regs, 1, &[1], false).expect("literal return");
    assert!(matches!(known(&lit).as_slice(), [Expr::Bool(false)]));
}

#[test]
fn terminal_return_reads_jump_site_register() {
    // RETURN R1 2 returns whatever R1 holds at the jump.
    let chunk = chunk_with(vec![abc(OP_RETURN, 1, 2, 0)], vec![]);
    let ctx = DecompileContext::new(&chunk);
    let regs = vec![name("a"), name("Apply")];
    let v = terminal_return_at(&ctx, &chunk.protos[0], &regs, 0, &[1], false).unwrap();
    assert!(matches!(known(&v).as_slice(), [Expr::Name(n)] if n == "Apply"));
}

#[test]
fn terminal_return_refusals() {
    let chunk = chunk_with(
        vec![
            abc(OP_RETURN, 0, 0, 0),  // 0: multret
            abc(OP_MOVE, 0, 1, 0),    // 1: not a terminal op
            abc(OP_RETURN, 0, 1, 0),  // 2
            abc(OP_RETURN, 1, 2, 0),  // 3: return R1
        ],
        vec![],
    );
    let ctx = DecompileContext::new(&chunk);
    let p = &chunk.protos[0];
    let regs = vec![name("a"), name("b")];
    assert_eq!(terminal_return_at(&ctx, p, &regs, 0, &[0], false).err(), Some("multret"));
    assert_eq!(terminal_return_at(&ctx, p, &regs, 1, &[0], false).err(), Some("not_terminal"));
    // Unknown tested register: the fire condition would mint a `vN`.
    let unk = vec![RegVal::Unknown, name("b")];
    assert_eq!(terminal_return_at(&ctx, p, &unk, 2, &[0], false).err(), Some("operand_unknown"));
    // A literal in every tested register is a stale value, never a real test.
    let lit = vec![RegVal::Expr(Expr::Nil), name("b")];
    assert_eq!(terminal_return_at(&ctx, p, &lit, 2, &[0], false).err(), Some("operand_literal"));
    // A returned register holding a literal it was not given by the block.
    let stale = vec![name("a"), RegVal::Expr(Expr::Bool(true))];
    assert_eq!(terminal_return_at(&ctx, p, &stale, 3, &[0], false).err(), Some("value_reg_literal"));
    // A call value would run twice unless it is the tested operand.
    let call = Expr::Call { func: Box::new(Expr::Name("f".into())), args: vec![] };
    let callregs = vec![name("a"), RegVal::Expr(call)];
    assert_eq!(terminal_return_at(&ctx, p, &callregs, 3, &[0], false).err(), Some("value_unstable"));
    let bound = terminal_return_at(&ctx, p, &callregs, 3, &[1], true).unwrap();
    assert!(matches!(bound.as_slice(), [RetVal::Operand(1)]));
}

#[test]
fn exit_alias_rules() {
    // [0, 3): 0 JUMPIFNOT R0 -> 4 ; 1 LOADK ; 2 JUMP -> 4 ; 3 LOADNIL ; 4 RETURN
    let code = vec![
        ad(OP_JUMPIFNOT, 0, 3),
        ad(OP_LOADK, 1, 0),
        ad(OP_JUMP, 0, 1),
        abc(OP_LOADNIL, 1, 0, 0),
        abc(OP_RETURN, 0, 1, 0),
    ];
    let chunk = chunk_with(code.clone(), vec![Constant::Number(1.0)]);
    let p = &chunk.protos[0];
    // Range [0,3) ends with `JUMP -> 4`: target 4 aliases to that JUMP (pc 2).
    assert_eq!(range_exit_alias(&code, p, 0, 0, 3, 4, false), Some(2));
    // In a loop nothing changes (loop-control track).
    assert_eq!(range_exit_alias(&code, p, 0, 0, 3, 4, true), None);
    // A target that is not the exit is left alone.
    assert_eq!(range_exit_alias(&code, p, 0, 0, 3, 5, false), None);
    // Range [0,2) falls through into `code[2] = JUMP -> 4`: alias to 2.
    assert_eq!(range_exit_alias(&code, p, 0, 0, 2, 4, false), Some(2));

    // A range ending in RETURN never falls into code[end].
    let code2 = vec![
        ad(OP_JUMPIFNOT, 0, 3),
        abc(OP_CALL, 1, 1, 1),
        abc(OP_RETURN, 0, 1, 0),
        ad(OP_JUMP, 0, 0),
        abc(OP_RETURN, 0, 1, 0),
    ];
    let chunk2 = chunk_with(code2.clone(), vec![]);
    assert_eq!(range_exit_alias(&code2, &chunk2.protos[0], 0, 0, 3, 4, false), None);
}


#[test]
fn else_alias_refused_when_it_would_strand_an_and_link() {
    // `5348f2ef UpdateColor` shape: then-arm [1, 5) ends with the else-skip
    // `JUMP -> 9` at pc 5; the second `and` link at pc 3 targets the else
    // start 6. Aliasing the else-skip would shrink the then-arm to [1, 5) and
    // strand that link out of range, so it must be refused.
    //   0: JUMPIFNOT R0 -> 6    (outer, lifted by the caller)
    //   1: LOADK R1 ; 2: MOVE R2 R1
    //   3: JUMPIFNOT R2 -> 6    (and-link to the else start)
    //   4: LOADK R3
    //   5: JUMP -> 9            (else-skip to the range's exit)
    //   6: LOADK R3 ; 7: LOADK R4
    //   8: JUMP -> 9            (range [0, 8) falls into this exit JUMP)
    //   9: RETURN
    let code = vec![
        ad(OP_JUMPIFNOT, 0, 5),
        ad(OP_LOADK, 1, 0),
        abc(OP_MOVE, 2, 1, 0),
        ad(OP_JUMPIFNOT, 2, 2),
        ad(OP_LOADK, 3, 0),
        ad(OP_JUMP, 0, 3),
        ad(OP_LOADK, 3, 0),
        ad(OP_LOADK, 4, 0),
        ad(OP_JUMP, 0, 0),
        abc(OP_RETURN, 0, 1, 0),
    ];
    let chunk = chunk_with(code.clone(), vec![Constant::Number(1.0)]);
    let p = &chunk.protos[0];
    use crate::decompiler::lifter::oor_guard::else_skip_exit_alias;
    assert!(!else_skip_exit_alias(&code, p, 0, 1, 6, 8, 5, 9, false));
    // Without the stranded link (pc 3 retargeted to 4) the alias applies.
    let mut code2 = code.clone();
    code2[3] = ad(OP_JUMPIFNOT, 2, 0);
    let chunk2 = chunk_with(code2.clone(), vec![Constant::Number(1.0)]);
    assert!(else_skip_exit_alias(&code2, &chunk2.protos[0], 0, 1, 6, 8, 5, 9, false));
}

#[test]
fn scope_hazard_counts_writes_after_scope_close() {
    use crate::ast::Stat;
    use crate::decompiler::lifter::scope_hazard_count;
    use std::collections::HashSet;
    let n = |s: &str| Expr::Name(s.to_string());
    // if c then local x = 1 end ; x = 2   -> the write is a GLOBAL write
    let bad = vec![
        Stat::If {
            condition: n("c"),
            then_body: vec![Stat::Local { names: vec!["x".into()], values: vec![Expr::Number(1.0)] }],
            elseif_clauses: vec![],
            else_body: None,
        },
        Stat::Assign { targets: vec![n("x")], values: vec![Expr::Number(2.0)] },
    ];
    assert_eq!(scope_hazard_count(&bad, &HashSet::new()), 1);
    // local x ; if c then x = 1 end ; return x   -> fine
    let good = vec![
        Stat::Local { names: vec!["x".into()], values: vec![] },
        Stat::If {
            condition: n("c"),
            then_body: vec![Stat::Assign { targets: vec![n("x")], values: vec![Expr::Number(1.0)] }],
            elseif_clauses: vec![],
            else_body: None,
        },
        Stat::Return { values: vec![n("x")] },
    ];
    assert_eq!(scope_hazard_count(&good, &HashSet::new()), 0);
}

#[test]
fn hazards_compare_per_class_and_count_distinct_reads() {
    use crate::ast::Stat;
    use crate::decompiler::lifter::{scope_hazards, Hazards};
    use std::collections::HashSet;
    let n = |s: &str| Expr::Name(s.to_string());
    // Re-reading the same unbound name twice is ONE hazard, not two.
    let out = vec![
        Stat::Return { values: vec![n("v4")] },
        Stat::Return { values: vec![n("v4")] },
    ];
    let (_, reads, uses) = scope_hazards(&out, &HashSet::new());
    assert_eq!(uses, 2);
    assert_eq!(reads.len(), 1);
    // A bare vN that is the proto's own upvalue name is bound.
    let ups: HashSet<String> = ["v4".to_string()].into_iter().collect();
    let (_, reads, _) = scope_hazards(&out, &ups);
    assert!(reads.is_empty());
    // A gain in one class cannot hide a loss in another.
    let on = Hazards { upvalue: 1, scope: 0, scope_uses: 0, orphan: 0 };
    let off = Hazards { upvalue: 0, scope: 2, scope_uses: 2, orphan: 0 };
    assert!(on.worse_than(off));
    assert!(!off.worse_than(off));
    // Scope and orphan hazards trade by their sum.
    let on = Hazards { upvalue: 0, scope: 0, scope_uses: 0, orphan: 1 };
    let off = Hazards { upvalue: 0, scope: 1, scope_uses: 1, orphan: 0 };
    assert!(!on.worse_than(off));
}

#[test]
fn name_dedup_alone_cannot_flip_an_on_lift_to_off() {
    use crate::decompiler::lifter::Hazards;
    // OFF looks cleaner only because its repeated reads were deduplicated.
    let on = Hazards { upvalue: 0, scope: 0, scope_uses: 0, orphan: 331 };
    let off = Hazards { upvalue: 0, scope: 138, scope_uses: 300, orphan: 192 };
    assert!(!on.worse_than(off));
}
