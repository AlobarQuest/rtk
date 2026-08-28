//! Compact CTest output while preserving failing-test details.

use anyhow::Result;
use regex::Regex;
use std::cmp::Ordering;
use std::ffi::OsString;
use std::sync::LazyLock;

use crate::core::runner::{self, RunOptions};
use crate::core::truncate::{CAP_LIST, CAP_WARNINGS};
use crate::core::utils::{resolved_command, strip_ansi};

const MAX_SLOWEST: usize = 3;
const MAX_RESULT_CONTINUATION_LINES: usize = 8;
const MAX_FAILURE_LINES: usize = CAP_WARNINGS;
const MAX_FAILED_LIST_LINES: usize = CAP_LIST;

static TEST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?s)^\s*\d+/\d+\s+Test\s+#\d+:\s+(.+?)\s+\.{2,}\s*(?:\*{3})?\s*(.+?)\s+([\d.]+)\s+sec\s*$",
    )
    .expect("invalid ctest result regex")
});
static RESULT_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*\d+/\d+\s+Test\s+#\d+:").expect("invalid ctest result prefix regex")
});
static RESULT_TERMINATOR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[\d.]+\s+sec\s*$").expect("invalid ctest result terminator regex")
});
static START_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*Start\s+\d+:").expect("invalid ctest start regex"));
static SUMMARY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*\d+%\s+tests passed,\s+(\d+)\s+tests failed out of\s+(\d+)")
        .expect("invalid ctest summary regex")
});
static TIME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*Total Test time \(real\)\s+=\s+([\d.]+)\s+sec")
        .expect("invalid ctest time regex")
});

#[derive(Debug, Clone)]
struct TestCase {
    name: String,
    status: String,
    reason: Option<String>,
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
    let clean = strip_ansi(output);
    let logical_lines = build_logical_lines(&clean);
    let mut lines = logical_lines
        .iter()
        .map(String::as_str)
        .filter(|line| !line.trim().is_empty());
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
        && lines.any(|line| is_no_tests_line(line) || parse_test_line(line, 0).is_some())
}

pub(crate) fn filter_ctest_output(output: &str) -> String {
    let clean = strip_ansi(output);
    let trimmed = clean.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let lines = build_logical_lines(&clean);
    let tests = parse_tests(&lines);
    let summary = lines.iter().find_map(|line| parse_summary(line));
    let total_time = lines.iter().find_map(|line| parse_total_time(line));

    if tests.is_empty() && summary.is_none() {
        if lines.iter().any(|line| is_no_tests_line(line)) {
            return "ctest: no tests found".to_string();
        }
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

    fn is_skipped(&self) -> bool {
        self.status.eq_ignore_ascii_case("skipped")
    }

    fn is_failure(&self) -> bool {
        !self.is_passed() && !self.is_disabled() && !self.is_skipped()
    }
}

fn build_logical_lines(output: &str) -> Vec<String> {
    let physical_lines: Vec<&str> = output.lines().collect();
    let mut logical_lines = Vec::with_capacity(physical_lines.len());
    let mut index = 0;

    while index < physical_lines.len() {
        let line = physical_lines[index];
        if !RESULT_PREFIX_RE.is_match(line) || RESULT_TERMINATOR_RE.is_match(line) {
            logical_lines.push(line.to_string());
            index += 1;
            continue;
        }

        let mut result_end = None;
        for continuation in 1..=MAX_RESULT_CONTINUATION_LINES {
            let continuation_index = index + continuation;
            let Some(continuation_line) = physical_lines.get(continuation_index) else {
                break;
            };
            if START_RE.is_match(continuation_line)
                || RESULT_PREFIX_RE.is_match(continuation_line)
                || parse_summary(continuation_line).is_some()
            {
                break;
            }
            if RESULT_TERMINATOR_RE.is_match(continuation_line) {
                result_end = Some(continuation_index);
                break;
            }
        }

        if let Some(end) = result_end {
            logical_lines.push(physical_lines[index..=end].join("\n"));
            index = end + 1;
        } else {
            logical_lines.push(line.to_string());
            index += 1;
        }
    }

    logical_lines
}

fn is_no_tests_line(line: &str) -> bool {
    line.trim() == "No tests were found!!!"
}

fn parse_tests(lines: &[String]) -> Vec<TestCase> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(line_index, line)| parse_test_line(line, line_index))
        .collect()
}

