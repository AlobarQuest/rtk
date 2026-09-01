//! Filters bun output — install logs, package lists, and pm commands.

use crate::core::utils::{join_or_ok, resolved_command, strip_ansi, truncate};
use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;
use std::ffi::OsString;

/// JSON structure for `bun pm ls --json` output.
///
/// `version` is required on purpose. Serde ignores unknown keys, so an
/// optional-only struct also accepts a grouped shape like
/// `{"dependencies": {"express": {...}}}` and reports the group names as
/// packages. Requiring it makes that shape fail to parse and fall through to
/// the tree parser instead.
#[derive(Debug, Deserialize)]
struct BunPmPackage {
    version: String,
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

        // Push the original line, not `trimmed`: bun indents the frames and
        // hints under each error, and flattening them loses which hint belongs
        // to which package on a multi-error install.
        result.push(line);
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
        .map(|(name, pkg)| format!("{}@{}", name, pkg.version))
        .collect();

    entries.sort();

    let count = entries.len();
    let mut result = format!("{} deps\n", count);
    result.push_str(&entries.join("\n"));

    Some(result)
}

/// Parse the tree form of `bun pm ls` output (`\u{251c}\u{2500}\u{2500} name@version` rows).
/// This is what real bun 1.x prints even when --json is passed (the flag is
/// silently ignored), so this is the path real runs take.
fn filter_bun_pm_ls_tree(raw: &str) -> Option<String> {
    let cleaned = strip_ansi(raw);
    let mut entries: Vec<&str> = cleaned
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with('\u{251c}') || trimmed.starts_with('\u{2514}')
        })
        .map(|line| {
            line.trim_start_matches(['\u{251c}', '\u{2514}', '\u{2502}', '\u{2500}', ' '])
                .trim_end()
        })
        .filter(|entry| !entry.is_empty())
        .collect();

    if entries.is_empty() {
        return None;
    }

    entries.sort_unstable();
    entries.dedup();

    let mut result = format!("{} deps\n", entries.len());
    result.push_str(&entries.join("\n"));
    Some(result)
}

/// Pick the pm ls parser by what bun actually printed, not by the flags we
/// passed: JSON if the output is JSON, tree if it is a tree, raw text otherwise.
fn filter_bun_pm_ls(raw: &str) -> String {
    if let Some(json_result) = filter_bun_pm_ls_json(raw) {
        return json_result;
    }
    if let Some(tree_result) = filter_bun_pm_ls_tree(raw) {
        return tree_result;
    }
    filter_bun_pm_ls_text(raw)
}

/// Text fallback for `bun pm ls`.
pub fn filter_bun_pm_ls_text(raw: &str) -> String {
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();

    truncate(&join_or_ok(&lines), 500)
}

/// Run `bun install`, `bun add`, or `bun remove` with filtered output.
///
/// Goes through the shared core runner so stdout and stderr stay interleaved
/// in the order bun wrote them (bun puts progress on stderr), and so tracking
/// records the output that was actually shown rather than the pre-guard filter
/// result.
pub fn run_pkg(subcmd: &str, args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = resolved_command("bun");
    cmd.args(pkg_argv(subcmd, args));

    if verbose > 0 {
        eprintln!("Running: bun {} {}", subcmd, args.join(" "));
    }

    let display = format!("{} {}", subcmd, args.join(" "));
    let tee_label = format!("bun_{}", subcmd);
    crate::core::runner::run_filtered(
        cmd,
        "bun",
        display.trim_end(),
        filter_bun_pkg,
        crate::core::runner::RunOptions::with_tee(&tee_label),
    )
}

pub fn run_pm_ls(args: &[String], verbose: u8) -> Result<i32> {
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

    let display = format!("pm ls {}", args.join(" "));
    crate::core::runner::run_filtered(
        cmd,
        "bun",
        display.trim_end(),
        filter_bun_pm_ls,
        crate::core::runner::RunOptions::with_tee("bun_pm_ls"),
    )
}

/// True when `bun build` writes its bundle to disk rather than to stdout.
/// Without one of these flags stdout IS the bundle, so nothing may filter it.
fn build_writes_to_disk(args: &[String]) -> bool {
    args.iter().any(|a| {
        a == "--outdir"
            || a == "--outfile"
            || a == "--compile"
            || a.starts_with("--outdir=")
            || a.starts_with("--outfile=")
    })
}

/// Run `bun build`. Args are passed as a vector, never via a shell.
///
/// With no output flag bun writes the bundled JS to stdout, so the command is
/// a plain passthrough: filtering it would replace a user's bundle with a
/// status line, and `bun build ./index.ts > bundle.js` would silently write
/// that line to the file. Only the write-to-disk forms print a summary that is
/// safe to filter.
pub fn run_build(args: &[String], verbose: u8) -> Result<i32> {
    if !build_writes_to_disk(args) {
        let mut passthrough: Vec<OsString> = vec![OsString::from("build")];
        passthrough.extend(args.iter().map(OsString::from));
        return crate::core::runner::run_passthrough("bun", &passthrough, verbose);
    }

    let mut cmd = resolved_command("bun");
    cmd.arg("build").args(args);
    let display = format!("build {}", args.join(" "));
    crate::core::runner::run_err_cmd(cmd, "bun", display.trim_end(), "bun_build", verbose)
}

