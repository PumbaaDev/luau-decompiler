//! Brookhaven `Jukebox` poll loop: a `while true` body whose guard arm
//! (`if not char then task.wait(1) continue end`) ends in a JUMP to the
//! JUMPBACK and whose else part holds its own branches (a distance compare
//! join and an `if state changed` block). The per-block lift inverted the guard
//! and dropped the else part, leaving `if Character then continue end
//! task.wait(1) continue` (`calls_not_emitted`, 5 calls expected, 2 emitted).

const FIX: &[u8] = include_bytes!("fixtures/cne_while_true_else_branches_jukebox.luac");

#[test]
fn poll_loop_keeps_its_else_part_and_guard_direction() {
    let out = crate::decompile(FIX).expect("fixture decompiles");
    let start = out.find("while true do").expect("poll loop present");
    let body = &out[start..];
    let body = &body[..body.find("end))").unwrap_or(body.len())];
    assert!(body.contains("CharacterUtil.distanceTo"), "distance call lost:
{out}");
    assert!(body.contains("NotityStateChanged"), "else part lost:
{out}");
    assert_eq!(body.matches("task.wait").count(), 2, "both waits expected:
{out}");
    assert!(
        !body.contains("then
                continue
            end
            task.wait(1)
            continue"),
        "guard still collapsed to bare continues:
{out}"
    );
}
