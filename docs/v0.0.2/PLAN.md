# nyq v0.0.2 — work plan

Design reference: `docs/v0.0.2/DESIGN.md`. In that document, "flowie" means nyq v0.0.2. We are not renaming nyq and not splitting it into a Cargo workspace in this release. Everything lands in the existing single crate.

Integration branch: `release/v0.0.2`. Each workstream runs on its own branch, created from the integration branch. The orchestrator merges them back into it.

## Rules for every workstream

- Stay inside the files listed for your workstream. If you must touch a file another workstream owns, keep the change minimal and say so in your final report.
- `cargo build`, `cargo test` and `cargo clippy --all-targets` must pass before you finish. Do not delete or weaken existing tests.
- Add unit tests for every new behaviour. Follow nyq's existing in-file `#[cfg(test)] mod tests` style.
- Binary size matters (`size-budget.json`, `scripts/size.sh`). New dependencies need a reason in your report. Prefer no new dependency.
- Old sessions, settings and permission files must keep loading (backward compatible).
- No network calls in tests. Use the existing tape and fixture patterns.
- Commit your work on your branch with clear messages. Do not push and do not open PRs.
- Final report: branch name, head SHA, a summary of what was done, what was deferred, and any cross-file touches.

## Wave 1 (parallel)

| ID | Workstream | Owned files | Design § |
|---|---|---|---|
| W0 | Split `src/agent.rs` into a `src/agent/` module tree (loop, dispatch, compaction, trailing note, permission/grant logic, tests). Pure move, **no behaviour change**. | `src/agent.rs` → `src/agent/*` | §1, §3 |
| W1 | Native DeepSeek provider | `src/provider/deepseek.rs` (new), `src/provider/{mod,hosted,anthropic}.rs`, `src/catalog.rs`, `src/models.rs`, config/login wiring | §10 |
| W2 | Edit tools: `multi_edit`, `apply_patch`, `edit_range`, read-before-edit and mtime guard | `src/tools/fs.rs`, `src/tools/patch.rs` (new), registration lines in `src/tools/mod.rs` | §6.3 |
| W3 | Sandbox hardening | `src/sandbox.rs`, `tests/sandbox.rs` | §7.1 |
| W4 | BuildSystem detection and validator library (standalone, not wired in) | `src/task/` (new): `build.rs`, `validate.rs`, `mod.rs` | §4.2 |

## Wave 2 (based on release/v0.0.2 after W0 merged; `src/agent/` is now a module tree)

| ID | Workstream | Owned files | Design § |
|---|---|---|---|
| W5 | Call journal, failed-attempt records, lenient load and recovery, `session_search` tool | `src/session.rs`, `src/tools/session_search.rs` (new); small hooks in `src/agent/dispatch.rs` and `src/agent/step.rs` | §2.1, §2.3 |
| W6 | Tiered compaction (prune → partial summary → never latch off), better token estimate, overflow-triggered compaction | `src/agent/compact.rs`, `src/usage.rs` | §5.4 |
| W7 | `Tool::effects`/`concurrency`, cached specs, argument schema validation, parallel disjoint writes, MCP annotations | `src/tools/mod.rs`, each tool's impl block, the parallel parts of `src/agent/dispatch.rs`, `src/tools/mcp*.rs` | §3, §6.1 |
| W8 | Taint and provenance labels, classifier for every tool | `src/taint.rs` (new), `src/classifier.rs`, `src/permissions.rs`, the settle and auto_review parts of `src/agent/dispatch.rs` | §7.2, §7.3 |
| W10 | Working set (open/focus/unfocus/close), outlines, trailing-note rendering of working set and intents | `src/tools/workset.rs` (new), `src/outline.rs` (new), `src/agent/note.rs` | §5.1, §5.2, §4.4 |
| W12 | Profiles (`--profile`, `--dump-config`, minimal/ci/sdk), new hook events, `nyq export --redact`, `nyq bench` scaffold | `src/settings.rs`, `src/cli.rs`, `src/hooks.rs`, `src/profiles.rs` (new), `src/export.rs` (new) | §2.4, §9, §12 |

## Wave 3 (after W4 is merged)

| ID | Workstream |
|---|---|
| W9 | Task engine wiring: `submit` tool, `--schema`, validators in the loop, budgets, escalation ladder, intent field, background verifier, repair priority, memo |
| W11 | Orchestration: typed subagents (`output_schema` and validators), worktree isolation, task graph, external agents over ACP, programs and `run_code` |
