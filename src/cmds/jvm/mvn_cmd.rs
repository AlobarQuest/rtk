//! Apache Maven filter — Surefire/Failsafe block collapse, compile error/warning
//! dedup, package/install pipeline with mode-toggle.
//!
//! Replaces the previous `src/filters/mvn-build.toml` filter with a Rust module
//! capable of state-machine parsing (block collapse, continuation tracking,
//! mode toggle) that TOML DSL cannot express.

use crate::core::runner::{self, RunOptions};
use crate::core::truncate::CAP_WARNINGS;
use crate::core::utils::{resolved_command, strip_ansi};
use anyhow::Result;
use regex::Regex;
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::Path;
use std::process::Command;
use std::sync::LazyLock;

/// Cap on emitted failing test-class blocks and `[ERROR] Failures:` summary
/// entries — test-failure cap class, same binding as pytest/rspec/rake/runner.
const MAX_MVN_FAILING_CLASSES: usize = CAP_WARNINGS;

// ── Shared regex patterns ────────────────────────────────────────────────────

/// `[INFO] Running com.example.app.FooTest`
static RUNNING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[INFO\] Running ").unwrap());

/// Surefire/Failsafe per-class close line. Captures `Failures` and `Errors`.
/// Tolerates the optional `<<< FAILURE!` / `<<< ERROR!` marker (3.5.5 emits
/// `<<< FAILURE!` even for errors-only classes — see
/// `mvn_test_multifail_slice_raw.txt`; `ERROR!` accepted defensively for
/// other Surefire versions; failure detection is via the captured counts,
/// not the marker). Separator is `-` (Surefire 2.x) or `--` (Surefire 3.x).
/// Prefix INFO/ERROR/WARNING (3.x emits WARNING for classes with only
/// skipped tests).
static CLOSE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\[(?:INFO|ERROR|WARNING)\] Tests run: \d+, Failures: (\d+), Errors: (\d+), Skipped: \d+, Time elapsed: [^ ]+ s(?:\s+<<<\s*(?:FAILURE|ERROR)!)?\s+--?\s+in (.+)$"
    ).unwrap()
});

/// Final BUILD footer.
static BUILD_FOOT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[(?:INFO|ERROR)\] BUILD (?:SUCCESS|FAILURE)$").unwrap());

/// `[INFO] Results:` separator before the aggregate.
static RESULTS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[INFO\] Results:\s*$").unwrap());

/// Aggregate counts line (no `Time elapsed`, no ` - in `).
static AGG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[(?:INFO|ERROR)\] Tests run: \d+, Failures: \d+, Errors: \d+, Skipped: \d+\s*$")
        .unwrap()
});

/// Plugin banner line: `[INFO] --- plugin:goal (id) @ module ---`.
static PLUGIN_BANNER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[INFO\] --- .* @ .* ---$").unwrap());

/// Module banner with project name in brackets.
static MODULE_BANNER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[INFO\] -+< .+ >-+$").unwrap());

/// Reactor summary header that opens the per-module pass/fail block at
/// the end of a multi-module build.
static REACTOR_SUMMARY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[INFO\] Reactor Summary for ").unwrap());

/// Compile-error coordinate substring to strip when deduping warnings/errors.
static FILE_COORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/[^:]+\.java:\[\d+,\d+\]").unwrap());

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

/// Boilerplate `[ERROR]` lines Maven emits after `Failed to execute goal` —
/// pure noise pointing at log files and help URLs, no signal for the user/LLM.
/// Deliberately excludes `[ERROR] After correcting the problems` and
/// `[ERROR]   mvn <args> -rf :…` (the resume hint is actionable signal for a
/// multi-module build) and `[ERROR] Failed to execute goal` (signal).
const BOILER_PREFIXES: &[&str] = &[
    "[ERROR] See ",
    "[ERROR] -> [Help",
    "[ERROR] To see the full stack trace",
    "[ERROR] Re-run Maven",
    "[ERROR] For more information",
    "[ERROR] [Help",
];

/// Post-failure help boilerplate, plus the bare `[ERROR]` divider lines Maven
/// emits between boilerplate blocks (same drop rules as `filter_quiet`).
fn is_boilerplate(line: &str) -> bool {
    BOILER_PREFIXES.iter().any(|p| line.starts_with(p)) || line.trim_end() == "[ERROR]"
}

/// Blank separator line as emitted by both binaries: plain `mvn` writes a
/// truly empty line between a Surefire failure trail and the next section;
/// `mvnd` routes everything through the daemon logger, which prefixes even
/// blank lines with `[INFO] ` (see `mvnd_test_fail_raw.txt`). Both terminate
/// (or bridge, in the re-arm state) a failure trail.
fn is_blank_separator(line: &str) -> bool {
    line.is_empty() || line.trim_end() == "[INFO]"
}

// ── Parallel-reactor lanes ──────────────────────────────────────────────────

/// mvnd parallel reactors prefix per-module log lines with `[module] ` while
/// stack traces stay raw and reactor-level lines stay unprefixed — and lines
/// from different modules interleave freely (see
/// `mvnd_reactor_fail_raw.txt`). Classification must therefore happen on the
/// prefix-stripped view, and Surefire block state must be tracked per module.
///
/// Returns `(Some(key), core)` for log-level lines — `key` is the module tag
/// (`""` for unprefixed reactor-level lines) and `core` is the line with the
/// module prefix stripped, for classification — or `(None, line)` for raw
/// lines (stack traces, stray stdout), whose owning lane is resolved by
/// [`Lanes::raw_owner`] (unique owner, or preserved verbatim).
///
/// Residual seam: a raw line of shape `[tag] text` where `tag` is not a log
/// level and `text` doesn't start with `[` (e.g. slf4j's `[main] INFO …` on
/// test stdout) keys to the root lane rather than classifying as raw, so
/// inside a trail it bypasses trail handling and falls to the root
/// keep-list. Real Surefire trail lines start with an FQCN, `at `,
/// `Caused by:`, or whitespace, so no realistic diagnostic takes this path.
fn split_lane(line: &str) -> (Option<&str>, &str) {
    if !line.starts_with('[') {
        return (None, line);
    }
    if let Some(end) = line.find("] ") {
        let tag = &line[1..end];
        let rest = &line[end + 2..];
        if !matches!(tag, "INFO" | "ERROR" | "WARNING" | "DEBUG" | "FATAL") && rest.starts_with('[')
        {
            return (Some(tag), rest);
        }
    }
    (Some(""), line)
}

/// `[ERROR] FQN.method -- Time elapsed: 0.030 s <<< FAILURE!` (or `<<< ERROR!`).
/// Distinguished from CLOSE by call position: only consulted when
/// `in_block == false` (CLOSE only occurs while a block is open). A
/// CLOSE-shaped line outside a block would match too — acceptable: the
/// disarm-on-take guard limits the effect to one stray line.
/// Note: the `[ERROR]   Class.test:25 …` failures-summary entries (3-space
/// indent, no `<<<` marker) do NOT match.
fn is_per_test_subline(line: &str) -> bool {
    line.starts_with("[ERROR] ")
        && (line.contains("<<< FAILURE!") || line.contains("<<< ERROR!"))
}

// ── English-footer guard ────────────────────────────────────────────────────

fn has_english_footer(stripped: &str) -> bool {
    stripped.lines().any(|l| {
        let t = l.trim();
        t.ends_with(" BUILD SUCCESS") || t.ends_with(" BUILD FAILURE")
    })
}

// ── Outside-block keep list (shared by surefire + package) ──────────────────

/// Multi-module reactor summary keeper. Reads `in_reactor_summary` and toggles
/// it on `[INFO] Reactor Summary for …` (enter) and `BUILD SUCCESS`/`BUILD
/// FAILURE` (exit). Returns `true` for every line while the flag is set so the
/// per-module status rows (`[INFO] foo ...... SUCCESS [  1.234 s]`, plain
/// `[INFO]` separators inside the summary, etc.) survive. Returns `false`
/// otherwise — the caller's outside-block keep-list still applies.
///
/// Designed to be called **before** `keep_outside_block` so the `BUILD_FOOT`
/// clears-flag side effect always runs regardless of `||` short-circuit.
fn reactor_summary_keep(line: &str, in_reactor_summary: &mut bool) -> bool {
    if REACTOR_SUMMARY.is_match(line) {
        *in_reactor_summary = true;
        return true;
    }
    if BUILD_FOOT.is_match(line) {
        *in_reactor_summary = false;
        return false;
    }
    *in_reactor_summary
}

fn keep_outside_block(line: &str) -> bool {
    // Help boilerplate must be rejected before the `[ERROR]` catch-all below
    // (non-quiet parity with `filter_quiet`'s boilerplate stripping).
    if is_boilerplate(line) {
        return false;
    }
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
///     emit (this seam enforces [`MAX_MVN_FAILING_CLASSES`])
///   - failure-trail handling for the exception/user-frame trail Surefire 3.x
///     emits **after** the close line, terminated by a blank line. Framework
///     frames (junit, jdk.proxy, java.base, etc.) are stripped from both the
///     buffered block and the trail; user-code frames are preserved.
///   - multi-failure classes: Surefire 3.x emits one blank-separated detail
///     block per failing test under a single CLOSE line. When a trail ends at
///     a blank line, `trail_rearm` remembers the keep/drop decision so the
///     next per-test subline re-enters the trail with the same decision.
///     End-of-input with `trail_rearm` still `Some` is harmless (nothing
///     pending in `out`); `finish()` / `flush_open_block_as_keep` need no
///     special handling.
struct SurefireBlock<'a> {
    block_lines: Vec<&'a str>,
    block_running: Option<&'a str>,
    in_block: bool,
    failure_trail: bool,
    /// When set together with `failure_trail`, consumes the trail (per-test
    /// `<<< FAILURE!` subline, exception, user frames) without writing it to
    /// `out`. Used when the caller capped a failing block via `drop_failing`.
    drop_trail: bool,
    /// Set when a trail ends at a blank line; holds the `drop_trail` value so
    /// the next per-test subline of the same class re-enters the trail with
    /// the same keep/drop decision (a capped class must drop **all** its
    /// per-test blocks, not just the first). Cleared by any non-blank
    /// non-subline line, by `RUNNING`, and by `commit_failing`/`drop_failing`.
    trail_rearm: Option<bool>,
}

enum SurefireStep<'a> {
    /// Inner machine consumed the line; outer loop should `continue;`.
    Consumed,
    /// A CLOSE line with `Failures > 0` or `Errors > 0` was reached. Outer
    /// loop decides whether to commit (via [`SurefireBlock::commit_failing`]).
    FailingClose {
        running: Option<&'a str>,
        lines: Vec<&'a str>,
        close: &'a str,
    },
    /// Inner machine did not handle the line; outer loop applies its own
    /// outside-block keep logic.
    Passthrough,
}

impl<'a> SurefireBlock<'a> {
    fn new() -> Self {
        Self {
            block_lines: Vec::new(),
            block_running: None,
            in_block: false,
            failure_trail: false,
            drop_trail: false,
            trail_rearm: None,
        }
    }

