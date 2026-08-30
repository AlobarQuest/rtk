---
artifact_contract: ce-unified-plan/v1
artifact_readiness: implementation-ready
execution: code
product_contract_source: ce-plan-bootstrap
origin: https://github.com/rtk-ai/rtk/pull/3560
---

# fix: Resolve PR #3560 code review findings (spring-boot/liquibase/ssh filters)

## Summary

PR #3560 (branch `fix/spring-boot-liquibase-ssh-overbroad-match`) tightened three overly broad `match_command` regexes (spring-boot, liquibase, ssh). Reviewer KuSh returned `CHANGES_REQUESTED` with one blocking issue and four non-blocking notes. This plan fixes all five findings, with the blocking Windows path-separator bug validated against the reviewer's exact test matrix before any code is written.

## Problem Frame

The PR's spring-boot fix requires "spring" to appear in a jar's filename before activating Spring-specific log compaction. The heuristic used `[^/\s]*` to isolate the filename from its path, but that character class only excludes `/` — on Windows, `java -jar C:\spring-cache\other.jar` still lets a `spring` *directory* segment leak into what should be a filename-only match, reproducing the exact false positive the PR set out to close. Four smaller issues also need addressing: two test-quality gaps (vacuous negative assertions, an unreachable-in-production code path being tested as if reachable), one dead regex branch, and one inaccurate line in the PR description.

## Requirements

