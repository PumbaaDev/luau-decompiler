//! Lightweight ANSI color helper.
//!
//! No external `colored` / `termcolor` crate — a single 32-byte struct
//! of `&'static str` escape codes that are empty when color is disabled.
//! Honors `NO_COLOR`, `CLICOLOR=0`, and non-TTY stdout automatically.

use std::io::IsTerminal;

/// A set of ANSI escapes that are either real codes or empty strings.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct Colors {
    pub reset: &'static str,
    pub bold: &'static str,
    pub dim: &'static str,
    pub red: &'static str,
    pub green: &'static str,
    pub yellow: &'static str,
    pub blue: &'static str,
    pub cyan: &'static str,
    pub magenta: &'static str,
}

pub const ON: Colors = Colors {
    reset: "\x1b[0m",
    bold: "\x1b[1m",
    dim: "\x1b[2m",
    red: "\x1b[31m",
    green: "\x1b[32m",
    yellow: "\x1b[33m",
    blue: "\x1b[34m",
    cyan: "\x1b[36m",
    magenta: "\x1b[35m",
};

pub const OFF: Colors = Colors {
    reset: "",
    bold: "",
    dim: "",
    red: "",
    green: "",
    yellow: "",
    blue: "",
    cyan: "",
    magenta: "",
};

/// Pick `ON` if `want` is true AND the environment allows color.
///
/// Precedence, highest first:
/// 1. `NO_COLOR` env set → OFF (per https://no-color.org/)
/// 2. `CLICOLOR=0` env → OFF
/// 3. `want == false` (caller forced off) → OFF
/// 4. stdout is not a TTY → OFF
/// 5. otherwise → ON
pub fn choose(want: bool) -> Colors {
    if std::env::var_os("NO_COLOR").is_some() {
        return OFF;
    }
    if std::env::var("CLICOLOR").ok().as_deref() == Some("0") {
        return OFF;
    }
    if !want {
        return OFF;
    }
    if !std::io::stdout().is_terminal() {
        return OFF;
    }
    ON
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_returns_empty_codes() {
        assert_eq!(OFF.reset, "");
        assert_eq!(OFF.bold, "");
        assert_eq!(OFF.red, "");
    }

    #[test]
    fn on_returns_real_codes() {
        assert!(ON.reset.starts_with("\x1b["));
        assert!(ON.red.starts_with("\x1b["));
    }

    #[test]
    fn choose_false_is_off() {
        let c = choose(false);
        assert_eq!(c.reset, "");
    }
}
