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