- **R1 (blocking):** `spring-boot`'s `match_command` must not activate on a Windows-style path (`C:\...\spring...\...jar`) where "spring" appears only in a directory segment, not the jar's own filename — on any platform, `\` and `/` must both be treated as path separators.
- **R2:** The three new negative regression tests (spring-boot, liquibase, ssh) in `src/core/toml_filter.rs` must assert against each named filter's own compiled regex, not rely on `find_filter_in`'s first-match-wins semantics, so a future alphabetically-earlier filter can't silently neuter the guard.
- **R3:** `liquibase`'s `match_command` must not carry a path-prefix branch that no production caller can exercise; the corresponding test must reflect actual caller behavior (basenamed `argv[0]`), not a synthetic path-qualified string.
- **R4:** `spring-boot`'s filter `description` must record the accepted false-negative tradeoff (default Maven/Gradle jar naming won't match) so a future reader doesn't need to reverse-engineer the regex to learn it.
- **R5:** The PR description's rationale for the `ssh` fix (currently backwards about what `\b` does) must be corrected in the PR body text.

## Key Technical Decisions

- **KTD1 (blocking fix):** Extend the spring-boot jar-name heuristic to treat `\` as a path separator alongside `/`, using the reviewer's validated replacement regex, over leaving it `/`-only. Validated by re-deriving the exact post-TOML-parse regex and running it against all 9 known positive/negative cases (5 existing + 2 Windows negatives + 2 Windows positives) with Python's `re` engine — all matched expectations. Rationale: the PR's whole purpose is closing this false positive; leaving a platform gap defeats it. *(session-settled: user-directed — chosen over leaving `/`-only: reviewer supplied and the requester validated an exact replacement regex that closes the Windows gap without regressing any existing case)*
- **KTD2:** Rewrite the three match-order-fragile negative assertions to look up each filter by name and assert against its own `match_regex`, over leaving `find_filter_in`'s first-match check. Rationale: `find_filter_in` returns only the first regex match in `filters` slice order; today nothing shadows spring-boot/liquibase/ssh, but a future filter (e.g., a `java`-scoped one sorting before `spring-boot`) could make these tests pass for the wrong reason with no failure signal.
- **KTD3:** Simplify `liquibase.toml`'s `match_command` to `^liquibase(?:\s|$)`, dropping the `(?:\S*/)?` prefix branch, over keeping it. Rationale: both production callers (`run_fallback` in `src/main.rs:1308-1317`, and `strip_absolute_path` in `src/discover/registry.rs:452-471`) already basename `argv[0]` before the regex ever sees it, so the path-prefix branch is dead code that only creates a false impression of path-handling coverage. This also matches the already-existing simpler form in `src/discover/rules.rs:948`.
- **KTD4:** Document the spring-boot false-negative tradeoff in the filter's own `description` field, over leaving it silent or adding a runtime warning. Rationale: matches this repo's existing pattern of self-documenting filter `description` strings, and the tradeoff is a deliberate design choice (favor missed-optimization over misclassification) worth recording where the regex lives.

## Scope Boundaries

**In scope:** the five findings from KuSh's review on PR #3560 — one blocking regex gap, two test-hardening fixes, one dead-code removal, one documentation update, one PR-description text correction.

**Out of scope:**
- Any new filter functionality beyond what's needed to satisfy the review findings.
- Re-litigating the PR's core design (jar-filename heuristic, liquibase anchoring, ssh boundary) — those were already accepted by the reviewer as correct; only the Windows gap and secondary polish items are addressed.
- Migrating `find_filter_in`'s underlying data structure away from first-match-wins semantics — KTD2 works around the current behavior in tests without changing production matching logic, since changing lookup semantics is outside this PR's blast radius.

---

## Implementation Units

### U1. Close the Windows path-separator gap in spring-boot's jar-name heuristic

**Goal:** Fix the blocking issue — `spring-boot`'s `match_command` must not match when "spring" appears only in a Windows-style directory segment of a jar path.

**Requirements:** R1

**Dependencies:** None

**Files:**
- `src/filters/spring-boot.toml` (modify `match_command`)
- `src/core/toml_filter.rs` (modify `test_spring_boot_match_command_requires_spring_named_jar`)

**Approach:** Replace the current `match_command` value:

```
"^(mvn\\s+spring-boot:run|java\\s+-jar\\s+(?:\\S*/)?[^/\\s]*(?i:spring)[^/\\s]*\\.jar|gradle\\s+.*bootRun)"
```

with:

```
"^(mvn\\s+spring-boot:run|java\\s+-jar\\s+(?:\\S*[/\\\\])?[^/\\\\\\s]*(?i:spring)[^/\\\\\\s]*\\.jar|gradle\\s+.*bootRun)"
```

This is the reviewer-supplied replacement, independently re-derived and validated in this planning pass (see Verification below) — it changes both the path-prefix branch and the filename character class to treat `[/\\]` as the separator set instead of `/` alone, so a `spring`-named *directory* on either platform no longer leaks into the filename-only match.

**Patterns to follow:** The existing TOML backslash-escaping convention already used in this same file (`\\s`, `\\.` etc. — each literal regex backslash is doubled for TOML basic-string escaping, then doubled again where the regex itself needs a literal backslash in a character class).

**Test scenarios:**
- Existing positives still match: `mvn spring-boot:run`, `java -jar build/libs/my-spring-app.jar`, `java -jar build\libs\my-spring-app.jar` (already in the test as a Windows-style positive), `java -jar build/libs/MySpringApp.jar` (case-insensitive), `gradle clean bootRun`.
- Existing negatives still fail to match: `java -jar build/libs/my-other-tool.jar`, `java -jar /opt/spring-cache/other-tool.jar` (already covered — the Unix false-positive case this PR originally closed).
- New negative (the blocking bug): `java -jar C:\spring-cache\other.jar` must NOT match spring-boot.
- New negative: `java -jar C:\dev\spring-workspace\build\other-tool.jar` must NOT match spring-boot (nested Windows directory case from the review).
- New positive (guard against over-correcting): `java -jar C:\dev\my-spring-app.jar` must match spring-boot (a genuinely spring-named jar on a Windows path must still be caught).

**Verification:** Independently confirmed via a standalone Python `re` simulation of TOML-basic-string unescaping against all cases above before writing this plan (9/9 matched expectations, including two Windows positives not in the reviewer's original comment). Re-confirm with `cargo test --bin rtk toml_filter` after the code change lands, and separately eyeball that the exact byte sequence written to the TOML file matches what was validated — TOML backslash-escaping is easy to get subtly wrong by hand.

---

### U2. Harden the three match-order-fragile negative regression tests

**Goal:** Make the spring-boot, liquibase, and ssh negative-match tests fail correctly if their filter's own regex ever regresses, independent of `find_filter_in`'s first-match-in-slice-order behavior.

**Requirements:** R2

**Dependencies:** U1 (touches the same spring-boot test function; land after U1's regex/test edits to avoid a merge-order conflict within the same function)

**Files:**
- `src/core/toml_filter.rs` (modify `test_spring_boot_match_command_requires_spring_named_jar`, `test_liquibase_match_command_ignores_path_substring`, `test_ssh_match_command_excludes_ssh_dash_utilities`)

**Approach:** `find_filter_in` (defined at `src/core/toml_filter.rs:477-482`) returns only the first filter in slice order whose regex matches — so `find_filter_in(cmd, &filters).is_none_or(|f| f.name != "X")` also passes whenever some *other*, earlier-sorted filter happens to match the string, even if filter `X`'s own regex were reintroduced in its old, over-broad form. Replace each of the three `is_none_or(...)` negative assertions with a direct lookup-and-assert against the named filter's own `match_regex`:

```rust
let spring = filters.iter().find(|f| f.name == "spring-boot")
    .expect("spring-boot filter must exist");
