//! Apache Maven filter — Surefire/Failsafe block collapse, compile error/warning
//! dedup, package/install pipeline with mode-toggle.
//!
//! Replaces the previous `src/filters/mvn-build.toml` filter with a Rust module
//! capable of state-machine parsing (block collapse, continuation tracking,
//! mode toggle) that TOML DSL cannot express.

use crate::core::runner::{self, RunOptions};
use crate::core::utils::{resolved_command, strip_ansi};
use anyhow::Result;
use lazy_static::lazy_static;
use regex::Regex;
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

// ── Shared regex patterns ────────────────────────────────────────────────────

lazy_static! {
    /// `[INFO] Running com.example.app.FooTest`
    static ref RUNNING: Regex = Regex::new(r"^\[INFO\] Running ").unwrap();

    /// Surefire/Failsafe per-class close line. Captures `Failures` and `Errors`.
    /// Tolerates the optional `<<< FAILURE!` marker. Separator is `-` (Surefire 2.x)
    /// or `--` (Surefire 3.x). Prefix INFO/ERROR/WARNING (3.x emits WARNING for
    /// classes with only skipped tests).
    static ref CLOSE: Regex = Regex::new(
        r"^\[(?:INFO|ERROR|WARNING)\] Tests run: \d+, Failures: (\d+), Errors: (\d+), Skipped: \d+, Time elapsed: [^ ]+ s(?:\s+<<<\s*FAILURE!)?\s+--?\s+in (.+)$"
    ).unwrap();

    /// Final BUILD footer.
    static ref BUILD_FOOT: Regex = Regex::new(r"^\[(?:INFO|ERROR)\] BUILD (?:SUCCESS|FAILURE)$").unwrap();

    /// `[INFO] Results:` separator before the aggregate.
    static ref RESULTS: Regex = Regex::new(r"^\[INFO\] Results:\s*$").unwrap();

    /// Aggregate counts line (no `Time elapsed`, no ` - in `).
    static ref AGG: Regex = Regex::new(
        r"^\[(?:INFO|ERROR)\] Tests run: \d+, Failures: \d+, Errors: \d+, Skipped: \d+\s*$"
    ).unwrap();

    /// Plugin banner line: `[INFO] --- plugin:goal (id) @ module ---`.
    static ref PLUGIN_BANNER: Regex = Regex::new(r"^\[INFO\] --- .* @ .* ---$").unwrap();

    /// Module banner with project name in brackets.
    static ref MODULE_BANNER: Regex = Regex::new(r"^\[INFO\] -+< .+ >-+$").unwrap();

    /// Compile-error coordinate substring to strip when deduping warnings/errors.
    static ref FILE_COORD: Regex = Regex::new(r"/[^:]+\.java:\[\d+,\d+\]").unwrap();
}

// ── Quiet-mode detection ────────────────────────────────────────────────────

/// `mvn -q` / `mvn --quiet` suppresses all `[INFO]` lines: no `BUILD SUCCESS`
/// footer, no `[INFO] Running` markers, no module banners. A passing run emits
/// **zero bytes**; a failing run emits only `[ERROR]`-prefixed lines plus the
/// stack trace. The standard filters key off `[INFO]` markers and the footer
/// guard, so they can't fire here — `filter_quiet` handles this case instead.
fn is_quiet(args: &[String]) -> bool {
    args.iter().any(|a| a == "-q" || a == "--quiet")
}

// ── Phase detection ─────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum MvnPhase {
    Test,        // test, integration-test (Failsafe = Surefire shape)
    Compile,     // compile, test-compile
    Package,     // package, install, verify, deploy
    Passthrough, // clean, site, plugin goals, version/help, empty
}

/// Scan args left-to-right, skip flags + `-D…` system props, pick the LAST
/// remaining token. If empty, plugin-form (`:`), or `clean`/`site` → Passthrough.
pub fn detect_phase(args: &[String]) -> MvnPhase {
    let last = args
        .iter()
        .filter(|a| !a.starts_with('-'))
        .map(|s| s.as_str())
        .next_back()
        .unwrap_or("");

    if last.is_empty() || last.contains(':') {
        return MvnPhase::Passthrough;
    }
    match last {
        "clean" | "site" | "site-deploy" => MvnPhase::Passthrough,
        "test" | "integration-test" => MvnPhase::Test,
        "compile" | "test-compile" => MvnPhase::Compile,
        "package" | "install" | "verify" | "deploy" => MvnPhase::Package,
        _ => MvnPhase::Passthrough,
    }
}

// ── Stack-frame deny-list ────────────────────────────────────────────────────

const FRAMEWORK_FRAME_PREFIXES: &[&str] = &[
    "at org.junit.",
    "at junit.",
    "at org.apache.maven.surefire.",
    "at sun.reflect.",
    "at jdk.internal.reflect.",
    "at jdk.proxy",
    "at java.base/",
    "at java.lang.reflect.",
    "at java.util.",
];

fn is_framework_frame(trimmed: &str) -> bool {
    FRAMEWORK_FRAME_PREFIXES
        .iter()
        .any(|p| trimmed.starts_with(p))
}

// ── English-footer guard ────────────────────────────────────────────────────

fn has_english_footer(stripped: &str) -> bool {
    stripped.lines().any(|l| {
        let t = l.trim();
        t.ends_with(" BUILD SUCCESS") || t.ends_with(" BUILD FAILURE")
    })
}

// ── Outside-block keep list (shared by surefire + package) ──────────────────

