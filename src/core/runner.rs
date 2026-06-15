//! Shared command execution skeleton for filter modules.

use anyhow::{Context, Result};
use regex::Regex;
use std::process::Command;
use std::sync::LazyLock;

use crate::core::stream::{self, FilterMode, StdinMode, StreamFilter};
use crate::core::tracking;
use crate::core::truncate::{CAP_LIST, CAP_WARNINGS};

/// Compose `filtered` with an optional recovery `hint`, cap the total at `raw`
/// (never emit more tokens than the command), print it, and return what was
/// emitted so the caller tracks exactly that.
pub fn emit_guarded(filtered: &str, hint: Option<&str>, raw: &str) -> String {
    let body = match hint {
        Some(h) => format!("{}\n{}", filtered, h),
        None => filtered.to_string(),
    };
    let shown = crate::core::guard::never_worse(raw, &body).to_string();
    println!("{}", shown);
    shown
}

pub fn print_with_hint(
    filtered: &str,
    tee_raw: &str,
    guard_raw: &str,
    tee_label: &str,
    exit_code: i32,
) -> String {
    let hint = crate::core::tee::tee_and_hint(tee_raw, tee_label, exit_code);
    emit_guarded(filtered, hint.as_deref(), guard_raw)
}

#[derive(Default)]
pub struct RunOptions<'a> {
    pub tee_label: Option<&'a str>,
    pub filter_stdout_only: bool,
    pub skip_filter_on_failure: bool,
    pub no_trailing_newline: bool,
    /// Forward rtk's own stdin to the child process. Needed for commands that
    /// can read from a pipe (e.g. `cat file | rtk wc`); without it the child
    /// gets an empty stdin and reports zero.
    pub inherit_stdin: bool,
}

impl<'a> RunOptions<'a> {
    pub fn with_tee(label: &'a str) -> Self {
        Self {
            tee_label: Some(label),
            ..Default::default()
        }
    }

    pub fn stdout_only() -> Self {
        Self {
            filter_stdout_only: true,
            ..Default::default()
        }
    }

    pub fn tee(mut self, label: &'a str) -> Self {
        self.tee_label = Some(label);
        self
    }

    pub fn early_exit_on_failure(mut self) -> Self {
        self.skip_filter_on_failure = true;
        self
    }

    pub fn no_trailing_newline(mut self) -> Self {
        self.no_trailing_newline = true;
        self
    }

    pub fn inherit_stdin(mut self) -> Self {
        self.inherit_stdin = true;
        self
    }
}

pub type CaptureFilter<'a> = Box<dyn Fn(&str) -> String + 'a>;
pub type ExitAwareCaptureFilter<'a> = Box<dyn Fn(&str, i32) -> String + 'a>;

