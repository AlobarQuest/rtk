use crate::tracking;
use anyhow::{Context, Result};
use std::ffi::OsString;
use std::process::Command;

/// Filter deno output: strip download lines and empty lines.
pub fn filter_deno_output(output: &str) -> String {
    let filtered: Vec<&str> = output
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.starts_with("Download ")
        })
        .collect();

    if filtered.is_empty() {
        "ok".to_string()
    } else {
        filtered.join("\n")
    }
}

pub fn run_lint(args: &[String], verbose: u8) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    let mut cmd = Command::new("deno");
    cmd.arg("lint");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: deno lint {}", args.join(" "));
    }

    let output = cmd
        .output()
        .context("Failed to run deno lint. Is Deno installed?")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    let exit_code = output
        .status
        .code()
        .unwrap_or(if output.status.success() { 0 } else { 1 });
    let filtered = filter_deno_output(&raw);

    if let Some(hint) = crate::tee::tee_and_hint(&raw, "deno_lint", exit_code) {
        println!("{}\n{}", filtered, hint);
    } else {
        println!("{}", filtered);
    }

    timer.track(
        &format!("deno lint {}", args.join(" ")),
        &format!("rtk deno lint {}", args.join(" ")),
        &raw,
        &filtered,
    );

    if !output.status.success() {
        std::process::exit(exit_code);
    }

    Ok(())
}

pub fn run_check(args: &[String], verbose: u8) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    let mut cmd = Command::new("deno");
    cmd.arg("check");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: deno check {}", args.join(" "));
    }

    let output = cmd
        .output()
        .context("Failed to run deno check. Is Deno installed?")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    let exit_code = output
        .status
        .code()
        .unwrap_or(if output.status.success() { 0 } else { 1 });
    let filtered = filter_deno_output(&raw);

    if let Some(hint) = crate::tee::tee_and_hint(&raw, "deno_check", exit_code) {
        println!("{}\n{}", filtered, hint);
    } else {
        println!("{}", filtered);
    }

    timer.track(
        &format!("deno check {}", args.join(" ")),
        &format!("rtk deno check {}", args.join(" ")),
        &raw,
        &filtered,
    );

    if !output.status.success() {
        std::process::exit(exit_code);
    }

    Ok(())
}

pub fn run_task(args: &[String], verbose: u8) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    let mut cmd = Command::new("deno");
    cmd.arg("task");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: deno task {}", args.join(" "));
    }

    let output = cmd
        .output()
        .context("Failed to run deno task. Is Deno installed?")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    let exit_code = output
        .status
        .code()
        .unwrap_or(if output.status.success() { 0 } else { 1 });
    let filtered = filter_deno_output(&raw);

    println!("{}", filtered);

    timer.track(
        &format!("deno task {}", args.join(" ")),
        &format!("rtk deno task {}", args.join(" ")),
        &raw,
        &filtered,
    );

    if !output.status.success() {
        std::process::exit(exit_code);
    }

    Ok(())
}

pub fn run_other(args: &[OsString], verbose: u8) -> Result<()> {
    if args.is_empty() {
        anyhow::bail!("deno: no subcommand specified");
    }

    let timer = tracking::TimedExecution::start();

    let subcommand = args[0].to_string_lossy();
    let mut cmd = Command::new("deno");
    cmd.arg(&*subcommand);
    for arg in &args[1..] {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: deno {} ...", subcommand);
    }

    let output = cmd
        .output()
        .with_context(|| format!("Failed to run deno {}", subcommand))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    print!("{}", stdout);
    eprint!("{}", stderr);

    timer.track(
        &format!("deno {}", subcommand),
        &format!("rtk deno {}", subcommand),
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
    fn test_filter_deno_output_empty() {
        let input = r#"Download https://deno.land/std@0.200.0/path/mod.ts

Download https://deno.land/x/oak@v12.6.1/mod.ts

"#;

        let result = filter_deno_output(input);
        assert_eq!(result, "ok");
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
