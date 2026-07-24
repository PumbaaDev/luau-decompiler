//! Phase C10S — drop stdlib-function-lvalue artifacts.
//!
//! When an upstream register miss leaves `Name("setmetatable")` (or any
//! other Luau stdlib *function*) as the base of a field-assign, the
//! resulting statement is guaranteed decompiler garbage because real
//! source never writes `setmetatable.X = ...`. The post-pass drops:
//!
//!   1. `Stat::Assign { target: Field { root: Name(shadow), .. } }`
//!   2. `Stat::MethodFunction { receiver: Name(shadow), .. }`
//!
//! `is_safe_receiver` in `reconstruct_method_assignments` also rejects
//! the same names up-front so the MethodFunction form is rarely reached,
//! but the post-pass still catches any that slip through a future path.

use super::super::post_passes::drop_stdlib_function_lvalue_artifacts;
use crate::ast::{Expr, Stat};

fn field(object: Expr, field: &str) -> Expr {
    Expr::Field { object: Box::new(object), field: field.to_string() }
}

#[test]
fn drops_assign_with_setmetatable_base() {
    // setmetatable.cameraType = Enum.CameraType.Fixed  → (dropped)
    let mut stmts = vec![Stat::Assign {
        targets: vec![field(Expr::Name("setmetatable".into()), "cameraType")],
        values: vec![Expr::Nil],
    }];
    drop_stdlib_function_lvalue_artifacts(&mut stmts);
    assert!(stmts.is_empty(), "setmetatable.X = v artifact should drop: {:?}", stmts);
}

#[test]
fn drops_method_function_with_setmetatable_receiver() {
    // function setmetatable.GetModuleName() end  → (dropped)
    let mut stmts = vec![Stat::MethodFunction {
        receiver: Expr::Name("setmetatable".into()),
        method: "GetModuleName".into(),
        is_method: false,
        func: Expr::Function { params: vec![], is_vararg: false, body: vec![] },
    }];
    drop_stdlib_function_lvalue_artifacts(&mut stmts);
    assert!(stmts.is_empty(), "function setmetatable.X() artifact should drop");
}

#[test]
fn drops_assign_with_pcall_base() {
    let mut stmts = vec![Stat::Assign {
        targets: vec![field(Expr::Name("pcall".into()), "foo")],
        values: vec![Expr::Number(1.0)],
    }];
    drop_stdlib_function_lvalue_artifacts(&mut stmts);
    assert!(stmts.is_empty());
}

#[test]
fn drops_nested_field_chain_with_shadow_root() {
    // require.A.B.C = v → (dropped, root is require)
    let base = field(field(field(Expr::Name("require".into()), "A"), "B"), "C");
    let mut stmts = vec![Stat::Assign {
        targets: vec![base],
        values: vec![Expr::Nil],
    }];
    drop_stdlib_function_lvalue_artifacts(&mut stmts);
    assert!(stmts.is_empty());
}

#[test]
fn keeps_assign_with_non_shadow_base() {
    // LegacyCamera.cameraType = v → preserved
    let mut stmts = vec![Stat::Assign {
        targets: vec![field(Expr::Name("LegacyCamera".into()), "cameraType")],
        values: vec![Expr::Nil],
    }];
    drop_stdlib_function_lvalue_artifacts(&mut stmts);
    assert_eq!(stmts.len(), 1, "legitimate user local must not drop");
}

#[test]
fn keeps_assign_with_math_base() {
    // math.pi = 3.14 is legal (if silly); math is a *table* not a function
    // so it's outside the function-only shadow list.
    let mut stmts = vec![Stat::Assign {
        targets: vec![field(Expr::Name("math".into()), "pi")],
        values: vec![Expr::Number(3.14)],
    }];
    drop_stdlib_function_lvalue_artifacts(&mut stmts);
    assert_eq!(stmts.len(), 1, "stdlib-table base must not drop (only functions do)");
}

#[test]
fn keeps_assign_with_game_base() {
    // Roblox globals like `game`, `workspace`, `script` are tables too.
    let mut stmts = vec![Stat::Assign {
        targets: vec![field(Expr::Name("game".into()), "Name")],
        values: vec![Expr::String("X".into())],
    }];
    drop_stdlib_function_lvalue_artifacts(&mut stmts);
    assert_eq!(stmts.len(), 1);
}