fn parse_test_line(line: &str, line_index: usize) -> Option<TestCase> {
    let caps = TEST_RE.captures(line.trim_end())?;
    let (status, reason) = split_status_reason(caps.get(2)?.as_str());
    Some(TestCase {
        name: caps.get(1)?.as_str().trim().to_string(),
        status,
        reason,
        duration: caps.get(3)?.as_str().parse().ok()?,
        line_index,
    })
}

/// CTest prints `<status>  <reason>` with two spaces between them; the status
/// itself only ever contains single spaces (`Not Run (Disabled)`, `Exception: SegFault`).
/// A regex-list reason spans physical lines (`Regex=[a\n]`), so its whitespace is
/// collapsed and the newline before the closing bracket dropped.
fn split_status_reason(raw: &str) -> (String, Option<String>) {
    let raw = raw.trim();
    let Some((status, reason)) = raw.split_once("  ") else {
        return (raw.to_string(), None);
    };

    let reason = reason
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(" ]", "]");

    (
        status.trim().to_string(),
        (!reason.is_empty()).then_some(reason),
    )
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

fn format_success(
    tests: &[TestCase],
    summary: Option<CtestSummary>,
    total_time: Option<f64>,
) -> String {
    let total = summary.map_or(tests.len(), |s| s.total);
    let skipped = tests.iter().filter(|test| test.is_skipped()).count();
    let passed = summary.map_or_else(
        || tests.iter().filter(|test| test.is_passed()).count(),
        |s| s.total.saturating_sub(s.failed).saturating_sub(skipped),
    );
    let disabled = tests.iter().filter(|test| test.is_disabled()).count();
    let mut out = format!("ctest: {passed}/{total} passed");
    if skipped > 0 {
        out.push_str(&format!(", {skipped} skipped"));
    }
    if disabled > 0 {
        out.push_str(&format!(", {disabled} disabled"));
    }
    out.push_str(&format_meta(total_time));
    append_skipped_list(&mut out, tests);

    let slowest = slowest_tests(tests);
    if !slowest.is_empty() {
        out.push_str("\nslowest:");
        for test in slowest {
            out.push_str(&format!(
                "\n  {} {}",
                test.name,
                format_seconds(test.duration)
            ));
        }
    }

    out
}

fn format_failure(
    lines: &[String],
    tests: &[TestCase],
    summary: Option<CtestSummary>,
    total_time: Option<f64>,
) -> String {
    let failed_tests: Vec<&TestCase> = tests.iter().filter(|test| test.is_failure()).collect();
    let failed = summary.map_or(failed_tests.len(), |s| s.failed);
    let total = summary.map_or(tests.len(), |s| s.total);
    let skipped = tests.iter().filter(|test| test.is_skipped()).count();
    let disabled = tests.iter().filter(|test| test.is_disabled()).count();
    let passed = summary.map_or_else(
        || tests.iter().filter(|test| test.is_passed()).count(),
        |s| s.total.saturating_sub(s.failed).saturating_sub(skipped),
    );

    let mut out = format!("ctest: {passed}/{total} passed, {failed} failed");
    if skipped > 0 {
        out.push_str(&format!(", {skipped} skipped"));
    }
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
    append_skipped_list(&mut out, tests);

    let details = failure_details(lines, &failed_tests);
    if !details.is_empty() {
        out.push_str("\n\n");
        out.push_str(&details.join("\n\n"));
    }

    out
}

fn append_skipped_list(out: &mut String, tests: &[TestCase]) {
    let skipped_tests: Vec<&TestCase> = tests.iter().filter(|test| test.is_skipped()).collect();
    if skipped_tests.is_empty() {
        return;
    }

    out.push_str("\nskipped:");
    for test in skipped_tests {
        out.push_str(&format!("\n  {}", test.name));
    }
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

fn failure_details(lines: &[String], failed_tests: &[&TestCase]) -> Vec<String> {
    let mut blocks: Vec<String> = failed_tests
        .iter()
        .filter_map(|test| collect_failure_block(lines, test))
        .collect();

    if let Some(section) = collect_failed_section(lines) {
        blocks.push(section);
    }

    blocks
}

fn collect_failure_block(lines: &[String], test: &TestCase) -> Option<String> {
    let mut block = Vec::new();
    let result_index = test.line_index;
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
        let line = &lines[index];
        if is_ctest_boundary(line) {
            break;
        }
        block.push(line.trim_end().to_string());
        index += 1;
    }

    trim_blank_edges(&mut block);
    let mut rendered = Vec::new();
    if let Some(reason) = &test.reason {
        rendered.push(reason.clone());
    }

    if block.len() > MAX_FAILURE_LINES {
        let hidden = block.len() - MAX_FAILURE_LINES;
        let full_block = block.join("\n");
        rendered.push(format!("... +{hidden} more lines"));
        rendered.extend(block[block.len() - MAX_FAILURE_LINES..].iter().cloned());
        if let Some(hint) = crate::core::tee::force_tee_hint(&full_block, "ctest-failure") {
            rendered.push(hint);
        }
    } else {
        rendered.extend(block);
    }

    (!rendered.is_empty()).then(|| rendered.join("\n"))
}

fn collect_failed_section(lines: &[String]) -> Option<String> {
    let start = lines
        .iter()
        .position(|line| line.trim() == "The following tests FAILED:")?;
    let mut block: Vec<String> = lines[start..]
        .iter()
        .map(|line| line.trim_end().to_string())
        .collect();

    trim_blank_edges(&mut block);
    let entry_count = block
        .iter()
        .skip(1)
        .take_while(|line| line.starts_with('\t'))
        .count();
    if entry_count <= MAX_FAILED_LIST_LINES {
        return (!block.is_empty()).then(|| block.join("\n"));
    }

    let entries = &block[1..1 + entry_count];
    let hidden = entry_count - MAX_FAILED_LIST_LINES;
    let mut rendered = vec![block[0].clone()];
    rendered.extend(entries.iter().take(MAX_FAILED_LIST_LINES).cloned());
    rendered.push(format!("... +{hidden} more lines"));
    let all_entries = entries.join("\n");
    if let Some(hint) = crate::core::tee::force_tee_tail_hint(
        &all_entries,
        "ctest-failed",
        MAX_FAILED_LIST_LINES + 1,
    ) {
        rendered.push(hint);
    }
    rendered.extend(block[1 + entry_count..].iter().cloned());

    Some(rendered.join("\n"))
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

    fn count_tokens(s: &str) -> usize {
        s.split_whitespace().count()
    }

    fn savings_pct(input: &str, output: &str) -> f64 {
        100.0 - (count_tokens(output) as f64 / count_tokens(input) as f64 * 100.0)
    }

    /// Tee hints carry a per-run file path, so truncation tests compare
    /// everything except those lines.
    fn without_tee_hints(output: &str) -> String {
        output
            .lines()
            .filter(|line| {
                !line.starts_with("[full output:") && !line.starts_with("[see remaining:")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn recognizes_only_the_exact_no_tests_line() {
        let no_tests = "Test project /tmp/build\nNo tests were found!!!\n";
        let diagnostic = "Test project /tmp/build\nERROR: No tests were found in setup\n";

        assert!(looks_like_ctest_output(no_tests));
        assert!(!looks_like_ctest_output(diagnostic));
        assert_eq!(filter_ctest_output(no_tests), "ctest: no tests found");
        assert_eq!(filter_ctest_output(diagnostic), diagnostic.trim());
    }

    #[test]
    fn parses_wrapped_regex_failure_fixture() {
        let input =
            include_str!("../../../tests/fixtures/ctest_regex_fail_output_on_failure_raw.txt");

        assert_eq!(
            filter_ctest_output(input),
            r#"ctest: 1/2 passed, 1 failed (0.01 sec)
failed:
  regex_fail (Failed, 0.00 sec)

Required regular expression not found. Regex=[expected-token]
nope

The following tests FAILED:
	  4 - regex_fail (Failed)
Errors while running CTest"#
        );
    }

    #[test]
    fn separates_skipped_tests_in_green_fixture() {
        let input = include_str!("../../../tests/fixtures/ctest_green_skipped_raw.txt");

        assert_eq!(
            filter_ctest_output(input),
            r#"ctest: 3/4 passed, 1 skipped, 1 disabled (0.45 sec)
skipped:
  skipped_case
slowest:
  pass_slow 0.31 sec
  pass_medium 0.12 sec
  pass_fast 0.00 sec"#
        );
    }

    #[test]
    fn keeps_failure_output_that_mentions_no_tests_found() {
        let input = include_str!(
            "../../../tests/fixtures/ctest_discovery_fail_output_on_failure_raw.txt"
        );

        assert_eq!(
            filter_ctest_output(input),
            r#"ctest: 1/2 passed, 1 failed (0.01 sec)
failed:
  discovery_fail (Failed, 0.00 sec)

ERROR: No tests were found in the discovery phase
assertion failed at bar.cpp:3

The following tests FAILED:
	 11 - discovery_fail (Failed)
Errors while running CTest"#
        );
    }

    #[test]
    fn tail_caps_noisy_failure_fixture() {
        let input =
            include_str!("../../../tests/fixtures/ctest_noisy_fail_output_on_failure_raw.txt");

        assert_eq!(
            without_tee_hints(&filter_ctest_output(input)),
            r#"ctest: 1/2 passed, 1 failed (0.01 sec)
failed:
  noisy_fail (Failed, 0.00 sec)

... +110 more lines
noise line 111
noise line 112
noise line 113
noise line 114
noise line 115
noise line 116
noise line 117
noise line 118
noise line 119
noise line 120

The following tests FAILED:
	 10 - noisy_fail (Failed)
Errors while running CTest"#
        );
    }

    #[test]
    fn filters_mixed_fixture() {
        let input = include_str!("../../../tests/fixtures/ctest_mixed_raw.txt");

        assert_eq!(
            without_tee_hints(&filter_ctest_output(input)),
            r#"ctest: 3/10 passed, 6 failed, 1 skipped, 1 disabled (1.55 sec)
failed:
  regex_fail (Failed, 0.02 sec)
  missing_case (Not Run, 0.00 sec)
  timeout_case (Timeout, 1.06 sec)
  plain_fail (Failed, 0.00 sec)
  noisy_fail (Failed, 0.00 sec)
  discovery_fail (Failed, 0.00 sec)
skipped:
  skipped_case

Required regular expression not found. Regex=[expected-token]

... +7 more lines
Debug/missing-command
MinSizeRel/missing-command
MinSizeRel/missing-command
RelWithDebInfo/missing-command
RelWithDebInfo/missing-command
Deployment/missing-command
Deployment/missing-command
Development/missing-command
Development/missing-command
Unable to find executable: missing-command

The following tests FAILED:
	  4 - regex_fail (Failed)
	  7 - missing_case (Not Run)
	  8 - timeout_case (Timeout)
	  9 - plain_fail (Failed)
	 10 - noisy_fail (Failed)
	 11 - discovery_fail (Failed)
Errors while running CTest
Output from these tests are in: /tmp/build/Testing/Temporary/LastTest.log
Use "--rerun-failed --output-on-failure" to re-run the failed cases verbosely."#
        );
    }

    #[test]
    fn noisy_fixture_saves_at_least_sixty_percent() {
        let input =
            include_str!("../../../tests/fixtures/ctest_noisy_fail_output_on_failure_raw.txt");
        let savings = savings_pct(input, &filter_ctest_output(input));

        assert!(
            savings >= 60.0,
            "ctest noisy failure: expected >=60% savings, got {savings:.1}%"
        );
    }

    #[test]
    fn green_skipped_fixture_saves_at_least_sixty_percent() {
        let input = include_str!("../../../tests/fixtures/ctest_green_skipped_raw.txt");
        let savings = savings_pct(input, &filter_ctest_output(input));

        assert!(
            savings >= 60.0,
            "ctest green skipped: expected >=60% savings, got {savings:.1}%"
        );
    }

    #[test]
    fn leaves_unterminated_wrapped_result_unjoined_and_parses_following_test() {
        let mut output = String::from(
            "Test project /tmp/build\n    Start 1: malformed_case\n1/2 Test #1: malformed_case ...................***Failed  wrapped reason\n",
        );
        for index in 1..=9 {
            output.push_str(&format!("continuation {index}\n"));
        }
        output.push_str(
            "    Start 2: following_case\n2/2 Test #2: following_case ...................   Passed    0.01 sec\n\n50% tests passed, 1 tests failed out of 2\n\nTotal Test time (real) =   0.01 sec\n",
        );

        let tests = parse_tests(&build_logical_lines(&output));

        assert_eq!(tests.len(), 1);
        assert_eq!(tests[0].name, "following_case");
        assert_eq!(
            filter_ctest_output(&output),
            "ctest: 1/2 passed, 1 failed (0.01 sec)"
        );
    }
}
