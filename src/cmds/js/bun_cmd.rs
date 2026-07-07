//! Filters bun output — install logs, package lists, and pm commands.

use crate::core::tracking;
use crate::core::utils::{exit_code_from_output, join_or_ok, resolved_command, strip_ansi, truncate};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::ffi::OsString;

/// JSON structure for `bun pm ls --json` output.
#[derive(Debug, Deserialize)]
struct BunPmPackage {
    version: Option<String>,
}

/// Build the argv for `bun <subcmd> <args>`. Specs pass through verbatim:
/// args reach bun as an argv vector (never a shell), so there is nothing to
/// escape or validate, and bun enforces its own spec syntax.
fn pkg_argv(subcmd: &str, args: &[String]) -> Vec<String> {
    std::iter::once(subcmd.to_string())
        .chain(args.iter().cloned())
        .collect()
}

/// Filter bun install/add/remove output — strip progress lines, version headers, empty lines.
pub fn filter_bun_pkg(output: &str) -> String {
    let cleaned = strip_ansi(output);
    let mut result = Vec::new();

    for line in cleaned.lines() {
        let trimmed = line.trim();

        if trimmed.is_empty() {
            continue;
        }

        // Skip progress lines like "[1/5] ..."
        if trimmed.starts_with('[') {
            if let Some(close) = trimmed.find(']') {
                let after_bracket = trimmed[close + 1..].trim();
                if after_bracket.ends_with("...") {
                    let bracket_content = &trimmed[1..close];
                    if bracket_content.contains('/') {
                        let parts: Vec<&str> = bracket_content.split('/').collect();
                        if parts.len() == 2
                            && parts[0].trim().parse::<u32>().is_ok()
                            && parts[1].trim().parse::<u32>().is_ok()
                        {
                            continue;
                        }
                    }
                }
            }
        }

        // Skip version headers like "bun install v1.1.0" / "bun add v1.1.0" / "bun remove v1.1.0"
        if (trimmed.starts_with("bun install v")
            || trimmed.starts_with("bun add v")
            || trimmed.starts_with("bun remove v"))
            && trimmed.split_whitespace().count() <= 4
        {
            continue;
        }

        result.push(trimmed);
    }

    join_or_ok(&result)
}

/// Parse JSON output from `bun pm ls --json`.
pub fn filter_bun_pm_ls_json(raw: &str) -> Option<String> {
    let packages: HashMap<String, BunPmPackage> = serde_json::from_str(raw).ok()?;

    if packages.is_empty() {
        return None;
    }

    let mut entries: Vec<String> = packages
        .iter()
        .map(|(name, pkg)| {
            if let Some(ver) = &pkg.version {
                format!("{}@{}", name, ver)
            } else {
                name.clone()
            }
        })
        .collect();

    entries.sort();

    let count = entries.len();
    let mut result = format!("{} deps\n", count);
    result.push_str(&entries.join("\n"));

    Some(result)
}

/// Text fallback for `bun pm ls`.
pub fn filter_bun_pm_ls_text(raw: &str) -> String {
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();

    truncate(&join_or_ok(&lines), 500)
}

/// Run `bun install`, `bun add`, or `bun remove` with filtered output.
pub fn run_pkg(subcmd: &str, args: &[String], verbose: u8) -> Result<i32> {
    let timer = tracking::TimedExecution::start();

    let mut cmd = resolved_command("bun");
    cmd.args(pkg_argv(subcmd, args));

    if verbose > 0 {
        eprintln!("Running: bun {} {}", subcmd, args.join(" "));
    }

    let output = cmd
        .output()
        .with_context(|| format!("Failed to run bun {}. Is bun installed?", subcmd))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}{}", stdout, stderr);

    // Filter the combined stream so warnings on stderr survive a passing run.
    let filtered = filter_bun_pkg(&combined);
    let exit_code = exit_code_from_output(&output, "bun");
    crate::core::runner::print_with_hint(
        &filtered,
        &combined,
        &combined,
        &format!("bun_{}", subcmd),
        exit_code,
    );

    timer.track(
        &format!("bun {} {}", subcmd, args.join(" ")),
        &format!("rtk bun {} {}", subcmd, args.join(" ")),
        &combined,
        &filtered,
    );

    Ok(exit_code)
}

