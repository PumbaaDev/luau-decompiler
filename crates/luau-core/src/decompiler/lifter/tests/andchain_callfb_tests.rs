//! RaceRallySchedule (live v14): `a and F.x:Get()` where the right operand ends
//! in CALLFB. `remap_chunk` turns CALLFB into CALL + NOP, which hid the
//! short-circuit chain and dropped the right operand.
//!
//! Bytecode (proto IsScheduled): GETUPVAL/GETTABLEKS/NAMECALL/CALL/NOP,
//! JUMPIFNOT R0, GETUPVAL/GETTABLEKS/NAMECALL/CALL/NOP, RETURN R0.

const SCHEDULE: &[u8] = include_bytes!("fixtures/andchain_callfb_race_rally_schedule.luac");

fn decompiled() -> String {
    crate::decompile(SCHEDULE).expect("fixture decompiles")
}

#[test]
fn is_scheduled_keeps_the_right_operand_of_the_and_chain() {
    let out = decompiled();
    assert!(
        out.contains("and RaceRallyFlags.ScheduleEnabled:Get()"),
        "right operand lost:
{out}"
    );
}

#[test]
fn claims_boundary_keeps_the_modulo_comparison() {
    let out = decompiled();
    assert!(
        out.contains("% RaceRallyFlags.ScheduleIntervalSeconds:Get() == 0"),
        "modulo comparison lost:
{out}"
    );
}

#[test]
fn no_stray_v2_local_is_minted() {
    let out = decompiled();
    assert!(!out.contains("v2"), "stray v2:
{out}");
}

// Duplicated-prefix regressions. With the CALLFB chain kept whole in the CFG, the
// condition block of a following `if` started at the top of a long run of
// statements, and the old double-lift split emitted that run (calls included)
// twice.

const GIVE_ITEM: &[u8] = include_bytes!("fixtures/andchain_callfb_rideapet_giveitem.luac");
const MODIFICATION: &[u8] = include_bytes!("fixtures/andchain_callfb_bloxfruits_modification.luac");

#[test]
fn give_item_emits_its_prefix_statements_once() {
    let out = crate::decompile(GIVE_ITEM).expect("fixture decompiles");
    assert_eq!(out.matches("arg4 = arg4 or 1").count(), 1, "duplicated:\n{out}");
    assert_eq!(out.matches("arg2.CharacterAdded:Wait()").count(), 1, "duplicated:\n{out}");
    assert_eq!(out.matches(":FindFirstChild(arg3.Name)").count(), 2, "duplicated:\n{out}");
}

const SAKURA: &[u8] = include_bytes!("fixtures/andchain_callfb_sakura_incubator.luac");
const WEATHER: &[u8] = include_bytes!("fixtures/andchain_callfb_rideapet_weather.luac");

/// `local prev = c.Archivable; c.Archivable = true; ..; c.Archivable = prev`:
/// the parked field read must be emitted before the store, or the restore
/// degenerates into a dropped self-assignment.
#[test]
fn archivable_read_is_emitted_before_the_store_that_clobbers_it() {
    let out = crate::decompile(SAKURA).expect("fixture decompiles");
    let set_true = out.find("Character.Archivable = true").expect("store present");
    let before = &out[..set_true];
    let read_line = before.rsplit('\n').nth(1).unwrap_or("");
    assert!(read_line.contains("= Character.Archivable"), "read lost: {read_line}\n{out}");
    assert!(out[set_true..].contains("Character.Archivable = Archivable"), "restore lost:\n{out}");
}

/// An `x and x:GetAttribute()` chain inside a polling loop is left to control
/// flow: keeping it whole rotated the `while` and duplicated the loop prefix.
#[test]
fn polling_loop_prefix_is_not_duplicated() {
    let out = crate::decompile(WEATHER).expect("fixture decompiles");
    assert_eq!(out.matches("task.wait(0.25)").count(), 1, "loop prefix duplicated:\n{out}");
}

#[test]
fn get_if_can_use_decodes_once_and_keeps_the_whole_or_chain() {
    let out = crate::decompile(MODIFICATION).expect("fixture decompiles");
    let start = out.find("tbl4.getIfCanUse(arg1").expect("getIfCanUse present");
    let body = &out[start..];
    let end = body.find("\nend\n").unwrap_or(body.len());
    let body = &body[..end];
    assert_eq!(body.matches("decodeAdorneeData(arg3)").count(), 1, "decode duplicated:\n{body}");
    assert_eq!(body.matches("decodeModificationData(arg2)").count(), 1, "decode duplicated:\n{body}");
    assert_eq!(body.matches("getIfUnlocked(").count(), 1, "chain wrong:\n{body}");
}

// Refinement regressions (adversarial review of 9e827fd).

const SWAP_UI: &[u8] = include_bytes!("fixtures/andchain_callfb_dqr_character_swap_ui.luac");
const AURA: &[u8] = include_bytes!("fixtures/andchain_callfb_bloxfruits_aura_visuals.luac");
const HYDRANT: &[u8] = include_bytes!("fixtures/andchain_callfb_adoptme_fire_hydrant.luac");
const CATALOG_HEADER: &[u8] = include_bytes!("fixtures/andchain_callfb_adoptme_catalog_header.luac");

/// `Parent:FindFirstChild(Name.."3d") or (Name == "label" and ...) or nil`: an
/// operand with a branch of its own must not fold (the `Name == "label"` guard
/// and the `labelShadow` lookup were dropped or duplicated).
#[test]
fn branching_or_operand_keeps_its_name_guard() {
    let out = crate::decompile(SWAP_UI).expect("fixture decompiles");
    assert!(!out.contains("child4 or self.Parent:FindFirstChild(\"labelShadow\")"), "guard dropped:\n{out}");
    assert!(!out.contains("labelShadow4 == \"label\""), "invented global compare:\n{out}");
    assert!(out.contains("self.Name == \"label\""), "guard lost:\n{out}");
}

/// A fold whose simplification would discard the operand's call is refused.
#[test]
fn chain_fold_does_not_discard_a_call() {
    let out = crate::decompile(AURA).expect("fixture decompiles");
    assert!(out.contains("if Callback or BusoColor3 then"), "BusoColor3 operand lost:\n{out}");
}

/// A rejected speculative chain lift must not leave its locals declared: the
/// real lift then emitted bare assignments and a module-level `local` hoist.
#[test]
fn rejected_chain_does_not_hoist_function_locals() {
    let out = crate::decompile(HYDRANT).expect("fixture decompiles");
    assert!(!out.contains("local fn, modifiers, sit_anim_name"), "hoisted:\n{out}");
    assert!(out.contains("local modifiers = arg2.modifiers"), "declaration lost:\n{out}");
}

/// Reads parked before a calling chain operand keep their place: the Frame
/// props table (with `self.layout_order`) is named before the child calls.
#[test]
fn pending_reads_are_named_before_a_calling_chain() {
    let out = crate::decompile(CATALOG_HEADER).expect("fixture decompiles");
    assert!(out.contains("local createElement = value4.createElement"), "createElement moved:\n{out}");
    assert!(out.contains("local tbl4 = {"), "props table not named:\n{out}");
    assert!(out.contains("return createElement(\"Frame\", tbl4, tbl2)"), "props inlined late:\n{out}");
}
