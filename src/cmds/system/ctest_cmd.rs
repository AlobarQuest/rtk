//! Compact CTest output while preserving failing-test details.

use anyhow::Result;
use regex::Regex;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::ffi::OsString;
use std::sync::LazyLock;

use crate::core::runner::{self, RunOptions};
use crate::core::truncate::{self, CAP_LIST, CAP_WARNINGS};
use crate::core::utils::{resolved_command, strip_ansi};

const MAX_SLOWEST: usize = 3;
const MAX_RESULT_CONTINUATION_LINES: usize = 8;
const MAX_FAILURE_LINES: usize = CAP_WARNINGS;
const MAX_FAILURE_HEAD_LINES: usize = 2;
const MAX_FAILED_LIST_LINES: usize = CAP_LIST;

static TEST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?s)^\s*(?:\d+/\d+\s+)?Test\s+#(\d+):\s+(.+?)\s+\.{2,}\s*(?:\*{3})?\s*(.+?)\s+([\d.]+)\s+sec\s*$",
    )
    .expect("invalid ctest result regex")
});
static RESULT_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:\d+/\d+\s+)?Test\s+#\d+:")
        .expect("invalid ctest result prefix regex")
});
static RESULT_TERMINATOR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[\d.]+\s+sec\s*$").expect("invalid ctest result terminator regex")
});
static START_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*Start\s+(\d+):\s*(.*?)\s*$").expect("invalid ctest start regex")
});
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
    number: u32,
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
    let framing_lines = collect_framing_line_indices(&lines, &tests);
    let summary = lines.iter().rev().find_map(|line| parse_summary(line));
    let total_time = lines
        .iter()
        .rev()
        .find_map(|line| parse_total_time(line));

    if tests.is_empty() && summary.is_none() {
        if lines.iter().any(|line| is_no_tests_line(line)) {
            return "ctest: no tests found".to_string();
        }
        return trimmed.to_string();
    }

    let has_failures = summary.map_or_else(
        || tests.iter().any(TestCase::is_failure),
        |summary| summary.failed > 0,
    );
    if has_failures {
        return format_failure(&lines, &tests, &framing_lines, summary, total_time);
    }

    format_success(&tests, summary, total_time)
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
    let mut tests: Vec<TestCase> = Vec::new();
    for (line_index, line) in lines.iter().enumerate() {
        let Some(test) = parse_test_line(line, line_index) else {
            continue;
        };
        if let Some(existing) = tests
            .iter_mut()
            .find(|existing| existing.number == test.number)
        {
            *existing = test;
        } else {
            tests.push(test);
        }
    }
    tests
}

