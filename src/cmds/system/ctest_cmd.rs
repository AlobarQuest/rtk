//! Compact CTest output while preserving failing-test details.

use anyhow::Result;
use lazy_static::lazy_static;
use regex::Regex;
use std::cmp::Ordering;
use std::ffi::OsString;

use crate::core::runner::{self, RunOptions};
use crate::core::utils::{resolved_command, strip_ansi};

const MAX_SLOWEST: usize = 3;

lazy_static! {
    static ref TEST_RE: Regex = Regex::new(
        r"^\s*\d+/\d+\s+Test\s+#\d+:\s+(.+?)\s+\.{2,}\s*(?:\*{3})?\s*(.+?)\s+([\d.]+)\s+sec\s*$",
    )
    .expect("invalid ctest result regex");
    static ref START_RE: Regex =
        Regex::new(r"^\s*Start\s+\d+:").expect("invalid ctest start regex");
    static ref SUMMARY_RE: Regex =
        Regex::new(r"^\s*\d+%\s+tests passed,\s+(\d+)\s+tests failed out of\s+(\d+)")
            .expect("invalid ctest summary regex");
    static ref TIME_RE: Regex =
        Regex::new(r"^\s*Total Test time \(real\)\s+=\s+([\d.]+)\s+sec")
            .expect("invalid ctest time regex");
}

#[derive(Debug, Clone)]
struct TestCase {
    name: String,
    status: String,
    duration: f64,
    line_index: usize,
}

#[derive(Debug, Clone, Copy)]
struct CtestSummary {
    failed: usize,
    total: usize,
}

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    if should_passthrough(args) {
        let os_args: Vec<OsString> = args.iter().map(OsString::from).collect();
        return runner::run_passthrough("ctest", &os_args, verbose);
    }

    let mut cmd = resolved_command("ctest");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: ctest {}", args.join(" "));
    }

    runner::run_filtered(
        cmd,
        "ctest",
        &args.join(" "),
        filter_ctest_output,
        RunOptions::with_tee("ctest"),
    )
}

fn should_passthrough(args: &[String]) -> bool {
    args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "-h" | "-H"
                | "--help"
                | "-help"
                | "-usage"
                | "/?"
                | "--version"
                | "-version"
                | "/V"
                | "-N"
                | "-V"
                | "-VV"
                | "--verbose"
                | "--extra-verbose"
                | "--debug"
                | "--show-only"
                | "--print-labels"
        ) || arg.starts_with("--show-only=")
            || arg.starts_with("--help-")
    })
}

pub(crate) fn looks_like_ctest_output(output: &str) -> bool {
    let mut lines = output.lines().filter(|line| !line.trim().is_empty());
    let Some(mut first) = lines.next() else {
        return false;
    };
    if first
        .trim_start()
        .starts_with("Internal ctest changing into directory: ")
    {
        let Some(project_line) = lines.next() else {
            return false;
        };
        first = project_line;
    }

    first.trim_start().starts_with("Test project ")
        && (output.contains("No tests were found")
            || lines.any(|line| parse_test_line(line, 0).is_some()))
}

pub(crate) fn filter_ctest_output(output: &str) -> String {
    let clean = strip_ansi(output);
    let trimmed = clean.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.contains("No tests were found") {
        return "ctest: no tests found".to_string();
    }

    let lines: Vec<&str> = clean.lines().collect();
    let tests = parse_tests(&lines);
    let summary = lines.iter().find_map(|line| parse_summary(line));
    let total_time = lines.iter().find_map(|line| parse_total_time(line));

    if tests.is_empty() && summary.is_none() {
        return trimmed.to_string();
    }

    let failed_tests: Vec<&TestCase> = tests.iter().filter(|test| test.is_failure()).collect();
    if failed_tests.is_empty() && summary.map_or(0, |s| s.failed) == 0 {
        return format_success(&tests, summary, total_time);
    }

    format_failure(&lines, &tests, summary, total_time)
}

