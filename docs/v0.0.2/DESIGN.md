# flowie — harness design

Status: draft v0.1 · 2026-11-12

flowie is a coding-agent harness that combines three earlier systems:

| Source | What it is | What flowie takes from it |
|---|---|---|
| **promptyped** (`projekts-archive/flowie/promptyped`, Haskell; design only) | Typed LLM workflows. `runAiFunc` takes a context, a model tier, a list of tools, an example return value of type `a`, and a validator `a -> AppM (Either err b)`, and loops until the validator passes. | **Typed outcomes and verification.** Tasks return typed values, validators decide when a task is finished, the build and tests run after every change, failure budgets are split into syntax and semantic, stalls are detected and escalated, steps can be memoised, files can be shown in full ("focused") or as an outline ("unfocused"), and the model states its intent each step. |
| **nyq** (`Znskr/nyq`, Rust, about 83k lines; **the codebase we build on**) | A small terminal coding agent with serious safety engineering. | **Mechanics and safety.** Provider-neutral `Item` transcript, retry only while nothing has streamed, a trailing per-request note that keeps the cached prefix stable, read-only shell analysis that rebuilds the command it checked, an LLM classifier with a trust boundary, rules that stop a project from loosening its own policy, writes that cannot escape the workspace and are refused if the file changed after approval, spilling large output to a file, deferred MCP tool search, and a durable journal for child jobs. |
| **DeepSeek Harness** (`deepseek-ai/deepseek-harness`, TypeScript, Cordis; architecture only) | "Everything is a plugin." Profiles are built from bundles of config. Every capability is a seam with a definition, providers, and consumers. | **Architecture.** Every request the model sees is rebuilt from an append-only log ("model-visible means logged"). Tools run through a pipeline of hooks. A call journal records whether a call never started or its outcome is unknown. Compaction preserves the cached prefix. Programmatic tool calling via `run_code`, teams that share a task graph, and profiles. |

The thesis in one sentence: **nyq's loop and safety, made swappable the way dsh is, and driven by promptyped's typed tasks that only finish when a validator passes.**

**Base: nyq.** flowie is not a new codebase. It is nyq, evolved in place: we own it, it already works, and every milestone below leaves a shippable agent. DeepSeek Harness contributes architecture only (no code is ported). promptyped contributes the design of the task engine, which is written fresh in Rust on top of nyq's loop.

Most harnesses have only the inner loop: the model calls tools until it stops talking. promptyped showed that putting a *program* above that loop is what makes long tasks reliable. The program has typed sub-goals, checks that live outside the model, and budgets. Neither nyq nor dsh treats typed tasks as a core primitive. dsh comes closest with `outputSchema` on subagents, `workflow`, and `ralph`, but none of these is a contract the loop actually enforces.

---

## 0. Design principles

