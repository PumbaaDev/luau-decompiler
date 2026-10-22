//! Three REAL game scripts from the v12 oracle (Potassium's reconstruction of
//! each is the ground truth quoted in the comments). Each one exercises a
//! distinct way an out-of-range conditional jump used to be lost or mangled:
//!
//! * `strCommas`: `#s % 3 == 0 and X or Y` - the jump to the terminal `RETURN`
//!   was dropped, so the function always returned `Y` (S2 terminal-return).
//! * `Heartbeat` loop (b928db02): `not a or b ~= c` whose first link targets the
//!   `RETURN` gap; joining the span dropped the loop body's `Run()` call.
//! * `UpdateButtonVisuals` closure: a `return` guard inside a `while` body sits
//!   in a pc gap of the loop's block list, so its jump left the lifted run and
//!   became `if USE_COOLDOWN then continue end` (swapped, wrong keyword).

const STRCOMMAS: &[u8] = include_bytes!("fixtures/oor_real_strcommas.luac");
const GAPINTO: &[u8] = include_bytes!("fixtures/oor_real_gap_into.luac");
const LOOPGUARD: &[u8] = include_bytes!("fixtures/oor_real_loopguard.luac");

fn norm(s: &str) -> String {
    s.lines().map(|l| l.trim_end()).collect::<Vec<_>>().join("\n")
}

fn decompiled(bytes: &[u8]) -> String {
    norm(&crate::decompile(bytes).expect("fixture decompiles"))
}

fn assert_has(out: &str, want: &str) {
    let want = norm(want);
    assert!(out.contains(&want), "missing:\n{want}\n--- in output ---\n{out}");
}

#[test]
fn str_commas_keeps_the_and_or_guard_return() {
    // Potassium: return #p1 % 3 == 0 and <X> or <Y>
    let out = decompiled(STRCOMMAS);
    assert_has(
        &out,
        "    if #self % 3 == 0 then
        local sub = self:reverse():gsub(\"(%d%d%d)\", \"%1,\"):reverse():sub(2)
        if sub then
            return sub
        end
    end
    local reversed4",
    );
}

#[test]
fn while_body_return_guard_is_a_return_not_a_continue() {
    // Potassium: while u2 do task.wait(1); if not u2 then return end; ... end
    let out = decompiled(LOOPGUARD);
    assert_has(
        &out,
        "            while USE_COOLDOWN do
                task.wait(1)
                if not USE_COOLDOWN then
                    return
                end",
    );
}

#[test]
fn loop_body_keeps_its_call_when_a_chain_targets_the_return_gap() {
    let out = decompiled(GAPINTO);
    assert!(out.contains("Run4()"), "loop body call dropped:
{out}");
}

const VALUE_CHAIN: &[u8] = include_bytes!("fixtures/oor_real_value_chain.luac");

#[test]
fn and_chain_value_register_is_not_aliased_to_the_range_end() {
    // IceSkating: `local trail = a and a:FindFirstChild("FL1") and ...` leaves the
    // chain's value in one register; the inner JUMPIFNOT targets the join past
    // the then-arm's end-skip JUMP. Treating that as a jump to the range end
    // dropped the value, so the local was declared and never assigned.
    let out = decompiled(VALUE_CHAIN);
    let from = out.find("local Trail2\n").expect("Trail2 declared");
    let to = from + out[from..].find("local Trail5").expect("Trail5 declared");
    let region = &out[from..to];
    assert!(region.contains("Trail2 ="), "Trail2 never assigned:\n{region}");
}

const ARM_VALUE: &[u8] = include_bytes!("fixtures/oor_real_arm_value.luac");

#[test]
fn arm_value_read_after_an_outside_join_is_not_bound_to_a_throwaway_local() {
    // CogsPerRound.Description: `if Op == "Add" then if Value < 0 then Op ..= "-" else
    // Op ..= "+" end elseif ..`. The inner arms jump straight to the OUTER end, past
    // the lifted range, where `Op ..= " Cogs Per Round"` reads Op. Re-aiming that
    // jump at the range end bound the arms' writes to a fresh `sel3` that nothing
    // read, so the Add path never set Op.
    let out = decompiled(ARM_VALUE);
    assert!(!out.contains("= sel3"), "arm result bound to a throwaway local:\n{out}");
    assert!(out.contains("Op = (Op2 .. \"-\")"), "Add arm no longer writes Op:\n{out}");
}

const LOOP_GAP_ORPHAN: &[u8] = include_bytes!("fixtures/oor_real_loop_gap_orphan.luac");

#[test]
fn loop_gap_join_does_not_write_an_orphan_name_that_becomes_a_global() {
    // Brookhaven CountdownTimer: joining a loop body across its early-exit gap
    // wrote `Position = 0` (and friends) inside a task.spawn closure with no
    // `local` in that body. A sibling function declares `local Position`, so the
    // chunk-level hoist refuses and every one of those writes became a GLOBAL
    // write. The hazard fallback must keep the gap-join-free lift here.
    let out = decompiled(LOOP_GAP_ORPHAN);
    for line in out.lines().map(str::trim) {
        assert!(line != "Position = 0", "orphan write that becomes a global:\n{out}");
    }
}
