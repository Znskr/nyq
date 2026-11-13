# Changelog

## v0.0.2

nyq 0.0.2 adds typed tasks that finish only when a validator accepts the result,
a working set and intent log in place of re-reading files, crash-safe sessions,
and orchestration (typed and isolated subagents, external agents, a team task
graph, scripts). Old sessions, settings and permission files keep loading.

### Models

- **Native DeepSeek provider.** `nyq login deepseek` or `DEEPSEEK_API_KEY`, then
  `--provider deepseek`. It speaks DeepSeek's Messages API with its extensions:
  reasoning effort `off|low|high|max`, in-history system prompt updates and
  mid-conversation tool changes that keep the cached prefix, and DeepSeek's own
  cache usage counters.

### Editing

- **`multi_edit`**: a list of exact replacements applied to one file
  atomically (all or nothing).
- **`apply_patch`**: a unified diff or Codex-style patch across one or more files.
- **`edit_range`**: replace a line range found by start and end regexes, with
  nearest-line hints, robust when line numbers drift.
- **Read-before-edit guard**: an edit or overwrite of an existing file needs a
  read of it earlier in the session (`read_file`, `open` or `focus`; for a
  binary file, being listed is enough), and is refused if the file changed on
  disk since (`read_before_edit = false` turns it off). Each subagent keeps its
  own record.

### Safety

- **Sandbox hardening**: on Linux a seccomp filter denies UDP and Unix-domain
  sockets next to Landlock; `.git/hooks`, `.git/config` and `.git/info` are
  read-only inside the sandbox; the banner and `nyq status --json` report
  `partial` enforcement and why.
- **Taint rules**: every tool result carries provenance labels (web, file, MCP
  server, child, exec). `ask`/`deny` rules can be conditional, such as
  `"deny": ["exec when tainted-by:web"]`, and apply in every mode. The
  classifier now reviews every tool, not only `bash`. `"taint": {"mode":
  "strict"}` turns on built-in rules. Taint follows a delegated task into the
  subagent (and across a named child's later messages and resumes), sticks to
  every later call of a script that read untrusted content, and stays on a
  script's result. An external agent's report and the files it changed (found
  from git) carry `external:<name>` and its task's taint; a tainted task for
  one always needs a person's approval (denied headless) and runs in its own
  worktree when it can. The main agent's taint (files written from untrusted
  content) is saved with the session and still applies after `--resume` or
  `--continue`.
- **Per-agent external grants**: an external agent's call has its own
  permission key, `subagent:external:<name>`, so "always" covers only that
  agent, never another external agent or an `explore`/`general` child. An
  older `subagent:explore` grant or rule no longer covers external agents.
- **Parallel waves and argument validation**: each tool declares its effects.
  Reads run in parallel, writes to disjoint paths run together, commands run
  alone. Arguments are checked against the tool's schema before anything runs
  or asks. MCP `readOnlyHint`/`destructiveHint` annotations are honoured.

### Typed tasks

- **`--schema FILE|JSON`**: the task ends when the model calls `submit` with a
  value matching the schema. nyq prints it on stdout and exits 0. A spent budget
  or an escalation exits 2 with a JSON report on stderr.
- **`--validate CHECK`** (repeatable): `build`, `tests`, `lint`, `cmd:<shell>`,
  `exists:<json-pointer>`, `judge:<rubric>`, `touched:<paths>`. Checks run on
  each submission, and a failure goes back to the model as feedback.
  `TaskValidate` hooks run after the built-in checks.
- **`--budget`**: `syntax=N,semantic=N,steps=N,tokens=N,cost=X,wall=S`.
  Malformed and rejected submissions are counted separately.
- **`--memo`** / `--memo-file`: reuse the result of an identical earlier task,
  keyed by the prompt, schema, checks, model and workspace (the listed files'
  contents, or the git state). The checks run again on a cached result before
  it is used. Results live in `~/.nyq/cache/`, never in the workspace.