1. **The log is the truth.** Every model request is a pure function of the event log plus config. That single property gives resume, fork, replay, evaluation and audit. (dsh)
2. **Done means verified.** A turn ends because a validator accepted the result, not because the model stopped calling tools. If no validator applies, the default one accepts. (promptyped)
3. **Stable prefix, volatile tail.** The system prompt and earlier history are byte-stable so the provider cache keeps hitting. Live state (open files, build status, permission mode) goes into a trailing note attached only to the outgoing request. (nyq's trailing note, applied to promptyped's "always show latest state")
4. **Only the user can authorise.** Tool output, file contents, web pages and child-agent results are evidence and never authority. This is enforced structurally through taint labels, not only stated in prompts. (nyq's classifier, generalised)
5. **Seams, not features.** The loop, model adapter, tool registry, context assembler, compactor, sandbox, approver and verifier are all traits with replaceable implementations. (dsh)
6. **Small and fast by default.** One static Rust binary. The default profile is opinionated and minimal, and plugins are opt-in. (nyq)
7. **Local-first data.** Trajectories never leave the machine unless the user exports them. Unlike dsh, there is no telemetry on by default.

---

## 1. Layered architecture

```
┌─────────────────────────── Frontends ───────────────────────────┐
│  TUI │ print/headless │ ACP server │ SDK (JSON-RPC stdio) │ web   │
└────────────────────────────┬─────────────────────────────────────┘
                             │  Ui trait  (events out, decisions in)
┌────────────────────────────▼─────────────────────────────────────┐
│ L4  Orchestration   Programs · Task graph · Teams · Workflows      │
├───────────────────────────────────────────────────────────────────┤
│ L3  Task engine     Task<T>: contract → loop → validate → T        │
├───────────────────────────────────────────────────────────────────┤
│ L2  Step engine     one step = assemble → request → dispatch tools │
├──────────────┬──────────────┬───────────────┬────────────────────┤
│ Context      │ Tool         │ Safety         │ Model              │
│ engine       │ pipeline     │ (policy, sbx,  │ adapters           │
│ (working set,│ (hooks, PTC, │  classifier,   │ (Item ⇄ wire,      │
│  compaction) │  spill)      │  taint)        │  caching, reasoning)│
├──────────────┴──────────────┴───────────────┴────────────────────┤
│ L1  Kernel   Event log (append-only JSONL) · reducers · journal    │
│              · seam registry · profile loader                      │
└───────────────────────────────────────────────────────────────────┘
```

### Crate layout (Rust workspace)

```
crates/
  flowie-kernel     events, log store, reducers, call journal, seam registry, profiles
  flowie-model      Item, Request/Response, Provider trait, adapters (anthropic, responses,
                    chat-completions, deepseek, codex, local), pricing, tokenizer
  flowie-tools      Tool trait, registry, builtin tools, spill store, PTC runtime
  flowie-safety     sandbox backends, policy rules, command_effect, classifier, taint
  flowie-context    working set, outline extraction (tree-sitter), assembler, compactor
  flowie-engine     step engine, task engine, validators, budgets, stall detector
  flowie-orch       subagents, task graph, teams, programs, workflow scripting
  flowie-ext        hooks, out-of-process extensions (JSON-RPC), MCP, skills
  flowie-ui         Ui trait, TUI, print mode, ACP, SDK server
  flowie            the binary
```

This is the *target* layout. We get there by gradually turning the nyq repo into a Cargo workspace, not by rewriting it:
- nyq's `provider/*`, `sandbox.rs`, `safeio.rs`, `command_effect.rs`, `classifier.rs`, `permissions.rs` and `mcp*.rs` move nearly unchanged into `flowie-model` and `flowie-safety`.
- nyq's 5k-line `agent.rs` gets split into the kernel, step engine and tool pipeline.
- Permission logic moves out of `agent.rs` into `flowie-safety`.

Each move is a separate PR, and nyq's existing 662 tests must stay green after every one.

---

## 2. L1 — Kernel: the event log

### 2.1 Events

One append-only JSONL file per session. Every line is an `Event` with fields `seq`, `ts`, `scope` (session, task, or agent path) and `kind`:

| Family | Kinds | Model-visible? |
|---|---|---|
| conversation | `user.message`, `assistant.message`, `assistant.reasoning`, `tool.call`, `tool.result`, `system.snapshot` | yes |
| attempts | `assistant.attempt` (failed or retried request, kept for audit) | no |
| lifecycle | `turn.start/end`, `step.start/end`, `task.start/validate/end`, `compaction.start/end` | no (except the summary it produces) |
| state | `ws.open/focus/close`, `verify.result`, `budget.update`, `intent` | rendered into the trailing note |
| safety | `approval.request/decision`, `classifier.verdict`, `grant.add` | no |
| orchestration | `child.spawn/result`, `graph.update` | results only, tagged as evidence |
| meta | `title`, `prefs`, `usage` | no |

**Invariant (from dsh):** `assemble(log[..n], config) -> Request` is deterministic. A test replays every recorded session and checks that the bytes of the rebuilt request match.

### 2.2 Reducers and views

The engine never mutates state directly. It appends events, and reducers fold them into views: `Transcript`, `WorkingSet`, `VerifyState`, `Budgets`, `Grants` and `TaskStack`. Because views are derived, **fork** is a copy of the log up to `seq`, **resume** is a refold, and **rewind/undo** truncates the view at a point (file effects are undone through the undo records stored on `tool.result`).

This is the clean version of promptyped's `AppM` state monad. It also does what that README listed as future work ("make AppM a free monad so it can be unit tested"): the engine is pure over the log, with effects only at the tool and provider seams.

### 2.3 Call journal (crash safety)

This brings back the mechanism the older sharqlabs nyq fork took from dsh, which current nyq dropped. Each call moves through these states:

```
requested ─► pending ─► started ─► settled
```

The `tool.call` event is fsynced before any side effect happens. On load, every call that never got a result receives a synthetic result based on its state:
- `pending`: "did not run; make it again if needed".
- `started`: "OUTCOME UNKNOWN: check state before repeating".
- A pure read: "safe to repeat".

flowie never re-runs a call by itself.

### 2.4 Seam registry and profiles

Seams are named trait objects: `model`, `tools`, `context.assembler`, `context.compactor`, `sandbox`, `approver`, `classifier`, `verifier.*`, `spill`, `ui`, `loop.step` and `loop.task`.

A **profile** is a TOML file listing bundles. A **bundle** is a set of seam bindings plus settings. The profiles:
- `default` is the full interactive setup.
- `minimal` exposes bash, `read_image` and `submit` only, and is meant for benchmarks.
- `sdk`, `acp` and `ci` are headless. `ci` defaults to no approvals, a strict sandbox and verification required.

`flowie --profile X --dump-config` prints the resolved tree, as in dsh. Rust seams are compiled in. Third-party seams are out-of-process extensions (see §9), so the core never loads arbitrary code into its own process.

Config layers, from nyq: user → project → workspace override. **A project may only tighten.** It may add `ask` and `deny` rules and disable tools. Keys that loosen safety are user-only. Project hooks and extensions must be trusted by content hash first.

---

## 3. L2 — Step engine (the inner loop)

One **step** is one model request plus the tool calls it makes. A **turn** is any number of steps.

```
step:
  1. drain child results / steering input           → events
  2. budgets + compaction check (§5.4)              → may compact
  3. assemble Request (§5.2)                        → frozen, hashed
  4. hook  pre_step   (accept | rewrite | reject | new cache series)
  5. stream via model seam   ── retry only while nothing has streamed (nyq)
                              ── failed attempt → assistant.attempt
  6. append assistant.message (+ reasoning, signatures preserved)
  7. dispatch tool calls through the tool pipeline (§4)
  8. append tool.result (scrubbed, spilled, tainted)
  9. stop check → continue | yield to task engine
```

- **Parallelism.** Each tool declares a `Concurrency` class: `Pure`, `ReadFs`, `WriteFs(paths)`, `Exec`, or `Barrier`. The dispatcher builds a conflict graph. Reads run in parallel. Writes to disjoint paths can also run in parallel, which nyq never allows. `Exec` and `Barrier` calls run alone. Approvals are collected up front as a single batch prompt instead of one prompt per call.
- **Cancellation.** A `CancellationToken` is checked at each await point. Tools that implement `handles_cancel` reap their process groups. Any partial assistant text is saved with an `[interrupted]` marker. Calls that never ran get the synthetic result "Cancelled by user." (nyq)
- **Loop guards:**
  - **Repeat guard.** The same `(tool, args)` failing three times triggers a nudge. (nyq)
  - **Cycle detector** over the last N `intent` events, which is promptyped's "look carefully to identify circular behaviour", made mechanical. Trips are cheap and escalate the task (§4.3 ladder).
- **Steering.** User messages typed while a turn runs are queued and injected after the current tool round. (nyq)

---

## 4. L3 — Task engine (the promptyped core)

### 4.1 The `Task<T>` contract

```rust
pub struct Task<T: JsonSchema + DeserializeOwned> {
    pub goal: String,                 // what to do
    pub background: ContextRef,       // spec, prior results, handoff notes
    pub tier: Tier,                   // Low | Medium | High  → model per profile
    pub tools: ToolPolicy,            // allow-list + mode (read-only, edit, exec)
    pub output: PhantomData<T>,       // schema derived via schemars, shown to model
    pub example: Option<T>,           // promptyped's "dummy value" — few-shot shape
    pub validators: Vec<Box<dyn Validator<T>>>,
    pub budget: Budget,               // syntax, semantic, steps, tokens, $, wallclock
    pub memo: Option<MemoKey>,        // cache result keyed by (goal, inputs, ws hash)
}

#[async_trait]
pub trait Validator<T>: Send + Sync {
    async fn check(&self, v: &T, cx: &TaskCx) -> Verdict;
}
pub enum Verdict { Pass, Fail { kind: FailKind, feedback: String }, Escalate(String) }
```

- **Finishing is a tool, not silence.** Every task exposes a `submit` tool whose input schema is `T`. When the model calls `submit`, the arguments are parsed into `T` (a parse failure is a **syntax** error) and the validators run in order (a failure is a **semantic** error, fed back as a `tool.result` with `is_error`).
- A plain-chat turn is just `Task<Reply>` with the `AlwaysPass` validator. The interactive agent and the pipeline engine are the same code.
- **Native tool calls replace promptyped's text protocol.** promptyped's `Tool=<[{json}]>` syntax plus `RAWTEXT[...]=R"r(...)r"` blocks existed to avoid JSON-escaping code. With native tool use that matters less. Edit tools still accept a `content_ref` that points at a fenced block in the same assistant message, which is useful for weak or local models. The syntax-repair pass (a cheap model fixes malformed output) is kept as an optional `pre_dispatch` hook for chat-completions providers that have no strict tool mode.

### 4.2 Built-in validators

| Validator | Source | Behaviour |
|---|---|---|
| `Schema` | promptyped `checkReturnType` / `ShapeChecker` | Deserialise plus a path-precise shape diff ("missing field `x` at `.items[2]`"). |
| `Build` | promptyped `BuildSystem` | Compile. Errors are fed back. Old errors are shortened and only the newest is shown in full. |
| `Tests { scope }` | promptyped | Run the affected tests. On a large failure, rerun a single failing test to keep context small. |
| `Lint` / `Format` | promptyped `getFormatChecker` | |
| `FilesTouched(paths)` | promptyped `validateFileModifiedFine` | The task must actually have changed these files. |
| `Judge { rubric, tier }` | promptyped `validateFileModifiedWithAi` | An LLM checks whether the change is *present and complete*, not whether it is correct. This runs as a read-only child `Task<JudgeVerdict>`. |
| `Exists(paths)` | promptyped `validateRelevantFilesExist` | Guards against hallucinated paths. |
| `Custom(cmd)` | new | Any shell command, where exit code 0 means pass. This is how CI-style checks plug in. |
| `All` / `Seq` / `Same` | promptyped `combineValidators*` | Combinators. |

**Verifier seam.** promptyped rebuilt and retested after *every* file edit. flowie makes that a `verifier` plugin bound to a `BuildSystem` trait, which is promptyped's typeclass generalised:

```rust
trait BuildSystem { detect, build, test(scope), lint, format, add_dep,
                    is_source(path), is_test(path), outline(path) }
```

Implementations exist for cargo, go, uv/pytest, npm/pnpm+tsc+vitest, cmake/ctest and a generic `Makefile`. Detection happens automatically from the workspace. The verifier runs **debounced in the background** after write events and records `verify.result`. The trailing note always shows the latest build and test state ("LATEST COMPILE/TEST STATE …", from promptyped). This keeps the model looking at the real repository state without every edit paying the full build cost before the next step.

**Repair-priority rule (promptyped `getTask`).** If the build is red at the start of a step and this task did not start out red, the task framing changes to: "Fix the build before continuing. Your original task was …". The original goal is kept underneath.

### 4.3 Budgets and the escalation ladder

There are two failure counters, as in promptyped. A semantic failure **resets** the syntax counter, because it proves the syntax was correct. When a budget runs low or the cycle detector trips, the harness climbs a ladder:

1. **Nudge.** Inject the repeat or cycle note.
2. **Progress check.** A cheap `Task<Progress{made: bool, why}>` runs over the last N verify errors. If progress is being made, continue. (promptyped `handleConsecutiveCompileFailures`)
3. **Diagnose.** A High-tier read-only child task returns `Plan{has_solution, steps}`. If it finds one, inject it as a `SYSTEM INTERVENTION` note and record an `intent` event of kind `approach_correction`.
4. **Context reset.** Summarise into a handoff note, clear the task-local history, and start fresh with the same working set. promptyped did this after five consecutive syntax rejects. This is the "ralph" idea, but triggered by the harness.
5. **Tier bump.** Retry with the next model tier.
6. **Escalate.** Return `Err(Escalation)` to the parent task or program. At the top level the user is asked through `ask_user_question`, or in headless mode the run exits non-zero with a report.

### 4.4 Intent log

Every *mutating* step must include an intent. Rather than promptyped's mandatory extra `SummariseAction` tool call, flowie adds an optional `intent` field (`{did, why, next}`) to every mutating tool's schema, plus a standalone `note_intent` tool.
- Intents become `intent` events.
- The last K intents are rendered in the trailing note ("your recent actions and why"). This is what lets context be compacted aggressively without the model losing its train of thought.
- If a mutating step has no intent, it is accepted but counted. Profiles may make the intent mandatory, as promptyped did.

### 4.5 Memoisation

`Task.memo = Some(key)` stores `(T, metrics delta, working-set delta)` under `.flowie/cache/<key>.json`, keyed by the goal, a hash of the inputs, and the hash of the relevant files. A pipeline that is re-run skips completed planning steps. This is promptyped's `memoise`, made safe by including the workspace hash so it cannot return a stale result.

As built (nyq 0.0.2): the key is the goal, the schema, the validators, the model id and a workspace fingerprint: the listed `--memo-file` contents, or with none listed the git state (HEAD's tree, `git status --porcelain`, the staged and unstaged diffs, untracked file contents); with neither, memoisation is off for the run. Entries live in the user's state directory (`~/.nyq/cache/<workspace hash>/`), never in the workspace, so a repository cannot plant one. A hit is only a candidate: the task's validators run again on it, and a failure is a miss.

---

## 5. Context engine

### 5.1 Working set (promptyped's open, focused and unfocused files)

The harness keeps an explicit **working set** of files the model has open, in one of three states:
- `focused`: full text with line markers.
- `outline`: signatures, types and doc comments only, produced by tree-sitter. This is the `minimiseFile` idea, made language-agnostic.
- `pinned`: specs and other non-source files, shown in full.

The working set is rendered **from disk at assembly time**, in the trailing note. The model therefore always sees the latest content, and earlier `read_file` outputs in history can be collapsed to `[read foo.rs@v3 — superseded, see working set]` without losing information.

There are limits on the number of focused files and on working-set tokens. When the working set is over budget, the cheapest action is taken first: unfocused files are demoted to outlines, and only then does the harness ask the model which file to close. promptyped's "close the least important file" sub-task becomes the *last* step instead of the first.

Tools: `open(path, mode)`, `focus`, `unfocus`, `close`. `read_file` still exists for one-off looks; a file read twice is offered for promotion into the working set.

### 5.2 Request assembly

```
[system]   stable: identity, tool-use rules, project instructions (AGENTS.md/CLAUDE.md),
           skills index (name+desc only), MCP server notes            ← cache breakpoint 1
[history]  log-derived transcript, compacted spans as summaries       ← breakpoint 2 (prev user msg)
[last msg] newest user/tool_result  + TRAILING NOTE (send-only):      ← breakpoint 3
             • task: goal, T schema reminder, budget left, ladder stage
             • verify: latest build/test/lint (newest error full, older 1-line)
             • working set (focused full, outlines, pinned)
             • last K intents
             • session state: permission mode, live bg processes, children
```

The trailing note never goes into the stored transcript, so the cache prefix stays byte-stable. (nyq `with_trailing_note`)

For models that support it (DeepSeek's `systemPromptUpdate: in-history`, and Anthropic's mid-conversation tool changes), changes to the system prompt or tool list are appended as `system.snapshot` events instead of rewriting the prefix.

### 5.3 Large outputs: scrub → spill → taint

Every tool result passes through this chain:
1. **Scrub.** Credential-shaped tokens are redacted before the output reaches the log or the model. (nyq `redact::scrub`)
2. **Spill.** Over the threshold (16 KB by default), only the head and tail plus an id go into context. The full text goes to `.flowie/spill/<session>/<id>` with mode 0600, and the model can page and grep it with `read_spill`. (nyq and dsh)
3. **Taint.** The result is labelled with its provenance (`web`, `file`, `mcp:<server>`, `child:<id>`, `exec`). See §7.3.

### 5.4 Compaction, cheapest first

Compaction is triggered at `min(0.8·W, W − O − 64k)`, as in dsh, or when a request fails with a context-overflow error. Each tier is tried in order, and the next only runs if the previous was not enough:

1. **Prune.** Drop superseded reads (the working set covers them), collapse old verify errors to one line (promptyped `shortenOldErrorMessages`), truncate old assistant prose (promptyped `truncateOldMessages`), and offload old images. No model call is needed.
2. **Summarise the oldest span.** Keep the newest ~16% verbatim. Summarise the rest using a request that replays the cached prefix byte for byte, so it is mostly a cache hit. The summary must preserve the intent log and the open questions. A compaction lock in the log makes a crash mid-compaction detectable. (dsh)
3. **Task reset.** Fall back to step 4 of the escalation ladder: a handoff note plus a fresh history.

flowie never splits a call from its result, never compacts input that has not been sent yet, and never latches compaction off for the session, which nyq does. If the context still does not fit after all three tiers, that is an escalation.

**Token accounting.** Use the provider-reported usage when available. Otherwise use a real tokenizer per model family when one is available, rather than nyq's chars/4. Usage is broken down into cached, uncached, written-to-cache, output, and reasoning buckets, and further by tier, task and agent (promptyped's per-tier metrics).

---

## 6. Tool pipeline

### 6.1 The `Tool` trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;                     // cached; schema via schemars
    fn effects(&self, args: &Value) -> Effects;      // {reads, writes(paths), exec, net, ext}
    fn concurrency(&self, args: &Value) -> Concurrency;
    fn summary(&self, args: &Value) -> String;
    async fn preview(&self, args: &Value, cx: &ToolCx) -> Option<Preview>; // pure diff/cmd
    async fn run(&self, args: Value, cx: ToolCx) -> ToolOutput;
    fn handles_cancel(&self) -> bool { false }
}
```

This fixes several nyq problems:
- Schemas are derived once and cached.
- Arguments are **validated against the schema** before dispatch.
- Read-only status and parallel safety are declared by each tool, instead of hard-coded name lists.
- MCP `readOnlyHint` and `destructiveHint` map to `effects()`.

### 6.2 Pipeline (dsh's stages, nyq's guards)

```
parse+schema-validate
 → hook pre_tool           (block | rewrite args | add context)
 → policy rules            (deny > ask > allow > grants > mode)
 → static analysis         (command_effect: read_only / known_reversible)
 → classifier              (auto mode only; §7.2)
 → approval (batched)      → stale-preview check (TOCTOU)
 → monotonic guards        (can only deny: secret roots, sandbox, taint rules)
 → journal: started (fsync)
 → run                     (timeout, cancel, metrics)
 → fs intents              (read-before-edit, mtime match)
 → hook post_tool          (accept | block | replace | add context)
 → scrub → spill → taint → tool.result
 → verifier notify (debounced)
