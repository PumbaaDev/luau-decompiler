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

const BILLBOARD_SLIDER: &[u8] = include_bytes!("fixtures/v14_props_billboard_slider.luac");
const ITEMS_CONTAINER: &[u8] = include_bytes!("fixtures/v14_items_container.luac");

#[test]
fn literal_initialised_locals_do_not_make_an_arm_guard_a_value_chain() {
    // Brookhaven PropsBillboardSlider: R2/R3 are declared by LOADB/LOADN before
    // three arms that each end in `JUMP -> join`. The inner `JUMPIFNOTLT` guards
    // target that join. They were refused as a "value chain" (R2/R3 are read at
    // the join) and the guarded bodies then ran unconditionally.
    let out = decompiled(BILLBOARD_SLIDER);
    assert!(out.contains("if 0.05 < Enabled32 then"), "X-arm guard lost:\n{out}");
    assert!(out.contains("if 0.05 < Enabled33 then"), "Y-arm guard lost:\n{out}");
}

#[test]
fn self_referencing_closures_are_not_folded_into_the_module_constructor() {
    // PetSimulator99 Items/Container: 33 closures capture the module table and
    // `new` stores `__index = tbl`. Folding them into `return { ... }` left
    // `tbl` undefined; the table must stay a named local.
    let out = decompiled(ITEMS_CONTAINER);
    assert!(out.contains("local tbl = {}"), "module table not kept:\n{out}");
    assert!(out.contains("function tbl.new("), "constructor not a field function:\n{out}");
    assert!(out.contains("return tbl"), "module does not return its table:\n{out}");
    assert!(!out.contains("return {\n"), "closures folded into return {{}}:\n{out}");
}
#[test]
fn else_arm_or_chain_keeps_its_second_link() {
    // Brookhaven PropsBillboardSlider proto@190, the last arm:
    //     59: JUMPIFLT    0.05 < |X| -> 70      first link: fire -> body
    //     61..67: |Y| = math.abs(pos.Y)
    //     68: JUMPIFNOTLT 0.05 >= |Y| -> 93     second link: skip the body
    //     70..92: if |X| > |Y| then .. else .. end; R2 = true
    // i.e. `if 0.05 < |X| or 0.05 < |Y| then .. end`. The first link was lifted
    // as an inline `if` whose then-arm [61, 70) held the second link, whose
    // target 93 lies past that arm, so it was dropped: the arm became
    // `if 0.05 >= |X| then <|Y| computed and discarded> end` and the body ran
    // unconditionally (Thumbstick1 always true, the slider always moved).
    let out = decompiled(BILLBOARD_SLIDER);
    assert!(
        out.contains(" or 0.05 < math.abs("),
        "second `or` link lost:\n{out}"
    );
    assert!(!out.contains("if 0.05 >= Y then"), "first link lifted as a bare guard:\n{out}");
}

const CAMERA_SCRIPT: &[u8] = include_bytes!("fixtures/v9_camera_script_or_and_else.luac");

#[test]
fn or_chain_whose_body_ands_into_the_else_arm_is_not_folded() {
    // v9 lv_0907 CameraScript OnCameraMovementModeChange:
    //     84: JUMPIFEQ    cameraType == Custom -> 88    first link: fire -> body
    //     86..87: JUMPIF  U6 -> 119                     second link: skip the body
    //     92: JUMPIFNOTEQ arg1 ~= Classic.Name -> 119   in the body: AND into the else arm
    //    118: JUMP -> 131                               else-skip; 119.. is the else arm
    // i.e. `if (Custom or not U6) and arg1 == Classic.Name then .. else ..`.
    // Folded as `if Custom or not U6 then <body> else ..` the pc 92 test has no
    // place to go and vanished: any Custom camera took the Classic branch.
    let out = decompiled(CAMERA_SCRIPT);
    assert!(
        out.contains("if arg1 == Enum.ComputerCameraMovementMode.Classic.Name then"),
        "the arg1 == Classic.Name conjunct was lost:\n{out}"
    );
}