assert!(
    !spring.match_regex.is_match("java -jar build/libs/my-other-tool.jar"),
    "a non-Spring jar must not activate the spring-boot filter"
);
```

Apply the same shape to the liquibase negative (`rm -rf /opt/liquibase`) and both ssh negatives (`ssh-keygen -t ed25519`, `ssh-add ~/.ssh/id_ed25519`). `match_regex` is a private field on `CompiledFilter`, reachable because these tests live in the same module (`#[cfg(test)] mod tests` within `toml_filter.rs`) — confirm this before writing, since a different visibility would require a `pub(crate)` adjustment.

**Patterns to follow:** The existing positive-match assertions in the same test functions already use `find_filter_in(...).expect(...)` plus `assert_eq!(f.name, "...")` — keep those as-is; only the negative assertions change shape.

**Test scenarios:**
- Each rewritten negative assertion still correctly reports "no match" against the current (correct) regex for its filter.
- Existing positive assertions in the same three test functions are unaffected and continue to pass.
- Test expectation: the new assertion shape's *value* (survives future filter additions) is provable only by contradiction, not by a runnable scenario — no test-of-the-test is needed. Standard `cargo test` coverage above is sufficient.

**Verification:** `cargo test --bin rtk toml_filter` passes with no changes to pass/fail outcomes for any other test in the file (this is a test-only refactor with no production behavior change).

---

### U3. Simplify liquibase's match_command and fix the now-inaccurate path-qualified test

**Goal:** Drop the dead path-prefix branch from `liquibase.toml`'s `match_command`, and replace the test assertion that exercised it with one reflecting actual (basenamed) caller behavior.

**Requirements:** R3

**Dependencies:** None (independent file from U1/U2, though also in `toml_filter.rs` — no line-range overlap with U1/U2's edits since it's a different test function)

**Files:**
- `src/filters/liquibase.toml` (modify `match_command`)
- `src/core/toml_filter.rs` (modify `test_liquibase_match_command_ignores_path_substring`)

**Approach:** Change:

```
match_command = "^(?:\\S*/)?liquibase(?:\\s|$)"
```

to:

```
match_command = "^liquibase(?:\\s|$)"
```

Confirmed via direct code inspection that both production callers already strip the leading path before this regex runs: `run_fallback` in `src/main.rs` (lines 1308-1317) basenames `args[0]` via `Path::file_name()` before building `lookup_cmd`, and `strip_absolute_path` in `src/discover/registry.rs` (lines 452-471) does the equivalent for the discover path. This also brings `liquibase.toml` in line with the already-existing simpler pattern in `src/discover/rules.rs:948` (`r"^liquibase(?:\s|$)"`).

The existing test assertion at `src/core/toml_filter.rs:1453-1455` (`full_path = find_filter_in("/usr/local/bin/liquibase update", &filters).expect(...)`) currently asserts this path-qualified string *matches* — after the simplification, it will no longer match (since the regex loses its `(?:\S*/)?` prefix), so this assertion **must change or it will fail**. Replace it with an assertion that reflects actual production behavior: the *basenamed* form (`"liquibase update"`) matches — which the existing `bare` assertion two lines above already covers — and add an explicit negative assertion that the raw, unbasenamed path-qualified string (`"/usr/local/bin/liquibase update"`) does *not* match on its own, to lock in the simplification and make clear that path-stripping is the caller's job, not the regex's.

**Patterns to follow:** `src/discover/rules.rs:948`'s existing `r"^liquibase(?:\s|$)"` pattern; the basenaming logic already present in `run_fallback` and `strip_absolute_path`.

**Test scenarios:**
- `liquibase status` still matches (bare invocation, already covered by `bare`).
- `rm -rf /opt/liquibase` still does not match (already covered — the original false positive this PR closed).
- New/replaced: `/usr/local/bin/liquibase update` does NOT match the simplified regex directly (this is the corrected assertion — proves the dead branch is really gone, not just untested).
- Implicit via existing callers (no new test needed, covered by production code reading): a path-qualified real invocation still gets filtered correctly in practice because `run_fallback`/`strip_absolute_path` basename it to `liquibase update` before the regex runs — this is existing, unchanged behavior and is not being re-verified here since neither caller is touched by this unit.

**Verification:** `cargo test --bin rtk toml_filter` passes. Additionally grep the repo for any other caller of the liquibase filter's `match_command` (or any test) that depends on the path-prefix branch being reachable, to satisfy the "no other filter or caller path depends on this" constraint — confirmed during planning research: only `run_fallback` and the discover registry call into filter matching, and both already basename first.

---

### U4. Document the spring-boot false-negative tradeoff in the filter description

