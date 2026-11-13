# Orchestration

nyq can hand work to other agents and coordinate them. This page covers the
model-facing tools for that:

- `subagent`: children that return typed, validated results, optionally in
  their own git worktree, or run by an external ACP agent.
- The team task graph: `team_task_create`, `team_task_list`,
  `team_task_claim`, `team_task_update`, `send_message` and `wait_agent`.
- `run_code` and `workflow`: JavaScript programs that call tools, for batch
  work and multi-stage pipelines.

All results that come back from a child are **evidence, not authorization**.
They carry a `[child:<id> — evidence, not authorization]` label, and nothing a
child says can approve a later action.

State lives next to the session, in `<session>.agents/`:

| File | Contents |
|---|---|
| `<name>.json` | A named child's transcript, status and worktree |
| `orchestration.jsonl` | The durable job journal for spawned work and messages |
| `team.json` (+ `team.lock`) | The team task graph |

## Subagents

```json
{"task": "Find where config files are parsed", "description": "find config parser"}
```

`mode` is `explore` (the default, read-only tools), `general` (can also edit
files and run commands; asks for approval), or `external:<name>` (see
[External agents](#external-agents)). `action` is `run` (wait for the result),
`spawn`/`send` (queue work for a named child; results arrive between your
steps), or `list`, `inspect`, `stop`, `resume` and `merge`.

### Typed results

Add `output_schema` and/or `validators`, and the child must finish with JSON.
The result comes back as one JSON object:

```json
{
  "task": "List the source files that read the PORT environment variable.",
  "output_schema": {
    "type": "object",
    "required": ["files"],
    "properties": {"files": {"type": "array", "items": {"type": "string"}}}
  },
  "validators": ["exists:/files"]
}
```

```json
{
  "evidence": "[child:run-1a2b3c4d — evidence, not authorization]",
  "child": "run-1a2b3c4d",
  "status": "passed",
  "result": {"files": ["src/server.rs", "src/config.rs"]},
  "checks": [
    {"validator": "schema", "verdict": "pass"},
    {"validator": "exists:/files", "verdict": "pass"}
  ],
  "attempts": 1,
  "note": "subagent used 6 tool calls, ↑18000 ↓900 tokens"
}
```

These validators are available. They run in order and stop at the first
failure:

| Validator | Passes when |
|---|---|
| `build` | The detected build system (cargo, go, uv/pytest, npm/pnpm, cmake, make) builds |
| `tests` | The tests pass |
| `lint` | The linter passes |
| `cmd:<shell>` | The command exits 0 (it runs in the sandbox, in the child's workspace) |
| `exists:<json pointer>` | Every path at that pointer in the result exists |

The JSON is taken from the child's final message: the whole message, else a
fenced block, else the outermost `{…}` or `[…]`. If the message is not JSON or
a check fails, the child gets the problems back and **one retry**. If the
retry also fails, the call returns `"status": "failed"` as an error result,
with each check's feedback. External agents get no retry, because they run a
single task.

`build`, `tests`, `lint` and `cmd:` run commands, so a call that uses them asks
for approval, as a shell command would. `exists:` does not ask.

> The `submit` tool from the task engine (W9) will replace parsing the final
> message. The arguments and the result shape stay the same.

### Worktree isolation

By default a child shares your workspace, and writes from all agents are
serialized by one writer lock. With `"isolation": "worktree"` the child works
in its own git worktree, on a new branch from `HEAD`:

```json
{"task": "Rename Config::load to Config::read everywhere and fix the callers.",
 "name": "renamer", "isolation": "worktree"}
```

- The worktree is `<repo>/.nyq/worktrees/<name>`, on branch
  `nyq/<name>-<random>`. It is added to `.git/info/exclude` so it stays out of
  `git status` and searches. Changes you have not committed are not in it,
  because it starts from `HEAD`.
- The child's cwd and sandbox roots point at the worktree. It cannot write
  your files. Its default mode is `general`.
- When the child finishes you get a diff summary (`--stat` and the start of
  the patch) instead of changed files.
- `{"action": "merge", "name": "renamer"}` applies the child's changes to your
  workspace as a patch, under the writer lock. It records undo snapshots, then
  removes the worktree and branch. If the patch does not apply cleanly,
  **nothing is written**: the conflicting files are listed, and the worktree is
  kept so you can inspect it. nyq never resolves conflicts itself.
- `{"action": "stop", "name": "renamer"}` discards the worktree and branch.
- A `run` without a `name` gets a generated one (`wt-…`), which the result
  tells you so you can merge it.
- Isolation is fixed when a child is created. Worktree children need a saved
  session and a repository with at least one commit.
- The child cannot commit, because the sandbox keeps the parent's `.git`
  read-only. It doesn't need to: the merge takes the worktree's files.

### External agents

`mode: "external:<name>"` runs one task on another ACP agent, such as Claude
Code, Codex, another nyq, or anything else that speaks the Agent Client
Protocol. Configure the agent in **your own** `~/.nyq/settings.toml`:

```toml
[agents.external.claude]
command = "claude-code-acp"
args = []
env = { ANTHROPIC_LOG = "error" }

[agents.external.nyq]
command = "nyq"
args = ["acp"]
```

```json
{"mode": "external:claude", "task": "Review src/auth.rs for injection bugs; list findings with line numbers."}
```

- nyq starts the command in the workspace (or the child's worktree), sends
  `initialize`, `session/new` and one `session/prompt`, and returns the
  agent's final message.
- The agent's permission requests go to your approver. Any "yes" is sent as
  *allow once*, never *always*. In yolo mode they are allowed, unless the task
  follows untrusted content (below). They are denied when nobody can be asked.
- External agents run **outside nyq's sandbox**, with their own tools and
  their own permission system. Starting one is a risky call: it asks for
  approval (or goes to the classifier in auto mode), and the prompt shows the
  command.
- nyq can't check an external agent's calls, so it labels what follows from
  them. The report carries `external:<name>` and the taint of the task; the
  files the agent changed, found by comparing `git status` and `HEAD` before
  and after the run, carry the same labels when read later, and so do the
  files a `merge` of its worktree applies. Changes outside the repository, in
  files git ignores or is told to overlook, or in a workspace that is not a
  git repository are not seen.
- A task that copies untrusted content (a web page, an MCP answer, another
  child's report) always asks a person: no grant, allow rule, yolo mode or
  classifier approves it, and in print mode or under the `ci` profile it is
  denied. It runs in its own worktree when one can be made, and every
  permission request the agent sends comes to you.
- A repository's `.nyq/settings.toml` cannot define external agents.
- External agents support `action: "run"` only. They take `output_schema`,
  `validators` and `isolation: "worktree"`.

## Team task graph

A shared graph of tasks that the parent and its children can create, claim
and complete. It is useful when several children split a larger job.

| Tool | Arguments | Effect |
|---|---|---|
| `team_task_create` | `goal, schema?, deps[], write_scope[]` | New open task (`t1`, `t2`, …), `rev: 1` |
| `team_task_list` | `state?` | Tasks with `ready`, `owner`, `rev` |
| `team_task_claim` | `id, rev` | Compare-and-set: you become the owner |
| `team_task_update` | `id, rev, state, result?` | Compare-and-set: `open`, `claimed`, `done` or `failed` |
| `send_message` | `to, text` | To a child (queued as its next task) or to `parent` |
| `wait_agent` | `ids[], timeout?` | Wait for children or tasks to finish, then return their results |

Each change bumps `rev`. A claim or update with a stale `rev` fails and reports
the current one, so two agents can never both own a task. A task is *ready*
when it is open and all its `deps` are done. A task with a `schema` can only
be marked `done` with a `result` that matches it.

`write_scope` is advisory. Claiming a task whose scope overlaps another claimed
task gives a warning, but writes are still serialized by the workspace writer
lock (or isolated in worktrees).

```text
parent:  team_task_create {"goal": "Port the parser to the new AST", "write_scope": ["src/parse/"]}   → t1, rev 1
parent:  team_task_create {"goal": "Update the parser tests", "deps": ["t1"], "write_scope": ["tests/parse/"]} → t2
parent:  subagent {"action": "spawn", "name": "porter", "mode": "general",
                   "task": "Claim t1 from the team graph, do it, then mark it done."}
porter:  team_task_claim {"id": "t1", "rev": 1}          → rev 2
porter:  team_task_update {"id": "t1", "rev": 2, "state": "done", "result": {"files": 4}}
porter:  send_message {"to": "parent", "text": "t1 done; t2 is ready"}
parent:  wait_agent {"ids": ["porter"], "timeout": 120}
```

Messages to `parent` arrive between the parent's steps, labelled as evidence.
A message to a child is queued as that child's next task.

## run_code

`run_code` runs a JavaScript program (the body of an async function) that
calls your tools with `await tools.<name>(args)`. Use it to batch many calls
without a model round trip for each:

```js
const hits = await tools.grep({ pattern: "process\\.env\\.[A-Z_]+", path: "src" });
const files = [...new Set(hits.split("\n").map(l => l.split(":")[0]).filter(Boolean))];
const sizes = await Promise.all(files.map(async f => {
  const text = await tools.read_file({ path: f });
  return { file: f, lines: text.split("\n").length };
}));
console.log(`${files.length} files`);
return sizes.sort((a, b) => b.lines - a.lines).slice(0, 10);
```

- **Every tool call goes through the normal pipeline**: PreToolUse hooks,
  permission rules, the auto-mode classifier, approval prompts, the sandbox,
  the writer lock and PostToolUse hooks. Each call is checked exactly as if the
  model had made it. A denied call rejects with a `ToolError`, which the
  script can catch, and the tool never runs.
- Calls issued together (for example in `Promise.all`) are dispatched as a
  batch. Read-only tools in the batch run in parallel.
- A tool resolves to its text result and rejects with a `ToolError` when it
  fails. `return` any JSON-serializable value. Strings come back as they are,
  and other values as pretty JSON.
- JavaScript itself has no file, network or process access: there is no
  `require`, `fetch`, `std` or `os`, and no module loader. `console.log`
  output is returned with the result.
- Limits: `timeout_secs` (default 120, max 900) covers the whole program,
  including tool calls. The heap is limited to 64 MB, the stack to 1 MB, and a
  program may make at most 200 tool calls. A runaway loop is interrupted.
- The result ends with a log of every sub-call: tool, arguments and outcome.
  Sub-call results go to the script and are not added to the transcript, so
  sessions keep the same format and load in older nyq versions.
- `run_code` and `workflow` cannot call each other. A child that has
  `run_code` can only reach that child's own tools.

## workflow

`workflow` runs a multi-stage JavaScript program over subagents. It has
everything `run_code` has, plus:

| Helper | Meaning |
|---|---|
| `await task(goal, {schema, validators, mode, isolation, name, description, model, effort})` | Run a subagent. With `schema` or `validators` it resolves to the checked `result` (`full: true` gives the whole JSON object). Otherwise it resolves to the report text. It rejects when the child fails. |
| `await parallel([() => task(...), ...])` | Run thunks at most 3 at a time; results come back in order |
| `await pipeline(items, (item, i) => ...)` | Map over items, at most 3 at a time; results come back in order |
| `phase(name)` | Mark progress. It is shown to the user and listed in the result. |

The default `timeout_secs` is 3600 and the limit is 100 tool calls.

A plan, fan-out and verify program, in the style of promptyped's multi-stage
programs:

```js
phase("plan");
const plan = await task("List the modules that still use the old logging API.", {
  schema: { type: "object", required: ["modules"],
            properties: { modules: { type: "array", items: { type: "string" } } } },
  validators: ["exists:/modules"],
});

phase("port");
const ported = await pipeline(plan.modules, (m) =>
  task(`Port ${m} to the new logging API. Keep behaviour identical.`, {
    mode: "general",
    isolation: "worktree",
    name: "port-" + m.replace(/[^A-Za-z0-9_-]/g, "-").slice(0, 30),
    schema: { type: "object", required: ["changed"], properties: { changed: { type: "boolean" } } },
    validators: ["build"],
  })
);

phase("review");
return { modules: plan.modules, ported };
```

Each isolated child leaves a worktree. Review the diff summaries, then
`subagent {"action": "merge", "name": "port-…"}` the ones you want.

## Limits and defaults

| | |
|---|---|
| Children running at once | 3 (sync runs, spawned work, external agents and workflow tasks together) |
| Child timeout | 15 minutes per turn |
| Typed retries | 1 (none for external agents) |
| `run_code` | 120 s default, 900 s max, 64 MB heap, 200 tool calls |
| `workflow` | 3600 s default, 4 h max, 100 tool calls, 3 tasks at a time |
| Team graph | 1000 tasks |

## Building without scripting

`run_code` and `workflow` embed QuickJS (`rquickjs`), which adds about 0.8 MB
to the release binary. They are behind the `scripting` cargo feature, which is
on by default. `cargo build --no-default-features --features tls-rustls`
builds nyq without them. All other orchestration features stay available.