#[test]
fn recurses_into_nested_function_bodies() {
    // local g = function()
    //     setmetatable.X = 1   → dropped
    //     return 2
    // end
    let mut stmts = vec![Stat::Local {
        names: vec!["g".into()],
        values: vec![Expr::Function {
            params: vec![],
            is_vararg: false,
            body: vec![
                Stat::Assign {
                    targets: vec![field(Expr::Name("setmetatable".into()), "X")],
                    values: vec![Expr::Number(1.0)],
                },
                Stat::Return { values: vec![Expr::Number(2.0)] },
            ],
        }],
    }];
    drop_stdlib_function_lvalue_artifacts(&mut stmts);
    assert_eq!(stmts.len(), 1);
    if let Stat::Local { values, .. } = &stmts[0] {
        if let Expr::Function { body, .. } = &values[0] {
            assert_eq!(body.len(), 1, "inner shadow-lvalue should have dropped");
            assert!(matches!(&body[0], Stat::Return { .. }));
        } else {
            panic!("expected function RHS");
        }
    } else {
        panic!("expected local g");
    }
}

#[test]
fn keeps_multi_target_assign() {
    // a, b = 1, 2 — not a field assign, never matches the shadow guard.
    let mut stmts = vec![Stat::Assign {
        targets: vec![Expr::Name("a".into()), Expr::Name("b".into())],
        values: vec![Expr::Number(1.0), Expr::Number(2.0)],
    }];
    drop_stdlib_function_lvalue_artifacts(&mut stmts);
    assert_eq!(stmts.len(), 1);
}

#[test]
fn drops_index_target_rooted_at_shadow() {
    // require.FormatAmount[v245] = Button  → (dropped)
    // target is Expr::Index { object: Expr::Field { object: Name("require") } }
    let target = Expr::Index {
        object: Box::new(field(Expr::Name("require".into()), "FormatAmount")),
        key: Box::new(Expr::Name("v245".into())),
    };
    let mut stmts = vec![Stat::Assign {
        targets: vec![target],
        values: vec![Expr::Name("Button".into())],
    }];
    drop_stdlib_function_lvalue_artifacts(&mut stmts);
    assert!(stmts.is_empty(), "require.X[i] = v artifact should drop");
}

#[test]
fn drops_deep_index_field_mix_rooted_at_shadow() {
    // setmetatable.A[k].B = v  → (dropped)
    let target = field(
        Expr::Index {
            object: Box::new(field(Expr::Name("setmetatable".into()), "A")),
            key: Box::new(Expr::Name("k".into())),
        },
        "B",
    );
    let mut stmts = vec![Stat::Assign {
        targets: vec![target],
        values: vec![Expr::Nil],
    }];
    drop_stdlib_function_lvalue_artifacts(&mut stmts);
    assert!(stmts.is_empty(), "mixed Field/Index chain rooted at shadow should drop");
}

#[test]
fn keeps_index_target_rooted_at_user_local() {
    // tbl.Data[i] = v → preserved (tbl is not a stdlib fn shadow)
    let target = Expr::Index {
        object: Box::new(field(Expr::Name("tbl".into()), "Data")),
        key: Box::new(Expr::Name("i".into())),
    };
    let mut stmts = vec![Stat::Assign {
        targets: vec![target],
        values: vec![Expr::Number(1.0)],
    }];
    drop_stdlib_function_lvalue_artifacts(&mut stmts);
    assert_eq!(stmts.len(), 1);
}

#[test]
fn drops_all_three_stdlib_function_forms() {
    let mut stmts = vec![
        // setmetatable.X = 1
        Stat::Assign {
            targets: vec![field(Expr::Name("setmetatable".into()), "X")],
            values: vec![Expr::Number(1.0)],
        },
        // pcall.Y = 2
        Stat::Assign {
            targets: vec![field(Expr::Name("pcall".into()), "Y")],
            values: vec![Expr::Number(2.0)],
        },
        // function tostring.Z() end
        Stat::MethodFunction {
            receiver: Expr::Name("tostring".into()),
            method: "Z".into(),
            is_method: false,
            func: Expr::Function { params: vec![], is_vararg: false, body: vec![] },
        },
        // Keeper
        Stat::Return { values: vec![Expr::Name("ok".into())] },
    ];
    drop_stdlib_function_lvalue_artifacts(&mut stmts);
    assert_eq!(stmts.len(), 1);
    assert!(matches!(&stmts[0], Stat::Return { .. }));
}
