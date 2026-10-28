//! `dropped_arm_value`: a loop body guarded by a call-valued test whose last
//! statement is a zero-result call on the tested register.
//!
//! AdoptMe `hide_billboards`: `for _, v in x:GetDescendants() do if
//! v:IsA("BillboardGui") then ... table.insert(t, c) end end`. The guard is
//! `JUMPIFNOT R6 -> FORGLOOP` and the body ends in `CALL R6 (0 results)`. The
//! and/or chain recogniser read the whole body as `R6 = R6 and <body>`, so the
//! CFG kept it whole, the guard was lost and the `IsA` call parked as a dead
//! local with the body running for every descendant.

const HIDE_BILLBOARDS: &[u8] = include_bytes!("fixtures/arm_hidebillboards.luac");

#[test]
fn loop_body_keeps_its_isa_guard() {
    let out = crate::decompile(HIDE_BILLBOARDS).expect("fixture decompiles");
    let start = out.find("local function hide_billboards").expect("function present");
    let body = &out[start..];
    let end = body.find("DescendantAdded").expect("tail present");
    let body = &body[..end];
    assert!(
        body.contains("if v:IsA(\"BillboardGui\") then")
            || body.contains("if not v:IsA(\"BillboardGui\") then
            continue"),
        "IsA guard lost:\n{body}"
    );
    assert!(!body.contains("local isType"), "guard parked as a dead local:\n{body}");
}