fn parse_test_line(line: &str, line_index: usize) -> Option<TestCase> {
    let caps = TEST_RE.captures(line.trim_end())?;
    let (status, reason) = split_status_reason(caps.get(3)?.as_str());
    Some(TestCase {
        number: caps.get(1)?.as_str().parse().ok()?,
        name: caps.get(2)?.as_str().trim().to_string(),
        status,
        reason,
        duration: caps.get(4)?.as_str().parse().ok()?,
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

/// Line indices that are genuine CTest framing. A test may print lines shaped
/// like `Start N:` or the summary, so `Start` lines are validated against the
/// parsed test set (number and name must both match) and the trailer lines are
/// honoured only at their last occurrence, where ctest prints them.
fn collect_framing_line_indices(lines: &[String], tests: &[TestCase]) -> HashSet<usize> {
    let mut framing_lines: HashSet<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| parse_test_line(line, index).map(|_| index))
        .collect();

    framing_lines.extend(lines.iter().enumerate().filter_map(|(index, line)| {
        let captures = START_RE.captures(line)?;
        let number = captures.get(1)?.as_str().parse::<u32>().ok()?;
        let name = captures.get(2)?.as_str().trim();
        tests
            .iter()
            .any(|test| test.number == number && test.name == name)
            .then_some(index)
    }));

    for pattern in [&*SUMMARY_RE, &*TIME_RE] {
        if let Some(index) = lines.iter().rposition(|line| pattern.is_match(line)) {
            framing_lines.insert(index);
        }
    }
    if let Some(index) = lines
        .iter()
        .rposition(|line| line.trim() == "The following tests FAILED:")
    {
        framing_lines.insert(index);
    }

    framing_lines
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
    framing_lines: &HashSet<usize>,
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
        out.push('\n');
        out.push_str(&format_failed_section(
            lines,
            &failed_tests,
            framing_lines,
        ));
    }
    append_skipped_list(&mut out, tests);

    let trailer = collect_failed_trailer(lines, framing_lines);
    if !trailer.is_empty() {
        out.push_str("\n\n");
        out.push_str(&trailer.join("\n"));
    }

    out
}

fn append_skipped_list(out: &mut String, tests: &[TestCase]) {
    let skipped_names: Vec<&str> = tests
        .iter()
        .filter(|test| test.is_skipped())
        .map(|test| test.name.as_str())
        .collect();
    if skipped_names.is_empty() {
        return;
    }

    out.push_str("\nskipped:");
    for name in skipped_names.iter().take(MAX_FAILED_LIST_LINES) {
        out.push_str(&format!("\n  {name}"));
    }
    if skipped_names.len() > MAX_FAILED_LIST_LINES {
        let hidden = skipped_names.len() - MAX_FAILED_LIST_LINES;
        out.push_str(&format!("\n  ... +{hidden} more skipped"));
        let all_names = skipped_names.join("\n");
        if let Some(hint) = crate::core::tee::force_tee_tail_hint(
            &all_names,
            "ctest-skipped",
            MAX_FAILED_LIST_LINES + 1,
        ) {
            out.push_str(&format!("\n  {hint}"));
        }
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

fn format_failed_section(
    lines: &[String],
    failed_tests: &[&TestCase],
    framing_lines: &HashSet<usize>,
) -> String {
    let section = build_failed_section(lines, failed_tests, framing_lines);
    let mut rendered = section.rendered;
    if section.truncated {
        if let Some(hint) = crate::core::tee::force_tee_hint(&section.full, "ctest-failed") {
            rendered.push_str(&format!("\n  {hint}"));
        }
    }
    rendered
}

/// The `failed:` section as shown, the untruncated text behind it, and whether
/// anything was hidden (so one section-level tee is owed).
struct FailedSection {
    rendered: String,
    full: String,
    truncated: bool,
}

/// Every block is collected untruncated before the list cap so a single tee
/// can hold the whole section; rendering then caps entries at
/// `MAX_FAILED_LIST_LINES` and each block at head+tail.
fn build_failed_section(
    lines: &[String],
    failed_tests: &[&TestCase],
    framing_lines: &HashSet<usize>,
) -> FailedSection {
    let blocks: Vec<Vec<String>> = failed_tests
        .iter()
        .map(|test| collect_failure_block(lines, test, framing_lines))
        .collect();
    let any_block_truncated = blocks.iter().any(|block| block.len() > MAX_FAILURE_LINES);
    let list_capped = failed_tests.len() > MAX_FAILED_LIST_LINES;

    let mut full = String::from("failed:");
    for (test, block) in failed_tests.iter().zip(&blocks) {
        full.push('\n');
        full.push_str(&format_failed_entry(test, block));
    }

    let mut rendered = String::from("failed:");
    for (test, block) in failed_tests
        .iter()
        .zip(&blocks)
        .take(MAX_FAILED_LIST_LINES)
    {
        rendered.push('\n');
        rendered.push_str(&format_failed_entry(test, &render_failure_block(block)));
    }
    if list_capped {
        let hidden = failed_tests.len() - MAX_FAILED_LIST_LINES;
        rendered.push_str(&format!("\n  ... +{hidden} more failed"));
    }

    FailedSection {
        rendered,
        full,
        truncated: any_block_truncated || list_capped,
    }
}

fn format_failed_entry(test: &TestCase, details: &[String]) -> String {
    let mut entry = format!(
        "  #{} {} ({}, {})",
        test.number,
        test.name,
        test.status,
        format_seconds(test.duration)
    );
    if let Some(reason) = &test.reason {
        entry.push_str("\n    ");
        entry.push_str(reason);
    }
    for detail in details {
        entry.push_str("\n    ");
        entry.push_str(detail);
    }
    entry
}

fn render_failure_block(block: &[String]) -> Vec<String> {
    if block.len() <= MAX_FAILURE_LINES {
        return block.to_vec();
    }

    let hidden = block.len() - MAX_FAILURE_LINES;
    // Preserve setup context at the head and assertion evidence at the tail.
    let tail_lines = truncate::reduced(MAX_FAILURE_LINES, MAX_FAILURE_HEAD_LINES);
    let mut rendered = Vec::with_capacity(MAX_FAILURE_LINES + 1);
    rendered.extend(block.iter().take(MAX_FAILURE_HEAD_LINES).cloned());
    rendered.push(format!("... +{hidden} more lines"));
    rendered.extend(block[block.len() - tail_lines..].iter().cloned());
    rendered
}

fn collect_failure_block(
    lines: &[String],
    test: &TestCase,
    framing_lines: &HashSet<usize>,
) -> Vec<String> {
    let mut block = Vec::new();
    let result_index = test.line_index;
    let boundary = (0..result_index)
        .rev()
        .find(|index| framing_lines.contains(index));

    // Under -j, lines following another result belong to that completed test.
    if let Some(start) = boundary.filter(|index| START_RE.is_match(&lines[*index])) {
        block.extend(
            lines[start + 1..result_index]
                .iter()
                .map(|line| line.trim_end().to_string()),
        );
    }

    let mut index = result_index + 1;
    while index < lines.len() {
        if framing_lines.contains(&index) {
            break;
        }
        block.push(lines[index].trim_end().to_string());
        index += 1;
    }

    trim_blank_edges(&mut block);
    block
}

fn collect_failed_trailer(lines: &[String], framing_lines: &HashSet<usize>) -> Vec<String> {
    let Some(start) = (0..lines.len()).rev().find(|index| {
        framing_lines.contains(index) && lines[*index].trim() == "The following tests FAILED:"
    }) else {
        return Vec::new();
    };
    let mut trailer: Vec<String> = lines[start + 1..]
        .iter()
        .filter(|line| !line.starts_with('\t'))
        .map(|line| line.trim_end().to_string())
        .collect();
    trim_blank_edges(&mut trailer);
    trailer
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
  #2 failing_case (Failed, 0.02 sec)
    expected: 42
    actual:   41

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
  #2 timeout_case (Timeout, 1.00 sec)
    timeout diagnostics
  #3 segfault_case (Exception: SegFault, 0.02 sec)
    fatal signal details

Errors while running CTest"#
        );
        assert!(!filtered.contains("[full output:"));
        let (_, _, truncated) = failed_section_parts(output);
        assert!(!truncated);
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
  #3 missing_case (Not Run, 0.00 sec)
    Could not find executable missing-command
    Looked in: Debug/missing-command
  #4 timeout_case (Timeout, 0.14 sec)

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
                let trimmed = line.trim_start();
                !trimmed.starts_with("[full output:")
                    && !trimmed.starts_with("[see remaining:")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn failed_section_parts(input: &str) -> (String, String, bool) {
        let lines = build_logical_lines(input);
        let tests = parse_tests(&lines);
        let framing_lines = collect_framing_line_indices(&lines, &tests);
        let failed_tests: Vec<&TestCase> =
            tests.iter().filter(|test| test.is_failure()).collect();
        let section = build_failed_section(&lines, &failed_tests, &framing_lines);
        (section.rendered, section.full, section.truncated)
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
  #4 regex_fail (Failed, 0.00 sec)
    Required regular expression not found. Regex=[expected-token]
    nope

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
    fn parses_repeat_until_pass_fixture() {
        let input = include_str!("../../../tests/fixtures/ctest_repeat_until_pass_raw.txt");

        assert_eq!(
            filter_ctest_output(input),
            r#"ctest: 2/2 passed (0.01 sec)
slowest:
  flaky_case 0.00 sec
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
  #11 discovery_fail (Failed, 0.00 sec)
    ERROR: No tests were found in the discovery phase
    assertion failed at bar.cpp:3

Errors while running CTest"#
        );
    }

    #[test]
    fn rejects_spoofed_framing_and_uses_the_final_summary() {
        let input = include_str!("../../../tests/fixtures/ctest_spoofed_framing_raw.txt");
        let filtered = filter_ctest_output(input);

        assert_eq!(
            filtered,
            r#"ctest: 1/3 passed, 2 failed (0.01 sec)
failed:
  #1 nested_out (Failed, 0.00 sec)
    setup ok
        Start 3: inner phase
    ASSERT FAILED: values differ
    expected 1 got 2
  #3 summary_spoof (Failed, 0.00 sec)
    running inner suite
    100% tests passed, 0 tests failed out of 1
    Total Test time (real) =   0.01 sec
    FAIL: outer check broke

Errors while running CTest"#
        );
        assert!(!filtered.contains("... +"));
        assert!(!filtered.contains("[full output:"));
    }

    #[test]
    fn parallel_start_cluster_rejects_wrong_name_spoof() {
        let output = r#"Test project /tmp/build
    Start 1: failing_case
    Start 2: passing_case
1/2 Test #1: failing_case .....................***Failed    0.01 sec
setup complete
    Start 2: inner phase
assertion failed after inner phase
2/2 Test #2: passing_case .....................   Passed    0.01 sec

50% tests passed, 1 tests failed out of 2

Total Test time (real) =   0.02 sec
"#;

        let filtered = filter_ctest_output(output);

        assert_eq!(
            filtered,
            r#"ctest: 1/2 passed, 1 failed (0.02 sec)
failed:
  #1 failing_case (Failed, 0.01 sec)
    setup complete
        Start 2: inner phase
    assertion failed after inner phase"#
        );
    }

    #[test]
    fn caps_noisy_failure_output_head_and_tail() {
        let input =
            include_str!("../../../tests/fixtures/ctest_noisy_fail_output_on_failure_raw.txt");
        let filtered = filter_ctest_output(input);

        assert_eq!(
            without_tee_hints(&filtered),
            r#"ctest: 1/2 passed, 1 failed (0.01 sec)
failed:
  #10 noisy_fail (Failed, 0.01 sec)
    noise line 1
    noise line 2
    ... +110 more lines
    noise line 113
    noise line 114
    noise line 115
    noise line 116
    noise line 117
    noise line 118
    noise line 119
    noise line 120

Errors while running CTest"#
        );
        assert_eq!(
            filtered
                .lines()
                .filter(|line| line.trim_start().starts_with("[full output:"))
                .count(),
            1
        );

        let (_, full_section, truncated) = failed_section_parts(input);
        assert!(truncated);
        assert_eq!(
            full_section
                .lines()
                .filter(|line| line.trim_start().starts_with("noise line "))
                .count(),
            120
        );
        assert!(!full_section.contains("... +"));
    }

    #[test]
    fn filters_mixed_fixture() {
        let input = include_str!("../../../tests/fixtures/ctest_mixed_raw.txt");

        assert_eq!(
            without_tee_hints(&filter_ctest_output(input)),
            r#"ctest: 3/10 passed, 6 failed, 1 skipped, 1 disabled (1.52 sec)
failed:
  #4 regex_fail (Failed, 0.01 sec)
    Required regular expression not found. Regex=[expected-token]
  #7 missing_case (Not Run, 0.00 sec)
    Could not find executable missing-command
    Looked in the following places:
    ... +6 more lines
    MinSizeRel/missing-command
    MinSizeRel/missing-command
    RelWithDebInfo/missing-command
    RelWithDebInfo/missing-command
    Deployment/missing-command
    Deployment/missing-command
    Development/missing-command
    Development/missing-command
  #8 timeout_case (Timeout, 1.06 sec)
  #9 plain_fail (Failed, 0.00 sec)
  #10 noisy_fail (Failed, 0.00 sec)
  #11 discovery_fail (Failed, 0.00 sec)
skipped:
  skipped_case

Unable to find executable: missing-command
Errors while running CTest
Output from these tests are in: /tmp/build/Testing/Temporary/LastTest.log
Use "--rerun-failed --output-on-failure" to re-run the failed cases verbosely."#
        );
    }

    #[test]
    fn filters_parallel_fixture_without_cross_test_attribution() {
        let input =
            include_str!("../../../tests/fixtures/ctest_parallel_output_on_failure_raw.txt");

        assert_eq!(
            without_tee_hints(&filter_ctest_output(input)),
            r#"ctest: 3/10 passed, 6 failed, 1 skipped, 1 disabled (1.09 sec)
failed:
  #10 discovery_fail (Failed, 0.00 sec)
    ERROR: No tests were found in the discovery phase
    assertion failed at bar.cpp:3
  #11 flaky_case (Failed, 0.01 sec)
    first try
  #4 regex_fail (Failed, 0.00 sec)
    Required regular expression not found. Regex=[expected-token]
    nope
  #7 missing_case (Not Run, 0.00 sec)
    Could not find executable missing-command
    Looked in the following places:
    ... +6 more lines
    MinSizeRel/missing-command
    MinSizeRel/missing-command
    RelWithDebInfo/missing-command
    RelWithDebInfo/missing-command
    Deployment/missing-command
    Deployment/missing-command
    Development/missing-command
    Development/missing-command
  #9 plain_fail (Failed, 0.00 sec)
    assertion failed at foo.cpp:12
  #8 timeout_case (Timeout, 1.08 sec)
skipped:
  skipped_case

Unable to find executable: missing-command
Errors while running CTest"#
        );
    }

    #[test]
    fn caps_failed_test_entries_with_one_complete_section_tee() {
        let mut input = String::from("Test project /tmp/build\n");
        for number in 1..=25 {
            input.push_str(&format!(
                "    Start {number}: failing_{number}\n{number}/25 Test #{number}: failing_{number} ................***Failed    0.00 sec\n"
            ));
            for detail in 1..=30 {
                input.push_str(&format!("detail {number}.{detail}\n"));
            }
        }
        input.push_str(
            "\n0% tests passed, 25 tests failed out of 25\n\nTotal Test time (real) =   0.01 sec\n",
        );

        let filtered = filter_ctest_output(&input);
        let (rendered_section, full_section, truncated) = failed_section_parts(&input);
        assert!(truncated);

        assert_eq!(
            filtered
                .lines()
                .filter(|line| line.starts_with("  #"))
                .count(),
            MAX_FAILED_LIST_LINES
        );
        assert!(filtered.contains("  ... +5 more failed"));
        assert_eq!(
            filtered
                .lines()
                .filter(|line| line.trim_start().starts_with("[full output:"))
                .count(),
            1
        );
        // The hint is appended by the tee step, never by the pure builder.
        assert!(rendered_section.contains("  ... +5 more failed"));
        assert!(!rendered_section.contains("[full output:"));
        assert_eq!(
            full_section
                .lines()
                .filter(|line| line.starts_with("  #"))
                .count(),
            25
        );
        assert_eq!(
            full_section
                .lines()
                .filter(|line| line.trim_start().starts_with("detail "))
                .count(),
            25 * 30
        );
        assert!(full_section.contains("detail 25.30"));
        assert!(!full_section.contains("... +"));
    }

    #[test]
    fn caps_skipped_test_entries_with_recovery_hint() {
        let mut input = String::from("Test project /tmp/build\n");
        for number in 1..=25 {
            input.push_str(&format!(
                "    Start {number}: skipped_{number}\n{number}/25 Test #{number}: skipped_{number} ................***Skipped   0.00 sec\n"
            ));
        }
        input.push_str(
            "\n100% tests passed, 0 tests failed out of 25\n\nTotal Test time (real) =   0.01 sec\n",
        );

        let filtered = filter_ctest_output(&input);

        assert_eq!(
            filtered
                .lines()
                .filter(|line| line.starts_with("  skipped_"))
                .count(),
            MAX_FAILED_LIST_LINES
        );
        assert!(filtered.contains("  ... +5 more skipped"));
        assert!(filtered.contains("  [see remaining:"));
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
    fn repeat_result_replaces_the_earlier_state_in_place() {
        let output = r#"1/1 Test #12: flaky_case .......................***Failed    0.00 sec
    Test #12: flaky_case .......................   Passed    0.00 sec
"#;

        let tests = parse_tests(&build_logical_lines(output));

        assert_eq!(tests.len(), 1);
        assert_eq!(tests[0].number, 12);
        assert_eq!(tests[0].status, "Passed");
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