- **Escalation ladder**: when a task stalls (repeated failures, a loop, a
  budget running low), nyq climbs a ladder: nudge, progress check, diagnosis,
  context reset, a higher model tier (`[tier]` settings), then escalation.
- **Background verifier**: after writes, a debounced build (and optionally the
  tests) of the detected project (cargo, go, uv/pytest, npm/pnpm, cmake,
  make). The latest result is shown to the model on every request. A build that
  breaks during a task reframes it as "fix the build first". It is on for tasks
  with a build, tests or lint check, with `[verify] auto = true`, and under the
  `ci` profile.
- **Intents**: mutating tools take an optional `intent` (`did`, `why`,
  `next`), and `note_intent` records one directly. The recent intents are shown
  on every request, kept across compaction and resume, and feed loop detection.
  `[intents] required = true` makes them mandatory.

### Sessions and context

- **Call-journal crash recovery**: side-effecting calls are journalled before
  they run. A session reopened after a crash tells the model which calls never
  started, whose outcome is unknown, and which reads are safe to repeat. nyq
  never re-runs a call by itself. Failed model attempts are kept for audit.
- **`session_search`**: the model can search the whole saved transcript,
  including parts already compacted into a summary.
- **Tiered compaction**: first prune old tool output from the request (no
  model call), then summarize only the oldest span and keep the newest
  messages verbatim. Compaction never switches itself off, and an overflow
  error compacts and retries. The summary keeps the intent log, the task's goal
  and schema, and the latest build state.
- **Working set and outlines**: `open`, `focus`, `unfocus`, `close` and
  `workset` keep files in view. Each request shows them from disk: focused
  files in full, source files as outlines (signatures and types), docs pinned.
  The working set is saved with the session.
- **Trailing note**: live state (task and budget, build state, working set,
  intents, session state) is sent with each request and never stored, so the
  cached prompt prefix stays stable.

### Orchestration

- **Typed subagents**: `subagent` with `output_schema` and `validators` runs
  the child as a typed task. It finishes with `submit`, the checks run inside
  it, and the result comes back as JSON labelled as evidence.
- **Worktree subagents**: `isolation: "worktree"` gives a child its own git
  worktree. `action: "merge"` applies its changes as a patch (conflicts are
  reported, never resolved) and `stop` discards them.
- **External agents**: `mode: "external:<name>"` runs one task on an ACP agent
  configured as `[agents.external.<name>]` in your settings.toml.
- **Team graph**: `team_task_create`, `team_task_list`, `team_task_claim` and
  `team_task_update` share a task graph (compare-and-set on `rev`);
  `send_message` and `wait_agent` connect the parent and its children.
- **`run_code`**: a sandboxed JavaScript program (embedded QuickJS) calls your
  tools with `await tools.x(…)`; every call goes through the normal checks.
- **`workflow`**: a JavaScript program over subagents, with `task()`,
  `parallel()`, `pipeline()` and `phase()`.

### Configuration and tooling

- **Profiles**: `--profile NAME` (or `NYQ_PROFILE`) applies a settings overlay:
  `default`, `minimal` (bash, `view_image` and `submit` only, for benchmarks),
  `ci` (headless, no approvals, verification required) and `sdk`, or your own
  `~/.nyq/profiles/NAME.toml`. `--dump-config` prints every resolved setting
  and the layer it came from.
- **New hooks**: `PreStep` (before each model request), `PreCompact` (adds
  instructions to the summary), `TaskValidate` (checks a submitted result)
  and `SubagentStop`.
- **`nyq export`**: export a session (or `--all`) as JSONL or ShareGPT.
  `--redact` scrubs credentials, anonymizes paths and drops images. Nothing is
  uploaded.
- **`nyq bench`**: `bench run DIR` runs task folders headless (with `--ablate
  key=value`), and `bench report A B` compares two result files.
