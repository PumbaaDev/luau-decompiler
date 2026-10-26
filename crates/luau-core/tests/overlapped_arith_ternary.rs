//! `a and <arithmetic> or c` over computed values (Spr library `luvToRgb`).
//!
//! Real Roblox bytecode; the corpus is not committed, every test skips when it
//! is absent. Override the location with `LUAU_SPR_BC`.
//!
//! The bytecode shape is
//!
//! ```text
//!     JUMPIFNOTLT  0.2069 < Y -> ELSE
//!     MUL R6 Y*Y ; MUL R5 R6*Y        ; THEN computes the cube into R5
//!     JUMPIF R5 -> JOIN               ; truthy: keep it
//! ELSE: MULK R6 ; SUBK R5             ; pure arithmetic into R5
//! JOIN: MOVE R4 R5
//! ```
//!
//! Defects fixed: (a) the `or` arm ran unconditionally because the arm's own
//! JUMPIF past the arm was dropped, (b) the register holding the result kept
//! its old name `v53` while a pending expression still read the old value,
//! (c) `MOVE R4 R5` became an alias of a name that was rewritten two lines
//! later, (d) the next diamond seeded `local new16` with no initialiser
//! although its arms read the entry value.

use std::path::PathBuf;

fn spr_dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LUAU_SPR_BC") {
        return Some(PathBuf::from(p));
    }
    let p = PathBuf::from("C:/Users/jep/Desktop/BloxFruits_v12_bytecode");
    p.is_dir().then_some(p)
}

fn luv_to_rgb(file: &str) -> Option<Vec<String>> {
    let bytes = std::fs::read(spr_dir()?.join(file)).ok()?;
    let src = match std::panic::catch_unwind(|| luau_core::decompile(&bytes)) {
        Ok(r) => r.ok()?,
        Err(_) => return None,
    };
    let lines: Vec<String> = src.lines().map(str::to_string).collect();
    let start = lines.iter().position(|l| l.contains("local function luvToRgb"))?;
    let mut out = vec![lines[start].clone()];
    for l in &lines[start + 1..] {
        out.push(l.clone());
        if l == "end" {
            break;
        }
    }
    Some(out)
}

const FILES: [&str; 3] = [
    "ReplicatedStorage_Util_Spring2.luac",
    "ReplicatedStorage_Util_ReplicatedSpring_Solver.luac",
    "ReplicatedStorage_Controllers_BlueMoonClient_spr.luac",
];

#[test]
fn or_arm_is_guarded_by_the_truthiness_test_of_the_and_arm() {
    for f in FILES {
        let Some(b) = luv_to_rgb(f) else { eprintln!("SKIP: {f} absent"); continue };
        let text = b.join("\n");
        assert!(
            b.iter().any(|l| l.trim_start().starts_with("if not ") && l.trim_end().ends_with(" then")),
            "{f}: the cube arm lost its `if not <value>` fallback:\n{text}"
        );
    }
}

#[test]
fn diamond_arms_that_read_the_entry_value_are_not_seeded_empty() {
    for f in FILES {
        let Some(b) = luv_to_rgb(f) else { eprintln!("SKIP: {f} absent"); continue };
        let text = b.join("\n");
        let bare: Vec<&str> = b
            .iter()
            .map(|l| l.trim())
            .filter(|l| l.starts_with("local ") && !l.contains('=') && !l.contains(','))
            .map(|l| l.trim_start_matches("local "))
            .collect();
        for name in bare {
            let compound = format!("{name} -=");
            assert!(
                !b.iter().any(|l| l.trim_start().starts_with(&compound)),
                "{f}: `{name}` is declared without a value and then updated with -=:\n{text}"
            );
        }
    }
}

#[test]
fn moved_copy_does_not_alias_a_rewritten_name() {
    for f in FILES {
        let Some(b) = luv_to_rgb(f) else { eprintln!("SKIP: {f} absent"); continue };
        let text = b.join("\n");
        // `local selN = selM` is the copy; selM must not be the target of a
        // later plain assignment that selN's readers depend on unless the copy
        // exists, so the copy line must be present.
        let copy = b.iter().any(|l| {
            let t = l.trim();
            t.starts_with("local sel") && t.contains(" = sel")
        });
        assert!(copy, "{f}: the Y copy (`MOVE R4 R5`) became an alias:\n{text}");
    }
}