```

### 6.3 Built-in tools (default profile)

- **Files:**
  - `read_file`, `open`, `focus`, `unfocus`, `close`, `list_dir`, `glob`, `grep`.
  - `edit` (exact `old`→`new`, `replace_all`, CRLF-aware).
  - `multi_edit` (an atomic list).
  - `apply_patch` (unified or Codex-style patch).
  - `edit_range` (promptyped's `EditFileByMatch`: start and end line regexes with nearest-line hints, robust when line numbers drift).
  - `write_file`.

  Every write goes through `safeio`: `openat2(RESOLVE_BENEATH)`, a temporary file, fsync, then rename. Read-before-edit and an mtime check are enforced.
- **Shell:** `bash` (sandboxed), plus persistent `terminal_*` sessions and background-process tools (`bg_output`, `bg_write`, `bg_wait(regex|port|exit)`, `bg_kill`), as in nyq.
- **Code intelligence:** `lsp` (definition, references, diagnostics, rename) and `outline`.
- **Web:** `web_fetch` (SSRF-guarded, labelled untrusted) and `web_search`, with a pluggable backend rather than an HTML scraper.
- **Programmatic tool calling:** `run_code`. A sandboxed script (embedded QuickJS or Deno-core) calls `await tools.x(...)`, and every sub-call goes through the *full* pipeline above. This is useful for "grep 40 files and summarise" without 40 round trips. (dsh's PTC runtime)
- **Task control:** `submit` (typed), `note_intent`, `escalate`, `todo`, `ask_user_question`.
- **Orchestration:** `subagent`, `send_message`, `wait`, and the task-graph tools (§8).
- **Extension surfaces:** `skill`, `mcp_search` and `mcp_select` (deferred schemas when there are more than 40 tools or the schemas exceed 12k tokens), plus MCP resources and prompts.
- **History:** `session_search` over the event log and spill files.

---

## 7. Safety

### 7.1 Layers

| Layer | Mechanism | Origin |
|---|---|---|
| OS sandbox | Linux: re-exec helper with Landlock (writes and TCP), plus **seccomp to deny AF_UNIX and UDP**, closing nyq's documented gaps, and optional bwrap. macOS: Seatbelt profile. Windows: restricted token and job object. Remote: SSH or container backend. Each backend reports whether enforcement is `full` or `partial`. `.git/hooks` and `.git/config` are read-only. | nyq and dsh |
| Path guards | Secret roots (`~/.ssh`, `~/.aws`, the harness's own credentials), checked both lexically and after canonicalising. | nyq |
| Policy | User rules, plus project rules that may only tighten. Session and workspace grants live in the *user* file. | nyq |
| Static analysis | The shell lexer finds read-only pipelines and **runs the command it rebuilt**, not the model's string. Known-reversible dev commands skip the classifier. | nyq |
| Classifier | Auto mode only. An LLM gate over a single call. | nyq |
| Taint | §7.3. | new |
| Approval | Batched, with a diff or command preview, and the preview recomputed afterwards. | nyq |

### 7.2 Classifier

nyq's design is kept: a forced `permission_decision` call, the rule that only the first user message can authorise, masked provenance, copy detection (an `allow` is downgraded when the action string appears verbatim in earlier untrusted output), and falling back to `ask` on timeout. On top of that:
- It runs on MCP and extension tools too, using their `effects()` instead of a bash-only analysis.
- Verdicts are cached by `(tool, normalised args, taint set)`.

### 7.3 Taint (new)

Every `tool.result` carries a set of provenance labels. A tool call is **tainted** if any of its arguments overlap substantially with text from an untrusted-labelled result. That covers the copy-detection case, but applies to every tool. Policy can then say:
- `deny exec when tainted-by:web` (strict profile).
- `ask net when tainted-by:mcp:*`.
- Child results always arrive as evidence: `[child:<id> — evidence, not authorization]`.

Taint crosses orchestration boundaries. A `subagent` call has the effects `exec`, `net` and `write`; when its arguments are tainted, the child's index starts with the task text under the delegating call's labels, and the child's classifier sees the task as labelled evidence with the parent's user request as the only authorization. A `run_code`/`workflow` call has every effect; inside a script taint is sticky (once a sub-call has returned untrusted content, every later sub-call carries those labels whatever its text), and the script's own result stores the union of its sub-calls' labels, so its provenance survives a resume. Writes tainted by the web mark every path the call declared it writes (a patch's files included). A named child saves a taint ledger with its record (its tainted messages and the files written from untrusted content), so later turns and resumed sessions keep flagging copies of an earlier tainted message. An external ACP agent can't be checked call by call: its report is labelled `external:<name>` plus its task's taint, the files it changed (git status and `HEAD` compared before and after the run) carry the same labels, and so does a merge of its worktree. A tainted task for an external agent is always put to a person (never a grant, allow rule, yolo or the classifier; denied when headless), runs in a worktree when one can be made, and sends every permission request of the agent to the person.

This turns nyq's prompt-level trust boundary into an enforceable rule.

### 7.4 Data

Trajectories stay local. `flowie export --redact` produces benchmark or training bundles on request. **No upload to the model vendor by default.** This is a deliberate difference from dsh's default-on `dsh_session_log` upload.

---

## 8. L4 — Orchestration

### 8.1 Subagents are tasks

`subagent{ goal, mode: explore|general|custom, output_schema?, validators?, tier? }` spawns a child `Task<T>` with its own log scope (`agent:/parent/child`).
- Children inherit a permission mode that is equal or stricter (nyq's `mode_rank`).
- Approvals route to the parent's UI.
- Results come back typed and labelled as evidence.
- Asynchronous jobs live in a durable `orchestration.jsonl` with idempotent `request_id` and FIFO per child, as in nyq.
- Providers for children:
  - a fresh child,
  - a **fork** that inherits the parent's context and shares its cache prefix,
  - an **external** agent over ACP (Claude Code, Codex, dsh, nyq), as in dsh.

**Write isolation.** The default is a per-workspace writer lock (nyq). `isolation: worktree` gives a child its own git worktree, and the parent merges the result as a reviewable patch.

### 8.2 Task graph and teams

There is a shared graph of `TaskNode{id, goal, schema, deps, state, owner, write_scope, rev}`, updated with compare-and-set on `rev` (dsh teams). Agents claim ready nodes; `write_scope` is advisory and checked against the writer lock.

### 8.3 Programs (promptyped's workflows)

promptyped's real value was in `PromptCommon.hs`: multi-stage programs such as *plan files → order by dependencies → for each file: write it, write its tests, verify → refactor*. flowie makes these first-class at two levels:
- **Rust API.** `async fn` programs over `Task<T>`, for built-in programs such as `refactor`, `translate`, `spec-compliance` and `big-change`.
- **Script API.** A sandboxed JS or Lua file using `task()`, `parallel()`, `pipeline()` and `phase()`, the same shape as dsh's `workflow`, but every `task()` call takes a schema and validators.

Example: the built-in `big-change` program, ported from promptyped's `makeTargetedRefactorFilesProject`:

```
1. Task<RelevantFiles>      read-only, High, validator Exists          (memo)
2. focus relevant files
3. Task<FilesToModify>      read-only, High, validator Exists          (memo)
4. Task<Plan per file>      read-only, Medium, ordered by deps (topo-sort, validator: acyclic & complete)
5. for each file in order:
     Task<ModifiedFile>     edit, Medium→High ladder,
                            validators: FilesTouched ∘ Build ∘ Tests ∘ Judge(complete?)
