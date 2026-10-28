//! `dropped_arm_value`: `if not a or not a:IsA(T) then <body> end` where the
//! body does NOT return and falls into the continuation.
//!
//! BloxFruits SafeHumanoidAddAccessory: `JUMPIFNOT R -> body`, `R:IsA(..)`,
//! `JUMPIF -> past body`, `error(..)`. The or-guard structurer only took bodies
//! whose every exit returns; this one falls into the code after it, so the
//! plain if/else fallback made `if self.Parent then <IsA parked dead> end`
//! followed by an UNCONDITIONAL `error(..)`.

const SAFE_HUMANOID: &[u8] = include_bytes!("fixtures/arm2_safehumanoid.luac");

#[test]
fn error_guard_keeps_both_terms_and_stays_conditional() {
    let out = crate::decompile(SAFE_HUMANOID).expect("fixture decompiles");
    let start = out.find("local function SafeHumanoidAddAccessory").expect("function present");
    let body = &out[start..];
    let body = &body[..body.find("local import2").expect("tail present")];
    assert!(
        body.contains("if not self.Parent or not self.Parent:IsA(\"Model\") then"),
        "both guard terms must survive:\n{body}"
    );
    assert!(
        body.contains("then\n        error("),
        "error must be inside the guard:\n{body}"
    );
    assert!(!body.contains("local isType"), "IsA parked as a dead local:\n{body}");
}
