//! Guards lost across joins, three real v12 scripts (each read against bytecode):
//!
//! * IrisLog ClientRuntime `renderReact` proto 98 pc89-126: `if tab then if U15
//!   then A else if not tab.LINE_RENDER_START then C end end end`. The nested
//!   jumps target the merge past the arm end (126 is the arm's `JUMP -> 131`),
//!   and the exit alias was refused because a generic `for` loop variable (R9/R10)
//!   looked like a value carried across the join.
//! * BloxFruits AssetComponent: `(Type and tbl[Type].DisplayName) or ""` is one
//!   expression, not an assignment inside `if Type` plus an unconditional `= ""`.
//! * AdoptMe DisplayStandUseHelper `server_use`: the pet checks of the `if` chain
//!   are folded into its condition and the else path writes `nil`.

const IRIS: &[u8] = include_bytes!("fixtures/guards1029_iris_client_runtime.luac");
const ASSET: &[u8] = include_bytes!("fixtures/guards1029_asset_component.luac");
const STAND: &[u8] = include_bytes!("fixtures/guards1029_display_stand.luac");

fn decompiled(bytes: &[u8]) -> String {
    crate::decompile(bytes).expect("fixture decompiles")
}

#[test]
fn nested_arm_guards_to_the_merge_past_the_arm_end_are_kept() {
    let out = decompiled(IRIS);
    assert!(out.contains("if v52 then"), "outer tab guard lost:\n{out}");
    assert!(out.contains("if not LINE_RENDER_START10 then"), "unset-only guard lost:\n{out}");
    assert!(out.contains("if not LINE_RENDER_START11 then"), "unset-only guard lost (popups):\n{out}");
}

#[test]
fn and_or_value_with_fallback_is_one_expression() {
    let out = decompiled(ASSET);
    assert!(out.contains(".DisplayName or \"\""), "and/or value not rebuilt:\n{out}");
    assert!(!out.contains("sel16 = \"\""), "fallback assigned unconditionally:\n{out}");
}

#[test]
fn pet_checks_stay_in_the_condition_and_else_writes_nil() {
    let out = decompiled(STAND);
    assert!(out.contains(".is_customizable and not result6.properties.temporary then"), "pet checks lost:\n{out}");
    assert!(out.contains("arg2.displayed_worn_items = nil"), "else arm no longer writes nil:\n{out}");
}