pub fn run_pm_ls(args: &[String], verbose: u8) -> Result<i32> {
    let timer = tracking::TimedExecution::start();

    let mut cmd = resolved_command("bun");
    cmd.arg("pm").arg("ls");
    if !args.iter().any(|a| a == "--json") {
        cmd.arg("--json");
    }
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: bun pm ls --json {}", args.join(" "));
    }

    let output = cmd
        .output()
        .context("Failed to run bun pm ls. Is bun installed?")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}{}", stdout, stderr);

    // JSON lives on stdout; the text fallback reads the combined stream so errors survive.
    let filtered = if let Some(json_result) = filter_bun_pm_ls_json(&stdout) {
        json_result
    } else {
        filter_bun_pm_ls_text(&combined)
    };

    let exit_code = exit_code_from_output(&output, "bun");
    crate::core::runner::print_with_hint(&filtered, &combined, &combined, "bun_pm_ls", exit_code);

    timer.track(
        &format!("bun pm ls {}", args.join(" ")),
        &format!("rtk bun pm ls {}", args.join(" ")),
        &combined,
        &filtered,
    );

    Ok(exit_code)
}

/// Run `bun build` with error-only filtering. Args are passed as a vector, never via a shell.
pub fn run_build(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = resolved_command("bun");
    cmd.arg("build").args(args);
    let display = format!("bun build {}", args.join(" "));
    crate::core::runner::run_err_cmd(cmd, display.trim_end(), verbose)
}

/// Run `bun test` showing only failures. Args are passed as a vector, never via a shell.
pub fn run_test(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = resolved_command("bun");
    cmd.arg("test").args(args);
    let display = format!("bun test {}", args.join(" "));
    crate::core::runner::run_test_cmd(cmd, display.trim_end(), verbose)
}

/// Run `bunx <tool>` with error-only filtering. Args are passed as a vector, never via a shell.
pub fn run_bunx(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = resolved_command("bunx");
    cmd.args(args);
    let display = format!("bunx {}", args.join(" "));
    crate::core::runner::run_err_cmd(cmd, display.trim_end(), verbose)
}

