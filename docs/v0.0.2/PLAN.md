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

## Wave 2 (after W0 is merged): task engine, journal and compaction, tool effects and taint, working set, orchestration

These will be specified once Wave 1 is merged.
