//! `while <call> do ... end` where the call result is held in a register.
//!
//! The header's pre-branch instructions run once before the loop and are
//! re-lifted at the bottom of the body (JUMPBACK re-executes them). The body
//! reuses the same register for other values, so by the time the tail
//! re-lift runs the register's recorded name is stale and the CALL result was
//! classified as a SHADOW: `local result = task.wait(..)` inside the body.
//! The `while result do` test reads the outer `result`, which nothing ever
//! writes again, so the loop never exits (or never re-tests the call).
//!
//! Fixture: `fixtures/tail_recompute_o1.lua` compiled by upstream
//! `luau-compile --fflags=LuauEmitCallFeedback=false --binary -O1 -g1`.

const FIXTURE: &[u8] = include_bytes!("fixtures/tail_recompute_o1.luac");

fn decompiled() -> String {
    crate::decompile(FIXTURE).expect("fixture decompiles")
}

/// Names tested by a `while NAME do` / `while not NAME do` line, with the
/// text of that loop's body (up to the matching indentation `end`).
fn while_bodies(out: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = out.lines().collect();
    let mut found = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        let Some(rest) = t.strip_prefix("while ") else { continue };
        let Some(cond) = rest.strip_suffix(" do") else { continue };
        let name = cond.strip_prefix("not ").unwrap_or(cond);
        if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let mut body = String::new();
        for l in &lines[i + 1..] {
            let li = l.len() - l.trim_start().len();
            if li == indent && l.trim() == "end" {
                break;
            }
            body.push_str(l);
            body.push('\n');
        }
        found.push((name.to_string(), body));
    }
    found
}

#[test]
fn loop_condition_local_is_assigned_not_redeclared_in_the_body() {
    let out = decompiled();
    let loops = while_bodies(&out);
    assert!(!loops.is_empty(), "no `while NAME do` loop found in:\n{out}");
    for (name, body) in &loops {
        let redeclared = format!("local {name} =");
        assert!(
            !body.contains(&redeclared),
            "loop `while {name}` re-declares `{name}` in its body:\n{out}"
        );
        let assigned = format!("{name} = ");
        assert!(
            body.contains(&assigned),
            "loop `while {name}` never re-assigns `{name}` in its body:\n{out}"
        );
    }
}
