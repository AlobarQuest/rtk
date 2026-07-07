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

/// Test-output ecosystem, chosen once at the boundary. Modules that know
/// their runner statically pass the variant directly; shell-string entry
/// points convert once via `detect`. Matching on the enum makes substring
/// co-firing ("cargo test" contains "go test") unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestEcosystem {
    Cargo,
    Pytest,
    Jest,
    Go,
    Bun,
    Deno,
    Unknown,
}

impl TestEcosystem {
    /// Detect from a display or shell string. First match wins; Cargo is
    /// checked before Go because "cargo test" contains "go test".
    pub fn detect(command: &str) -> Self {
        if command.contains("cargo test") {
            Self::Cargo
        } else if command.contains("pytest") {
            Self::Pytest
        } else if command.contains("jest")
            || command.contains("npm test")
            || command.contains("yarn test")
        {
            Self::Jest
        } else if command.contains("bun test") {
            Self::Bun
        } else if command.contains("deno test") {
            Self::Deno
        } else if command.contains("go test") {
            Self::Go
        } else {
            Self::Unknown
        }
    }
}

/// Run a prebuilt test command (no shell), showing only failures.
/// `display` is used only for logging and tracking, never executed.
pub fn run_test_cmd(cmd: Command, display: &str, eco: TestEcosystem, verbose: u8) -> Result<i32> {
    if verbose > 0 {
        eprintln!("Running tests: {}", display);
    }
    run_filtered(
        cmd,
        "test",
        display,
        move |raw| extract_test_summary(raw, eco),
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

fn extract_test_summary(output: &str, eco: TestEcosystem) -> String {
    // Test runners colorize even when piped (deno does), so anchor on clean text.
    let cleaned = crate::core::utils::strip_ansi(output);
    let lines: Vec<&str> = cleaned.lines().collect();

    let mut result = Vec::new();
    let mut failures = Vec::new();
    let mut failure_lines = Vec::new();
    let mut in_failure = false;
    let mut in_failures_list = false;

    for line in lines.iter() {
        match eco {
            TestEcosystem::Cargo => {
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

            TestEcosystem::Pytest => {
                if line.contains(" passed") || line.contains(" failed") || line.contains(" error") {
                    result.push(line.to_string());
                }
                if line.contains("FAILED") {
                    failures.push(line.to_string());
                }
            }

            TestEcosystem::Jest => {
                if line.contains("Tests:") || line.contains("Test Suites:") {
                    result.push(line.to_string());
                }
                if line.contains("✕") || line.contains("FAIL") {
                    failures.push(line.to_string());
                }
            }

            TestEcosystem::Go => {
                if line.starts_with("ok") || line.starts_with("FAIL") || line.starts_with("---") {
                    result.push(line.to_string());
                }
                if line.contains("FAIL") {
                    failures.push(line.to_string());
                }
            }

            TestEcosystem::Bun => {
                let trimmed = line.trim_start();
                // Anchored count lines (" 6 pass", " 4 fail") and the "Ran N tests" footer.
                // A loose `contains(" fail")` also matches bun's echoed source context when
                // a test NAME contains "fails", flooding the summary with duplicate snippets.
                if is_bun_count_line(trimmed) || trimmed.starts_with("Ran ") {
                    result.push(line.to_string());
                }
                // Bun prints the diagnostic BEFORE the failure marker:
                //   error: expect(received).toBe(expected)
                //   Expected: 3
                //   Received: 2
                //         at <anonymous> (...)
                //   (fail) t2 fails [1.86ms]
                if trimmed.starts_with("(fail)") || line.contains('✗') {
                    failures.push(line.to_string());
                    in_failure = false;
                } else if trimmed.starts_with("error:") {
                    in_failure = true;
                    failure_lines.push(line.to_string());
                } else if in_failure && !trimmed.is_empty() && !trimmed.starts_with("at ") {
                    failure_lines.push(line.to_string());
                }
            }

            TestEcosystem::Deno => {
                // Full trim: ANSI background padding leaves " FAILURES " with
                // trailing whitespace after stripping.
                let trimmed = line.trim();
                // Current deno (2.x): "FAILED | 3 passed | 2 failed (17ms)" footer,
                // " FAILURES " section listing "name => file:line:col".
                // Legacy deno mimicked cargo ("test result:", "failures:").
                if trimmed.starts_with("FAILED |")
                    || trimmed.starts_with("ok |")
                    || line.contains("test result:")
                {
                    result.push(line.to_string());
                    in_failures_list = false;
                } else if trimmed == "FAILURES" || line.starts_with("failures:") {
                    in_failures_list = true;
                } else if in_failures_list && !trimmed.is_empty() {
                    failures.push(line.to_string());
                }
                // Diagnostics live in the ERRORS section: an "error:" line, then
                // [Diff] and +/- lines, then a stack that ends the block.
                // "error: Test failed" is deno's generic footer, not a diagnostic.
                if trimmed.starts_with("error:") && trimmed != "error: Test failed" {
                    in_failure = true;
                    failure_lines.push(line.to_string());
                } else if in_failure {
                    if trimmed.starts_with("at ") {
                        in_failure = false;
                    } else if !trimmed.is_empty()
                        && trimmed != "^"
                        && !trimmed.starts_with("throw ")
                    {
                        // "throw new AssertionError(...)" is deno-std's echoed
                        // internal throw site; the failing test's location is
                        // already in the FAILURES marker.
                        failure_lines.push(line.to_string());
                    }
                }
            }

            TestEcosystem::Unknown => {}
        }
    }

    let mut output = String::new();

    if !failures.is_empty() {
        output.push_str("[FAIL] FAILURES:\n");
        for f in failures.iter().take(MAX_RUNNER_FAILURES) {
            output.push_str(&format!("  {}\n", f.trim()));
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

/// True for bun's summary count lines: " 6 pass", " 4 fail", " 2 skip", " 1 todo".
/// Anchored on the exact two-token shape so echoed source lines never match.
fn is_bun_count_line(trimmed: &str) -> bool {
    let mut parts = trimmed.split_whitespace();
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(count), Some("pass" | "fail" | "skip" | "todo"), None)
            if count.chars().all(|c| c.is_ascii_digit())
    )
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
        let out = extract_test_summary(raw, TestEcosystem::Bun);
        assert!(out.contains("[FAIL]"), "expected failure block, got: {out}");
        assert!(out.contains("adds numbers"));
        assert!(out.contains("1 fail"));
    }

    #[test]
    fn test_extract_deno_test_failures() {
        let raw = "running 2 tests\ntest add ... ok\ntest sub ... FAILED\nfailures:\n    sub\ntest result: FAILED. 1 passed; 1 failed; 0 ignored";
        let out = extract_test_summary(raw, TestEcosystem::Deno);
        assert!(out.contains("[FAIL]"), "expected failure block, got: {out}");
        assert!(out.contains("test result:"));
    }

    fn count_tokens(s: &str) -> usize {
        s.split_whitespace().count()
    }

    #[test]
    fn test_bun_multifail_golden() {
        let raw = include_str!("../../tests/fixtures/bun_test_multifail_raw.txt");
        let out = extract_test_summary(raw, TestEcosystem::Bun);
        let expected = r#"[FAIL] FAILURES:
  (fail) t2 fails [1.86ms]
  (fail) t4 fails [0.08ms]
  (fail) t6 fails [0.21ms]
  (fail) t8 fails [0.97ms]
  error: expect(received).toBe(expected)
  Expected: 3
  Received: 2
  error: expect(received).toBe(expected)
  Expected: 4
  Received: 3
  error: expect(received).toContain(expected)
  Expected to contain: "bye"
  Received: "hello"
  error: expect(received).toEqual(expected)
  {
  -   "a": 2,
  +   "a": 1,
  }
  - Expected  - 1
  + Received  + 1

SUMMARY:
   6 pass
   4 fail
  Ran 10 tests across 1 file. [62.00ms]
"#;
        assert_eq!(out, expected);
    }

    #[test]
    fn test_bun_multifail_keeps_diagnostics_drops_noise() {
        let raw = include_str!("../../tests/fixtures/bun_test_multifail_raw.txt");
        let out = extract_test_summary(raw, TestEcosystem::Bun);
        // The one thing an agent needs: expected vs received, per failure.
        assert!(
            out.contains("error: expect(received).toBe(expected)"),
            "{out}"
        );
        assert!(out.contains("Expected: 3"), "{out}");
        assert!(out.contains("Received: 2"), "{out}");
        assert!(out.contains("Expected to contain: \"bye\""), "{out}");
        assert!(out.contains("(fail) t2 fails"), "{out}");
        assert!(out.contains("(fail) t8 fails"), "{out}");
        // Echoed source context must not leak into the summary.
        assert!(!out.contains("test(\"t1 passes\""), "{out}");
        assert!(!out.contains("test(\"t2 fails\""), "{out}");
        // Stack frames are noise once the failing test is named.
        assert!(!out.contains("at <anonymous>"), "{out}");
        assert!(out.contains("4 fail"), "{out}");
        assert!(out.contains("Ran 10 tests"), "{out}");
    }

    #[test]
    fn test_bun_multifail_savings() {
        let raw = include_str!("../../tests/fixtures/bun_test_multifail_raw.txt");
        let out = extract_test_summary(raw, TestEcosystem::Bun);
        let savings = 100.0 - (count_tokens(&out) as f64 / count_tokens(raw) as f64 * 100.0);
        assert!(savings >= 60.0, "expected >=60% savings, got {savings:.1}%");
    }

    #[test]
    fn test_bun_all_pass_summary_only() {
        let raw = include_str!("../../tests/fixtures/bun_test_pass_raw.txt");
        let out = extract_test_summary(raw, TestEcosystem::Bun);
        assert!(!out.contains("[FAIL]"), "{out}");
        assert!(out.contains("3 pass"), "{out}");
        assert!(out.contains("0 fail"), "{out}");
        assert!(out.contains("Ran 3 tests"), "{out}");
    }

    #[test]
    fn test_bun_thrown_error_skip_todo() {
        let raw = include_str!("../../tests/fixtures/bun_test_throw_skip_raw.txt");
        let out = extract_test_summary(raw, TestEcosystem::Bun);
        assert!(out.contains("error: boom: connection refused"), "{out}");
        assert!(out.contains("(fail) throws"), "{out}");
        assert!(out.contains("1 skip"), "{out}");
        assert!(out.contains("1 todo"), "{out}");
        assert!(!out.contains("at <anonymous>"), "{out}");
    }

    #[test]
    fn test_ecosystem_detect_first_match_wins() {
        // "cargo test" contains "go test" as a substring; the enum makes
        // that co-firing unrepresentable.
        assert_eq!(
            TestEcosystem::detect("cargo test --all"),
            TestEcosystem::Cargo
        );
        assert_eq!(TestEcosystem::detect("go test ./..."), TestEcosystem::Go);
        assert_eq!(TestEcosystem::detect("bun test"), TestEcosystem::Bun);
        assert_eq!(
            TestEcosystem::detect("deno test --allow-read"),
            TestEcosystem::Deno
        );
        assert_eq!(TestEcosystem::detect("pytest -x"), TestEcosystem::Pytest);
        assert_eq!(TestEcosystem::detect("npm test"), TestEcosystem::Jest);
        assert_eq!(TestEcosystem::detect("make check"), TestEcosystem::Unknown);
    }

    #[test]
    fn test_deno_multifail_golden() {
        // Real deno 2.9.1 output: ANSI-colored, " FAILURES " section header,
        // "FAILED | 3 passed | 2 failed" footer. No "test result:" lines.
        let raw = include_str!("../../tests/fixtures/deno_test_multifail_raw.txt");
        let out = extract_test_summary(raw, TestEcosystem::Deno);
        let expected = r#"[FAIL] FAILURES:
  subs fails => ./math_test.ts:3:6
  len fails => ./math_test.ts:5:6
  includes fails => ./more_test.ts:3:6
  object fails => ./more_test.ts:5:6
  error: AssertionError: Values are not equal.
  [Diff] Actual / Expected
  -   2
  +   1
  error: AssertionError: Values are not equal.
  [Diff] Actual / Expected
  -   3
  +   4
  error: AssertionError: Expected actual: "hello world" to contain: "bye".
  error: AssertionError: Values are not equal.
  [Diff] Actual / Expected
  {
  a: 1,
  -     b: 2,
  +     b: 3,
  }

SUMMARY:
  FAILED | 9 passed | 4 failed (64ms)
"#;
        assert_eq!(out, expected);
    }

    #[test]
    fn test_deno_multifail_savings() {
        let raw = include_str!("../../tests/fixtures/deno_test_multifail_raw.txt");
        let out = extract_test_summary(raw, TestEcosystem::Deno);
        let savings = 100.0 - (count_tokens(&out) as f64 / count_tokens(raw) as f64 * 100.0);
        assert!(savings >= 60.0, "expected >=60% savings, got {savings:.1}%");
    }

    #[test]
    fn test_deno_all_pass_summary_only() {
        let raw = include_str!("../../tests/fixtures/deno_test_pass_raw.txt");
        let out = extract_test_summary(raw, TestEcosystem::Deno);
        assert!(!out.contains("[FAIL]"), "{out}");
        assert!(out.contains("ok | 3 passed | 0 failed"), "{out}");
    }

    #[test]
    fn test_bun_count_line_anchoring() {
        assert!(is_bun_count_line("6 pass"));
        assert!(is_bun_count_line("0 fail"));
        assert!(is_bun_count_line("1 skip"));
        assert!(is_bun_count_line("1 todo"));
        // Echoed source naming a test "fails"/"passes" must not match.
        assert!(!is_bun_count_line(
            "3 | test(\"t2 fails\", () => { expect(1 + 1).toBe(3); });"
        ));
        assert!(!is_bun_count_line("pass"));
        assert!(!is_bun_count_line("6 passing"));
        assert!(!is_bun_count_line("x fail"));
        assert!(!is_bun_count_line("10 expect() calls"));
    }
}