pub enum RunMode<'a> {
    Filtered(CaptureFilter<'a>),
    FilteredWithExit(ExitAwareCaptureFilter<'a>),
    Streamed(Box<dyn StreamFilter + 'a>),
    Passthrough,
}

fn run_captured_filter<F>(
    mut cmd: Command,
    tool_name: &str,
    cmd_label: &str,
    filter_fn: F,
    opts: RunOptions<'_>,
    timer: tracking::TimedExecution,
) -> Result<i32>
where
    F: Fn(&str, i32) -> String,
{
    let stdin_mode = if opts.inherit_stdin {
        StdinMode::Inherit
    } else {
        StdinMode::Null
    };
    let result = stream::run_streaming(&mut cmd, stdin_mode, FilterMode::CaptureOnly)
        .with_context(|| format!("Failed to run {}", tool_name))?;

    let exit_code = result.exit_code;
    let raw = &result.raw;
    let raw_stdout = &result.raw_stdout;

    if opts.skip_filter_on_failure && exit_code != 0 {
        if !result.raw_stdout.trim().is_empty() {
            print!("{}", result.raw_stdout);
        }
        if !result.raw_stderr.trim().is_empty() {
            eprint!("{}", result.raw_stderr);
        }
        timer.track(cmd_label, &format!("rtk {}", cmd_label), raw, raw);
        return Ok(exit_code);
    }

    let text_to_filter = if opts.filter_stdout_only {
        raw_stdout
    } else {
        raw
    };
    let filtered = filter_fn(text_to_filter, exit_code);

    let raw_for_tracking = if opts.filter_stdout_only {
        raw_stdout
    } else {
        raw
    };

    let shown = if let Some(label) = opts.tee_label {
        print_with_hint(&filtered, raw, raw_for_tracking, label, exit_code)
    } else {
        let guarded = crate::core::guard::never_worse(raw_for_tracking, &filtered).to_string();
        if opts.no_trailing_newline {
            print!("{}", guarded);
        } else {
            println!("{}", guarded);
        }
        guarded
    };

    timer.track(
        cmd_label,
        &format!("rtk {}", cmd_label),
        raw_for_tracking,
        &shown,
    );
    Ok(exit_code)
}

pub fn run(
    mut cmd: Command,
    tool_name: &str,
    args_display: &str,
    mode: RunMode<'_>,
    opts: RunOptions<'_>,
) -> Result<i32> {
    let timer = tracking::TimedExecution::start();
    let cmd_label = format!("{} {}", tool_name, args_display);

    match mode {
        RunMode::Filtered(filter_fn) => run_captured_filter(
            cmd,
            tool_name,
            &cmd_label,
            move |text, _| filter_fn(text),
            opts,
            timer,
        ),
        RunMode::FilteredWithExit(filter_fn) => run_captured_filter(
            cmd,
            tool_name,
            &cmd_label,
            move |text, exit_code| filter_fn(text, exit_code),
            opts,
            timer,
        ),
        RunMode::Streamed(filter) => {
            let result =
                stream::run_streaming(&mut cmd, StdinMode::Null, FilterMode::Streaming(filter))
                    .with_context(|| format!("Failed to run {}", tool_name))?;

            if let Some(label) = opts.tee_label {
                if let Some(hint) =
                    crate::core::tee::tee_and_hint(&result.raw, label, result.exit_code)
                {
                    println!("{}", hint);
                }
            }

            timer.track(
                &cmd_label,
                &format!("rtk {}", cmd_label),
                &result.raw,
                &result.filtered,
            );
            Ok(result.exit_code)
        }
        RunMode::Passthrough => {
            let result =
                stream::run_streaming(&mut cmd, StdinMode::Inherit, FilterMode::Passthrough)
                    .with_context(|| format!("Failed to run {}", tool_name))?;

            timer.track_passthrough(&cmd_label, &format!("rtk {} (passthrough)", cmd_label));
            Ok(result.exit_code)
        }
    }
}

pub fn run_filtered<F>(
    cmd: Command,
    tool_name: &str,
    args_display: &str,
    filter_fn: F,
    opts: RunOptions<'_>,
) -> Result<i32>
where
    F: Fn(&str) -> String,
{
    run(
        cmd,
        tool_name,
        args_display,
        RunMode::Filtered(Box::new(filter_fn)),
        opts,
    )
}

pub fn run_filtered_with_exit<F>(
    cmd: Command,
    tool_name: &str,
    args_display: &str,
    filter_fn: F,
    opts: RunOptions<'_>,
) -> Result<i32>
where
    F: Fn(&str, i32) -> String,
{
    run(
        cmd,
        tool_name,
        args_display,
        RunMode::FilteredWithExit(Box::new(filter_fn)),
        opts,
    )
}

pub fn run_passthrough(tool: &str, args: &[std::ffi::OsString], verbose: u8) -> Result<i32> {
    if verbose > 0 {
        eprintln!("{} passthrough: {:?}", tool, args);
    }
    let mut cmd = crate::core::utils::resolved_command(tool);
    cmd.args(args);
    let args_str = tracking::args_display(args);
    run(
        cmd,
        tool,
        &args_str,
        RunMode::Passthrough,
        RunOptions::default(),
    )
}

pub fn run_streamed(
    cmd: Command,
    tool_name: &str,
    args_display: &str,
    filter: Box<dyn StreamFilter + '_>,
    opts: RunOptions<'_>,
) -> Result<i32> {
    run(
        cmd,
        tool_name,
        args_display,
        RunMode::Streamed(filter),
        opts,
    )
}

// Ecosystem-agnostic err/test command runners. Used by cargo, bun, deno, and the
// shell-string wrappers in cmds::rust::runner.

const MAX_RUNNER_FAILURES: usize = CAP_WARNINGS;
const MAX_RUNNER_LINES: usize = CAP_LIST;

static ERROR_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        // Generic errors
        Regex::new(r"(?i)^.*error[\s:\[].*$").unwrap(),
        Regex::new(r"(?i)^.*\berr\b.*$").unwrap(),
        Regex::new(r"(?i)^.*warning[\s:\[].*$").unwrap(),
        Regex::new(r"(?i)^.*\bwarn\b.*$").unwrap(),
        Regex::new(r"(?i)^.*failed.*$").unwrap(),
        Regex::new(r"(?i)^.*failure.*$").unwrap(),
        Regex::new(r"(?i)^.*exception.*$").unwrap(),
        Regex::new(r"(?i)^.*panic.*$").unwrap(),
        // Rust specific
        Regex::new(r"^error\[E\d+\]:.*$").unwrap(),
        Regex::new(r"^\s*--> .*:\d+:\d+$").unwrap(),
        // Python
        Regex::new(r"^Traceback.*$").unwrap(),
        Regex::new(r#"^\s*File ".*", line \d+.*$"#).unwrap(),
        // JavaScript/TypeScript
        Regex::new(r"^\s*at .*:\d+:\d+.*$").unwrap(),
        // Go
        Regex::new(r"^.*\.go:\d+:.*$").unwrap(),
    ]
});