fn keep_outside_block(line: &str) -> bool {
    RESULTS.is_match(line)
        || AGG.is_match(line)
        || BUILD_FOOT.is_match(line)
        || MODULE_BANNER.is_match(line)
        || line.starts_with("[INFO] Total time:")
        || line.starts_with("[INFO] Finished at:")
        || line.starts_with("[INFO] Building ")
        || line.starts_with("[INFO] Scanning ")
        || line.starts_with("[INFO] Installing ")
        || line.starts_with("[ERROR] Failures:")
        || line.starts_with("[ERROR] Errors:")
        || (line.starts_with("[ERROR]") && !line.starts_with("[ERROR] Tests run:"))
        || line.starts_with("[INFO] Building war:")
        || line.starts_with("[INFO] Building jar:")
        || line.starts_with("[INFO] Building ear:")
}

// ── Surefire block filter ───────────────────────────────────────────────────

/// Shared state machine driving the inner Surefire block + failure-trail
/// behaviour for `filter_surefire` and `filter_package`. Each filter wraps it
/// with its own outside-block keep logic (`[WARNING]` dedup, module-banner
/// keep, `keep_continuation` for compile-error continuations, etc.) which is
/// applied on the [`SurefireStep::Passthrough`] arm.
///
/// Inner machine responsibilities:
///   - `[INFO] --- … @ … ---` plugin banner skip
///   - `[INFO] Running <FQN>` opens a buffered block (flushes any prior open
///     block as keep — happens on truncated output)
///   - in-block buffering until the next CLOSE line
///   - CLOSE with `Failures > 0` or `Errors > 0` → yields
///     [`SurefireStep::FailingClose`] so the outer loop can decide whether to
///     emit (Commit D uses this seam to enforce `mvn_max_failures`)
///   - failure-trail handling for the exception/user-frame trail Surefire 3.x
///     emits **after** the close line, terminated by a blank line. Framework
///     frames (junit, jdk.proxy, java.base, etc.) are stripped from both the
///     buffered block and the trail; user-code frames are preserved.
struct SurefireBlock {
    block_lines: Vec<String>,
    block_running: Option<String>,
    in_block: bool,
    failure_trail: bool,
}

enum SurefireStep {
    /// Inner machine consumed the line; outer loop should `continue;`.
    Consumed,
    /// A CLOSE line with `Failures > 0` or `Errors > 0` was reached. Outer
    /// loop decides whether to commit (via [`SurefireBlock::commit_failing`]).
    FailingClose {
        running: Option<String>,
        lines: Vec<String>,
        close: String,
    },
    /// Inner machine did not handle the line; outer loop applies its own
    /// outside-block keep logic.
    Passthrough,
}

impl SurefireBlock {
    fn new() -> Self {
        Self {
            block_lines: Vec::new(),
            block_running: None,
            in_block: false,
            failure_trail: false,
        }
    }

    fn step(&mut self, line: &str, out: &mut String) -> SurefireStep {
        if PLUGIN_BANNER.is_match(line) {
            return SurefireStep::Consumed;
        }

        if RUNNING.is_match(line) {
            if self.in_block {
                self.flush_open_block_as_keep(out);
            }
            self.block_lines.clear();
            self.block_running = Some(line.to_string());
            self.in_block = true;
            self.failure_trail = false;
            return SurefireStep::Consumed;
        }

        if self.in_block {
            if let Some(caps) = CLOSE.captures(line) {
                let fail = caps.get(1).map(|m| m.as_str() != "0").unwrap_or(false);
                let err = caps.get(2).map(|m| m.as_str() != "0").unwrap_or(false);
                if fail || err {
                    let lines = std::mem::take(&mut self.block_lines);
                    let running = self.block_running.take();
                    self.in_block = false;
                    return SurefireStep::FailingClose {
                        running,
                        lines,
                        close: line.to_string(),
                    };
                }
                self.block_lines.clear();
                self.block_running = None;
                self.in_block = false;
                return SurefireStep::Consumed;
            }
            self.block_lines.push(line.to_string());
            return SurefireStep::Consumed;
        }

        if self.failure_trail {
            if line.is_empty() {
                out.push('\n');
                self.failure_trail = false;
                return SurefireStep::Consumed;
            }
            let t = line.trim_start();
            if t.starts_with("at ") && is_framework_frame(t) {
                return SurefireStep::Consumed;
            }
            out.push_str(line);
            out.push('\n');
            return SurefireStep::Consumed;
        }

        SurefireStep::Passthrough
    }

    /// Commit a `FailingClose` to `out`: writes `running`, then `lines` (with
    /// framework frames stripped), then `close`. Enables `failure_trail` so
    /// the post-close exception/user-frame trail is preserved.
    fn commit_failing(
        &mut self,
        out: &mut String,
        running: Option<&str>,
        lines: &[String],
        close: &str,
    ) {
        if let Some(r) = running {
            out.push_str(r);
            out.push('\n');
        }
        for l in lines {
            let t = l.trim_start();
            if t.starts_with("at ") && is_framework_frame(t) {
                continue;
            }
            out.push_str(l);
            out.push('\n');
        }
        out.push_str(close);
        out.push('\n');
        self.failure_trail = true;
    }

    /// End-of-stream flush: if a block opened and never closed (truncated
    /// output), surface what we have rather than dropping it silently.
    fn finish(&mut self, out: &mut String) {
        if self.in_block {
            self.flush_open_block_as_keep(out);
        }
    }

    fn flush_open_block_as_keep(&mut self, out: &mut String) {
        if let Some(r) = self.block_running.take() {
            out.push_str(&r);
            out.push('\n');
        }
        for l in self.block_lines.drain(..) {
            out.push_str(&l);
            out.push('\n');
        }
        self.in_block = false;
    }
}