    /// Matching is done on `core` (the module-prefix-stripped view of the
    /// line — identical to `line` outside mvnd parallel reactors); `line` is
    /// the original, which is what gets buffered and emitted so module
    /// identity survives in the output.
    fn step(&mut self, line: &'a str, core: &str, out: &mut String) -> SurefireStep<'a> {
        if PLUGIN_BANNER.is_match(core) {
            return SurefireStep::Consumed;
        }

        if RUNNING.is_match(core) {
            if self.in_block {
                self.flush_open_block_as_keep(out);
            }
            self.block_lines.clear();
            self.block_running = Some(line);
            self.in_block = true;
            self.failure_trail = false;
            // Load-bearing: a capped multi-failure class followed by a kept
            // class must not re-arm into the new class's trail decision.
            self.trail_rearm = None;
            return SurefireStep::Consumed;
        }

        if self.in_block {
            if let Some(caps) = CLOSE.captures(core) {
                let fail = caps.get(1).map(|m| m.as_str() != "0").unwrap_or(false);
                let err = caps.get(2).map(|m| m.as_str() != "0").unwrap_or(false);
                if fail || err {
                    let lines = std::mem::take(&mut self.block_lines);
                    let running = self.block_running.take();
                    self.in_block = false;
                    return SurefireStep::FailingClose {
                        running,
                        lines,
                        close: line,
                    };
                }
                self.block_lines.clear();
                self.block_running = None;
                self.in_block = false;
                return SurefireStep::Consumed;
            }
            self.block_lines.push(line);
            return SurefireStep::Consumed;
        }

        if self.failure_trail {
            if is_blank_separator(core) {
                if !self.drop_trail {
                    out.push('\n');
                }
                // Arm re-entry: a following per-test subline belongs to the
                // same class and must inherit this trail's keep/drop decision.
                self.trail_rearm = Some(self.drop_trail);
                self.failure_trail = false;
                self.drop_trail = false;
                return SurefireStep::Consumed;
            }
            let t = core.trim_start();
            if t.starts_with("at ") && is_framework_frame(t) {
                return SurefireStep::Consumed;
            }
            if self.drop_trail {
                return SurefireStep::Consumed;
            }
            out.push_str(line);
            out.push('\n');
            return SurefireStep::Consumed;
        }

        if let Some(dropped) = self.trail_rearm {
            if is_blank_separator(core) {
                // Tolerate extra blanks between per-test blocks: stay armed,
                // let the blank fall through (outer keep-lists drop it).
                return SurefireStep::Passthrough;
            }
            self.trail_rearm = None; // disarm unconditionally on non-blank (load-bearing)
            if is_per_test_subline(core) {
                self.failure_trail = true;
                self.drop_trail = dropped;
                if !dropped {
                    out.push_str(line);
                    out.push('\n');
                }
                return SurefireStep::Consumed;
            }
            // Non-subline: trail is over; already disarmed — fall through.
        }

        SurefireStep::Passthrough
    }

    /// Mark a `FailingClose` as dropped (cap exceeded). The block itself is
    /// already extracted by `step()`; this sets `failure_trail` so the
    /// post-close trail (per-test subline, exception, user frames) is
    /// consumed and silently dropped until the next blank line.
    fn drop_failing(&mut self) {
        self.failure_trail = true;
        self.drop_trail = true;
        // Belt-and-suspenders: a CLOSE can only follow a RUNNING (which
        // already cleared `trail_rearm`), but keep the invariant local too.
        self.trail_rearm = None;
    }

    /// Commit a `FailingClose` to `out`: writes `running`, then `lines` (with
    /// framework frames stripped), then `close`. Enables `failure_trail` so
    /// the post-close exception/user-frame trail is preserved.
    fn commit_failing(
        &mut self,
        out: &mut String,
        running: Option<&str>,
        lines: &[&str],
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
        // Belt-and-suspenders: see `drop_failing`.
        self.trail_rearm = None;
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
            out.push_str(r);
            out.push('\n');
        }
        for l in self.block_lines.drain(..) {
            out.push_str(l);
            out.push('\n');
        }
        self.in_block = false;
    }
}

/// `[ERROR] Failures:` summary block cap. Maven emits a summary at the end of
/// a failing test run:
///
/// ```text
/// [ERROR] Failures:
/// [ERROR]   ClassA.testFoo:25 expected: <a> but was: <b>
/// [ERROR]   ClassB.testBar:42 expected: <c> but was: <d>
/// [INFO]
/// [ERROR] Tests run: 100, Failures: 50, Errors: 0, Skipped: 0
/// ```
///
/// The aggregate `[ERROR] Tests run:` line is matched by `AGG` and kept; the
/// `[ERROR]   ` entries are kept by the catch-all `[ERROR]` keeper. On builds
/// with hundreds of failures this can be quite large. Cap entries at
/// [`MAX_MVN_FAILING_CLASSES`] and emit `\n… +N more failures\n` immediately
/// before the `Tests run:` aggregate when entries were dropped.
///
/// The budget is **reactor-wide**, not per module: a parallel reactor emits one
/// summary block per failing module, so each lane keeps its own `in_summary`
/// flag ([`SurefireLane::in_summary`]) while the entry count is shared. The
/// budget spans the whole filter invocation and never resets — whether module
/// summaries interleave or run back-to-back, the run keeps at most `cap`
/// entries total, never `modules × cap`. `dropped` alone resets when a tail is
/// emitted, so each `… +N more` reports the drops since the previous tail.
/// This also means a `verify` run whose Surefire and Failsafe phases each emit
/// a summary shares one budget across both — the second summary can get zero
/// entries, with the tail still reporting every drop. That is the documented
/// semantics: at most `cap` entries per run.
struct FailuresSummaryCap {
    cap: usize,
    emitted: usize,
    dropped: usize,
}

impl FailuresSummaryCap {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            emitted: 0,
            dropped: 0,
        }
    }

    /// If `core` is an `[ERROR]   ` entry inside the calling lane's failures
    /// summary, write `line` (the original, module prefix included) — or count
    /// it as dropped — and return `true` so the caller skips its own keep-list.
    /// Returns `false` otherwise.
    fn handle_entry(&mut self, in_summary: bool, core: &str, line: &str, out: &mut String) -> bool {
        if !in_summary || !core.starts_with("[ERROR]   ") {
            return false;
        }
        // Per core cap policy, `0` means summary-only: no entries, tail still counts.
        if self.emitted < self.cap {
            out.push_str(line);
            out.push('\n');
            self.emitted += 1;
        } else {
            self.dropped += 1;
        }
        true
    }

    /// Detect the `[ERROR] Failures:` header so subsequent `[ERROR]   ` lines
    /// get capped. Caller is responsible for writing the header to `out`.
    fn handle_header(&mut self, line: &str, in_summary: &mut bool) {
        if !line.starts_with("[ERROR] Failures:") || *in_summary {
            return;
        }
        *in_summary = true;
    }

    /// Pre-emit the `… +N more failures` tail when the aggregate
    /// `[ERROR] Tests run:` line is about to be written, then close this lane's
    /// summary. Caller writes the AGG line itself afterwards.
    fn handle_aggregate(&mut self, line: &str, out: &mut String, in_summary: &mut bool) {
        if !*in_summary || !AGG.is_match(line) {
            return;
        }
        if self.dropped > 0 {
            out.push_str(&format!("\n… +{} more failures\n", self.dropped));
            self.dropped = 0;
        }
        *in_summary = false;
    }

    /// End-of-stream tail emission for cases where the AGG line never arrives
    /// (truncated output). Emits the tail with no trailing newline guard so
    /// the resulting filtered output is still well-formed.
    fn finish(&mut self, out: &mut String) {
        if self.dropped > 0 {
            out.push_str(&format!("\n… +{} more failures\n", self.dropped));
        }
    }
}

/// Per-module filter state for parallel reactors: each module gets its own
/// Surefire block machine, continuation flag, and summary-open flag, because
/// mvnd interleaves module output line-by-line (a `[child-b]` close can land
/// between a `[child-a]` `Running` and its close). The failures-summary
/// *budget* is deliberately not here — see [`FailuresSummaryCap`].
struct SurefireLane<'a> {
    block: SurefireBlock<'a>,
    keep_continuation: bool,
    in_summary: bool,
}

impl<'a> SurefireLane<'a> {
    fn new() -> Self {
        Self {
            block: SurefireBlock::new(),
            keep_continuation: false,
            in_summary: false,
        }
    }
}

/// The set of per-module lanes plus the "hot" lane raw lines fall back to
/// when no block, trail, or armed continuation exists anywhere. `hot` has a
/// single writer — a failing close (stray raw lines after its trail ends
/// attribute to the failing lane's keep-list); every other ownership claim
/// (trails, open blocks, armed continuations) is resolved by
/// [`Lanes::raw_owner`] scanning per-lane state for a unique owner. Lane 0
/// (`""`) is the root lane, the only one a plain-`mvn` (or single-module
/// mvnd) run ever uses. Insertion order is preserved so end-of-stream
/// flushes are deterministic; lookups are a linear scan (a reactor has a
/// handful of modules, not thousands).
struct Lanes<'a> {
    lanes: Vec<(&'a str, SurefireLane<'a>)>,
    hot: usize,
}

impl<'a> Lanes<'a> {
    fn new() -> Self {
        Self {
            lanes: vec![("", SurefireLane::new())],
            hot: 0,
        }
    }

    fn get(&mut self, idx: usize) -> &mut SurefireLane<'a> {
        &mut self.lanes[idx].1
    }

    /// Lane index for a line: by module key for log-level lines (creating the
    /// lane on first sight), by ownership rules for raw lines. `None` means a
    /// raw line's ownership is genuinely ambiguous and the caller must
    /// preserve it verbatim rather than guess a lane that may drop it.
    fn route(&mut self, key: Option<&'a str>) -> Option<usize> {
        match key {
            Some(k) => Some(match self.lanes.iter().position(|(t, _)| *t == k) {
                Some(i) => i,
                None => {
                    self.lanes.push((k, SurefireLane::new()));
                    self.lanes.len() - 1
                }
            }),
            None => self.raw_owner(),
        }
    }

    /// Lane owning an unprefixed raw line (stack trace, stray stdout — mvnd
    /// emits these without a module tag even in parallel builds).
    ///
    /// One rule, applied literally: a raw line is routed only when its owner
    /// is **unique** — exactly one lane in a failure trail (trails outrank
    /// open blocks: that's where actionable diagnostics land), else exactly
    /// one lane with an open block. Any tie is genuine ambiguity → `None`,
    /// and the caller preserves the line verbatim rather than guessing a lane
    /// that may drop it. With nothing open at all there is no block or trail
    /// to misroute into, so the hot lane's outside-block keep-list decides.
    ///
    /// An armed compile continuation is itself a competing claim, wherever
    /// its lane sits — the arming lane need not be `hot`, since a failing
    /// close elsewhere steals `hot` unconditionally. Its raw `symbol:` /
    /// `location:` lines must be neither buffered into another lane's open
    /// block (destroyed on a green close) nor consumed by a trail (silently,
    /// if that trail is dropping), so any block or trail open alongside an
    /// armed lane is a tie too, and several armed lanes are a tie on their
    /// own. With nothing open, a unique armed lane outranks the hot lane —
    /// its continuations are the only raw lines anyone is expecting.
    ///
    /// Deliberate over-keep, never loss: when a tie preserves verbatim, a
    /// capped class's framework frames (or a passing block's stack chatter)
    /// can leak into the output for the duration of the tie — and an armed
    /// claim persists until its lane's next keyed line, so the tie window
    /// can outlive the continuations themselves. That trades a few noise
    /// lines for the guarantee that actionable diagnostics are never routed
    /// into a lane that discards them.
    ///
    /// A unique *dropping* trail consuming raw lines silently is loss-free
    /// even with blocks open concurrently: a dropping trail exists only once
    /// the class cap is exhausted ([`FailingClassCap::admit`] is monotonic),
    /// so a concurrent block's failing close would be capped and dropped
    /// too, and a green close discards its buffer by definition.
    fn raw_owner(&self) -> Option<usize> {
        let mut armed_lanes = self
            .lanes
            .iter()
            .enumerate()
            .filter(|(_, (_, l))| l.keep_continuation);
        let armed = match (armed_lanes.next(), armed_lanes.next()) {
            (Some((i, _)), None) => Some(i),
            (Some(_), Some(_)) => return None,
            _ => None,
        };
        let mut trails = self
            .lanes
            .iter()
            .enumerate()
            .filter(|(_, (_, l))| l.block.failure_trail);
        match (trails.next(), trails.next()) {
            (Some((i, _)), None) if armed.is_none() => return Some(i),
            (Some(_), _) => return None,
            _ => {}
        }
        let mut open = self
            .lanes
            .iter()
            .enumerate()
            .filter(|(_, (_, l))| l.block.in_block);
        match (open.next(), open.next()) {
            (Some((i, _)), None) if armed.is_none() => Some(i),
            (Some(_), _) => None,
            // Nothing open: the unique armed lane's continuation handling,
            // else the hot lane's outside-block keep-list, decides.
            _ => Some(armed.unwrap_or(self.hot)),
        }
    }

    /// End-of-stream flush of every lane's block machine, in lane order.
    fn finish(&mut self, out: &mut String) {
        for (_, lane) in &mut self.lanes {
            lane.block.finish(out);
        }
    }
}