/// Run `bun test` showing only failures. Args are passed as a vector, never via a shell.
pub fn run_test(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = resolved_command("bun");
    cmd.arg("test").args(args);
    let display = format!("test {}", args.join(" "));
    crate::core::runner::run_test_cmd(
        cmd,
        "bun",
        display.trim_end(),
        "bun_test",
        crate::core::runner::TestEcosystem::Bun,
        verbose,
    )
}

/// Run `bunx <tool>`. Args are passed as a vector, never via a shell.
///
/// Uses the same light filter as the npx path rather than an errors-only one:
/// bunx hosts arbitrary tools, and for many of them stdout is the whole point.
pub fn run_bunx(args: &[String], verbose: u8, skip_env: bool) -> Result<i32> {
    crate::cmds::js::npm_cmd::exec_with("bunx", args, verbose, skip_env)
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
    fn test_filter_bun_pm_ls_tree_real_output() {
        // Real bun 1.3.6 output: bun silently ignores --json for pm ls and
        // prints this tree, so the tree parser is the path real runs take.
        let raw = include_str!("../../../tests/fixtures/bun_pm_ls_raw.txt");
        let out = filter_bun_pm_ls_tree(raw).expect("tree output should parse");
        assert!(out.starts_with("6 deps"), "{out}");
        for dep in [
            "@types/bun@1.3.14",
            "@types/node@26.1.0",
            "date-fns@4.4.0",
            "lodash@4.18.1",
            "typescript@6.0.3",
            "zod@4.4.3",
        ] {
            assert!(out.contains(dep), "missing {dep} in {out}");
        }
        assert!(!out.contains('\u{251c}'), "tree glyphs must be stripped");
        assert!(!out.contains("/home/user/project"), "{out}");
    }

    #[test]
    fn test_filter_bun_pm_ls_selects_by_content_not_flag() {
        // Selection keys on what bun PRINTED, not on the --json flag we
        // passed: tree text must never hit the JSON parser's 500-char
        // truncation fallback.
        let raw = include_str!("../../../tests/fixtures/bun_pm_ls_raw.txt");
        let out = filter_bun_pm_ls(raw);
        assert!(out.starts_with("6 deps"), "{out}");

        let json = r#"{"express": {"version": "4.18.2"}}"#;
        let out = filter_bun_pm_ls(json);
        assert!(out.starts_with("1 deps"), "{out}");

        let err = "error: No package.json was found for directory \"/home/user\"\nnote: Run \"bun init\" to initialize a project";
        let out = filter_bun_pm_ls(err);
        assert!(out.contains("No package.json"), "{out}");
    }

    #[test]
    fn test_bun_build_without_output_flag_is_passthrough() {
        // With no output flag the bundle IS stdout, so it must not be filtered:
        // `bun build ./index.ts > bundle.js` would otherwise write a status line.
        assert!(!build_writes_to_disk(&["./index.ts".to_string()]));
        assert!(!build_writes_to_disk(&[]));
    }

    #[test]
    fn test_bun_build_with_output_flag_is_filtered() {
        for flag in [
            "--outdir",
            "--outfile",
            "--compile",
            "--outdir=dist",
            "--outfile=out.js",
        ] {
            assert!(
                build_writes_to_disk(&["./index.ts".to_string(), flag.to_string()]),
                "{flag}"
            );
        }
    }

    #[test]
    fn test_filter_bun_pm_ls_json_rejects_grouped_shape() {
        // Group names are not packages. Without a required `version`, serde
        // accepts this and reports "dependencies"/"devDependencies" as deps.
        let grouped = r#"{"dependencies": {"express": {"version": "4.18.2"}}, "devDependencies": {"vitest": {"version": "1.0.0"}}}"#;
        assert!(filter_bun_pm_ls_json(grouped).is_none());
        // It falls through to the raw text fallback rather than reporting the
        // two group names as a confident dependency list.
        let out = filter_bun_pm_ls(grouped);
        assert!(!out.starts_with("2 deps"), "{out}");
    }

    #[test]
    fn test_filter_bun_pkg_keeps_indentation() {
        let raw = "bun install v1.3.6
error: failed to resolve left-pad
    hint: check the registry
";
        let out = filter_bun_pkg(raw);
        assert!(out.contains("    hint: check the registry"), "{out}");
    }

    #[test]
    fn test_filter_bun_pm_ls_tree_dedups_nested_all() {
        // `bun pm ls --all` can list the same package under several parents.
        let raw = "/home/user/project node_modules\n\u{251c}\u{2500}\u{2500} a@1.0.0\n\u{2502} \u{2514}\u{2500}\u{2500} shared@2.0.0\n\u{2514}\u{2500}\u{2500} b@1.0.0\n  \u{2514}\u{2500}\u{2500} shared@2.0.0";
        let out = filter_bun_pm_ls_tree(raw).expect("should parse");
        assert!(out.starts_with("3 deps"), "{out}");
        assert_eq!(out.matches("shared@2.0.0").count(), 1, "{out}");
    }

    #[test]
    fn test_filter_bun_pm_ls_tree_rejects_non_tree() {
        assert!(filter_bun_pm_ls_tree("error: something broke").is_none());
        assert!(filter_bun_pm_ls_tree("").is_none());
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