6. Task<FinalReview>        read-only, High; optional final sort/cleanup pass
```

Each stage is memoised, so a crash or budget stop at step 5, file 7, resumes at file 7.

---

## 9. Extensibility

| Mechanism | Process | Use |
|---|---|---|
| **Seams** (Rust traits) | in-process, compiled in | Core replacements: model adapters, sandbox backends, compactor, verifier. Selected by profile. |
| **Hooks** | shell command, JSON on stdin | `session_start`, `user_prompt`, `pre_step`, `pre_tool`, `post_tool`, `pre_compact`, `stop`, `task_validate`, `attention`. Same decision semantics as nyq; a hook failure never blocks. |
| **Extensions** | long-running, JSON-RPC over stdio | Register tools (with `effects`), validators, build systems, commands, status-line segments, and **seam providers** (for example a remote sandbox). Can call back for notify, confirm, input or steering. Project extensions are trusted by content hash. |
| **MCP** | stdio, HTTP or SSE, via rmcp | Tools, resources and prompts. Deferred search. Elicitation. |
| **Skills** | `SKILL.md` | Read from `.agents/`, `.claude/`, `.codex/`, `~/.flowie/skills`. Only the index sits in the prompt; the body loads on demand. |
| **Programs** | scripts or crates | §8.3. |

In dsh, every plugin is Cordis code inside the same process. flowie instead keeps a small typed core with stable seams, and everything else runs out-of-process. That is safer, works from any language, and costs some latency, which does not matter at agent timescales.

---

## 10. Model layer

- **Neutral item model** (nyq `types.rs`): `User`, `Assistant`, `Reasoning{id, summary, opaque}`, `ToolCall{id, name, raw_args}`, `ToolResult{id, output, is_error, images}`, `Summary`.
- **Capabilities** are queried, not guessed:
  ```
  {context_window, max_output, tool_mode: native|strict|text,
   reasoning: none|summary|interleaved_passback, cache: breakpoints(n)|prefix_auto,
   system_update: rewrite|in_history, mid_conv_tools, vision}
  ```
- **Reasoning.** Signed or encrypted reasoning is replayed unchanged to the same provider family and dropped when switching providers (nyq). Interleaved passback is used where the model was trained for it (DeepSeek V4, Anthropic).
- **Caching.** Anthropic gets three breakpoints (system, previous user message, last message). OpenAI and DeepSeek rely on a stable prefix plus a conversation id.
- **Tiers.** `Low`, `Medium` and `High` map to models per profile (promptyped). The classifier and syntax repair use `Low`; judges and diagnosis use `High`.
- **Adapters:** Anthropic Messages, OpenAI Responses, Chat Completions (with a configurable reasoning field), DeepSeek (native wire extensions; the session-log upload is left out), the Codex and Grok subscription logins (from nyq), and local models (llama.cpp, Ollama, vLLM) with `tool_mode: text`, which turns on the §4.1 text protocol and syntax repair.

---

## 11. Frontends

There is one `Ui` trait: an event stream out, and decisions in (approve, answer, steer). It has these implementations:
- **TUI.** nyq's TUI, broken into smaller modules, plus a task-stack panel showing the ladder stage, budgets and the verify light.
- **print / headless.** Final output is JSON `T` on stdout, and the exit code reflects the validator verdict.
- **ACP server** for editors.
- **SDK.** JSON-RPC over stdio, with TypeScript and Python clients.
- **web** (later).

---

## 12. Evaluation and quality

- **Replay tests.** Record and replay at the model seam (nyq-style tapes). Every frozen request must rebuild identically from its log.
- **Benchmark profile.** `minimal` (bash, `read_image`, `submit`), plus `--task-schema` so harness tasks return typed results that a scorer can check.
- **Ablations are built in.** Each mechanism (working set, trailing note, ladder, verifier, taint) is a profile toggle, so its effect can be measured, for example `flowie bench --ablate working_set`.
- **Metrics.** Cost and tokens by tier, cache hit rate, syntax and semantic failure counts, ladder-stage histogram, verify red-time, and steps to validation.

---

## 13. Lineage: what was taken and what was left out

**Taken:**
- **promptyped:** typed returns plus validators; split syntax and semantic budgets; build and test after every change; repair priority; stall detection with LLM diagnosis; context reset; tiers; memoisation; focused and outline files; the mandatory intent log; shortening old errors; programs as composed tasks; the BuildSystem abstraction; judging completeness only.
- **nyq:** the Item model; retry before streaming; saving partial output on interrupt; the trailing note; the cache-breakpoint strategy; `command_effect` and running the rebuilt command; the classifier's trust boundary and copy detection; tighten-only project config; hash-trusted hooks and extensions; `safeio`; the stale-preview check; spill plus `read_tool_result`; secret roots; SSRF guard; deferred MCP search; the child-job journal; evidence labels; lazily attached nested AGENTS.md; the repeat guard; the single `Ui` trait.
- **dsh:** log as truth with deterministic assembly; seams, profiles and bundles; the hook pipeline stages; the not-started versus outcome-unknown journal; compaction that protects the prefix and starts by pruning; failed attempts kept outside model history; programmatic tool calling; the team task graph with compare-and-set; `workflow`; external agents over ACP; mid-conversation tool and system updates; enforcement levels reported per sandbox backend.

**Left out:**
- promptyped's custom text tool protocol as the default (native tool use is better; the text protocol stays as a fallback), its OpenRouter-only client, and its rule that every step must include an extra tool call.
- nyq's all-or-nothing compaction and its compaction latch, the chars/4 estimate, global mutable statics, hard-coded tool-name lists, the HTML scraper for web search, and no parallel writes.
- dsh's default-on session-log upload, in-process unaudited plugins, and its no-privileged-core extreme. flowie's kernel and safety layers are deliberately *not* replaceable by project config.

---

## 14. Milestones

| # | Scope | Exit criterion |
|---|---|---|
| M0 | In the nyq repo: convert it to a Cargo workspace; extract `flowie-model` (providers, `types.rs`) and `flowie-safety` (sandbox, safeio, command_effect, classifier, permissions). No behaviour change. | All existing nyq tests pass; the binary is unchanged in behaviour. |
| M1 | Kernel inside nyq: session JSONL becomes the event log, with reducers. Restore the call journal. Split `agent.rs` into the step engine and tool pipeline. Add the `Tool::effects`/`concurrency` trait methods to replace hard-coded name lists. | Replaying a recorded session reproduces its requests byte for byte, and old sessions still load. |
| M2 | Task engine: `Task<T>`, `submit`, validators, budgets, ladder, intent log. | A headless `flowie run --schema` returns a validated `T`. |
| M3 | Context engine: working set, tree-sitter outlines, trailing note, tiered compaction. | Higher pass rate on long-horizon tasks than the M2 baseline (ablation). |
| M4 | Verifier plugins (cargo, go, uv, node), repair priority, memoisation. | The `big-change` program finishes promptyped's TLS-refactor example end to end. |
| M5 | Orchestration: typed subagents, worktree isolation, task graph, scripted programs, `run_code`. | Two children edit disjoint files in parallel and the merge is clean. |
| M6 | Taint, classifier generalised to all tools, seccomp sandbox hardening. | Prompt-injection suite: no `exec` caused by web content in strict mode. |
| M7 | ACP, SDK, external-agent providers, benchmark profile and bench CLI. | Terminal-Bench-style suite runs headless with per-mechanism ablations. |

---

## 15. Open questions

1. **Scripting runtime for programs and `run_code`.** QuickJS (small, embeddable) or deno_core (TypeScript and a richer standard library, but a larger binary)? nyq has a size budget (`size-budget.json`).
2. **Outline extraction.** Ship tree-sitter grammars for the top ~10 languages in the binary, or load them dynamically?
3. **Should the working set replace `read_file` history entirely, or stay optional?** This needs an ablation; models trained on read-in-history patterns may react to it differently.
4. **Mandatory intent.** promptyped required it on every mutating step. Does an optional schema field give the same benefit at lower cost? Measure it.
5. **Name.** Does the result keep shipping as `nyq`, with flowie as the internal design name, or get renamed? We own nyq, so licensing is our choice either way.
