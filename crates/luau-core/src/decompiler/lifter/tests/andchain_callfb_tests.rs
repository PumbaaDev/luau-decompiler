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