/// Passthrough for `bun run` and other unfiltered subcommands.
pub fn run_passthrough(args: &[OsString], verbose: u8) -> Result<i32> {
    crate::core::runner::run_passthrough("bun", args, verbose)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn count_tokens(text: &str) -> usize {
        text.split_whitespace().count()
    }

    #[test]
    fn test_filter_bun_install_strips_progress() {
        let output = r#"bun install v1.1.0
[1/5] Resolving packages...
[2/5] Fetching packages...
[3/5] Linking packages...
[4/5] Building fresh packages...
[5/5] Cleaning up...

+ installed express@4.18.2
+ installed lodash@4.17.21
3 packages installed in 1.2s
"#;
        let result = filter_bun_pkg(output);
        assert!(!result.contains("[1/5]"));
        assert!(!result.contains("bun install v1.1.0"));
        assert!(result.contains("express"));
        assert!(result.contains("3 packages installed"));
    }

    #[test]
    fn test_filter_bun_install_token_savings() {
        // Realistic bun install output with many progress lines and version header
        let input = r#"bun install v1.2.5 (a1b2c3d4)
[1/10] Resolving packages...
[2/10] Fetching packages...
[3/10] Linking dependencies...
[4/10] Building fresh packages...
[5/10] Compiling native modules...
[6/10] Running lifecycle scripts...
[7/10] Generating lockfile...
[8/10] Deduplicating packages...
[9/10] Cleaning cache...
[10/10] Writing lockfile...

+ installed express@4.18.2
+ installed lodash@4.17.21
10 packages installed in 2.3s
"#;
        let output = filter_bun_pkg(input);
        let savings = 100.0 - (count_tokens(&output) as f64 / count_tokens(input) as f64 * 100.0);
        assert!(
            savings >= 60.0,
            "Bun install filter: expected >=60% savings, got {:.1}%",
            savings
        );
    }

    #[test]
    fn test_filter_bun_install_empty_output() {
        let output = "\n\n\n";
        let result = filter_bun_pkg(output);
        assert_eq!(result, "ok");
    }

    #[test]
    fn test_filter_bun_install_strips_ansi() {
        let output = "\x1b[32m[1/3] Resolving packages...\x1b[0m\n\x1b[32m[2/3] Fetching packages...\x1b[0m\n\x1b[32m[3/3] Linking packages...\x1b[0m\n+ installed express@4.18.2\n";
        let result = filter_bun_pkg(output);
        assert!(!result.contains("[1/3]"));
        assert!(result.contains("express"));
    }

    #[test]
    fn test_filter_bun_install_preserves_errors() {
        let output = r#"bun install v1.1.0
[1/4] Resolving packages...
error: PackageNotFound - "nonexistent-pkg" not found in registry
"#;
        let result = filter_bun_pkg(output);
        assert!(result.contains("error:"));
        assert!(result.contains("nonexistent-pkg"));
    }

    #[test]
    fn test_filter_bun_install_handles_remove() {
        let output = "bun remove v1.1.0\n- removed express@4.18.2\n1 package removed in 0.5s\n";
        let result = filter_bun_pkg(output);
        assert!(!result.contains("bun remove v1.1.0"));
        assert!(result.contains("removed express"));
    }

    #[test]
    fn test_filter_bun_pm_ls_json() {
        let json = r#"{
            "express": {"version": "4.18.2"},
            "lodash": {"version": "4.17.21"},
            "axios": {"version": "1.6.0"}
        }"#;
        let result = filter_bun_pm_ls_json(json).expect("should parse");
        assert!(result.starts_with("3 deps"));
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines[1], "axios@1.6.0");
        assert_eq!(lines[2], "express@4.18.2");
        assert_eq!(lines[3], "lodash@4.17.21");
    }

    #[test]
    fn test_filter_bun_pm_ls_json_token_savings() {
        // Real `bun pm ls --json` carries resolved URLs and integrity hashes per dep.
        let input = r#"{
            "express": {"version": "4.18.2", "resolved": "https://registry.npmjs.org/express/-/express-4.18.2.tgz", "integrity": "sha512-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
            "lodash": {"version": "4.17.21", "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz", "integrity": "sha512-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},
            "axios": {"version": "1.6.0", "resolved": "https://registry.npmjs.org/axios/-/axios-1.6.0.tgz", "integrity": "sha512-cccccccccccccccccccccccccccccccccccccccccccc"}
        }"#;
        let output = filter_bun_pm_ls_json(input).expect("should parse");
        let savings = 100.0 - (count_tokens(&output) as f64 / count_tokens(input) as f64 * 100.0);
        assert!(
            savings >= 60.0,
            "Bun pm ls json filter: expected >=60% savings, got {:.1}%",
            savings
        );
    }

    #[test]
    fn test_filter_bun_pm_ls_json_empty() {
        let result = filter_bun_pm_ls_json("{}");
        assert!(result.is_none());
    }

    #[test]
    fn test_filter_bun_pm_ls_json_invalid() {
        let result = filter_bun_pm_ls_json("not json");
        assert!(result.is_none());
    }

    #[test]
    fn test_filter_bun_pm_ls_text_truncates() {
        let long_output = (0..100)
            .map(|i| format!("pkg-{i}@1.0.0"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = filter_bun_pm_ls_text(&long_output);
        assert!(result.len() <= 520);
    }

    #[test]
    fn test_pkg_argv_passes_specs_verbatim() {
        // Every spec bun itself accepts must reach bun untouched. rtk rejecting
        // chars like ^ ~ : # broke semver ranges and protocol specifiers.
        let specs = [
            "express",
            "@types/node",
            "lodash@^4.17.21",
            "pkg@~1.2.3",
            "@scope/pkg@>=1.0.0 <2.0.0",
            "npm:react@^18",
            "github:user/repo#branch",
            "git+https://github.com/user/repo.git",
            "workspace:*",
            "file:../sibling-pkg",
            // Shell metacharacters are inert: args reach bun as an argv
            // vector, never through a shell, so nothing needs rejecting.
            "pkg;rm -rf /",
        ];
        for spec in specs {
            let argv = pkg_argv("add", &[spec.to_string()]);
            assert_eq!(argv, vec!["add".to_string(), spec.to_string()]);
        }
    }
}