impl TestCase {
    fn is_passed(&self) -> bool {
        self.status.eq_ignore_ascii_case("passed")
    }

    fn is_disabled(&self) -> bool {
        self.status.eq_ignore_ascii_case("not run (disabled)")
    }

    fn is_failure(&self) -> bool {
        !self.is_passed() && !self.is_disabled()
    }
}

fn parse_tests(lines: &[&str]) -> Vec<TestCase> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(line_index, line)| parse_test_line(line, line_index))
        .collect()
}

fn parse_test_line(line: &str, line_index: usize) -> Option<TestCase> {
    let caps = TEST_RE.captures(line.trim_end())?;
    Some(TestCase {
        name: caps.get(1)?.as_str().trim().to_string(),
        status: caps.get(2)?.as_str().trim().to_string(),
        duration: caps.get(3)?.as_str().parse().ok()?,
        line_index,
    })
}

fn parse_summary(line: &str) -> Option<CtestSummary> {
    let caps = SUMMARY_RE.captures(line)?;
    Some(CtestSummary {
        failed: caps.get(1)?.as_str().parse().ok()?,
        total: caps.get(2)?.as_str().parse().ok()?,
    })
}

fn parse_total_time(line: &str) -> Option<f64> {
    TIME_RE.captures(line)?.get(1)?.as_str().parse().ok()
}

fn format_success(tests: &[TestCase], summary: Option<CtestSummary>, total_time: Option<f64>) -> String {
    let total = summary.map_or(tests.len(), |s| s.total);
    let passed = summary.map_or_else(
        || tests.iter().filter(|test| test.is_passed()).count(),
        |s| s.total,
    );
    let disabled = tests.iter().filter(|test| test.is_disabled()).count();
    let mut out = format!("ctest: {passed}/{total} passed");
    if disabled > 0 {
        out.push_str(&format!(", {disabled} disabled"));
    }
    out.push_str(&format_meta(total_time));

    let slowest = slowest_tests(tests);
    if !slowest.is_empty() {
        out.push_str("\nslowest:");
        for test in slowest {
            out.push_str(&format!("\n  {} {}", test.name, format_seconds(test.duration)));
        }
    }

    out
}

fn format_failure(
    lines: &[&str],
    tests: &[TestCase],
    summary: Option<CtestSummary>,
    total_time: Option<f64>,
) -> String {
    let failed_tests: Vec<&TestCase> = tests.iter().filter(|test| test.is_failure()).collect();
    let failed = summary.map_or(failed_tests.len(), |s| s.failed);
    let total = summary.map_or(tests.len(), |s| s.total);
    let disabled = tests.iter().filter(|test| test.is_disabled()).count();
    let passed = tests
        .iter()
        .filter(|test| test.is_passed())
        .count()
        .max(total.saturating_sub(failed));

    let mut out = format!("ctest: {passed}/{total} passed, {failed} failed");
    if disabled > 0 {
        out.push_str(&format!(", {disabled} disabled"));
    }
    out.push_str(&format_meta(total_time));
    if !failed_tests.is_empty() {
        out.push_str("\nfailed:");
        for test in &failed_tests {
            out.push_str(&format!(
                "\n  {} ({}, {})",
                test.name,
                test.status,
                format_seconds(test.duration)
            ));
        }
    }

    let details = failure_details(lines, &failed_tests);
    if !details.is_empty() {
        out.push_str("\n\n");
        out.push_str(&details.join("\n\n"));
    }

    out
}

fn format_meta(total_time: Option<f64>) -> String {
    total_time
        .map(|seconds| format!(" ({})", format_seconds(seconds)))
        .unwrap_or_default()
}

fn format_seconds(seconds: f64) -> String {
    format!("{seconds:.2} sec")
}

fn slowest_tests(tests: &[TestCase]) -> Vec<&TestCase> {
    let mut slowest: Vec<&TestCase> = tests.iter().filter(|test| test.is_passed()).collect();
    slowest.sort_by(|a, b| {
        b.duration
            .partial_cmp(&a.duration)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
    });
    slowest.truncate(MAX_SLOWEST);
    slowest
}