struct ErrorStreamFilter {
    in_error_block: bool,
    blank_count: usize,
    emitted_any: bool,
}

impl ErrorStreamFilter {
    fn new() -> Self {
        Self {
            in_error_block: false,
            blank_count: 0,
            emitted_any: false,
        }
    }
}

impl StreamFilter for ErrorStreamFilter {
    fn feed_line(&mut self, line: &str) -> Option<String> {
        let is_error = ERROR_PATTERNS.iter().any(|p| p.is_match(line));
        if is_error {
            self.in_error_block = true;
            self.blank_count = 0;
            self.emitted_any = true;
            Some(format!("{}\n", line))
        } else if self.in_error_block {
            if line.trim().is_empty() {
                self.blank_count += 1;
                if self.blank_count >= 2 {
                    self.in_error_block = false;
                    None
                } else {
                    self.emitted_any = true;
                    Some(format!("{}\n", line))
                }
            } else if line.starts_with(' ') || line.starts_with('\t') {
                self.blank_count = 0;
                self.emitted_any = true;
                Some(format!("{}\n", line))
            } else {
                self.in_error_block = false;
                None
            }
        } else {
            None
        }
    }

    fn flush(&mut self) -> String {
        String::new()
    }

    fn on_exit(&mut self, exit_code: i32, raw: &str) -> Option<String> {
        if self.emitted_any {
            return None;
        }
        if exit_code == 0 {
            Some("[ok] Command completed successfully (no errors)".to_string())
        } else {
            let mut msg = format!("[FAIL] Command failed (exit code: {})\n", exit_code);
            let lines: Vec<&str> = raw.lines().collect();
            for line in lines.iter().rev().take(10).rev() {
                msg.push_str(&format!("  {}\n", line));
            }
            Some(msg)
        }
    }
}

/// Run a prebuilt command (no shell) and filter output to show only errors/warnings.
/// `display` is used only for logging, tee keys, and tracking, never executed.
pub fn run_err_cmd(cmd: Command, display: &str, verbose: u8) -> Result<i32> {
    if verbose > 0 {
        eprintln!("Running: {}", display);
    }
    run_streamed(
        cmd,
        "err",
        display,
        Box::new(ErrorStreamFilter::new()),
        RunOptions::with_tee("err"),
    )
}

/// Run a prebuilt test command (no shell), showing only failures.
/// `display` is used only for logging and tool detection, never executed.
pub fn run_test_cmd(cmd: Command, display: &str, verbose: u8) -> Result<i32> {
    if verbose > 0 {
        eprintln!("Running tests: {}", display);
    }
    let display_owned = display.to_string();
    run_filtered(
        cmd,
        "test",
        display,
        move |raw| extract_test_summary(raw, &display_owned),
        RunOptions::with_tee("test"),
    )
}

#[cfg(test)]
fn filter_errors(output: &str) -> String {
    let mut result = Vec::new();
    let mut in_error_block = false;
    let mut blank_count = 0;

    for line in output.lines() {
        let is_error_line = ERROR_PATTERNS.iter().any(|p| p.is_match(line));

        if is_error_line {
            in_error_block = true;
            blank_count = 0;
            result.push(line.to_string());
        } else if in_error_block {
            if line.trim().is_empty() {
                blank_count += 1;
                if blank_count >= 2 {
                    in_error_block = false;
                } else {
                    result.push(line.to_string());
                }
            } else if line.starts_with(' ') || line.starts_with('\t') {
                result.push(line.to_string());
                blank_count = 0;
            } else {
                in_error_block = false;
            }
        }
    }

    result.join("\n")
}

