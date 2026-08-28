# System and Generic Utilities

> Part of [`src/cmds/`](../README.md) — see also [docs/contributing/TECHNICAL.md](../../../docs/contributing/TECHNICAL.md)

## Specifics

- `read.rs` uses `core/filter` for language-aware code stripping (FilterLevel: none/minimal/aggressive)
- `search.rs` backs both `rtk grep` and `rtk rg`: it runs the invoked engine (never substituting one for the other) and groups its output, reading `core/config` for `limits.grep_max_results` and `limits.grep_max_per_file`. Format-altering flags (`-c`, `-l`, `-L`, `-o`, `-Z`) bypass RTK filtering and run raw.
- `ctest_cmd.rs` treats the CTest summary as authoritative, keeps the last result for each test number (`--repeat`), labels per-failure blocks as `#N name` with `CAP_WARNINGS` head+tail recovery, caps failed/skipped lists at `CAP_LIST`, and attributes diagnostics safely under `-j`; explicit verbose/show-only/help/version flags bypass filtering.
- `local_llm.rs` (`rtk smart`) uses `core/filter` for heuristic file summarization
- `format_cmd.rs` is a cross-ecosystem dispatcher: auto-detects and routes to `prettier_cmd` or `ruff_cmd` (black is handled inline, not as a separate module)

## Cross-command

- `format_cmd` routes to `cmds/js/prettier_cmd` and `cmds/python/ruff_cmd`