/// Buffered single-pass filter for `mvn test` / `mvn integration-test`.
///
/// Drives [`SurefireBlock`] for the inner block/trail machine; applies the
/// outside-block keep-list with `keep_continuation` for indented compile-error
/// continuations (`symbol:` / `location:` after a `[ERROR] cannot find symbol`
/// line).
///
/// English-footer guard: if no `BUILD SUCCESS`/`BUILD FAILURE` line is present,
/// return the ANSI-stripped raw input (non-English locale or truncated output).
pub fn filter_surefire(raw: &str) -> String {
    let stripped = strip_ansi(raw);
    if !has_english_footer(&stripped) {
        return stripped;
    }

    let mut out = String::new();
    let mut block = SurefireBlock::new();
    let mut keep_continuation = false;

    for line in stripped.lines() {
        match block.step(line, &mut out) {
            SurefireStep::Consumed => continue,
            SurefireStep::FailingClose {
                running,
                lines,
                close,
            } => {
                block.commit_failing(&mut out, running.as_deref(), &lines, &close);
                keep_continuation = false;
                continue;
            }
            SurefireStep::Passthrough => {}
        }

        if keep_continuation && (line.starts_with(' ') || line.starts_with('\t')) {
            out.push_str(line);
            out.push('\n');
            continue;
        }

        if keep_outside_block(line) {
            out.push_str(line);
            out.push('\n');
            keep_continuation = line.starts_with("[ERROR]")
                && !line.starts_with("[ERROR] Tests run:")
                && !line.starts_with("[ERROR] Failures:")
                && !line.starts_with("[ERROR] Errors:");
            continue;
        }
    }

    block.finish(&mut out);
    out
}

// ── Compile filter ──────────────────────────────────────────────────────────

