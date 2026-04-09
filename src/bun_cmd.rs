use crate::tracking;
use crate::utils::truncate;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::ffi::OsString;
use std::process::Command;

/// JSON structure for `bun pm ls --json` output.
#[derive(Debug, Deserialize)]
struct BunPmPackage {
    version: Option<String>,
}

/// Filter bun install/add output — strip progress lines, version headers, empty lines.
pub fn filter_bun_install(output: &str) -> String {
    let mut result = Vec::new();

    for line in output.lines() {
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

        // Skip version headers like "bun install v1.1.0"
        if (trimmed.starts_with("bun install v") || trimmed.starts_with("bun add v"))
            && trimmed.split_whitespace().count() <= 4
        {
            continue;
        }

        result.push(trimmed.to_string());
    }

    if result.is_empty() {
        "ok".to_string()
    } else {
        result.join("\n")
    }
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

    if lines.is_empty() {
        return "ok".to_string();
    }

    let joined = lines.join("\n");
    truncate(&joined, 500)
}

pub fn run_install(args: &[String], verbose: u8) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    let mut cmd = Command::new("bun");
    cmd.arg("install");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: bun install {}", args.join(" "));
    }

    let output = cmd
        .output()
        .context("Failed to run bun install. Is bun installed?")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    let filtered = filter_bun_install(&raw);
    println!("{}", filtered);

    timer.track(
        &format!("bun install {}", args.join(" ")),
        &format!("rtk bun install {}", args.join(" ")),
        &raw,
        &filtered,
    );

    if !output.status.success() {
        std::process::exit(output.status.code().unwrap_or(1));
    }

    Ok(())
}

pub fn run_pm_ls(args: &[String], verbose: u8) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    let mut cmd = Command::new("bun");
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
    let raw = format!("{}\n{}", stdout, stderr);

    let filtered = if let Some(json_result) = filter_bun_pm_ls_json(&stdout) {
        json_result
    } else {
        filter_bun_pm_ls_text(&raw)
    };

    println!("{}", filtered);

    timer.track(
        &format!("bun pm ls {}", args.join(" ")),
        &format!("rtk bun pm ls {}", args.join(" ")),
        &raw,
        &filtered,
    );

    if !output.status.success() {
        std::process::exit(output.status.code().unwrap_or(1));
    }

    Ok(())
}

pub fn run_run(args: &[String], verbose: u8) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    let mut cmd = Command::new("bun");
    cmd.arg("run");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: bun run {}", args.join(" "));
    }

    let output = cmd
        .output()
        .context("Failed to run bun run. Is bun installed?")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    print!("{}", stdout);
    eprint!("{}", stderr);

    timer.track(
        &format!("bun run {}", args.join(" ")),
        &format!("rtk bun run {}", args.join(" ")),
        &raw,
        &raw,
    );

    if !output.status.success() {
        std::process::exit(output.status.code().unwrap_or(1));
    }

    Ok(())
}

pub fn run_other(args: &[OsString], verbose: u8) -> Result<()> {
    if args.is_empty() {
        anyhow::bail!("bun: no subcommand specified");
    }

    let timer = tracking::TimedExecution::start();

    let subcommand = args[0].to_string_lossy();
    let mut cmd = Command::new("bun");
    cmd.arg(&*subcommand);
    for arg in &args[1..] {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: bun {} ...", subcommand);
    }

    let output = cmd
        .output()
        .with_context(|| format!("Failed to run bun {}", subcommand))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    print!("{}", stdout);
    eprint!("{}", stderr);

    timer.track(
        &format!("bun {}", subcommand),
        &format!("rtk bun {}", subcommand),
        &raw,
        &raw,
    );

    if !output.status.success() {
        std::process::exit(output.status.code().unwrap_or(1));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let result = filter_bun_install(output);
        assert!(!result.contains("[1/5]"));
        assert!(!result.contains("bun install v1.1.0"));
        assert!(result.contains("express"));
        assert!(result.contains("3 packages installed"));
    }

    #[test]
    fn test_filter_bun_install_empty_output() {
        let output = "\n\n\n";
        let result = filter_bun_install(output);
        assert_eq!(result, "ok");
    }

    #[test]
    fn test_filter_bun_pm_ls_json() {
        let json = r#"{
            "express": {"version": "4.18.2"},
            "lodash": {"version": "4.17.21"},
            "axios": {"version": "1.6.0"}
        }"#;
        let result = filter_bun_pm_ls_json(json).unwrap();
        assert!(result.starts_with("3 deps"));
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines[1], "axios@1.6.0");
        assert_eq!(lines[2], "express@4.18.2");
        assert_eq!(lines[3], "lodash@4.17.21");
    }

    #[test]
    fn test_filter_bun_pm_ls_json_empty() {
        let result = filter_bun_pm_ls_json("{}");
        assert!(result.is_none());
    }

    #[test]
    fn test_filter_bun_install_preserves_errors() {
        let output = r#"bun install v1.1.0
[1/4] Resolving packages...
error: PackageNotFound - "nonexistent-pkg" not found in registry
"#;
        let result = filter_bun_install(output);
        assert!(result.contains("error:"));
        assert!(result.contains("nonexistent-pkg"));
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
}
