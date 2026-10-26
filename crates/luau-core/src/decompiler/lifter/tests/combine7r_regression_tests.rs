//! Four shapes the combined branch made WORSE than the control build, each
//! pinned on the real script that showed it (bytecode-verified).
//!
//! * `value_join_inside_open_arm` kept a value join in the flat span although
//!   the open arm's else part carried branches of its own and the span went on
//!   past the arm's exit: AdoptMe OfferPane `_start_fade_promise` lost its
//!   `arg2 == "picking" or arg2 == "profile"` guard (pc27/29 JUMPXEQKS, join at
//!   pc32), BloxFruits Skyjump lost its KitsuneSkyJump / Pain / DBSkyJump
//!   chain, PetSimulator99 Egg_Opening_Frontend grew a fabricated 5-arm chain
//!   after `R88 = 0.5; if Eggs2 .. elseif v35 or v36 ..` (pc1828-1845).
//! * the `c and f() or nil` nil-tail rewrites reset a register that still
//!   parked an unemitted `x or "Normal"` default: Slayers2 TextPlusUtility
//!   `fontface` (pc47 `LOADK R5 "Normal"`) lost the string.

const OFFER_PANE: &[u8] = include_bytes!("fixtures/combine7r_offerpane.luac");
const SKYJUMP: &[u8] = include_bytes!("fixtures/combine7r_skyjump.luac");
const EGG_OPENING: &[u8] = include_bytes!("fixtures/combine7r_egg_opening.luac");
const TEXTPLUS: &[u8] = include_bytes!("fixtures/combine7r_textplus.luac");

fn decompiled(bytes: &[u8]) -> String {
    crate::decompile(bytes).expect("fixture decompiles")
}

/// The text of the function that starts at the first line containing `head`,
/// up to the next line at the same indentation that is `end`.
fn function_body(out: &str, head: &str) -> String {
    let lines: Vec<&str> = out.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.contains("function") && l.contains(head))
        .expect("function present");
    let indent = lines[start].len() - lines[start].trim_start().len();
    let mut body = Vec::new();
    for l in &lines[start..] {
        body.push(*l);
        let ind = l.len() - l.trim_start().len();
        if body.len() > 1 && ind == indent && l.trim() == "end" {
            break;
        }
    }
    body.join("\n")
}

#[test]
fn offer_pane_fade_promise_keeps_the_picking_or_profile_guard() {
    let out = decompiled(OFFER_PANE);
    let f = function_body(&out, ":_start_fade_promise(");
    let guard = f.find("if arg2 == \"picking\" or arg2 == \"profile\" then").expect("guard kept");
    let tween = f.find("load5.new(").expect("tween call present");
    assert!(guard < tween, "the tween must sit inside the guard:\n{f}");
}

#[test]
fn skyjump_keeps_the_animation_selection_chain() {
    let out = decompiled(SKYJUMP);
    for want in ["\"KitsuneSkyJump\"", "\"PainSkyJumpR\"", "\"DBSkyJump\"", "v184 = 1.2"] {
        assert!(out.contains(want), "missing {want}");
    }
    assert!(
        !out.contains("Anims:Get(Character, import5"),
        "fabricated import5 fallback in the Anims:Get argument"
    );
}

#[test]
fn egg_opening_keeps_the_wait_click_selection_without_a_fabricated_chain() {
    let out = decompiled(EGG_OPENING);
    assert!(out.contains("elseif v35 or v36 then"), "elseif arm lost");
    assert!(
        !out.contains("not v35 and v36"),
        "fabricated if/elseif chain over the v35/v36 operands"
    );
    assert!(out.contains("WaitClick(Fire3)"), "WaitClick must read the selected register");
}

#[test]
fn textplus_fontface_keeps_the_normal_style_default() {
    let out = decompiled(TEXTPLUS);
    assert!(out.contains("\"Normal\""), "the \"Normal\" default string was dropped:\n{out}");
}
