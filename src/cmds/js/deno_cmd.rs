//! Filters deno output — lint, check, and task command output.

use crate::core::utils::{join_or_ok, resolved_command, strip_ansi};
use anyhow::Result;
use std::ffi::OsString;

/// Filter deno output: strip ANSI codes, download lines, and empty lines.
pub fn filter_deno_output(output: &str) -> String {
    let cleaned = strip_ansi(output);
    let filtered: Vec<&str> = cleaned
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.starts_with("Download ")
        })
        .collect();

    join_or_ok(&filtered)
}

/// Run a deno subcommand through the shared core runner, which applies the
/// filter, tee recovery, tracking, and the never_worse output guard.
fn run_filtered_subcmd(subcmd: &str, args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = resolved_command("deno");
    cmd.arg(subcmd);
    cmd.args(args);

    if verbose > 0 {
        eprintln!("Running: deno {} {}", subcmd, args.join(" "));
    }

    let display = format!("{} {}", subcmd, args.join(" "));
    let tee_label = format!("deno_{}", subcmd);
    crate::core::runner::run_filtered(
        cmd,
        "deno",
        display.trim_end(),
        filter_deno_output,
        crate::core::runner::RunOptions::with_tee(&tee_label),
    )
}

pub fn run_lint(args: &[String], verbose: u8) -> Result<i32> {
    run_filtered_subcmd("lint", args, verbose)
}

pub fn run_check(args: &[String], verbose: u8) -> Result<i32> {
    run_filtered_subcmd("check", args, verbose)
}

/// Run `deno compile` with error-only filtering. Args are passed as a vector, never via a shell.
pub fn run_compile(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = resolved_command("deno");
    cmd.arg("compile").args(args);
    let display = format!("deno compile {}", args.join(" "));
    crate::core::runner::run_err_cmd(cmd, display.trim_end(), verbose)
}

/// Run `deno test` showing only failures. Args are passed as a vector, never via a shell.
pub fn run_test(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = resolved_command("deno");
    cmd.arg("test").args(args);
    let display = format!("deno test {}", args.join(" "));
    crate::core::runner::run_test_cmd(
        cmd,
        display.trim_end(),
        crate::core::runner::TestEcosystem::Deno,
        verbose,
    )
}

/// Passthrough for `deno run`, `deno task`, and other unfiltered subcommands.
pub fn run_passthrough(args: &[OsString], verbose: u8) -> Result<i32> {
    crate::core::runner::run_passthrough("deno", args, verbose)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn count_tokens(text: &str) -> usize {
        text.split_whitespace().count()
    }

    #[test]
    fn test_filter_deno_output_strips_download() {
        let input = r#"Download https://deno.land/std@0.200.0/path/mod.ts
Download https://deno.land/x/oak@v12.6.1/mod.ts
error: Expected ';' at main.ts:5:10
some warning here"#;

        let result = filter_deno_output(input);
        assert!(!result.contains("Download "));
        assert!(result.contains("error: Expected ';' at main.ts:5:10"));
        assert!(result.contains("some warning here"));
    }

    #[test]
    fn test_filter_deno_output_token_savings() {
        // Realistic deno output with many download lines before actual content
        let input = r#"Download https://deno.land/std@0.200.0/path/mod.ts
Download https://deno.land/x/oak@v12.6.1/mod.ts
Download https://deno.land/std@0.200.0/fmt/colors.ts
Download https://deno.land/std@0.200.0/io/mod.ts
Download https://deno.land/std@0.200.0/http/server.ts
Download https://deno.land/std@0.200.0/async/mod.ts
Download https://deno.land/std@0.200.0/testing/asserts.ts
Download https://deno.land/std@0.200.0/encoding/base64.ts
Download https://deno.land/std@0.200.0/crypto/mod.ts
Download https://deno.land/std@0.200.0/streams/mod.ts
Download https://deno.land/std@0.200.0/bytes/mod.ts
Download https://deno.land/std@0.200.0/collections/mod.ts
Download https://deno.land/std@0.200.0/datetime/mod.ts
Download https://deno.land/std@0.200.0/flags/mod.ts
Download https://deno.land/std@0.200.0/uuid/mod.ts
Check file:///project/main.ts
error: Expected ';' at main.ts:5:10
warning: Unused variable 'x' at main.ts:3:7
"#;
        let output = filter_deno_output(input);
        let savings = 100.0 - (count_tokens(&output) as f64 / count_tokens(input) as f64 * 100.0);
        assert!(
            savings >= 60.0,
            "Deno filter: expected >=60% savings, got {:.1}%",
            savings
        );
    }

    #[test]
    fn test_filter_deno_output_empty() {
        let input = r#"Download https://deno.land/std@0.200.0/path/mod.ts

Download https://deno.land/x/oak@v12.6.1/mod.ts

"#;

        let result = filter_deno_output(input);
        assert_eq!(result, "ok");
    }

    #[test]
    fn test_filter_deno_strips_ansi() {
        let input = "\x1b[33mDownload https://deno.land/std@0.200.0/path/mod.ts\x1b[0m\n\x1b[31merror: something\x1b[0m\n";
        let result = filter_deno_output(input);
        assert!(!result.contains("Download"));
        assert!(result.contains("error: something"));
    }

    #[test]
    fn test_filter_deno_preserves_check_lines() {
        let input = "Check file:///project/main.ts\n";
        let result = filter_deno_output(input);
        assert!(result.contains("Check"));
    }

    #[test]
    fn test_filter_deno_preserves_errors_strips_downloads() {
        let input = r#"Download https://deno.land/std@0.210.0/path/mod.ts
error: Module not found "https://deno.land/x/nonexistent/mod.ts"
"#;
        let result = filter_deno_output(input);
        assert!(result.contains("error:"));
        assert!(result.contains("Module not found"));
        assert!(!result.contains("Download"));
    }
}