fn failure_details(lines: &[&str], failed_tests: &[&TestCase]) -> Vec<String> {
    let mut blocks: Vec<String> = failed_tests
        .iter()
        .filter_map(|test| collect_failure_block(lines, test.line_index))
        .collect();

    if let Some(section) = collect_failed_section(lines) {
        blocks.push(section);
    }

    blocks
}

fn collect_failure_block(lines: &[&str], result_index: usize) -> Option<String> {
    let mut block = Vec::new();
    let before_result = lines[..result_index]
        .iter()
        .rposition(|line| START_RE.is_match(line))
        .map_or(result_index, |index| index + 1);

    block.extend(
        lines[before_result..result_index]
            .iter()
            .map(|line| line.trim_end().to_string()),
    );

    let mut index = result_index + 1;

    while index < lines.len() {
        let line = lines[index];
        if is_ctest_boundary(line) {
            break;
        }
        block.push(line.trim_end().to_string());
        index += 1;
    }

    trim_blank_edges(&mut block);
    (!block.is_empty()).then(|| block.join("\n"))
}

fn collect_failed_section(lines: &[&str]) -> Option<String> {
    let start = lines
        .iter()
        .position(|line| line.trim() == "The following tests FAILED:")?;
    let mut block: Vec<String> = lines[start..]
        .iter()
        .map(|line| line.trim_end().to_string())
        .collect();

    trim_blank_edges(&mut block);
    (!block.is_empty()).then(|| block.join("\n"))
}

fn is_ctest_boundary(line: &str) -> bool {
    let trimmed = line.trim();
    START_RE.is_match(line)
        || parse_test_line(line, 0).is_some()
        || parse_summary(line).is_some()
        || parse_total_time(line).is_some()
        || trimmed == "The following tests FAILED:"
}