fn extract_test_summary(output: &str, command: &str) -> String {
    let mut result = Vec::new();
    let lines: Vec<&str> = output.lines().collect();

    let is_cargo = command.contains("cargo test");
    let is_pytest = command.contains("pytest");
    let is_jest =
        command.contains("jest") || command.contains("npm test") || command.contains("yarn test");
    let is_go = command.contains("go test");
    let is_bun = command.contains("bun test");
    // Deno's test output is cargo-shaped ("test result:", "FAILED", "failures:").
    let is_deno = command.contains("deno test");

    let mut failures = Vec::new();
    let mut in_failure = false;
    let mut failure_lines = Vec::new();

    for line in lines.iter() {
        if is_cargo {
            if line.contains("test result:") {
                result.push(line.to_string());
            }
            if line.contains("FAILED") && !line.contains("test result") {
                failures.push(line.to_string());
            }
            if line.starts_with("failures:") {
                in_failure = true;
            }
            if in_failure && line.starts_with("    ") {
                failure_lines.push(line.to_string());
            }
        }

        if is_pytest {
            if line.contains(" passed") || line.contains(" failed") || line.contains(" error") {
                result.push(line.to_string());
            }
            if line.contains("FAILED") {
                failures.push(line.to_string());
            }
        }

        if is_jest {
            if line.contains("Tests:") || line.contains("Test Suites:") {
                result.push(line.to_string());
            }
            if line.contains("✕") || line.contains("FAIL") {
                failures.push(line.to_string());
            }
        }

        if is_go {
            if line.starts_with("ok") || line.starts_with("FAIL") || line.starts_with("---") {
                result.push(line.to_string());
            }
            if line.contains("FAIL") {
                failures.push(line.to_string());
            }
        }

        if is_bun {
            let trimmed = line.trim_start();
            if line.contains(" pass") || line.contains(" fail") || trimmed.starts_with("Ran ") {
                result.push(line.to_string());
            }
            if line.contains('✗') || line.contains("(fail)") {
                failures.push(line.to_string());
            }
        }

        if is_deno {
            if line.contains("test result:") {
                result.push(line.to_string());
            }
            if line.contains("FAILED") && !line.contains("test result") {
                failures.push(line.to_string());
            }
            if line.starts_with("failures:") {
                in_failure = true;
            }
            if in_failure && line.starts_with("    ") {
                failure_lines.push(line.to_string());
            }
        }
    }

    let mut output = String::new();

    if !failures.is_empty() {
        output.push_str("[FAIL] FAILURES:\n");
        for f in failures.iter().take(MAX_RUNNER_FAILURES) {
            output.push_str(&format!("  {}\n", f));
        }
        if failures.len() > MAX_RUNNER_FAILURES {
            output.push_str(&format!(
                "  ... +{} more failures\n",
                failures.len() - MAX_RUNNER_FAILURES
            ));
        }
        for f in failure_lines.iter().take(MAX_RUNNER_LINES) {
            output.push_str(&format!("  {}\n", f.trim()));
        }
        if failure_lines.len() > MAX_RUNNER_LINES {
            output.push_str(&format!(
                "  ... +{} more\n",
                failure_lines.len() - MAX_RUNNER_LINES
            ));
        }
        output.push('\n');
    }

    if !result.is_empty() {
        output.push_str("SUMMARY:\n");
        for r in &result {
            output.push_str(&format!("  {}\n", r));
        }
    } else {
        output.push_str("OUTPUT (last 5 lines):\n");
        let start = lines.len().saturating_sub(5);
        for line in &lines[start..] {
            if !line.trim().is_empty() {
                output.push_str(&format!("  {}\n", line));
            }
        }
    }

    output
}

#[cfg(test)]
mod err_test_runner_tests {
    use super::*;

    #[test]
    fn test_filter_errors() {
        let output = "info: compiling\nerror: something failed\n  at line 10\ninfo: done";
        let filtered = filter_errors(output);
        assert!(filtered.contains("error"));
        assert!(!filtered.contains("info"));
    }

    #[test]
    fn test_extract_bun_test_failures() {
        let raw = "bun test v1.1.0\nsrc/math.test.ts:\n✗ adds numbers [1ms]\n 3 pass\n 1 fail\nRan 4 tests across 1 file.";
        let out = extract_test_summary(raw, "bun test");
        assert!(out.contains("[FAIL]"), "expected failure block, got: {out}");
        assert!(out.contains("adds numbers"));
        assert!(out.contains("1 fail"));
    }

    #[test]
    fn test_extract_deno_test_failures() {
        let raw = "running 2 tests\ntest add ... ok\ntest sub ... FAILED\nfailures:\n    sub\ntest result: FAILED. 1 passed; 1 failed; 0 ignored";
        let out = extract_test_summary(raw, "deno test");
        assert!(out.contains("[FAIL]"), "expected failure block, got: {out}");
        assert!(out.contains("test result:"));
    }
}
