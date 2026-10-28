//! `dropped_arm_value`: `x and x:IsA(T)` chains whose pass path is laid out
//! AFTER the fail path, so the guard jumps cross over a shared fail block.

const DRAG_DELETE: &[u8] = include_bytes!("fixtures/arm_dragdelete.luac");
const FIREFLIES: &[u8] = include_bytes!("fixtures/arm_fireflies.luac");

#[test]
fn inlined_title_text_keeps_both_guard_terms_and_the_else() {
    let out = crate::decompile(DRAG_DELETE).expect("decompiles");
    let i = out.find("local function syncToGui").expect("function present");
    let body = &out[i..];
    let body = &body[..body.find("local function hookGui").expect("next function")];
    assert!(
        body.contains("if not Title3 or not Title3:IsA(\"TextLabel\") then"),
        "both guard terms must survive:
{body}"
    );
    assert!(body.contains("= Title3.Text"), "else arm lost:
{body}");
    assert!(!body.contains("local isType"), "IsA parked as a dead local:
{body}");
}