fn trim_blank_edges(lines: &mut Vec<String>) {
    while lines.first().is_some_and(|line| line.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_all_pass_output_to_summary_and_slowest_tests() {
        let output = r#"Test project /tmp/build
    Start 1: fast_case
1/3 Test #1: fast_case ........................   Passed    0.01 sec
    Start 2: slow_case
2/3 Test #2: slow_case ........................   Passed    1.20 sec
    Start 3: medium_case
3/3 Test #3: medium_case ......................   Passed    0.30 sec

100% tests passed, 0 tests failed out of 3

Total Test time (real) =   1.51 sec
"#;

        let filtered = filter_ctest_output(output);

        assert!(filtered.contains("ctest: 3/3 passed (1.51 sec)"));
        assert!(filtered.contains("slow_case 1.20 sec"));
        assert!(filtered.contains("medium_case 0.30 sec"));
        assert!(!filtered.contains("Start 1"));
        assert!(!filtered.contains("Test #1"));
    }

    #[test]
    fn preserves_failure_output_and_drops_passing_noise() {
        let output = r#"Test project /tmp/build
    Start 1: passing_case
1/2 Test #1: passing_case .....................   Passed    0.01 sec
    Start 2: failing_case
2/2 Test #2: failing_case .....................***Failed    0.02 sec
expected: 42
actual:   41

50% tests passed, 1 tests failed out of 2

Total Test time (real) =   0.03 sec

The following tests FAILED:
	  2 - failing_case (Failed)
Errors while running CTest
Use "--rerun-failed --output-on-failure" to re-run the failed cases verbosely.
"#;

        let filtered = filter_ctest_output(output);

        assert_eq!(
            filtered,
            r#"ctest: 1/2 passed, 1 failed (0.03 sec)
failed:
  failing_case (Failed, 0.02 sec)

expected: 42
actual:   41

The following tests FAILED:
	  2 - failing_case (Failed)
Errors while running CTest
Use "--rerun-failed --output-on-failure" to re-run the failed cases verbosely."#
        );
    }

    #[test]
    fn preserves_timeout_and_exception_failure_details() {
        let output = r#"Test project /tmp/build
    Start 1: passing_case
1/3 Test #1: passing_case .....................   Passed    0.01 sec
    Start 2: timeout_case
2/3 Test #2: timeout_case .....................***Timeout   1.00 sec
timeout diagnostics
    Start 3: segfault_case
3/3 Test #3: segfault_case ....................***Exception: SegFault  0.02 sec
fatal signal details

33% tests passed, 2 tests failed out of 3

Total Test time (real) =   1.03 sec

The following tests FAILED:
	  2 - timeout_case (Timeout)
	  3 - segfault_case (SEGFAULT)
Errors while running CTest
"#;

        let filtered = filter_ctest_output(output);

        assert_eq!(
            filtered,
            r#"ctest: 1/3 passed, 2 failed (1.03 sec)
failed:
  timeout_case (Timeout, 1.00 sec)
  segfault_case (Exception: SegFault, 0.02 sec)

timeout diagnostics

fatal signal details

The following tests FAILED:
	  2 - timeout_case (Timeout)
	  3 - segfault_case (SEGFAULT)
Errors while running CTest"#
        );
    }

    #[test]
    fn separates_disabled_tests_and_preserves_pre_result_diagnostics() {
        let output = r#"Test project /tmp/build
    Start 1: passing_case
1/4 Test #1: passing_case .....................   Passed    0.00 sec
    Start 2: disabled_case
2/4 Test #2: disabled_case ....................***Not Run (Disabled)   0.00 sec
    Start 3: missing_case
Could not find executable missing-command
Looked in: Debug/missing-command
3/4 Test #3: missing_case .....................***Not Run   0.00 sec
    Start 4: timeout_case
4/4 Test #4: timeout_case .....................***Timeout   0.14 sec

33% tests passed, 2 tests failed out of 3

Total Test time (real) =   0.15 sec

The following tests did not run:
	  2 - disabled_case (Disabled)

The following tests FAILED:
	  3 - missing_case (Not Run)
	  4 - timeout_case (Timeout)
Unable to find executable: missing-command
Errors while running CTest
"#;

        let filtered = filter_ctest_output(output);

        assert_eq!(
            filtered,
            r#"ctest: 1/3 passed, 2 failed, 1 disabled (0.15 sec)
failed:
  missing_case (Not Run, 0.00 sec)
  timeout_case (Timeout, 0.14 sec)

Could not find executable missing-command
Looked in: Debug/missing-command

The following tests FAILED:
	  3 - missing_case (Not Run)
	  4 - timeout_case (Timeout)
Unable to find executable: missing-command
Errors while running CTest"#
        );
    }

    #[test]
    fn summarizes_disabled_tests_without_counting_them_as_passed() {
        let output = r#"Test project /tmp/build
    Start 1: passing_case
1/2 Test #1: passing_case .....................   Passed    0.01 sec
    Start 2: disabled_case
2/2 Test #2: disabled_case ....................***Not Run (Disabled)   0.00 sec

100% tests passed, 0 tests failed out of 1

Total Test time (real) =   0.01 sec
"#;

        assert_eq!(
            filter_ctest_output(output),
            "ctest: 1/1 passed, 1 disabled (0.01 sec)\nslowest:\n  passing_case 0.01 sec"
        );
    }

    #[test]
    fn passes_unknown_output_through() {
        let output = "ctest custom output\nwith no recognizable summary\n";
        assert_eq!(filter_ctest_output(output), output.trim());
    }

    #[test]
    fn verbose_flags_passthrough() {
        assert!(should_passthrough(&["-V".to_string()]));
        assert!(should_passthrough(&["--show-only=json-v1".to_string()]));
        assert!(should_passthrough(&["--help-command".to_string()]));
        assert!(should_passthrough(&["/?".to_string()]));
        assert!(!should_passthrough(&["--output-on-failure".to_string()]));
    }
}