/// Reactor-wide cap on emitted failing test classes, with the
/// `… +N more failing test classes` tail. Shared by
/// `filter_surefire_with_cap` and `filter_package_with_cap`.
struct FailingClassCap {
    cap: usize,
    emitted: usize,
    dropped: usize,
}

impl FailingClassCap {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            emitted: 0,
            dropped: 0,
        }
    }

    /// `true` when the next failing class still fits under the cap.
    fn admit(&mut self) -> bool {
        if self.emitted < self.cap {
            self.emitted += 1;
            true
        } else {
            self.dropped += 1;
            false
        }
    }

    fn finish(&self, out: &mut String) {
        if self.dropped > 0 {
            out.push_str(&format!(
                "\n… +{} more failing test classes\n",
                self.dropped
            ));
        }
    }
}

/// Shared per-line front half of `filter_surefire_with_cap` and
/// `filter_package_with_cap`: route the line to its lane, drive the lane's
/// Surefire block machine, and commit/drop failing closes against the
/// reactor-wide class cap. Returns `Some((lane index, core, keyed))` when
/// the line fell through to the caller's outside-block keep-list — `keyed`
/// is whether the line carried a module prefix (routed by key) or was raw
/// (routed by ownership claim); callers must only disarm a lane's armed
/// continuation on that lane's own keyed lines, since a raw line reached
/// the lane *because of* the claim. `None` when the line was consumed — or
/// preserved verbatim on ambiguous raw-line ownership.
fn drive_surefire_line<'a>(
    lanes: &mut Lanes<'a>,
    line: &'a str,
    classes: &mut FailingClassCap,
    out: &mut String,
) -> Option<(usize, &'a str, bool)> {
    let (key, core) = split_lane(line);
    let idx = match lanes.route(key) {
        Some(i) => i,
        None => {
            // Ambiguous ownership: preserve rather than risk dropping a
            // failing module's diagnostics into a passing block.
            out.push_str(line);
            out.push('\n');
            return None;
        }
    };

    let step = lanes.get(idx).block.step(line, core, out);
    // A lane inside a Surefire block has no pending javac continuations:
    // entering a block retires any stale armed claim, so a single lane can't
    // hold a permanent armed-vs-block tie against raw-line routing.
    if lanes.get(idx).block.in_block {
        lanes.get(idx).keep_continuation = false;
    }
    match step {
        SurefireStep::Consumed => None,
        SurefireStep::FailingClose {
            running,
            lines,
            close,
        } => {
            if classes.admit() {
                lanes.get(idx).block.commit_failing(out, running, &lines, close);
            } else {
                lanes.get(idx).block.drop_failing();
            }
            // While the trail is active, raw_owner routes by trail uniqueness;
            // `hot` claims only the nothing-open fallback for stray raw lines
            // after the trail ends.
            lanes.hot = idx;
            lanes.get(idx).keep_continuation = false;
            None
        }
        SurefireStep::Passthrough => Some((idx, core, key.is_some())),
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
    filter_surefire_with_cap(raw, MAX_MVN_FAILING_CLASSES)
}

fn filter_surefire_with_cap(raw: &str, cap: usize) -> String {
    let stripped = strip_ansi(raw);
    if !has_english_footer(&stripped) {
        return stripped;
    }

    let mut out = String::new();
    let mut lanes = Lanes::new();
    let mut classes = FailingClassCap::new(cap);
    let mut summary = FailuresSummaryCap::new(cap);
    let mut in_reactor_summary = false;

    for line in stripped.lines() {
        let (idx, core, keyed) =
            match drive_surefire_line(&mut lanes, line, &mut classes, &mut out) {
                Some(v) => v,
                None => continue,
            };
        if lanes.get(idx).keep_continuation && (core.starts_with(' ') || core.starts_with('\t')) {
            out.push_str(line);
            out.push('\n');
            continue;
        }

        // Failures-summary cap: gate `[ERROR]   ` entries, emit `+N more` tail
        // before AGG. The helper consumes only summary entries — other lines
        // (header, AGG) fall through to the keep-list below.
        if summary.handle_entry(lanes.get(idx).in_summary, core, line, &mut out) {
            continue;
        }

        // Order matters: call reactor_summary_keep first so its BUILD_FOOT
        // clears-flag side effect always runs regardless of `||` short-circuit.
        let reactor_keep = reactor_summary_keep(core, &mut in_reactor_summary);
        if reactor_keep || keep_outside_block(core) {
            // Pre-emit the summary tail when we're about to write AGG.
            summary.handle_aggregate(core, &mut out, &mut lanes.get(idx).in_summary);
            // Detect summary header so subsequent `[ERROR]   ` entries get capped.
            summary.handle_header(core, &mut lanes.get(idx).in_summary);
            out.push_str(line);
            out.push('\n');
            // The armed per-lane flag is an owner claim in its own right:
            // raw_owner routes (or verbatim-preserves) the raw indented
            // continuations by scanning all lanes for it. Only this lane's
            // own keyed lines may rewrite the claim (a kept raw line can't
            // arm anyway — starting with `[ERROR]` would have keyed it).
            if keyed {
                lanes.get(idx).keep_continuation = core.starts_with("[ERROR]")
                    && !core.starts_with("[ERROR] Tests run:")
                    && !core.starts_with("[ERROR] Failures:")
                    && !core.starts_with("[ERROR] Errors:");
            }
            continue;
        }
        // Dropped keyed line (e.g. help boilerplate): reset so a stale flag
        // can't keep an indented line that follows a dropped `[ERROR]` line.
        // Parity with filter_package's fall-through reset. Raw fall-through
        // lines never disarm: a raw line only routed here *because of* the
        // armed claim, so letting it clear that claim would drop the
        // `symbol:` / `location:` continuations whenever another module's
        // stray stdout lands in the arm window.
        if keyed {
            lanes.get(idx).keep_continuation = false;
        }
    }

    lanes.finish(&mut out);
    summary.finish(&mut out);
    classes.finish(&mut out);
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
    // Continuation ownership is per module: javac emits `symbol:` / `location:`
    // as raw indented lines *after* the `[ERROR] … cannot find symbol` line, and
    // mvnd interleaves reactor modules, so a `[child-b] [INFO]` line landing in
    // between must not clear the flag armed by `[child-a] [ERROR]`. Compile
    // never opens Surefire blocks, so `route` resolves raw lines to the unique
    // armed lane (or preserves them verbatim when several lanes are armed).
    let mut lanes = Lanes::new();
    let mut seen_warnings: HashSet<String> = HashSet::new();

    for line in stripped.lines() {
        // Classify on the module-prefix-stripped view; emit the original so
        // module identity survives in mvnd parallel reactors.
        let (key, core) = split_lane(line);
        let keyed = key.is_some();
        let idx = match lanes.route(key) {
            Some(i) => i,
            // Reachable when two modules are armed concurrently (a tie):
            // preserve the raw line verbatim rather than guess an owner.
            None => {
                out.push_str(line);
                out.push('\n');
                continue;
            }
        };
        if MODULE_BANNER.is_match(core) {
            out.push_str(line);
            out.push('\n');
            lanes.get(idx).keep_continuation = false;
            continue;
        }
        if BUILD_FOOT.is_match(core)
            || core.starts_with("[INFO] Building ")
            || core.starts_with("[INFO] Total time:")
            || core.starts_with("[INFO] Finished at:")
            || core.starts_with("[INFO] Scanning ")
        {
            out.push_str(line);
            out.push('\n');
            lanes.get(idx).keep_continuation = false;
            continue;
        }
        // Help boilerplate: drop before the `[ERROR]` catch-all (parity with
        // keep_outside_block / filter_quiet). Raw boilerplate must not
        // disarm the claim that routed it here — see the fall-through reset.
        if is_boilerplate(core) {
            if keyed {
                lanes.get(idx).keep_continuation = false;
            }
            continue;
        }
        if core.starts_with("[ERROR]") {
            out.push_str(line);
            out.push('\n');
            // Armed flag is an owner claim scanned by raw_owner — no `hot`
            // bookkeeping needed.
            lanes.get(idx).keep_continuation = true;
            continue;
        }
        if lanes.get(idx).keep_continuation && (core.starts_with(' ') || core.starts_with('\t')) {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if core.starts_with("[WARNING]") {
            let payload = core.strip_prefix("[WARNING] ").unwrap_or(core);
            let norm = FILE_COORD.replace_all(payload, "").to_string();
            if seen_warnings.insert(norm) {
                out.push_str(line);
                out.push('\n');
            }
            lanes.get(idx).keep_continuation = false;
            continue;
        }
        // Drop everything else. Only keyed lines disarm: a raw stray only
        // routed to this lane because of its armed claim, and clearing it
        // would drop the `symbol:` / `location:` continuations that follow.
        if keyed {
            lanes.get(idx).keep_continuation = false;
        }
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
    filter_package_with_cap(raw, MAX_MVN_FAILING_CLASSES)
}

fn filter_package_with_cap(raw: &str, cap: usize) -> String {
    let stripped = strip_ansi(raw);
    if !has_english_footer(&stripped) {
        return stripped;
    }

    let mut out = String::new();
    // Per-module lanes + raw-line routing: see drive_surefire_line.
    let mut lanes = Lanes::new();
    let mut classes = FailingClassCap::new(cap);
    let mut summary = FailuresSummaryCap::new(cap);
    let mut in_reactor_summary = false;
    // Warning dedup is deliberately global: the same warning surfacing from
    // several reactor modules is still the same warning.
    let mut seen_warnings: HashSet<String> = HashSet::new();

    for line in stripped.lines() {
        let (idx, core, keyed) =
            match drive_surefire_line(&mut lanes, line, &mut classes, &mut out) {
                Some(v) => v,
                None => continue,
            };
        // Failures-summary cap (see filter_surefire_with_cap for details).
        if summary.handle_entry(lanes.get(idx).in_summary, core, line, &mut out) {
            continue;
        }

        // Order matters: call reactor_summary_keep first so its BUILD_FOOT
        // clears-flag side effect always runs regardless of `||` short-circuit.
        let reactor_keep = reactor_summary_keep(core, &mut in_reactor_summary);
        // Outside any Surefire block: compile-keep AND surefire-outside-keep merge.
        if reactor_keep || MODULE_BANNER.is_match(core) || keep_outside_block(core) {
            summary.handle_aggregate(core, &mut out, &mut lanes.get(idx).in_summary);
            summary.handle_header(core, &mut lanes.get(idx).in_summary);
            out.push_str(line);
            out.push('\n');
            // Armed flag is an owner claim scanned by raw_owner; only keyed
            // lines rewrite it — see filter_surefire_with_cap.
            if keyed {
                lanes.get(idx).keep_continuation = core.starts_with("[ERROR]")
                    && !core.starts_with("[ERROR] Tests run:")
                    && !core.starts_with("[ERROR] Failures:")
                    && !core.starts_with("[ERROR] Errors:");
            }
            continue;
        }
        if lanes.get(idx).keep_continuation && (core.starts_with(' ') || core.starts_with('\t')) {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if core.starts_with("[WARNING]") {
            // `[WARNING]`-prefixed lines are always keyed (split_lane keys
            // any `[`-initial log line), so this reset never hits a claim.
            let payload = core.strip_prefix("[WARNING] ").unwrap_or(core);
            let norm = FILE_COORD.replace_all(payload, "").to_string();
            if seen_warnings.insert(norm) {
                out.push_str(line);
                out.push('\n');
            }
            lanes.get(idx).keep_continuation = false;
            continue;
        }
        // Raw fall-through lines never disarm — see filter_surefire_with_cap.
        if keyed {
            lanes.get(idx).keep_continuation = false;
        }
    }

    lanes.finish(&mut out);
    summary.finish(&mut out);
    classes.finish(&mut out);
    out
}

// ── Quiet-mode filter ───────────────────────────────────────────────────────

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
            failure_trail =
                line.contains("<<< FAILURE!") || line.contains("<<< ERROR!");
            continue;
        }

        // Per-test failure subline: `[ERROR] FQN.method -- Time elapsed: … <<< FAILURE!`
        // (or `<<< ERROR!` for thrown exceptions).
        if is_per_test_subline(line) {
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

        // Drop post-failure help boilerplate and bare `[ERROR]` dividers
        // (shared with the non-quiet filters — see BOILER_PREFIXES).
        if is_boilerplate(line) {
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

/// Maven Daemon (`mvnd`) has no project-local wrapper of its own, so it is
/// never substituted by `./mvnw`: the user asked for the daemon explicitly.
fn mvn_binary(daemon: bool) -> &'static str {
    if daemon {
        "mvnd"
    } else if cfg!(windows) {
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

fn new_mvn_command(args: &[String], daemon: bool) -> Command {
    let mut cmd = if daemon {
        resolved_command("mvnd")
    } else if cfg!(windows) {
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
    run_tool(args, false, verbose)
}

/// `rtk mvnd` — Maven Daemon. Non-interactive `mvnd` output is plain Maven
/// output (the rolling console UI only engages on a TTY), so the same phase
/// detection and filters apply; only the executed binary differs.
pub fn run_daemon(args: &[String], verbose: u8) -> Result<i32> {
    run_tool(args, true, verbose)
}

fn run_tool(args: &[String], daemon: bool, verbose: u8) -> Result<i32> {
    // Verbose flags bypass filtering — user wants full output.
    if args
        .iter()
        .any(|a| matches!(a.as_str(), "-X" | "--debug" | "-e" | "--errors"))
    {
        let osargs: Vec<OsString> = args.iter().map(OsString::from).collect();
        return runner::run_passthrough(mvn_binary(daemon), &osargs, verbose);
    }

    let tool = mvn_binary(daemon);
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
            new_mvn_command(args, daemon),
            tool,
            &args_display,
            filter_quiet,
            RunOptions::with_tee("mvn_quiet"),
        );
    }

    let phase = detect_phase(args);

    match phase {
        MvnPhase::Test => runner::run_filtered(
            new_mvn_command(args, daemon),
            tool,
            &args_display,
            filter_surefire,
            RunOptions::with_tee("mvn_test"),
        ),
        MvnPhase::Compile => runner::run_filtered(
            new_mvn_command(args, daemon),
            tool,
            &args_display,
            filter_compile,
            RunOptions::with_tee("mvn_compile"),
        ),
        MvnPhase::Package => runner::run_filtered(
            new_mvn_command(args, daemon),
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

    // ── Binary selection ─────────────────────────────────────────────────────

    /// rtk-ai/rtk#3184 — the daemon is never swapped for `mvn`/`./mvnw`,
    /// whatever wrapper happens to sit in the working directory.
    #[test]
    fn mvnd_binary_is_never_the_wrapper() {
        assert_eq!(mvn_binary(true), "mvnd");
    }

    // ── Maven Daemon fixtures ────────────────────────────────────────────────
    //
    // Real output captured with Apache Maven Daemon 1.0.6 (Maven 3.9.16,
    // non-TTY) on the skeleton projects under `tests/fixtures/`. Non-TTY mvnd
    // output is plain Maven output plus daemon-specific lines with no
    // `[INFO]`-only shape the keep-lists would retain: `Processing build on
    // daemon <id>`, `BuildTimeEventSpy is registered.`, the SmartBuilder
    // thread-count line, and the concurrency stats block. Parallel reactor
    // builds additionally prefix per-module log lines with `[module] `.

    /// Warmed `mvnd clean install` on the multi-module skeleton: the parallel
    /// reactor case. Per-module `[module] [INFO] …` lines and daemon chatter
    /// are noise; the (unprefixed) reactor summary and footer are the signal.
    #[test]
    fn mvnd_reactor_pass_keeps_summary_drops_daemon_noise() {
        let i = include_str!("../../../tests/fixtures/mvnd_reactor_pass_raw.txt");
        let o = filter_package(i);
        assert!(o.contains("[INFO] Reactor Summary for multi-module-skeleton 1.0.0-SNAPSHOT:"));
        assert!(o.contains("child-a ............................................ SUCCESS"));
        assert!(o.contains("child-b ............................................ SUCCESS"));
        assert!(o.contains("[INFO] BUILD SUCCESS"));
        assert!(o.contains("[INFO] Total time:"));
        // Module identity survives on keeper lines (parity with plain-mvn
        // reactors, whose banners/artifact lines are kept).
        assert!(o.contains("[child-b] [INFO] Building jar:"));
        // Daemon chatter and per-module noise are dropped.
        assert!(!o.contains("Processing build on daemon"));
        assert!(!o.contains("BuildTimeEventSpy"));
        assert!(!o.contains("SmartBuilder"));
        assert!(!o.contains("Bottleneck projects"));
        assert!(!o.contains("skip non existing resourceDirectory"));
        assert!(!o.contains("[INFO] Deleting"));
    }

    #[test]
    fn mvnd_reactor_pass_savings() {
        let i = include_str!("../../../tests/fixtures/mvnd_reactor_pass_raw.txt");
        let o = filter_package(i);
        let savings = 100.0 - (count_tokens(&o) as f64 / count_tokens(i) as f64 * 100.0);
        assert!(savings >= 60.0, "expected >=60% savings, got {savings:.1}%");
    }

    /// Warmed `mvnd test` on the failing skeleton (exit code 1). Same Surefire
    /// shape as `mvn`: failure names, messages, and user stack frames survive;
    /// daemon chatter, framework frames, and help boilerplate do not.
    #[test]
    fn mvnd_test_fail_preserves_failures() {
        let i = include_str!("../../../tests/fixtures/mvnd_test_fail_raw.txt");
        let o = filter_surefire(i);
        assert!(o.contains("[INFO] Running com.example.rtk.BoomTest"));
        assert!(o.contains("[INFO] Running com.example.rtk.CalcTest"));
        assert!(o.contains("failOne: addition should equal five ==> expected: <5> but was: <4>"));
        assert!(o.contains("at com.example.rtk.CalcTest.failOne(CalcTest.java:12)"));
        assert!(o.contains("[ERROR]   CalcTest.failOne:12"));
        assert!(o.contains("[ERROR] Tests run: 16, Failures: 1, Errors: 2, Skipped: 0"));
        // Passing classes are collapsed entirely.
        assert!(!o.contains("PassOneTest"));
        assert!(o.contains("[INFO] BUILD FAILURE"));
        assert!(o.contains("[ERROR] Failed to execute goal"));
        // Daemon chatter, framework frames, and boilerplate are dropped.
        assert!(!o.contains("Processing build on daemon"));
        assert!(!o.contains("BuildTimeEventSpy"));
        assert!(!o.contains("SmartBuilder"));
        assert!(!o.contains("at java.base/"));
        assert!(!o.contains("Re-run Maven"));
    }

    #[test]
    fn mvnd_test_fail_savings() {
        let i = include_str!("../../../tests/fixtures/mvnd_test_fail_raw.txt");
        let o = filter_surefire(i);
        let savings = 100.0 - (count_tokens(&o) as f64 / count_tokens(i) as f64 * 100.0);
        assert!(savings >= 60.0, "expected >=60% savings, got {savings:.1}%");
    }

    /// Failing test inside a parallel reactor (`mvnd clean test` on the
    /// multi-module-fail-skeleton, exit code 1): module output interleaves
    /// line-by-line and per-module lines carry a `[module] ` prefix, while
    /// the assertion message and stack frames arrive raw (unprefixed). The
    /// failing class, its message, user frame, and summary entry must all
    /// survive — with module identity — and the interleaved passing classes
    /// from the other module must not bleed into the failing block.
    #[test]
    fn mvnd_parallel_reactor_fail_preserves_diagnostics() {
        let i = include_str!("../../../tests/fixtures/mvnd_reactor_fail_raw.txt");
        let o = filter_surefire(i);
        assert!(o.contains("[child-a] [INFO] Running com.example.rtk.ParallelFailTest"));
        assert!(o.contains(
            "[child-a] [ERROR] Tests run: 1, Failures: 1, Errors: 0, Skipped: 0, Time elapsed:"
        ));
        assert!(o.contains("parallel reactor diagnostic ==> expected: <1> but was: <2>"));
        assert!(o.contains("at com.example.rtk.ParallelFailTest.reactorDiagnostic(ParallelFailTest.java:10)"));
        assert!(o.contains("[child-a] [ERROR]   ParallelFailTest.reactorDiagnostic:10"));
        assert!(o.contains("[child-a] [ERROR] Tests run: 3, Failures: 1, Errors: 0, Skipped: 0"));
        // Reactor summary keeps the per-module verdicts.
        assert!(o.contains("child-a ............................................ FAILURE"));
        assert!(o.contains("[INFO] BUILD FAILURE"));
        assert!(o.contains("[ERROR] Failed to execute goal"));
        // Interleaved passing classes are collapsed; a passing close from the
        // other module must never be attributed to the failing block.
        assert!(!o.contains("PassBetaTest"));
        assert!(!o.contains("PassGammaTest"));
        assert!(!o.contains("PassAlphaTest"));
        // Framework frames, daemon chatter, and boilerplate are dropped.
        assert!(!o.contains("at org.junit.jupiter"));
        assert!(!o.contains("at java.base/"));
        assert!(!o.contains("Processing build on daemon"));
        assert!(!o.contains("Re-run Maven"));
    }

    #[test]
    fn mvnd_parallel_reactor_fail_savings() {
        let i = include_str!("../../../tests/fixtures/mvnd_reactor_fail_raw.txt");
        let o = filter_surefire(i);
        let savings = 100.0 - (count_tokens(&o) as f64 / count_tokens(i) as f64 * 100.0);
        assert!(savings >= 60.0, "expected >=60% savings, got {savings:.1}%");
    }

    // The tests below use inline strings rather than captures: they pin
    // *interleavings* (which module's line lands between which two others),
    // and a real `mvnd` run can't be made to emit a chosen interleaving on
    // demand — the captured fixtures above happen to keep each failure trail
    // contiguous. Line shapes are copied verbatim from
    // `mvnd_reactor_fail_raw.txt` / `mvnd_compile_error_raw.txt`, only the
    // ordering is arranged.

    /// Another module opening a Surefire block *between* a failing close and
    /// its raw (unprefixed) exception trail must not steal the trail: routing
    /// those lines into the passing block discards them when it closes green.
    /// An active failure trail outranks an ordinary open block. Asserted on
    /// both entry points that drive the lane machinery.
    fn assert_interleaved_trail_survives(filter: fn(&str) -> String) {
        let i = "[INFO] Scanning for projects...\n\
             [child-a] [INFO] Running com.example.rtk.ParallelFailTest\n\
             [child-b] [INFO] Running com.example.rtk.PassBetaTest\n\
             [child-b] [INFO] Tests run: 2, Failures: 0, Errors: 0, Skipped: 0, Time elapsed: 0.157 s -- in com.example.rtk.PassBetaTest\n\
             [child-a] [ERROR] Tests run: 1, Failures: 1, Errors: 0, Skipped: 0, Time elapsed: 0.153 s <<< FAILURE! -- in com.example.rtk.ParallelFailTest\n\
             [child-a] [ERROR] com.example.rtk.ParallelFailTest.reactorDiagnostic -- Time elapsed: 0.098 s <<< FAILURE!\n\
             [child-b] [INFO] Running com.example.rtk.PassGammaTest\n\
             org.opentest4j.AssertionFailedError: parallel reactor diagnostic ==> expected: <1> but was: <2>\n\
             \tat com.example.rtk.ParallelFailTest.reactorDiagnostic(ParallelFailTest.java:10)\n\
             [child-b] [INFO] Tests run: 1, Failures: 0, Errors: 0, Skipped: 0, Time elapsed: 0.009 s -- in com.example.rtk.PassGammaTest\n\
             [child-a] [INFO] \n\
             [INFO] BUILD FAILURE\n";
        let o = filter(i);
        assert!(
            o.contains("parallel reactor diagnostic ==> expected: <1> but was: <2>"),
            "assertion message survives the interleave; got:\n{o}"
        );
        assert!(
            o.contains(
                "at com.example.rtk.ParallelFailTest.reactorDiagnostic(ParallelFailTest.java:10)"
            ),
            "user frame survives the interleave; got:\n{o}"
        );
        // The interleaving module's passing classes are still collapsed.
        assert!(!o.contains("PassBetaTest"), "got:\n{o}");
        assert!(!o.contains("PassGammaTest"), "got:\n{o}");
    }

    #[test]
    fn mvnd_interleaved_block_does_not_steal_failure_trail() {
        assert_interleaved_trail_survives(filter_surefire);
    }

    #[test]
    fn mvnd_package_interleaved_block_does_not_steal_failure_trail() {
        assert_interleaved_trail_survives(filter_package);
    }

    /// Raw lines with no unambiguous owner — several plain blocks open, no
    /// failure trail — are preserved verbatim rather than buffered into a
    /// guessed lane that may drop them.
    fn assert_ambiguous_raw_line_preserved(filter: fn(&str) -> String) {
        let i = "[INFO] Scanning for projects...\n\
             [child-a] [INFO] Running com.example.rtk.PassAlphaTest\n\
             [child-b] [INFO] Running com.example.rtk.PassBetaTest\n\
             java.lang.IllegalStateException: stray reactor stdout\n\
             [child-a] [INFO] Tests run: 2, Failures: 0, Errors: 0, Skipped: 0, Time elapsed: 0.026 s -- in com.example.rtk.PassAlphaTest\n\
             [child-b] [INFO] Tests run: 2, Failures: 0, Errors: 0, Skipped: 0, Time elapsed: 0.157 s -- in com.example.rtk.PassBetaTest\n\
             [INFO] BUILD SUCCESS\n";
        let o = filter(i);
        assert!(
            o.contains("stray reactor stdout"),
            "ambiguous raw line preserved; got:\n{o}"
        );
    }

    #[test]
    fn mvnd_ambiguous_raw_line_is_preserved() {
        assert_ambiguous_raw_line_preserved(filter_surefire);
    }

    #[test]
    fn mvnd_package_ambiguous_raw_line_is_preserved() {
        assert_ambiguous_raw_line_preserved(filter_package);
    }

    /// javac emits `symbol:` / `location:` as raw indented lines *after* the
    /// `[ERROR] … cannot find symbol` line. A line from another reactor module
    /// landing in between must not clear the continuation flag — ownership is
    /// per lane.
    #[test]
    fn mvnd_parallel_compile_keeps_interleaved_continuations() {
        let i = "[INFO] Scanning for projects...\n\
             [child-a] [ERROR] /C:/work/child-a/src/main/java/com/example/rtk/A.java:[7,9] cannot find symbol\n\
             [child-b] [INFO] Compiling 1 source file with javac [debug target 21] to target\\classes\n\
             \x20 symbol:   variable bar\n\
             \x20 location: class com.example.rtk.A\n\
             [INFO] BUILD FAILURE\n";
        let o = filter_compile(i);
        assert!(
            o.contains("symbol:   variable bar"),
            "continuation survives the interleave; got:\n{o}"
        );
        assert!(
            o.contains("location: class com.example.rtk.A"),
            "continuation survives the interleave; got:\n{o}"
        );
    }

    /// The `[ERROR] Failures:` cap is reactor-wide: two modules' summaries
    /// interleaving must share one budget, not get `modules × cap` entries.
    fn assert_summary_cap_shared_across_lanes(filter: fn(&str, usize) -> String) {
        let i = "[INFO] Scanning for projects...\n\
             [child-a] [ERROR] Failures: \n\
             [child-a] [ERROR]   ChildATest.one:11 boom a1\n\
             [child-b] [ERROR] Failures: \n\
             [child-b] [ERROR]   ChildBTest.one:11 boom b1\n\
             [child-a] [ERROR]   ChildATest.two:12 boom a2\n\
             [child-b] [ERROR]   ChildBTest.two:12 boom b2\n\
             [child-a] [ERROR] Tests run: 4, Failures: 2, Errors: 0, Skipped: 0\n\
             [child-b] [ERROR] Tests run: 4, Failures: 2, Errors: 0, Skipped: 0\n\
             [INFO] BUILD FAILURE\n";
        let o = filter(i, 2);
        assert_eq!(
            o.matches("boom ").count(),
            2,
            "cap=2 bounds the whole reactor, not each module; got:\n{o}"
        );
        assert!(
            o.contains("… +2 more failures"),
            "tail counts both dropped entries; got:\n{o}"
        );
    }

    #[test]
    fn mvnd_failures_summary_cap_is_shared_across_lanes() {
        assert_summary_cap_shared_across_lanes(filter_surefire_with_cap);
    }

    #[test]
    fn mvnd_package_failures_summary_cap_is_shared_across_lanes() {
        assert_summary_cap_shared_across_lanes(filter_package_with_cap);
    }

    /// The failures-summary budget spans the whole invocation: module
    /// summaries that run back-to-back (child A opens and closes its summary
    /// before child B opens) share one budget too — sequential lanes must not
    /// each get a fresh `cap`.
    fn assert_summary_cap_spans_sequential_lanes(filter: fn(&str, usize) -> String) {
        let i = "[INFO] Scanning for projects...\n\
             [child-a] [ERROR] Failures: \n\
             [child-a] [ERROR]   ChildATest.one:11 boom a1\n\
             [child-a] [ERROR]   ChildATest.two:12 boom a2\n\
             [child-a] [ERROR] Tests run: 4, Failures: 2, Errors: 0, Skipped: 0\n\
             [child-b] [ERROR] Failures: \n\
             [child-b] [ERROR]   ChildBTest.one:11 boom b1\n\
             [child-b] [ERROR]   ChildBTest.two:12 boom b2\n\
             [child-b] [ERROR] Tests run: 4, Failures: 2, Errors: 0, Skipped: 0\n\
             [INFO] BUILD FAILURE\n";
        let o = filter(i, 2);
        assert_eq!(
            o.matches("boom ").count(),
            2,
            "cap=2 bounds the whole run, sequential summaries included; got:\n{o}"
        );
        assert!(
            o.contains("… +2 more failures"),
            "tail reports the dropped second-module entries; got:\n{o}"
        );
    }

    #[test]
    fn mvnd_failures_summary_cap_spans_sequential_lanes() {
        assert_summary_cap_spans_sequential_lanes(filter_surefire_with_cap);
    }

    #[test]
    fn mvnd_package_failures_summary_cap_spans_sequential_lanes() {
        assert_summary_cap_spans_sequential_lanes(filter_package_with_cap);
    }

    /// A compile error surfacing inside a test/package run: the raw indented
    /// `symbol:` / `location:` continuations must route back to the module
    /// that armed them even when another module's line lands in between —
    /// arming claims raw-line ownership on these paths too, not just in
    /// `filter_compile`.
    fn assert_interleaved_compile_continuation_survives(filter: fn(&str) -> String) {
        let i = "[INFO] Scanning for projects...\n\
             [child-a] [ERROR] /C:/work/child-a/src/main/java/com/example/rtk/A.java:[7,9] cannot find symbol\n\
             [child-b] [INFO] Compiling 1 source file with javac [debug target 21] to target\\classes\n\
             \x20 symbol:   variable bar\n\
             \x20 location: class com.example.rtk.A\n\
             [INFO] BUILD FAILURE\n";
        let o = filter(i);
        assert!(
            o.contains("symbol:   variable bar"),
            "continuation survives the interleave; got:\n{o}"
        );
        assert!(
            o.contains("location: class com.example.rtk.A"),
            "continuation survives the interleave; got:\n{o}"
        );
    }

    #[test]
    fn mvnd_surefire_interleaved_compile_continuation_survives() {
        assert_interleaved_compile_continuation_survives(filter_surefire);
    }

    #[test]
    fn mvnd_package_interleaved_compile_continuation_survives() {
        assert_interleaved_compile_continuation_survives(filter_package);
    }

    // ── Exhaustive interleaving sweeps ──────────────────────────────────────
    //
    // mvnd's scheduler controls interleaving, not us: any order-preserving
    // merge of two modules' output is a run that can really happen. Rather
    // than pinning hand-picked orderings one review round at a time, sweep
    // every merge and assert the failure signal survives all of them —
    // routed into its lane or preserved verbatim, never dropped.

    /// All order-preserving merges of `a` and `b` (each module's own lines
    /// keep their order; the interleaving varies).
    fn merges<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<Vec<&'a str>> {
        fn rec<'a>(
            a: &[&'a str],
            b: &[&'a str],
            prefix: &mut Vec<&'a str>,
            out: &mut Vec<Vec<&'a str>>,
        ) {
            if a.is_empty() && b.is_empty() {
                out.push(prefix.clone());
                return;
            }
            if let Some((&h, t)) = a.split_first() {
                prefix.push(h);
                rec(t, b, prefix, out);
                prefix.pop();
            }
            if let Some((&h, t)) = b.split_first() {
                prefix.push(h);
                rec(a, t, prefix, out);
                prefix.pop();
            }
        }
        let mut out = Vec::new();
        rec(a, b, &mut Vec::new(), &mut out);
        out
    }

    fn sweep_input(merge: &[&str]) -> String {
        format!(
            "[INFO] Scanning for projects...\n{}\n[INFO] BUILD FAILURE\n",
            merge.join("\n")
        )
    }

    /// child-a: one failing class — Running, failing close, per-test subline,
    /// raw exception message, raw user frame, blank trail terminator. Line
    /// shapes copied from `mvnd_reactor_fail_raw.txt`.
    const SWEEP_FAIL_A: [&str; 6] = [
        "[child-a] [INFO] Running com.example.rtk.ParallelFailTest",
        "[child-a] [ERROR] Tests run: 1, Failures: 1, Errors: 0, Skipped: 0, Time elapsed: 0.153 s <<< FAILURE! -- in com.example.rtk.ParallelFailTest",
        "[child-a] [ERROR] com.example.rtk.ParallelFailTest.reactorDiagnostic -- Time elapsed: 0.098 s <<< FAILURE!",
        "org.opentest4j.AssertionFailedError: parallel reactor diagnostic ==> expected: <1> but was: <2>",
        "\tat com.example.rtk.ParallelFailTest.reactorDiagnostic(ParallelFailTest.java:10)",
        "[child-a] [INFO] ",
    ];

    /// child-b: two passing classes (open/close, open/close).
    const SWEEP_PASS_B: [&str; 4] = [
        "[child-b] [INFO] Running com.example.rtk.PassBetaTest",
        "[child-b] [INFO] Tests run: 2, Failures: 0, Errors: 0, Skipped: 0, Time elapsed: 0.157 s -- in com.example.rtk.PassBetaTest",
        "[child-b] [INFO] Running com.example.rtk.PassGammaTest",
        "[child-b] [INFO] Tests run: 1, Failures: 0, Errors: 0, Skipped: 0, Time elapsed: 0.009 s -- in com.example.rtk.PassGammaTest",
    ];

    /// child-a variant: a compile error with raw indented continuations —
    /// arming a continuation must survive racing another module's open block.
    const SWEEP_COMPILE_A: [&str; 3] = [
        "[child-a] [ERROR] /C:/work/child-a/src/main/java/com/example/rtk/A.java:[7,9] cannot find symbol",
        "  symbol:   variable bar",
        "  location: class com.example.rtk.A",
    ];

    /// child-b variant: one failing class of its own, for the capped sweep.
    const SWEEP_FAIL_B: [&str; 6] = [
        "[child-b] [INFO] Running com.example.rtk.OtherFailTest",
        "[child-b] [ERROR] Tests run: 1, Failures: 1, Errors: 0, Skipped: 0, Time elapsed: 0.120 s <<< FAILURE! -- in com.example.rtk.OtherFailTest",
        "[child-b] [ERROR] com.example.rtk.OtherFailTest.otherDiagnostic -- Time elapsed: 0.080 s <<< FAILURE!",
        "org.opentest4j.AssertionFailedError: other reactor diagnostic ==> expected: <3> but was: <4>",
        "\tat com.example.rtk.OtherFailTest.otherDiagnostic(OtherFailTest.java:8)",
        "[child-b] [INFO] ",
    ];

    /// Failing module × passing module, all 210 merges: the failure's close
    /// line, assertion message, and user frame survive every interleaving,
    /// and the passing module's classes stay collapsed in every one.
    fn assert_every_interleaving_keeps_diagnostics(filter: fn(&str) -> String) {
        for (n, m) in merges(&SWEEP_FAIL_A, &SWEEP_PASS_B).iter().enumerate() {
            let i = sweep_input(m);
            let o = filter(&i);
            assert!(
                o.contains("expected: <1> but was: <2>")
                    && o.contains("ParallelFailTest.reactorDiagnostic(ParallelFailTest.java:10)")
                    && o.contains("<<< FAILURE! -- in com.example.rtk.ParallelFailTest"),
                "merge #{n} lost failure signal;\ninput:\n{i}\noutput:\n{o}"
            );
            assert!(
                !o.contains("PassBetaTest") && !o.contains("PassGammaTest"),
                "merge #{n} leaked passing classes;\ninput:\n{i}\noutput:\n{o}"
            );
        }
    }

    #[test]
    fn mvnd_every_interleaving_keeps_diagnostics() {
        assert_every_interleaving_keeps_diagnostics(filter_surefire);
    }

    #[test]
    fn mvnd_package_every_interleaving_keeps_diagnostics() {
        assert_every_interleaving_keeps_diagnostics(filter_package);
    }

    /// Two failing modules under `cap = 1`, all 924 merges: whichever class
    /// the cap admits keeps its full diagnostics in every interleaving — a
    /// capped (dropping) trail in one lane must never swallow the admitted
    /// lane's raw exception or frames — and the `+1 more` tail always
    /// reports the capped class.
    fn assert_every_interleaving_keeps_admitted_class(filter: fn(&str, usize) -> String) {
        for (n, m) in merges(&SWEEP_FAIL_A, &SWEEP_FAIL_B).iter().enumerate() {
            let i = sweep_input(m);
            let o = filter(&i, 1);
            let a = o.contains("<<< FAILURE! -- in com.example.rtk.ParallelFailTest");
            let b = o.contains("<<< FAILURE! -- in com.example.rtk.OtherFailTest");
            assert!(
                a != b,
                "merge #{n}: exactly one class admitted under cap=1;\ninput:\n{i}\noutput:\n{o}"
            );
            let (msg, frame) = if a {
                (
                    "expected: <1> but was: <2>",
                    "ParallelFailTest.reactorDiagnostic(ParallelFailTest.java:10)",
                )
            } else {
                (
                    "expected: <3> but was: <4>",
                    "OtherFailTest.otherDiagnostic(OtherFailTest.java:8)",
                )
            };
            assert!(
                o.contains(msg) && o.contains(frame),
                "merge #{n}: admitted class lost its diagnostics;\ninput:\n{i}\noutput:\n{o}"
            );
            assert!(
                o.contains("+1 more failing test classes"),
                "merge #{n}: cap tail missing;\ninput:\n{i}\noutput:\n{o}"
            );
        }
    }

    #[test]
    fn mvnd_every_interleaving_keeps_admitted_class() {
        assert_every_interleaving_keeps_admitted_class(filter_surefire_with_cap);
    }

    #[test]
    fn mvnd_package_every_interleaving_keeps_admitted_class() {
        assert_every_interleaving_keeps_admitted_class(filter_package_with_cap);
    }

    /// Compile-error module × passing test module, all 35 merges: the raw
    /// `symbol:` / `location:` continuations survive every interleaving —
    /// in particular when they race another module's open Surefire block,
    /// which must not buffer them into a green close that discards them.
    fn assert_every_interleaving_keeps_compile_continuation(filter: fn(&str) -> String) {
        for (n, m) in merges(&SWEEP_COMPILE_A, &SWEEP_PASS_B).iter().enumerate() {
            let i = sweep_input(m);
            let o = filter(&i);
            assert!(
                o.contains("symbol:   variable bar")
                    && o.contains("location: class com.example.rtk.A"),
                "merge #{n} lost compile continuation;\ninput:\n{i}\noutput:\n{o}"
            );
            assert!(
                !o.contains("PassBetaTest") && !o.contains("PassGammaTest"),
                "merge #{n} leaked passing classes;\ninput:\n{i}\noutput:\n{o}"
            );
        }
    }

    #[test]
    fn mvnd_every_interleaving_keeps_compile_continuation() {
        assert_every_interleaving_keeps_compile_continuation(filter_surefire);
    }

    #[test]
    fn mvnd_package_every_interleaving_keeps_compile_continuation() {
        assert_every_interleaving_keeps_compile_continuation(filter_package);
    }

    /// Compile-error module × *failing* test module, all 84 merges: the armed
    /// continuation claim must survive a failing close stealing `hot` — the
    /// admitted class's diagnostics and the compile continuations both
    /// survive every interleaving.
    fn assert_every_interleaving_keeps_continuation_and_failure(filter: fn(&str) -> String) {
        for (n, m) in merges(&SWEEP_COMPILE_A, &SWEEP_FAIL_B).iter().enumerate() {
            let i = sweep_input(m);
            let o = filter(&i);
            assert!(
                o.contains("symbol:   variable bar")
                    && o.contains("location: class com.example.rtk.A"),
                "merge #{n} lost compile continuation;\ninput:\n{i}\noutput:\n{o}"
            );
            assert!(
                o.contains("expected: <3> but was: <4>")
                    && o.contains("OtherFailTest.otherDiagnostic(OtherFailTest.java:8)")
                    && o.contains("<<< FAILURE! -- in com.example.rtk.OtherFailTest"),
                "merge #{n} lost failure signal;\ninput:\n{i}\noutput:\n{o}"
            );
        }
    }

    #[test]
    fn mvnd_every_interleaving_keeps_continuation_and_failure() {
        assert_every_interleaving_keeps_continuation_and_failure(filter_surefire);
    }

    #[test]
    fn mvnd_package_every_interleaving_keeps_continuation_and_failure() {
        assert_every_interleaving_keeps_continuation_and_failure(filter_package);
    }

    /// Dropping-trail variant of the orphaned-continuation case, cap=1: A's
    /// class is admitted, B's is capped (its trail is consuming raw lines
    /// silently), and C arms a compile continuation. The continuations land
    /// while B's dropping trail is active — an armed lane alongside a trail
    /// is a tie, so they must be preserved verbatim, not swallowed by the
    /// dropping trail.
    fn assert_continuation_survives_dropping_trail(filter: fn(&str, usize) -> String) {
        let i = "[INFO] Scanning for projects...\n\
             [child-a] [INFO] Running com.example.rtk.ParallelFailTest\n\
             [child-a] [ERROR] Tests run: 1, Failures: 1, Errors: 0, Skipped: 0, Time elapsed: 0.153 s <<< FAILURE! -- in com.example.rtk.ParallelFailTest\n\
             org.opentest4j.AssertionFailedError: parallel reactor diagnostic ==> expected: <1> but was: <2>\n\
             [child-a] [INFO] \n\
             [child-b] [INFO] Running com.example.rtk.OtherFailTest\n\
             [child-b] [ERROR] Tests run: 1, Failures: 1, Errors: 0, Skipped: 0, Time elapsed: 0.120 s <<< FAILURE! -- in com.example.rtk.OtherFailTest\n\
             [child-c] [ERROR] /C:/work/child-c/src/main/java/com/example/rtk/C.java:[7,9] cannot find symbol\n\
             \x20 symbol:   variable bar\n\
             \x20 location: class com.example.rtk.C\n\
             [child-b] [INFO] \n\
             [INFO] BUILD FAILURE\n";
        let o = filter(i, 1);
        assert!(
            o.contains("symbol:   variable bar") && o.contains("location: class com.example.rtk.C"),
            "continuations survive a concurrent dropping trail; got:\n{o}"
        );
        assert!(
            o.contains("expected: <1> but was: <2>"),
            "admitted class keeps its diagnostics; got:\n{o}"
        );
        assert!(
            o.contains("+1 more failing test classes"),
            "capped class reported in the tail; got:\n{o}"
        );
    }

    #[test]
    fn mvnd_continuation_survives_dropping_trail() {
        assert_continuation_survives_dropping_trail(filter_surefire_with_cap);
    }

    #[test]
    fn mvnd_package_continuation_survives_dropping_trail() {
        assert_continuation_survives_dropping_trail(filter_package_with_cap);
    }

    /// Entering a Surefire block retires a lane's stale armed claim: a lane
    /// that armed a continuation and then opened its own block must not hold
    /// a permanent armed-vs-block tie that leaks its in-block stdout
    /// verbatim past a green close.
    fn assert_block_entry_retires_armed_claim(filter: fn(&str) -> String) {
        let i = "[INFO] Scanning for projects...\n\
             [child-a] [ERROR] /C:/work/child-a/src/main/java/com/example/rtk/A.java:[7,9] cannot find symbol\n\
             [child-a] [INFO] Running com.example.rtk.PassAlphaTest\n\
             stray in-block stdout line\n\
             [child-a] [INFO] Tests run: 2, Failures: 0, Errors: 0, Skipped: 0, Time elapsed: 0.026 s -- in com.example.rtk.PassAlphaTest\n\
             [INFO] BUILD FAILURE\n";
        let o = filter(i);
        assert!(
            !o.contains("stray in-block stdout line"),
            "green-closing block's stdout stays collapsed after arm retires; got:\n{o}"
        );
        assert!(
            o.contains("cannot find symbol"),
            "the [ERROR] diagnostic line itself survives; got:\n{o}"
        );
    }

    #[test]
    fn mvnd_block_entry_retires_armed_claim() {
        assert_block_entry_retires_armed_claim(filter_surefire);
    }

    #[test]
    fn mvnd_package_block_entry_retires_armed_claim() {
        assert_block_entry_retires_armed_claim(filter_package);
    }

    /// An unrelated raw stdout line (another module's, unprefixed) must not
    /// disarm a pending continuation claim: raw fall-through lines never
    /// reset `keep_continuation` — only a lane's own keyed lines do. Swept
    /// through every position of the compile-error sequence, on all three
    /// filter paths.
    fn assert_raw_stray_does_not_disarm_continuation(filter: fn(&str) -> String) {
        const STRAY: [&str; 1] = ["stray stdout from another reactor module"];
        for (n, m) in merges(&SWEEP_COMPILE_A, &STRAY).iter().enumerate() {
            let i = sweep_input(m);
            let o = filter(&i);
            assert!(
                o.contains("symbol:   variable bar")
                    && o.contains("location: class com.example.rtk.A"),
                "stray at position #{n} disarmed the continuation;\ninput:\n{i}\noutput:\n{o}"
            );
        }
    }

    #[test]
    fn mvnd_raw_stray_does_not_disarm_continuation() {
        assert_raw_stray_does_not_disarm_continuation(filter_surefire);
    }

    #[test]
    fn mvnd_package_raw_stray_does_not_disarm_continuation() {
        assert_raw_stray_does_not_disarm_continuation(filter_package);
    }

    #[test]
    fn mvnd_compile_raw_stray_does_not_disarm_continuation() {
        assert_raw_stray_does_not_disarm_continuation(filter_compile);
    }

    // Snapshot regression tests locking the full filtered output of every
    // mvnd fixture (insta, per docs/contributing/CODING_PRACTICES.md) — the
    // substring assertions above document intent; the snapshots catch
    // everything else.

    #[test]
    fn mvnd_reactor_pass_snapshot() {
        let i = include_str!("../../../tests/fixtures/mvnd_reactor_pass_raw.txt");
        insta::assert_snapshot!(filter_package(i));
    }

    #[test]
    fn mvnd_test_fail_snapshot() {
        let i = include_str!("../../../tests/fixtures/mvnd_test_fail_raw.txt");
        insta::assert_snapshot!(filter_surefire(i));
    }

    #[test]
    fn mvnd_parallel_reactor_fail_snapshot() {
        let i = include_str!("../../../tests/fixtures/mvnd_reactor_fail_raw.txt");
        insta::assert_snapshot!(filter_surefire(i));
    }

    #[test]
    fn mvnd_compile_error_snapshot() {
        let i = include_str!("../../../tests/fixtures/mvnd_compile_error_raw.txt");
        insta::assert_snapshot!(filter_compile(i));
    }

    /// `mvnd compile` on a syntax error (exit code 1): compile diagnostics
    /// (file, coordinates, message) survive the compile filter.
    #[test]
    fn mvnd_compile_error_preserves_diagnostics() {
        let i = include_str!("../../../tests/fixtures/mvnd_compile_error_raw.txt");
        let o = filter_compile(i);
        assert!(o.contains("Calc.java:[5,21] ';' expected"));
        assert!(o.contains("[INFO] BUILD FAILURE"));
        assert!(o.contains("[ERROR] Failed to execute goal"));
        assert!(!o.contains("Processing build on daemon"));
        assert!(!o.contains("BuildTimeEventSpy"));
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

    // ── Multi-failure class (trail re-arm) ──────────────────────────────────

    /// Surefire 3.x emits one blank-separated detail block per failing test
    /// under a single CLOSE line. All per-test exception messages must survive
    /// (not just the first), framework frames must stay stripped throughout.
    /// Real fixture: `CalcTest` (1 failure + 1 error) + `BoomTest` (errors-only).
    #[test]
    fn surefire_keeps_all_failures_in_multi_failure_class() {
        let i = include_str!("../../../tests/fixtures/mvn_test_multifail_slice_raw.txt");
        let o = filter_surefire(i);
        assert!(
            o.contains("AssertionFailedError: failOne: addition should equal five"),
            "first failure message preserved; got:\n{}",
            o
        );
        assert!(
            o.contains("IllegalStateException: failTwo: induced error"),
            "second failure (ERROR! subline) message preserved; got:\n{}",
            o
        );
        assert!(
            o.contains("at com.example.rtk.CalcTest.failOne(CalcTest.java:12)"),
            "first user frame preserved; got:\n{}",
            o
        );
        assert!(
            o.contains("at com.example.rtk.CalcTest.failTwo(CalcTest.java:17)"),
            "second user frame preserved; got:\n{}",
            o
        );
        assert!(
            !o.contains("at org.junit."),
            "junit frames stripped; got:\n{}",
            o
        );
        assert!(
            !o.contains("at java.base/"),
            "jdk frames stripped; got:\n{}",
            o
        );
    }

    /// Same multi-failure fixture through `filter_package` (drift guard —
    /// the install/verify route shares `SurefireBlock` and must not diverge).
    #[test]
    fn package_keeps_all_failures_in_multi_failure_class() {
        let i = include_str!("../../../tests/fixtures/mvn_test_multifail_slice_raw.txt");
        let o = filter_package(i);
        assert!(
            o.contains("AssertionFailedError: failOne: addition should equal five"),
            "first failure message preserved; got:\n{}",
            o
        );
        assert!(
            o.contains("IllegalStateException: failTwo: induced error"),
            "second failure message preserved; got:\n{}",
            o
        );
        assert!(
            !o.contains("at org.junit."),
            "junit frames stripped; got:\n{}",
            o
        );
        assert!(
            !o.contains("at java.base/"),
            "jdk frames stripped; got:\n{}",
            o
        );
    }

    /// A capped (dropped) multi-failure class must drop **all** its per-test
    /// blocks — the re-arm inherits the drop decision — and the tail counts
    /// classes, not failures. The existing `surefire_caps_failing_blocks_emits_tail`
    /// only covers single-failure classes.
    #[test]
    fn surefire_drop_failing_drops_all_sublines_of_capped_class() {
        let i = "[INFO] Scanning for projects...\n\
                 [INFO] -----< x >-----\n\
                 [INFO] Running x.FailA\n\
                 [ERROR] Tests run: 1, Failures: 1, Errors: 0, Skipped: 0, Time elapsed: 0.011 s <<< FAILURE! -- in x.FailA\n\
                 [ERROR] x.FailA.one -- Time elapsed: 0.010 s <<< FAILURE!\n\
                 org.opentest4j.AssertionFailedError: boomA\n\
                 \tat x.FailA.one(FailA.java:10)\n\
                 \n\
                 [INFO] Running x.MultiFail\n\
                 [ERROR] Tests run: 2, Failures: 1, Errors: 1, Skipped: 0, Time elapsed: 0.051 s <<< FAILURE! -- in x.MultiFail\n\
                 [ERROR] x.MultiFail.first -- Time elapsed: 0.020 s <<< FAILURE!\n\
                 org.opentest4j.AssertionFailedError: boomFirst\n\
                 \tat x.MultiFail.first(MultiFail.java:20)\n\
                 \n\
                 [ERROR] x.MultiFail.second -- Time elapsed: 0.030 s <<< ERROR!\n\
                 java.lang.IllegalStateException: boomSecond\n\
                 \tat x.MultiFail.second(MultiFail.java:30)\n\
                 \n\
                 [INFO] BUILD FAILURE\n";
        let o = filter_surefire_with_cap(i, 1);

        assert!(o.contains("boomA"), "first class kept; got:\n{}", o);
        assert!(
            !o.contains("Running x.MultiFail") && !o.contains("boomFirst"),
            "capped class first block dropped; got:\n{}",
            o
        );
        assert!(
            !o.contains("x.MultiFail.second") && !o.contains("boomSecond"),
            "capped class second per-test block dropped (re-arm inherits drop); got:\n{}",
            o
        );
        assert!(
            o.contains("… +1 more failing test classes"),
            "tail counts one class, not one per failure; got:\n{}",
            o
        );
    }

    /// A non-subline line (`[INFO] Results:`) immediately after a trail blank
    /// must disarm the re-arm and be kept normally by the outside-block list.
    #[test]
    fn surefire_rearm_disarms_at_results_boundary() {
        let i = "[INFO] -----< x >-----\n\
                 [INFO] Running x.MultiFail\n\
                 [ERROR] Tests run: 2, Failures: 2, Errors: 0, Skipped: 0, Time elapsed: 0.051 s <<< FAILURE! -- in x.MultiFail\n\
                 [ERROR] x.MultiFail.first -- Time elapsed: 0.020 s <<< FAILURE!\n\
                 org.opentest4j.AssertionFailedError: boomFirst\n\
                 \n\
                 [ERROR] x.MultiFail.second -- Time elapsed: 0.030 s <<< FAILURE!\n\
                 org.opentest4j.AssertionFailedError: boomSecond\n\
                 \n\
                 [INFO] Results:\n\
                 [ERROR] Tests run: 2, Failures: 2, Errors: 0, Skipped: 0\n\
                 [INFO] BUILD FAILURE\n";
        let o = filter_surefire(i);
        assert!(o.contains("boomSecond"), "second block kept; got:\n{}", o);
        assert!(
            o.contains("[INFO] Results:"),
            "Results boundary disarms re-arm and is kept; got:\n{}",
            o
        );
        assert!(
            o.contains("[ERROR] Tests run: 2, Failures: 2"),
            "aggregate kept; got:\n{}",
            o
        );
    }

    /// Double blank between per-test blocks: stay armed across the extra
    /// blank, still re-enter the trail — and no spurious blank lines leak.
    #[test]
    fn surefire_tolerates_double_blank_between_failure_blocks() {
        let i = "[INFO] -----< x >-----\n\
                 [INFO] Running x.MultiFail\n\
                 [ERROR] Tests run: 2, Failures: 2, Errors: 0, Skipped: 0, Time elapsed: 0.051 s <<< FAILURE! -- in x.MultiFail\n\
                 [ERROR] x.MultiFail.first -- Time elapsed: 0.020 s <<< FAILURE!\n\
                 org.opentest4j.AssertionFailedError: boomFirst\n\
                 \n\
                 \n\
                 [ERROR] x.MultiFail.second -- Time elapsed: 0.030 s <<< FAILURE!\n\
                 org.opentest4j.AssertionFailedError: boomSecond\n\
                 \n\
                 [INFO] BUILD FAILURE\n";
        let o = filter_surefire(i);
        assert!(o.contains("boomFirst"), "first block kept; got:\n{}", o);
        assert!(
            o.contains("boomSecond"),
            "second block re-enters trail across double blank; got:\n{}",
            o
        );
        assert!(
            !o.contains("\n\n\n"),
            "no spurious blank lines leak; got:\n{:?}",
            o
        );
    }

    /// Byte-exact pin of the single-failure path: the re-arm machinery must
    /// not change output for single-failure fixtures (no extra blank lines,
    /// no reordering). Literal captured from `filter_surefire` at the commit
    /// preceding the trail re-arm change.
    #[test]
    fn surefire_single_failure_output_unchanged() {
        let i = include_str!("../../../tests/fixtures/mvn_test_fail_slice_raw.txt");
        let o = filter_surefire(i);
        let expected = "[INFO] Scanning for projects...\n\
                        [INFO] ----------------------< commons-cli:commons-cli >-----------------------\n\
                        [INFO] Building Apache Commons CLI 1.11.1-SNAPSHOT\n\
                        [INFO] Running org.apache.commons.cli.RtkInducedFailTest\n\
                        [ERROR] Tests run: 1, Failures: 1, Errors: 0, Skipped: 0, Time elapsed: 0.033 s <<< FAILURE! -- in org.apache.commons.cli.RtkInducedFailTest\n\
                        [ERROR] org.apache.commons.cli.RtkInducedFailTest.rtkInducedFailure -- Time elapsed: 0.025 s <<< FAILURE!\n\
                        org.opentest4j.AssertionFailedError: expected: <expected> but was: <actual>\n\
                        \tat org.apache.commons.cli.RtkInducedFailTest.rtkInducedFailure(RtkInducedFailTest.java:25)\n\
                        \n\
                        [INFO] Results:\n\
                        [ERROR] Failures:\n\
                        [ERROR]   RtkInducedFailTest.rtkInducedFailure:25 expected: <expected> but was: <actual>\n\
                        [ERROR] Tests run: 978, Failures: 1, Errors: 0, Skipped: 61\n\
                        [INFO] BUILD FAILURE\n\
                        [INFO] Total time:  01:05 min\n\
                        [INFO] Finished at: 2026-05-21T14:57:09Z\n\
                        [ERROR] Failed to execute goal org.apache.maven.plugins:maven-surefire-plugin:3.5.5:test (default-test) on project commons-cli: There are test failures.\n";
        assert_eq!(o, expected, "single-failure output must be byte-identical");
    }

    /// Savings on the multifail slice. Threshold is low by design: the slice
    /// is nearly all kept failure signal (two failing classes, three per-test
    /// detail blocks), so the droppable share is small — measured 42.3% after
    /// non-quiet boilerplate stripping (19.9% before it; precedent:
    /// reactor-fail pins ≥30% with a "short fixture" note).
    #[test]
    fn savings_mvn_test_multifail_slice() {
        let i = include_str!("../../../tests/fixtures/mvn_test_multifail_slice_raw.txt");
        let o = filter_surefire(i);
        let savings = 100.0 - (count_tokens(&o) as f64 / count_tokens(i) as f64 * 100.0);
        assert!(
            savings >= 30.0,
            "multifail slice ≥30% savings (dense failure-signal fixture), got {:.1}%",
            savings
        );
    }

    /// Non-quiet runs must strip the post-failure help boilerplate
    /// (`-> [Help 1]`, `Re-run Maven`, `See …`, bare `[ERROR]` dividers) the
    /// same way `filter_quiet` does, while keeping the `Failed to execute
    /// goal` terminator (signal).
    #[test]
    fn surefire_drops_help_boilerplate_in_nonquiet_mode() {
        let i = include_str!("../../../tests/fixtures/mvn_test_multifail_slice_raw.txt");
        let o = filter_surefire(i);
        assert!(
            o.contains("[ERROR] Failed to execute goal"),
            "goal terminator kept; got:\n{}",
            o
        );
        assert!(!o.contains("[Help 1]"), "help link stripped; got:\n{}", o);
        assert!(
            !o.contains("Re-run Maven"),
            "re-run hint stripped; got:\n{}",
            o
        );
        assert!(
            !o.contains("To see the full stack trace"),
            "stack-trace hint stripped; got:\n{}",
            o
        );
        assert!(
            !o.contains("See dump files"),
            "dump-file pointer stripped; got:\n{}",
            o
        );
        assert!(
            !o.lines().any(|l| l.trim_end() == "[ERROR]"),
            "bare [ERROR] dividers stripped; got:\n{}",
            o
        );
    }

    /// CLOSE regex accepts a `<<< ERROR!` marker (defensive — Surefire 3.5.5
    /// emits `<<< FAILURE!` even for errors-only classes, per the multifail
    /// fixture capture; other versions may emit `ERROR!`).
    #[test]
    fn close_line_matches_error_marker() {
        let line = "[ERROR] Tests run: 1, Failures: 0, Errors: 1, Skipped: 0, Time elapsed: 0.006 s <<< ERROR! -- in com.example.rtk.BoomTest";
        let caps = CLOSE
            .captures(line)
            .expect("CLOSE must match an ERROR!-marked close line");
        assert_eq!(caps.get(1).expect("failures group").as_str(), "0");
        assert_eq!(caps.get(2).expect("errors group").as_str(), "1");
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

    // ── CRLF line-ending compatibility ───────────────────────────────────────

    /// `str::lines()` strips single `\r\n` pairs entirely, so real Maven CRLF
    /// output filters cleanly. The hazard is a fixture *already checked out
    /// with CRLF* (e.g. `core.autocrlf=true` without `.gitattributes`): the
    /// `\n` → `\r\n` synthesis below would then produce `\r\r\n`, leaving a
    /// stray `\r` per line that `$`-anchored regexes reject. Normalise the
    /// embedded fixture back to LF first — correct regardless of checkout
    /// state (defense-in-depth alongside `tests/fixtures/** -text`).
    #[test]
    fn surefire_handles_crlf_line_endings() {
        let i_lf = include_str!("../../../tests/fixtures/mvn_test_pass_slice_raw.txt")
            .replace("\r\n", "\n");
        let o_lf = filter_surefire(&i_lf);
        let i_crlf = i_lf.replace('\n', "\r\n");
        let o_crlf = filter_surefire(&i_crlf);
        assert_eq!(
            o_lf,
            o_crlf.replace("\r\n", "\n"),
            "CRLF filtered output must match LF (modulo line endings)"
        );
    }

    #[test]
    fn package_handles_crlf_line_endings() {
        let i_lf = include_str!("../../../tests/fixtures/mvn_install_slice_raw.txt")
            .replace("\r\n", "\n");
        let o_lf = filter_package(&i_lf);
        let i_crlf = i_lf.replace('\n', "\r\n");
        let o_crlf = filter_package(&i_crlf);
        assert_eq!(
            o_lf,
            o_crlf.replace("\r\n", "\n"),
            "CRLF filtered output must match LF (modulo line endings)"
        );
    }

    // ── Cap: failing-class blocks ────────────────────────────────────────────

    /// Synthetic fixture with 5 failing classes; with `cap = 3` we expect
    /// the first 3 failing blocks emitted in full and a
    /// `… +2 more failing test classes` tail.
    #[test]
    fn surefire_caps_failing_blocks_emits_tail() {
        let mut i = String::from(
            "[INFO] Scanning for projects...\n\
             [INFO] -----< x >-----\n",
        );
        for n in 1..=5 {
            i.push_str(&format!(
                "[INFO] Running x.Fail{n}\n\
                 [ERROR] Tests run: 1, Failures: 1, Errors: 0, Skipped: 0, Time elapsed: 0.0{n}1 s <<< FAILURE! -- in x.Fail{n}\n\
                 [ERROR] x.Fail{n}.bar -- Time elapsed: 0.0{n}0 s <<< FAILURE!\n\
                 org.opentest4j.AssertionFailedError: boom{n}\n\
                 \tat x.Fail{n}.bar(Fail{n}.java:25)\n\
                 \n",
                n = n
            ));
        }
        i.push_str("[INFO] BUILD FAILURE\n");

        let o = filter_surefire_with_cap(&i, 3);

        // First 3 blocks emitted with their close lines.
        for n in 1..=3 {
            assert!(
                o.contains(&format!("Running x.Fail{}", n)),
                "Fail{n} kept; got:\n{}",
                o,
                n = n
            );
            assert!(
                o.contains(&format!("in x.Fail{}", n)),
                "Fail{n} close line kept; got:\n{}",
                o,
                n = n
            );
        }
        // Blocks 4 and 5 dropped.
        for n in 4..=5 {
            assert!(
                !o.contains(&format!("Running x.Fail{}", n)),
                "Fail{n} dropped; got:\n{}",
                o,
                n = n
            );
            assert!(
                !o.contains(&format!("AssertionFailedError: boom{}", n)),
                "Fail{n} exception dropped; got:\n{}",
                o,
                n = n
            );
        }
        assert!(
            o.contains("… +2 more failing test classes"),
            "tail emitted; got:\n{}",
            o
        );
    }

    /// Cap of 0 means summary-only (core cap policy): no failing-class blocks
    /// emitted, tail still counts every dropped class.
    #[test]
    fn surefire_cap_zero_emits_summary_only() {
        let mut i = String::from(
            "[INFO] Scanning for projects...\n\
             [INFO] -----< x >-----\n",
        );
        for n in 1..=5 {
            i.push_str(&format!(
                "[INFO] Running x.Fail{n}\n\
                 [ERROR] Tests run: 1, Failures: 1, Errors: 0, Skipped: 0, Time elapsed: 0.0{n}1 s <<< FAILURE! -- in x.Fail{n}\n\
                 \n",
                n = n
            ));
        }
        i.push_str("[INFO] BUILD FAILURE\n");
        let o = filter_surefire_with_cap(&i, 0);
        for n in 1..=5 {
            assert!(
                !o.contains(&format!("Running x.Fail{}", n)),
                "Fail{n} dropped under cap=0; got:\n{}",
                o,
                n = n
            );
        }
        assert!(
            o.contains("+5 more failing test classes"),
            "tail counts all 5 under cap=0; got:\n{}",
            o
        );
    }

    /// `[ERROR] Failures:` summary block cap: with N>cap entries, expect the
    /// first `cap` entries plus a `\n… +(N-cap) more failures\n` tail
    /// emitted before the aggregate `[ERROR] Tests run:` line.
    #[test]
    fn failures_summary_block_is_capped() {
        let mut i = String::from(
            "[INFO] -----< x >-----\n\
             [INFO] Results:\n\
             [INFO]\n\
             [ERROR] Failures:\n",
        );
        for n in 1..=5 {
            i.push_str(&format!(
                "[ERROR]   ClassA.test{n}:25 expected: <a> but was: <b{n}>\n",
                n = n
            ));
        }
        i.push_str(
            "[INFO]\n\
             [ERROR] Tests run: 100, Failures: 5, Errors: 0, Skipped: 0\n\
             [INFO] BUILD FAILURE\n",
        );
        let o = filter_surefire_with_cap(&i, 3);

        // First 3 entries kept.
        for n in 1..=3 {
            assert!(
                o.contains(&format!("ClassA.test{}:25", n)),
                "entry {n} kept; got:\n{}",
                o,
                n = n
            );
        }
        // Entries 4-5 dropped.
        for n in 4..=5 {
            assert!(
                !o.contains(&format!("ClassA.test{}:25", n)),
                "entry {n} dropped; got:\n{}",
                o,
                n = n
            );
        }
        // Tail emitted before aggregate.
        let tail_idx = o
            .find("… +2 more failures")
            .unwrap_or_else(|| panic!("tail must appear; got:\n{}", o));
        let agg_idx = o
            .find("[ERROR] Tests run: 100")
            .unwrap_or_else(|| panic!("aggregate must appear; got:\n{}", o));
        assert!(
            tail_idx < agg_idx,
            "tail must precede aggregate; tail@{} agg@{}; got:\n{}",
            tail_idx,
            agg_idx,
            o
        );
    }

    // ── Multi-module reactor summary ─────────────────────────────────────────

    /// `mvn install` on a multi-module reactor build that passes everywhere
    /// must preserve the `Reactor Summary for <root>` header and the per-module
    /// `[INFO] foo ...... SUCCESS [ 1.234 s]` rows.
    #[test]
    fn reactor_summary_kept_on_multi_module_pass() {
        let i = include_str!("../../../tests/fixtures/mvn_reactor_pass_slice_raw.txt");
        let o = filter_package(i);
        assert!(
            o.contains("Reactor Summary for multi-module-skeleton"),
            "reactor summary header preserved; got:\n{}",
            o
        );
        assert!(
            o.contains("[INFO] child-a ............................................ SUCCESS"),
            "per-module SUCCESS row preserved; got:\n{}",
            o
        );
        assert!(
            o.contains("[INFO] child-b ............................................ SUCCESS"),
            "second per-module SUCCESS row preserved; got:\n{}",
            o
        );
        assert!(
            o.contains("BUILD SUCCESS"),
            "footer preserved; got:\n{}",
            o
        );
    }

    /// `mvn install` on a multi-module reactor build where one module fails
    /// must preserve the Reactor Summary with the `FAILURE` row plus the
    /// `[ERROR] Failed to execute goal …` terminator that already survives
    /// via `keep_outside_block`.
    #[test]
    fn reactor_summary_kept_on_multi_module_fail() {
        let i = include_str!("../../../tests/fixtures/mvn_reactor_fail_slice_raw.txt");
        let o = filter_package(i);
        assert!(
            o.contains("Reactor Summary for multi-module-skeleton"),
            "reactor summary header preserved; got:\n{}",
            o
        );
        assert!(
            o.contains("child-a ............................................ SUCCESS"),
            "successful module row preserved; got:\n{}",
            o
        );
        assert!(
            o.contains("child-b ............................................ FAILURE"),
            "failing module row preserved; got:\n{}",
            o
        );
        assert!(o.contains("BUILD FAILURE"), "footer preserved; got:\n{}", o);
        assert!(
            o.contains("[ERROR] Failed to execute goal"),
            "goal terminator preserved; got:\n{}",
            o
        );
        assert!(
            o.contains("mvn <args> -rf :child-b"),
            "resume hint preserved (actionable signal); got:\n{}",
            o
        );
        assert!(!o.contains("[Help 1]"), "help boilerplate stripped; got:\n{}", o);
        assert!(
            !o.contains("Re-run Maven"),
            "re-run hint stripped; got:\n{}",
            o
        );
        let savings = 100.0 - (count_tokens(&o) as f64 / count_tokens(i) as f64 * 100.0);
        assert!(
            savings >= 30.0,
            "reactor-fail slice savings >=30% (short fixture); got {:.1}%",
            savings
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
        assert!(
            !o.contains("[Help 1]"),
            "help boilerplate stripped in compile path; got:\n{}",
            o
        );
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