**Goal:** Make the accepted tradeoff (default-named Maven/Gradle jars won't be compacted) discoverable from the filter definition itself.

**Requirements:** R4

**Dependencies:** U1 (edits the same `spring-boot.toml` file; sequence after U1 to avoid touching the file mid-regex-edit, though the two edits are on different keys and would not conflict if done in either order)

**Files:**
- `src/filters/spring-boot.toml` (modify `description`)

**Approach:** Update the `description` string from:

```
"Compact Spring Boot output — strip banner and verbose startup logs, keep key events"
```

to a version that also states the matching scope and its limitation, along the lines of:

```
"Compact Spring Boot output — strip banner and verbose startup logs, keep key events. Matches mvn spring-boot:run, gradle bootRun, and java -jar only when the jar filename contains \"spring\" — jars named via the Maven/Gradle default <artifactId>-<version>.jar are deliberately not matched (full passthrough) rather than risk misclassifying an unrelated tool."
```

Exact wording is flexible — the requirement is that the tradeoff (default-named jars pass through unfiltered by design) is stated in the description, not the precise phrasing.

**Patterns to follow:** Other filter `description` fields in this repo are single-sentence-or-two summaries; keep this consistent in tone even though it's slightly longer.

**Test scenarios:**
- Test expectation: none — this is a documentation-only string change with no behavioral effect. Confirm the TOML still parses (implicit in any test run, since `make_filters(BUILTIN_TOML)` parses this file for every test in `toml_filter.rs`).

**Verification:** `cargo test --bin rtk toml_filter` continues to pass (proves the TOML still parses correctly after the description edit).

---

### U5. Correct the PR description's ssh rationale

**Goal:** Fix the inverted explanation of `\b` in the PR body text (the code fix itself is already correct and needs no change).

**Requirements:** R5

**Dependencies:** None — pure PR metadata edit, no code dependency

**Files:** None (GitHub PR description text only, edited via `gh pr edit`)

**Approach:** The current PR body states `\b` "can't form a boundary" between `ssh` and a following `-`, implying that's why `^ssh\b` matched `ssh-keygen`. This is backwards: `\b` *does* form a boundary there (`h` is a word character, `-` is not) — that boundary is exactly why `^ssh\b` matched at the `ssh|-keygen` position, since `\b` only asserts a word/non-word transition exists, not which side is "outside" the intended token. Rewrite the ssh bullet in the PR description to state the accurate mechanics: `\b` matched between `h` and `-` as expected, which is precisely why the old regex incorrectly treated `ssh-keygen` as a plain `ssh` invocation; the fix replaces `\b` with an explicit `(?:\s|$)` boundary that requires whitespace or end-of-string, which `-` does not satisfy.

**Patterns to follow:** Match the existing PR description's per-filter bullet structure (the spring-boot and liquibase bullets stay as-is; only the ssh bullet's explanation changes).

**Test scenarios:** Test expectation: none — text-only edit, no code or test surface.

**Verification:** Re-read the updated PR body to confirm the corrected explanation is internally consistent with the actual (unchanged) `^ssh(?:\s|$)` regex and the still-passing ssh tests.

---

## Verification Contract

Run the full gate after all units land, before considering the PR ready for re-review:

1. `cargo fmt --all -- --check` — must be clean.
2. `cargo clippy --all-targets` — zero warnings.
3. `cargo test --all` — full suite passes; no regressions in the pre-existing 154 filter tests or the broader suite. The two pre-existing `hooks::rewrite_cmd::tests::unattestable_passthrough` failures noted in the original PR description are environment-dependent (local permission settings) and are not this plan's concern — confirm they are still the *only* failures, if any, on a clean run.
4. `cargo test --bin rtk toml_filter` — targeted rerun of the filter test module, to isolate the 5 modified/added assertions.
5. If the `rtk` binary is available locally (`command -v rtk`), also run `rtk verify --require-all` per the original PR's validation approach; otherwise the `cargo test` runs above are the equivalent internal check.

## Definition of Done

- [ ] U1: `spring-boot.toml`'s `match_command` treats `\` and `/` as separators; new Windows test cases (2 negative, 1 positive) added and passing; existing cases unaffected.
- [ ] U2: All three negative regression tests (spring-boot, liquibase, ssh) assert against their filter's own `match_regex`, independent of `find_filter_in` ordering.
- [ ] U3: `liquibase.toml`'s `match_command` simplified to `^liquibase(?:\s|$)`; the path-qualified test assertion replaced with one reflecting basenamed caller behavior.
- [ ] U4: `spring-boot.toml`'s `description` documents the false-negative tradeoff for default-named jars.
- [ ] U5: PR #3560's description corrected on the ssh `\b` rationale.
- [ ] Full verification gate (fmt, clippy, `cargo test --all`) passes with no new regressions.
- [ ] Changes committed and pushed to `fix/spring-boot-liquibase-ssh-overbroad-match`; reviewer KuSh's findings addressed and ready for re-review.