/// Buffered single-pass filter for `mvn compile` / `test-compile`.
///
/// Keeps module banners, `[INFO] Building …`, `[INFO] BUILD …`, totals, finish
/// time, scanning line, install lines, and `[ERROR]` blocks with indented
/// continuation (`  symbol:`, `  ^`, `  required:`). Deduplicates `[WARNING]`
/// lines by normalised message (strip file coordinates).
pub fn filter_compile(raw: &str) -> String {
    let stripped = strip_ansi(raw);
    if !has_english_footer(&stripped) {
        return stripped;
    }

    let mut out = String::new();
    let mut keep_continuation = false;
    let mut seen_warnings: HashSet<String> = HashSet::new();

    for line in stripped.lines() {
        if MODULE_BANNER.is_match(line) {
            out.push_str(line);
            out.push('\n');
            keep_continuation = false;
            continue;
        }
        if BUILD_FOOT.is_match(line)
            || line.starts_with("[INFO] Building ")
            || line.starts_with("[INFO] Total time:")
            || line.starts_with("[INFO] Finished at:")
            || line.starts_with("[INFO] Scanning ")
        {
            out.push_str(line);
            out.push('\n');
            keep_continuation = false;
            continue;
        }
        if line.starts_with("[ERROR]") {
            out.push_str(line);
            out.push('\n');
            keep_continuation = true;
            continue;
        }
        if keep_continuation && (line.starts_with(' ') || line.starts_with('\t')) {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if line.starts_with("[WARNING]") {
            let norm = FILE_COORD
                .replace_all(&line["[WARNING] ".len().min(line.len())..], "")
                .to_string();
            if seen_warnings.insert(norm) {
                out.push_str(line);
                out.push('\n');
            }
            keep_continuation = false;
            continue;
        }
        // Drop everything else
        keep_continuation = false;
    }

    out
}

// ── Package filter ──────────────────────────────────────────────────────────

/// Buffered single-pass filter for `mvn package`/`install`/`verify`/`deploy`.
///
/// Mode toggle: starts in `Compile` mode, switches to `Surefire` when a
/// `[INFO] Running …` line is seen, switches back on `Tests run:` close.
/// Outside any Surefire block, applies the unified keep-list (compile keepers
/// + install/artifact lines).
pub fn filter_package(raw: &str) -> String {
    let stripped = strip_ansi(raw);
    if !has_english_footer(&stripped) {
        return stripped;
    }

    let mut out = String::new();
    let mut block = SurefireBlock::new();
    let mut keep_continuation = false;
    let mut seen_warnings: HashSet<String> = HashSet::new();

    for line in stripped.lines() {
        match block.step(line, &mut out) {
            SurefireStep::Consumed => continue,
            SurefireStep::FailingClose {
                running,
                lines,
                close,
            } => {
                block.commit_failing(&mut out, running.as_deref(), &lines, &close);
                keep_continuation = false;
                continue;
            }
            SurefireStep::Passthrough => {}
        }

        // Outside any Surefire block: compile-keep AND surefire-outside-keep merge.
        if MODULE_BANNER.is_match(line) || keep_outside_block(line) {
            out.push_str(line);
            out.push('\n');
            keep_continuation = line.starts_with("[ERROR]")
                && !line.starts_with("[ERROR] Tests run:")
                && !line.starts_with("[ERROR] Failures:")
                && !line.starts_with("[ERROR] Errors:");
            continue;
        }
        if keep_continuation && (line.starts_with(' ') || line.starts_with('\t')) {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if line.starts_with("[WARNING]") {
            let norm = FILE_COORD
                .replace_all(&line["[WARNING] ".len().min(line.len())..], "")
                .to_string();
            if seen_warnings.insert(norm) {
                out.push_str(line);
                out.push('\n');
            }
            keep_continuation = false;
            continue;
        }
        keep_continuation = false;
    }

    block.finish(&mut out);
    out
}

// ── Quiet-mode filter ───────────────────────────────────────────────────────

/// Boilerplate `[ERROR]` lines Maven emits after `Failed to execute goal` —
/// pure noise pointing at log files and help URLs, no signal for the user/LLM.
const QUIET_BOILER_PREFIXES: &[&str] = &[
    "[ERROR] See ",
    "[ERROR] -> [Help",
    "[ERROR] To see the full stack trace",
    "[ERROR] Re-run Maven",
    "[ERROR] For more information",
    "[ERROR] [Help",
];

/// Filter for `mvn -q` invocations.
///
/// Under `-q`, Maven 3.x suppresses all `[INFO]` lines, so the standard
/// `filter_surefire` / `filter_compile` / `filter_package` pipelines (which
/// key off the English `BUILD SUCCESS` footer and `[INFO] Running` markers)
/// can't fire. This filter handles the residual `-q` output shape:
///
/// - Green run: input is empty → output is empty (0 → 0, no overhead).
/// - Failure run: keeps the Surefire close-line (`[ERROR] Tests run: …
///   <<< FAILURE! -- in FQN`), the per-test failure subline, exception class,
///   user-code stack frames, the failure summary block (`[ERROR] Failures:`,
///   indented entries, aggregate `Tests run: N, Failures: F, …`), and the
///   `[ERROR] Failed to execute goal` terminator. Drops framework stack
///   frames and the post-failure boilerplate block (`See …`, `[Help 1]`,
///   `Re-run Maven`, `To see the full stack trace`, etc.).
pub fn filter_quiet(raw: &str) -> String {
    let stripped = strip_ansi(raw);
    if stripped.trim().is_empty() {
        return String::new();
    }

    let mut out = String::new();
    let mut failure_trail = false;

    for line in stripped.lines() {
        // Surefire close-line for a failed class — keep + enter failure trail.
        if CLOSE.is_match(line) {
            out.push_str(line);
            out.push('\n');
            failure_trail = line.contains("<<< FAILURE!");
            continue;
        }

        // Per-test failure subline: `[ERROR] FQN.method -- Time elapsed: … <<< FAILURE!`.
        if line.starts_with("[ERROR] ") && line.contains("<<< FAILURE!") {
            out.push_str(line);
            out.push('\n');
            failure_trail = true;
            continue;
        }

        // Failure-trail body: exception class, user-code frames; drop framework frames.
        if failure_trail {
            if line.trim().is_empty() {
                out.push('\n');
                failure_trail = false;
                continue;
            }
            let t = line.trim_start();
            if t.starts_with("at ") && is_framework_frame(t) {
                continue;
            }
            out.push_str(line);
            out.push('\n');
            continue;
        }

        // Failure summary keepers.
        if line.starts_with("[ERROR] Tests run:")
            || line.starts_with("[ERROR] Failures:")
            || line.starts_with("[ERROR] Errors:")
            || line.starts_with("[ERROR]   ")
            || line.starts_with("[ERROR] Failed to execute goal")
        {
            out.push_str(line);
            out.push('\n');
            continue;
        }

        // Drop post-failure help boilerplate.
        if QUIET_BOILER_PREFIXES.iter().any(|p| line.starts_with(p)) {
            continue;
        }

        // Drop empty `[ERROR]` / `[ERROR] ` divider lines Maven emits between blocks.
        if line.trim_end() == "[ERROR]" {
            continue;
        }

        // Safety net: keep anything else (unexpected output under `-q` is rare;
        // do not silently drop signal we haven't classified).
        out.push_str(line);
        out.push('\n');
    }

    out
}

// ── Wrapper detection ───────────────────────────────────────────────────────

fn mvn_binary() -> &'static str {
    if cfg!(windows) {
        if Path::new(".\\mvnw.cmd").exists() {
            ".\\mvnw.cmd"
        } else {
            "mvn"
        }
    } else if Path::new("./mvnw").exists() {
        "./mvnw"
    } else {
        "mvn"
    }
}

fn new_mvn_command(args: &[String]) -> Command {
    let mut cmd = if cfg!(windows) {
        if Path::new(".\\mvnw.cmd").exists() {
            Command::new(".\\mvnw.cmd")
        } else {
            resolved_command("mvn")
        }
    } else if Path::new("./mvnw").exists() {
        Command::new("./mvnw")
    } else {
        resolved_command("mvn")
    };
    cmd.args(args);
    cmd
}

// ── Entry point ─────────────────────────────────────────────────────────────

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    // Verbose flags bypass filtering — user wants full output.
    if args
        .iter()
        .any(|a| matches!(a.as_str(), "-X" | "--debug" | "-e" | "--errors"))
    {
        let osargs: Vec<OsString> = args.iter().map(OsString::from).collect();
        return runner::run_passthrough(mvn_binary(), &osargs, verbose);
    }

    let tool = mvn_binary();
    let args_display = args.join(" ");

    // Quiet mode: standard footer guard can't fire (no `BUILD SUCCESS` line
    // under `-q`). Route to `filter_quiet` for any non-passthrough phase so
    // failure output gets framework frames + help boilerplate stripped.
    if is_quiet(args) {
        let phase = detect_phase(args);
        if matches!(phase, MvnPhase::Passthrough) {
            let osargs: Vec<OsString> = args.iter().map(OsString::from).collect();
            return runner::run_passthrough(tool, &osargs, verbose);
        }
        return runner::run_filtered(
            new_mvn_command(args),
            tool,
            &args_display,
            filter_quiet,
            RunOptions::with_tee("mvn_quiet"),
        );
    }

    let phase = detect_phase(args);

    match phase {
        MvnPhase::Test => runner::run_filtered(
            new_mvn_command(args),
            tool,
            &args_display,
            filter_surefire,
            RunOptions::with_tee("mvn_test"),
        ),
        MvnPhase::Compile => runner::run_filtered(
            new_mvn_command(args),
            tool,
            &args_display,
            filter_compile,
            RunOptions::with_tee("mvn_compile"),
        ),
        MvnPhase::Package => runner::run_filtered(
            new_mvn_command(args),
            tool,
            &args_display,
            filter_package,
            RunOptions::with_tee("mvn_package"),
        ),
        MvnPhase::Passthrough => {
            let osargs: Vec<OsString> = args.iter().map(OsString::from).collect();
            runner::run_passthrough(tool, &osargs, verbose)
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use std::io::Read;

    fn count_tokens(s: &str) -> usize {
        s.split_whitespace().count()
    }

    fn gunzip(bytes: &[u8]) -> String {
        let mut s = String::new();
        GzDecoder::new(bytes)
            .read_to_string(&mut s)
            .expect("gunzip");
        s
    }

    fn s<S: Into<String>>(it: impl IntoIterator<Item = S>) -> Vec<String> {
        it.into_iter().map(Into::into).collect()
    }

    // ── Phase detection ──────────────────────────────────────────────────────

    #[test]
    fn phase_test() {
        assert_eq!(detect_phase(&s(["test"])), MvnPhase::Test);
    }
    #[test]
    fn phase_integration_test() {
        assert_eq!(detect_phase(&s(["integration-test"])), MvnPhase::Test);
    }
    #[test]
    fn phase_compile() {
        assert_eq!(detect_phase(&s(["compile"])), MvnPhase::Compile);
    }
    #[test]
    fn phase_test_compile() {
        assert_eq!(detect_phase(&s(["test-compile"])), MvnPhase::Compile);
    }
    #[test]
    fn phase_install() {
        assert_eq!(detect_phase(&s(["install"])), MvnPhase::Package);
    }
    #[test]
    fn phase_package() {
        assert_eq!(detect_phase(&s(["package"])), MvnPhase::Package);
    }
    #[test]
    fn phase_verify() {
        assert_eq!(detect_phase(&s(["verify"])), MvnPhase::Package);
    }
    #[test]
    fn phase_deploy() {
        assert_eq!(detect_phase(&s(["deploy"])), MvnPhase::Package);
    }
    #[test]
    fn phase_clean_install_is_pkg() {
        assert_eq!(detect_phase(&s(["clean", "install"])), MvnPhase::Package);
    }
    #[test]
    fn phase_flags_before_goal() {
        assert_eq!(
            detect_phase(&s(["-B", "-DskipTests", "test"])),
            MvnPhase::Test
        );
    }
    #[test]
    fn phase_clean_only_passthrough() {
        assert_eq!(detect_phase(&s(["clean"])), MvnPhase::Passthrough);
    }
    #[test]
    fn phase_site_passthrough() {
        assert_eq!(detect_phase(&s(["site"])), MvnPhase::Passthrough);
    }
    #[test]
    fn phase_plugin_goal_passthrough() {
        assert_eq!(
            detect_phase(&s(["dependency:tree"])),
            MvnPhase::Passthrough
        );
    }
    #[test]
    fn phase_empty_passthrough() {
        let v: Vec<String> = Vec::new();
        assert_eq!(detect_phase(&v), MvnPhase::Passthrough);
    }
    #[test]
    fn phase_version_long() {
        assert_eq!(detect_phase(&s(["--version"])), MvnPhase::Passthrough);
    }
    #[test]
    fn phase_version_short() {
        assert_eq!(detect_phase(&s(["-v"])), MvnPhase::Passthrough);
    }
    #[test]
    fn phase_version_java_style() {
        assert_eq!(detect_phase(&s(["-version"])), MvnPhase::Passthrough);
    }
    #[test]
    fn phase_help() {
        assert_eq!(detect_phase(&s(["--help"])), MvnPhase::Passthrough);
    }

    // ── Surefire filter ──────────────────────────────────────────────────────

    #[test]
    fn filter_surefire_pass_output_compact() {
        let i = include_str!("../../../tests/fixtures/mvn_test_pass_slice_raw.txt");
        let o = filter_surefire(i);
        // Passing fixture has 5 close lines; all should be dropped (no per-class line in output).
        assert!(!o.contains("Running org.apache.commons.cli.help.UtilTest"));
        assert!(!o.contains("Time elapsed: 1.023 s -- in"));
        let savings = 100.0 - (count_tokens(&o) as f64 / count_tokens(i) as f64 * 100.0);
        assert!(
            savings >= 50.0,
            "pass-fixture savings >=50%, got {:.1}%",
            savings
        );
    }

    #[test]
    fn filter_surefire_fail_keeps_signal() {
        let i = include_str!("../../../tests/fixtures/mvn_test_fail_slice_raw.txt");
        let o = filter_surefire(i);
        assert!(o.contains("BUILD FAILURE"));
        assert!(o.contains("Failures: 1"));
    }

    #[test]
    fn surefire_drops_passing_block() {
        let i = include_str!("../../../tests/fixtures/mvn_test_pass_slice_raw.txt");
        let o = filter_surefire(i);
        assert!(
            !o.contains("at org.junit."),
            "framework frames stripped; got:\n{}",
            o
        );
        assert!(
            !o.contains("Running org.apache.commons.cli.ConverterTests"),
            "passing-test Running line dropped; got:\n{}",
            o
        );
        assert!(
            o.contains("BUILD SUCCESS"),
            "footer preserved; got:\n{}",
            o
        );
        assert!(
            o.contains("Tests run: 977, Failures: 0"),
            "aggregate preserved; got:\n{}",
            o
        );
    }

    #[test]
    fn surefire_preserves_failing_signal() {
        let i = include_str!("../../../tests/fixtures/mvn_test_fail_slice_raw.txt");
        let o = filter_surefire(i);
        assert!(
            o.contains("Failures: 1"),
            "failing aggregate preserved; got:\n{}",
            o
        );
        assert!(
            o.contains("AssertionFailedError"),
            "exception class preserved; got:\n{}",
            o
        );
        assert!(
            o.contains("at org.apache.commons.cli.RtkInducedFailTest.rtkInducedFailure"),
            "user-code frame preserved; got:\n{}",
            o
        );
        assert!(
            !o.contains("at org.junit."),
            "framework frames stripped in failing block; got:\n{}",
            o
        );
    }

    /// 2.x compat: CLOSE regex must still match the single-dash separator emitted
    /// by Surefire 2.x. Locks the `--?` regex against accidental tightening.
    #[test]
    fn surefire_matches_legacy_2x_close_line() {
        let i = "[INFO] -----< x >-----\n[INFO] Running x.Foo\n[INFO] Tests run: 3, Failures: 0, Errors: 0, Skipped: 0, Time elapsed: 0.123 s - in x.Foo\n[INFO] BUILD SUCCESS\n";
        let o = filter_surefire(i);
        // CLOSE matched → passing block dropped silently.
        assert!(
            !o.contains("Running x.Foo"),
            "2.x ` - in ` close-line matched; passing block dropped; got:\n{}",
            o
        );
        assert!(
            o.contains("BUILD SUCCESS"),
            "footer preserved; got:\n{}",
            o
        );
    }

    /// 3.x WARNING-prefixed close line (class with only skipped tests) must
    /// match CLOSE so the block is dropped (no failures, no errors).
    #[test]
    fn surefire_matches_warning_skipped_close_line() {
        let i = "[INFO] -----< x >-----\n[INFO] Running x.Skip\n[WARNING] Tests run: 5, Failures: 0, Errors: 0, Skipped: 5, Time elapsed: 0.010 s -- in x.Skip\n[INFO] BUILD SUCCESS\n";
        let o = filter_surefire(i);
        assert!(
            !o.contains("Running x.Skip"),
            "[WARNING] close-line matched; block dropped; got:\n{}",
            o
        );
    }

    /// 3.x failure-trail: after a CLOSE with `<<< FAILURE!`, the exception
    /// class and user-code frames Surefire emits *outside* the block must be
    /// preserved until the next blank line.
    #[test]
    fn surefire_preserves_3x_failure_trail() {
        let i = "[INFO] -----< x >-----\n\
                 [INFO] Running x.Foo\n\
                 [ERROR] Tests run: 1, Failures: 1, Errors: 0, Skipped: 0, Time elapsed: 0.033 s <<< FAILURE! -- in x.Foo\n\
                 [ERROR] x.Foo.bar -- Time elapsed: 0.025 s <<< FAILURE!\n\
                 org.opentest4j.AssertionFailedError: expected: <a> but was: <b>\n\
                 \tat x.Foo.bar(Foo.java:25)\n\
                 \tat org.junit.jupiter.api.Assertions.assertEquals(Assertions.java:1)\n\
                 \n\
                 [INFO] BUILD FAILURE\n";
        let o = filter_surefire(i);
        assert!(o.contains("AssertionFailedError"), "exception preserved; got:\n{}", o);
        assert!(o.contains("at x.Foo.bar"), "user frame preserved; got:\n{}", o);
        assert!(
            !o.contains("at org.junit."),
            "framework frame stripped in trail; got:\n{}",
            o
        );
    }

    /// `mvn test` whose compile step fails before Surefire runs must still
    /// keep the `[ERROR]` block's indented `symbol:` / `location:` continuation
    /// lines. Regression guard for the P0 reviewer ask: `filter_surefire`
    /// previously dropped them because it had no `keep_continuation` flag.
    #[test]
    fn surefire_keeps_compile_continuation_on_test_phase() {
        let i = include_str!("../../../tests/fixtures/mvn_test_compile_fail_slice_raw.txt");
        let o = filter_surefire(i);
        assert!(o.contains("cannot find symbol"), "ERROR line preserved; got:\n{}", o);
        assert!(
            o.contains("symbol:   variable bar"),
            "indented `symbol:` continuation preserved; got:\n{}",
            o
        );
        assert!(
            o.contains("location: class org.apache.commons.cli.CompileBreaker"),
            "indented `location:` continuation preserved; got:\n{}",
            o
        );
        assert!(o.contains("BUILD FAILURE"), "footer preserved; got:\n{}", o);
    }

    /// Regression guard on the package path so the install/verify route does
    /// not silently drift the other way after the `filter_surefire` continuation
    /// fix. Uses the existing compile-error slice — `filter_package` is the
    /// `install`-phase entry point and must keep the same continuation lines.
    #[test]
    fn package_still_keeps_compile_error_continuation_after_refactor() {
        let i = include_str!("../../../tests/fixtures/mvn_compile_error_slice_raw.txt");
        let o = filter_package(i);
        assert!(o.contains("cannot find symbol"), "ERROR line preserved; got:\n{}", o);
        assert!(
            o.contains("symbol:   variable bar"),
            "indented `symbol:` continuation preserved; got:\n{}",
            o
        );
        assert!(
            o.contains("location: class org.apache.commons.cli.CompileBreaker"),
            "indented `location:` continuation preserved; got:\n{}",
            o
        );
    }

    #[test]
    fn surefire_keeps_module_banner() {
        let i = "[INFO] Scanning for projects...\n[INFO] -----< com.example:myapp >-----\n[INFO] BUILD SUCCESS\n";
        let o = filter_surefire(i);
        assert!(o.contains("-----< com.example:myapp >-----"));
    }

    /// Production must ship raw `Time elapsed` and `Total time` durations
    /// untouched — the LLM/user needs the actual numbers to diagnose perf
    /// regressions. Earlier revisions normalised these to `T s`; that was
    /// only ever needed for deterministic snapshots and never belonged in
    /// the production path.
    #[test]
    fn surefire_preserves_real_durations() {
        let i = "[INFO] -----< x >-----\n[INFO] Running x.Foo\n[ERROR] Tests run: 1, Failures: 1, Errors: 0, Skipped: 0, Time elapsed: 2.341 s <<< FAILURE! - in x.Foo\n[INFO] BUILD FAILURE\n[INFO] Total time:  4.567 s\n";
        let o = filter_surefire(i);
        assert!(
            o.contains("2.341 s"),
            "raw close-line duration preserved; got:\n{}",
            o
        );
        assert!(
            o.contains("Total time:  4.567 s"),
            "raw total time preserved; got:\n{}",
            o
        );
        assert!(
            !o.contains("Time elapsed: T s"),
            "no normalisation in production; got:\n{}",
            o
        );
    }

    #[test]
    fn footer_guard_french_passthrough() {
        let i = include_str!("../../../tests/fixtures/mvn_locale_fr_raw.txt");
        let o = filter_surefire(i);
        assert!(
            o.contains("BUILD ÉCHEC"),
            "footer-guard must pass through non-English output; got:\n{}",
            o
        );
        // Confirm we did NOT filter — input length preserved (modulo ANSI strip, which is a no-op here)
        assert_eq!(
            o.lines().count(),
            i.lines().count(),
            "footer-guard returns raw input"
        );
    }

    #[test]
    fn footer_guard_no_pom_passthrough() {
        let i = include_str!("../../../tests/fixtures/mvn_no_pom_raw.txt");
        let o = filter_surefire(i);
        // No BUILD footer → passthrough; user sees the `[ERROR] no POM` line.
        assert!(
            o.contains("there is no POM"),
            "no-pom error preserved; got:\n{}",
            o
        );
    }

    // ── Compile filter ───────────────────────────────────────────────────────

    #[test]
    fn filter_compile_error_compact() {
        let i = include_str!("../../../tests/fixtures/mvn_compile_error_slice_raw.txt");
        let o = filter_compile(i);
        let savings = 100.0 - (count_tokens(&o) as f64 / count_tokens(i) as f64 * 100.0);
        assert!(
            savings >= 30.0,
            "compile-error fixture is small; >=30% savings, got {:.1}%",
            savings
        );
    }

    #[test]
    fn compile_preserves_error_continuation() {
        let i = include_str!("../../../tests/fixtures/mvn_compile_error_slice_raw.txt");
        let o = filter_compile(i);
        assert!(o.contains("cannot find symbol"), "ERROR line preserved");
        assert!(
            o.contains("symbol:   variable bar"),
            "indented continuation preserved"
        );
        assert!(o.contains("BUILD FAILURE"), "footer preserved");
    }

    #[test]
    fn compile_dedupes_warnings() {
        let i = "[INFO] -----< x >-----\n\
                 [WARNING] /a.java:[1,2] uses deprecated API\n\
                 [WARNING] /b.java:[3,4] uses deprecated API\n\
                 [WARNING] /a.java:[5,6] unchecked cast\n\
                 [INFO] BUILD SUCCESS\n";
        let o = filter_compile(i);
        let warns = o.matches("[WARNING]").count();
        assert_eq!(warns, 2, "dedup by normalised message; got:\n{}", o);
    }

    // ── Package filter ───────────────────────────────────────────────────────

    #[test]
    fn filter_package_install_compact() {
        let i = include_str!("../../../tests/fixtures/mvn_install_slice_raw.txt");
        let o = filter_package(i);
        let savings = 100.0 - (count_tokens(&o) as f64 / count_tokens(i) as f64 * 100.0);
        assert!(
            savings >= 50.0,
            "install-slice savings >=50%, got {:.1}%",
            savings
        );
    }

    #[test]
    fn package_keeps_install_lines() {
        let i = include_str!("../../../tests/fixtures/mvn_install_slice_raw.txt");
        let o = filter_package(i);
        assert!(
            o.contains("Installing"),
            "install line preserved; got:\n{}",
            o
        );
        assert!(
            o.contains("Building jar:"),
            "jar line preserved; got:\n{}",
            o
        );
        assert!(
            !o.contains("at org.junit."),
            "framework frames stripped; got:\n{}",
            o
        );
    }

    // ── Token-savings (FULL gzipped fixtures) ───────────────────────────────

    #[test]
    #[ignore]
    fn print_savings_summary() {
        let pf = gunzip(include_bytes!("../../../tests/fixtures/mvn_test_pass_full_raw.txt.gz"));
        let pf_out = filter_surefire(&pf);
        let pf_in_tok = count_tokens(&pf);
        let pf_out_tok = count_tokens(&pf_out);
        let pf_s = 100.0 - (pf_out_tok as f64 / pf_in_tok as f64 * 100.0);
        println!(
            "mvn_test_pass_full: {} -> {} tokens ({:.1}% savings)",
            pf_in_tok, pf_out_tok, pf_s
        );

        let inst = gunzip(include_bytes!("../../../tests/fixtures/mvn_install_full_raw.txt.gz"));
        let inst_out = filter_package(&inst);
        let inst_in_tok = count_tokens(&inst);
        let inst_out_tok = count_tokens(&inst_out);
        let inst_s = 100.0 - (inst_out_tok as f64 / inst_in_tok as f64 * 100.0);
        println!(
            "mvn_install_full:   {} -> {} tokens ({:.1}% savings)",
            inst_in_tok, inst_out_tok, inst_s
        );
    }

    #[test]
    fn savings_mvn_test_pass_full() {
        let bytes = include_bytes!("../../../tests/fixtures/mvn_test_pass_full_raw.txt.gz");
        let i = gunzip(bytes);
        let o = filter_surefire(&i);
        let savings = 100.0 - (count_tokens(&o) as f64 / count_tokens(&i) as f64 * 100.0);
        assert!(
            savings >= 90.0,
            "mvn test ≥90% savings on full fixture, got {:.1}% (raw={} tok, filtered={} tok)",
            savings,
            count_tokens(&i),
            count_tokens(&o)
        );
    }

    #[test]
    fn savings_mvn_install_full() {
        let bytes = include_bytes!("../../../tests/fixtures/mvn_install_full_raw.txt.gz");
        let i = gunzip(bytes);
        let o = filter_package(&i);
        let savings = 100.0 - (count_tokens(&o) as f64 / count_tokens(&i) as f64 * 100.0);
        assert!(
            savings >= 85.0,
            "mvn install ≥85% savings on full fixture, got {:.1}% (raw={} tok, filtered={} tok)",
            savings,
            count_tokens(&i),
            count_tokens(&o)
        );
    }

    // ── Quiet mode (`mvn -q`) ────────────────────────────────────────────────

    #[test]
    fn quiet_detects_short_flag() {
        assert!(is_quiet(&s(["-q", "test"])));
        assert!(is_quiet(&s(["test", "-q"])));
        assert!(is_quiet(&s(["-B", "-q", "-DskipFoo", "install"])));
    }

    #[test]
    fn quiet_detects_long_flag() {
        assert!(is_quiet(&s(["--quiet", "test"])));
    }

    #[test]
    fn quiet_does_not_match_unrelated_flags() {
        assert!(!is_quiet(&s(["-Q", "test"])));
        assert!(!is_quiet(&s(["-quiet", "test"])));
        assert!(!is_quiet(&s(["-B", "test"])));
    }

    /// Green `mvn -q test` emits zero bytes; filter must return empty.
    #[test]
    fn quiet_green_run_is_empty() {
        assert_eq!(filter_quiet(""), "");
        assert_eq!(filter_quiet("   \n\n  \n"), "");
    }

    /// Failure under `-q`: keep close-line, exception, user frame, summary,
    /// goal terminator. Drop framework frames + help boilerplate block.
    #[test]
    fn quiet_fail_strips_framework_and_boilerplate() {
        let i = include_str!("../../../tests/fixtures/mvn_quiet_fail_raw.txt");
        let o = filter_quiet(i);

        // Kept: failure signal.
        assert!(
            o.contains("Tests run: 1, Failures: 1, Errors: 0, Skipped: 0"),
            "close-line preserved; got:\n{}",
            o
        );
        assert!(
            o.contains("AssertionFailedError"),
            "exception class preserved; got:\n{}",
            o
        );
        assert!(
            o.contains("at x.FailTest.this_will_fail"),
            "user-code frame preserved; got:\n{}",
            o
        );
        assert!(
            o.contains("[ERROR] Failures:"),
            "failure summary header preserved; got:\n{}",
            o
        );
        assert!(
            o.contains("[ERROR] Tests run: 6, Failures: 1, Errors: 0, Skipped: 0"),
            "aggregate preserved; got:\n{}",
            o
        );
        assert!(
            o.contains("[ERROR] Failed to execute goal"),
            "goal terminator preserved; got:\n{}",
            o
        );

        // Dropped: framework frames.
        assert!(
            !o.contains("at org.junit."),
            "junit frame stripped; got:\n{}",
            o
        );
        assert!(
            !o.contains("at java.base/"),
            "java.base frame stripped; got:\n{}",
            o
        );

        // Dropped: help boilerplate.
        assert!(
            !o.contains("To see the full stack trace"),
            "help boilerplate stripped; got:\n{}",
            o
        );
        assert!(
            !o.contains("[Help 1] http"),
            "help link stripped; got:\n{}",
            o
        );
        assert!(
            !o.contains("See /tmp/") && !o.contains("See dump files"),
            "log-pointer lines stripped; got:\n{}",
            o
        );
    }

    /// Savings target on the real `mvn -q test` fail fixture.
    #[test]
    fn savings_mvn_quiet_fail() {
        let i = include_str!("../../../tests/fixtures/mvn_quiet_fail_raw.txt");
        let o = filter_quiet(i);
        let savings = 100.0 - (count_tokens(&o) as f64 / count_tokens(i) as f64 * 100.0);
        assert!(
            savings >= 50.0,
            "mvn -q fail ≥50% savings, got {:.1}% (raw={} tok, filtered={} tok)",
            savings,
            count_tokens(i),
            count_tokens(&o)
        );
    }

    /// Safety net: if the `[ERROR]` line isn't on the known keep/drop lists,
    /// the filter must NOT silently drop it. Better to leak a line than to
    /// hide signal.
    #[test]
    fn quiet_unknown_error_line_kept_as_safety_net() {
        let i = "[ERROR] Some unexpected error output we don't classify\n";
        let o = filter_quiet(i);
        assert!(
            o.contains("Some unexpected error output"),
            "unclassified ERROR line preserved; got:\n{}",
            o
        );
    }
}
