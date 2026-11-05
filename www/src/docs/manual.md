nyq is a small coding agent for your terminal. It reads your code, runs your
tests and edits files, using the ChatGPT or Grok plan you already pay for, an
Anthropic or OpenAI key, or a local model. Shell commands run in a sandbox, and you
choose how much it may do without asking.

## Install

```sh
curl -fsSL https://nyq.zalthar.com/install.sh | sh
```

The installer picks the build for your machine from the latest
[release](https://github.com/Znskr/nyq/releases), verifies it against the release's
`SHA256SUMS` and installs it to `~/.local/bin`. `NYQ_VERSION=0.0.1` pins a version and
`NYQ_INSTALL_DIR` picks another directory. Then run `nyq`, or `nyq login` to sign in
with a ChatGPT subscription (see [Pick a model](#pick-a-model)).

Prebuilt binaries cover Linux (static musl builds: no particular glibc needed) and
macOS on x86_64 and arm64, plus Windows on x86_64. On Windows, download the zip from
the releases page; shell commands use Git Bash when it is installed, otherwise
`cmd /C`, and there is no sandbox.

### Build from source

```sh
cargo build --release
./target/release/nyq --help
cargo test
```

The default build uses rustls on ring, so Linux needs no OpenSSL. A lean
distribution build uses the platform TLS stack instead (Security.framework on
macOS, OpenSSL on Linux) and is about 9% smaller (5.62 vs 6.19 MiB):

```sh
cargo build --profile lean --no-default-features --features tls-native
```

## Pick a model

**ChatGPT subscription (Codex backend).** Run `nyq login` to sign in through your
browser; nyq stores the login in `~/.nyq/chatgpt-auth.json` (mode 0600) and refreshes
it itself. The sign-in listens on `localhost:1455` (`1457` when that port is busy)
and always prints its URL; `NYQ_NO_OPEN_BROWSER=1` skips launching the browser
(for SSH or headless machines). If you use the codex CLI, nyq can also borrow its login
(`~/.codex/auth.json`, read-only) while that is valid. `NYQ_CODEX_AUTH`
points at a specific file. The default Codex model is `gpt-6-sol`; pick another
with `/model` (remembered) or `--model`.

**Grok subscription (SuperGrok / X Premium).** Run `nyq login grok` to sign in
through your browser. nyq prints the sign-in URL and opens it unless
`NYQ_NO_OPEN_BROWSER` is set; if the browser does not come back, paste the
authorization code and press Enter. The login is stored in
`~/.nyq/grok-auth.json` (mode 0600), refreshed automatically, and revoked on
`nyq logout grok`. Chat goes to `https://cli-chat-proxy.grok.com/v1/responses`.
Model names, reasoning efforts and context windows come from that proxy. This
is separate from an xAI API key (`XAI_API_KEY` or `nyq login xai`). The default
model is `grok-4`.

**Anthropic API, or any Anthropic-compatible endpoint.** Set `ANTHROPIC_API_KEY`
or run `nyq login anthropic` (alias `claude`), then `--provider anthropic` or
`nyq provider anthropic`. The key is checked against `GET /v1/models` (with
`x-api-key` and `anthropic-version`) before it is saved. Chat uses the Messages API:
streaming, tool use, image input, prompt caching (breakpoints on the system prompt,
the newest message and the previous user turn; cache reads and writes are counted
and priced), and thinking. Models that think adaptively (Claude 4.6 and later) get
`thinking: adaptive` with the effort sent as `output_config.effort`; older models and
other vendors' models get a thinking budget sized by the effort. The model list,
context windows, output caps, efforts and thinking style come from the live Models
API, with models.dev as the fallback (and for prices). The default model is
`claude-opus-5-5` (or `ANTHROPIC_MODEL`).

```sh
export ANTHROPIC_BASE_URL=https://api.deepseek.com/anthropic   # nyq appends /v1 when missing
export ANTHROPIC_AUTH_TOKEN=...   # sent as Authorization: Bearer instead of x-api-key
export ANTHROPIC_MODEL=deepseek-chat
nyq --provider anthropic
```

`ANTHROPIC_BASE_URL` (or `NYQ_ANTHROPIC_BASE_URL`, which wins) points the provider at
any Anthropic-compatible endpoint: DeepSeek, Kimi (Moonshot), MiniMax, z.ai, a
LiteLLM proxy, or a local gateway. Endpoints without a Models API fall back to the
models.dev catalog of the vendor (picked from the host) plus the configured model.
Key precedence: `ANTHROPIC_API_KEY`, then `ANTHROPIC_AUTH_TOKEN`, then the key from
`nyq login anthropic`. nyq uses API keys and compatible endpoints only: it does not
sign in with Claude Pro/Max subscriptions, because Anthropic's terms do not allow
third-party tools to use consumer subscriptions.

**Any OpenAI-compatible API** (OpenAI, OpenRouter, vLLM, Ollama, llama.cpp, …):

```sh
export NYQ_BASE_URL=https://api.openai.com/v1   # nyq appends /chat/completions
export NYQ_MODEL=gpt-5.5
export NYQ_API_KEY=sk-...                       # if the endpoint needs one
```

…or put `base_url`, `model`, `api_key` in `~/.nyq/model.json`.

**OpenCode Go / OpenCode Zen, xAI, Anthropic** (API keys):

| Provider | Key | Endpoint |
|---|---|---|
| `anthropic` (alias `claude`) | `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN` (bearer), or `nyq login anthropic` | `https://api.anthropic.com/v1` (Messages API); `ANTHROPIC_BASE_URL` / `NYQ_ANTHROPIC_BASE_URL` |
| `opencode-go` | `OPENCODE_API_KEY`, the OpenCode CLI's login, or `nyq login opencode-go` | `https://opencode.ai/zen/go/v1` |
| `opencode` (Zen) | `OPENCODE_API_KEY`, the OpenCode CLI's login, or `nyq login opencode` | `https://opencode.ai/zen/v1` |
| `xai` | `XAI_API_KEY` or `nyq login xai` | `https://api.x.ai/v1` (Responses API) |

`nyq login <provider>` asks for the key (hidden input; piping it in works too), checks
it with the provider (its models list), and
refuses a key the provider rejects (HTTP 401/403). If the check cannot be made
(offline), the key is saved with a warning. Keys are stored in
`~/.nyq/auth.json` (mode 0600); `nyq logout <provider>` removes one (`nyq logout` alone removes every sign-in nyq stored). The OpenCode CLI's login is read from `~/.local/share/opencode/auth.json` (also
`$XDG_DATA_HOME`, and `~/Library/Application Support/opencode` on macOS). OpenCode
serves each model over its own protocol: models.dev says which, so OpenAI models use
the Responses API, Claude-style models the Anthropic Messages API, and the rest chat
completions. Model names, context sizes, reasoning efforts and prices come from
models.dev, refreshed at most daily into `~/.nyq/models.json` (falling back to the
OpenCode CLI's cache). Every provider with a key gets its own tab in the model switcher.

`--provider codex|grok|anthropic|openai|ollama|opencode-go|opencode|xai` and `--model NAME`
override the choice. `NYQ_HOME` relocates `~/.nyq`.

A request that fails before anything was streamed (HTTP 429, 500, 502, 503, 504,
Anthropic's 529 overload, the same errors reported inside the stream, or a
connection error or timeout) is retried up to three times with backoff,
honoring the server's `Retry-After` up to 8 seconds; the status line shows
`Retrying in Ns`. A `Retry-After` over a minute (a spent quota) is reported at
once. Errors with a JSON body read like
`API access denied · HTTP 403 · Provider: X · code: message`.

### Add a compatible provider (no config editing)

Run `nyq login custom`, or use `/login` → **Add compatible provider** (also in
`/setup` → **Connections** and `nyq setup`). The guided flow asks for:

1. **OpenAI-compatible** (Chat Completions) or **Anthropic-compatible** (Messages).
2. A unique provider id, such as `work-gateway`, and its base URL including the API
   prefix (for example `https://gateway.example/v1`). Do not put credentials in the URL.
3. An API key in a hidden prompt, or the explicit **No key** option for a local or
   unauthenticated endpoint. Never put keys in command arguments or chat messages.
4. An initial model from the discovered list. If discovery is unsupported or offline,
   enter the model id manually; HTTP 401/403 refuses the save and keeps any old key.

The endpoint, protocol, model and isolated key are saved in `~/.nyq/providers.json`
(mode 0600). Existing ids require an explicit **Replace provider** choice. Models
appear in `/model`, including the saved model when discovery is unavailable.
Use `nyq provider work-gateway` to make it the default, choose it in `/setup` →
**Model provider**, or pass `--provider work-gateway` for one run.

`nyq login work-gateway` (or `/login work-gateway`) safely replaces only its key;
`nyq logout work-gateway` removes only the key, keeping the endpoint and model.
Remove the entire configuration through `/setup` → **Connections** → the provider
→ **Remove provider configuration**, or **Remove a compatible provider** in
`nyq setup`; removal requires confirmation. Custom endpoints never borrow a
built-in provider's environment key.

### Accounts

| Command | |
|---|---|
| `/login [provider]` | ChatGPT or Grok browser sign-in (paste the code if the browser does not return), or an API key typed into a masked prompt (never echoed or kept in history); without a provider, a menu |
| `/logout [provider]` | forget nyq's stored sign-in (a menu of the stored ones without a provider) |
| `/setup` | connections (sign in, add compatible provider, replace a key, sign out or remove a custom endpoint), model provider, model, credential source |
| `nyq provider [NAME]` | set the default provider (`settings.toml` `provider` plus `last-model.json`, so the next start uses it); aliases `chatgpt`, `claude`, `zen`, `go` work. `grok` is the Grok subscription, not the xAI API key |
| `nyq setup` | connections and the default; on a terminal, add/remove compatible providers, sign in/out or choose the default |
| `nyq permissions [--json]` | the permission mode and the allow/ask/deny rules |

### Local models (Ollama)

```sh
export NYQ_BASE_URL=http://127.0.0.1:11434/v1 NYQ_MODEL=qwen3:4b NYQ_CONTEXT_WINDOW=32768
nyq --provider openai
```

Start Ollama with `OLLAMA_CONTEXT_LENGTH=32768` (its 4k default is too small for an agent).

## Use it

```sh
nyq                         # interactive
nyq "fix the failing test"  # interactive, starting with a message
nyq -p "summarize src/"     # print the answer and exit (stdin works too)
nyq ask "summarize src/"    # same as -p; reads stdin when the prompt is omitted
nyq ask --json "fix it"     # one JSON object: answer, tool calls, exit code, session
nyq ask -r 3f2a "and now?"  # -r/-c work after `ask` too
nyq -c                      # continue the last conversation in this directory
nyq -r 3f2a                 # resume a conversation by id prefix
nyq resume 3f2a             # same as -r
nyq sessions                # list this directory's conversations (--all, --limit N, --json)
nyq session 3f2a            # print a transcript summary (same as `session show`; --json)
nyq session resume 3f2a     # same as -r
nyq session delete 3f2a     # delete a saved conversation
nyq workspace               # list additional workspace directories
nyq workspace add ../other  # remember another directory for this folder
nyq --add-dir ../other      # add a directory for this run only
nyq --no-additional-dirs    # ignore directories saved for this folder
nyq login                   # sign in with your ChatGPT subscription
nyq login grok              # sign in with a Grok subscription (SuperGrok / X Premium)
nyq login anthropic         # store an API key (anthropic, opencode-go, opencode, xai)
```

Flags that work with every command:

| Flag | |
|---|---|
| `-m, --model ID` / `--provider NAME` | model and provider for this run |
| `--yolo`, `--permission-mode ask\|auto\|yolo` | how tool calls are approved (see [permissions](#permissions)) |
| `--sandbox MODE`, `--allow-network` | confine shell commands (see [sandbox](#sandbox)) |
| `--add-dir PATH`, `--no-additional-dirs` | extra workspace directories for this run |
| `-c, --continue`, `-r, --resume [ID]` | continue or resume a conversation |
| `--context-limit NAME=BYTES\|off` | override a prompt budget (see [limits](#environment-and-limits)) |
| `--debug` | developer-only features for this run (also `NYQ_DEBUG=1`); for now the [`/tools` menu](#tools-and-approvals) |

Two looks are built in. The default, `ui_style = "classic"`, is the plain one: a
one-line banner, `┃` bars before the input and your messages, `●` before tool calls
and a single gray status line. `ui_style = "fancy"` (also in `/settings`, applied
at once) draws the look described next.

With `ui_style = "fancy"` the interactive screen opens with a small banner (the logo, model · provider ·
folder, and a tip; one line under 60 columns) and keeps the conversation in normal
scrollback, above a rounded input box (`›`, with a placeholder while empty) that
stays pinned to the bottom, with a status line (`effort · model`, session title,
provider, a context gauge, tokens, sandbox and permission mode (green ask, blue
auto, red yolo), folder, branch; `/statusline` picks which, and segments drop out
when the terminal is narrow). Tool calls are grouped per step
(`✓ 3 tool calls · 2 read · 1 command` with `├`/`└` rows); each call shows a
spinner while it runs, then `✓` or `✗` and how long it took. Edits and writes
show their diff with line numbers under the row (folded after 12 lines), and a
running command shows its last few output lines live. Reasoning collapses to one
`• Thought for Ns` line. The working line counts the turn's time (paused while nyq
waits for you), tokens and tool calls, with the interrupt key on the right. Menus
show `↑↓ Navigate · Enter Use · Esc Close`. `NO_COLOR` turns colors off, and a
locale that isn't UTF-8 (`LC_ALL`, `LC_CTYPE` or `LANG`) switches to ASCII glyphs. Answers
are rendered as markdown: code blocks are syntax highlighted, links and URLs are
clickable (OSC 8 hyperlinks, as are file paths in tool rows), and quotes, lists and
long lines wrap with a hanging indent. After a resize the screen is repainted at
the new width. The window title shows the session title and whether nyq is working.
On exit nyq prints `Resume with: nyq -r <id>`.

Colors follow the terminal's background (asked with OSC 11, else `COLORFGBG`):
set `NYQ_THEME=light` or `dark`, or the **Theme** setting, to choose. Terminals
that don't announce truecolor (`COLORTERM`) get the 256-color palette.

The input always accepts text, even while nyq is working:

| Key | Action |
|---|---|
| `Enter` | send; while working, queue as **steering** (delivered after the current step) |
| `Alt+Enter` | while working, queue a **follow-up** (runs when the current task is done) |
| `Alt+↑` | pull the newest queued message back into the input to edit; press again for the next |
| `Esc` | interrupt (queued messages return to the input); when idle, `Esc` twice clears the input |
| `Ctrl+L` or `/model` | **model switcher**: every model from your Codex login, Grok subscription (when signed in), local Ollama, the configured OpenAI-compatible endpoint and each hosted provider with a key; `Tab` switches provider tab, type to filter (words, or letters in order: `gpt55`) |
| `/model ID\|QUERY [effort] [fast\|normal]` | switch directly: an exact id (`gpt-5.5`, `opencode/claude-sonnet-5`) or the only model matching a search; several matches open the switcher filtered by it. The effort and fast mode are checked against what the model offers (`/model gpt-5.5 high fast`). An id no provider lists is set on the current provider |
| `/effort` or `/thinking` | open the reasoning-effort picker; without a terminal, show the current level, supported values and direct usage |
| `/effort LEVEL` or `/thinking LEVEL` | set and remember a supported reasoning effort (case-insensitive), or `auto` for the model default; unsupported values leave the current level unchanged |
| `Shift+Tab` | cycle and remember reasoning effort (auto → each level the current model supports → auto) |
| `Ctrl+O` or `/transcript` | **full transcript**: the whole conversation with expanded tool output, full screen; `↑↓`/`PgUp`/`PgDn`/`Home`/`End` or the mouse wheel scroll, `←`/`→` (or `Tab`) switch between the compact and full view, `/` or `Ctrl+F` searches, `n`/`N` jump between matches, `q`/`Esc` closes |
| `Ctrl+V` or `/paste` | attach an image from the clipboard (wl-paste, xclip, pngpaste or PowerShell) |
| `Shift+Enter` / `Ctrl+J` / trailing `\` | newline |
| `↑` / `↓`, `Ctrl+P` / `Ctrl+N` | move by screen row (keeping the column) in a long input; history on the first/last row, this workspace's prompts first |
| `Ctrl+R` | search history backwards (type to refine, `Ctrl+R` again for older, `Esc` or `Ctrl+G` restores the draft) |
| `/` | command menu: exact, then prefix, then substring matches (`/ode` finds `/model`) and aliases, scrolling over every command; after a command and a space it offers its subcommands (`/permissions `, `/mcp `, `/allowlist local `, …). `↑↓` + `Enter` runs, `Tab` completes; skills follow the commands |
| `@` | **file picker**: fuzzy search over workspace files (ignore files apply); `Tab` or `Enter` inserts `@path`, `Esc` closes |
| `$` | skill picker: inserts `$skill-name`, which sends that skill's `SKILL.md` with your message |
| `/help` | every command by category (General, Session, Account, Model, Appearance, Security, Workspace, Media, Agents, Extensions, Product): `Tab`/`Shift+Tab` picks a category, type to filter, `Enter` runs |
| `Ctrl+G` | when the status line says `update ready`, reload into the installed upgrade on the same conversation (see [upgrades](#upgrades)) |
| `Ctrl+C` | copy the selection; otherwise clear the input; on an empty input, twice to quit |
| `Ctrl+Z` | suspend to the shell (`fg` resumes; Unix) |
| `Ctrl+D` / `/exit` | quit |
| `/keys` | list the [key bindings](#key-bindings) (change them in `~/.nyq/keybindings.toml`) |

Editing keys in the input:

| Key | Action |
|---|---|
| `Ctrl+A` / `Ctrl+E`, `Home` / `End` | start / end of the line |
| `Ctrl+Home` / `Ctrl+End`, `Alt+<` / `Alt+>` | start / end of the whole input |
| `Ctrl+B` / `Ctrl+F`, `Alt+B` / `Alt+F`, `Ctrl+←` / `Ctrl+→` | character / word left and right |
| `Ctrl+W`, `Alt+Backspace` / `Alt+D`, `Ctrl+Delete` | delete the word before / after the cursor |
| `Ctrl+U` / `Ctrl+K` | delete to the start / end of the line (at the end, `Ctrl+K` joins the next line) |
| `Ctrl+Y` | paste back the last text deleted with the keys above (consecutive deletes join) |
| `Ctrl+_` | undo; `Alt+_` or `Ctrl+Shift+Z` redo |
| `Shift+←→↑↓`, `Shift+Home/End` | select; typing or `Backspace` replaces it, `Ctrl+C` copies, `Ctrl+X` cuts (system clipboard and OSC 52) |

Pastes longer than 1000 characters show as `[Pasted text #N, M lines]` and are
sent in full; the placeholder moves and deletes as one unit.

Switching models keeps the conversation, even across providers, and the last
choice is remembered (`~/.nyq/last-model.json`) unless flags or `NYQ_*`
variables say otherwise. `!cmd` runs a shell command yourself. A line that starts
with `/` but is not a command goes to the model as a prompt when it has more words or
another `/` in its first word (`/tmp/x.log what is this?`, `/usr/bin/foo`); a lone
unknown `/word` is reported as an unknown command. With piped input or `-p`, nyq
prints plain text instead. Menus (`/skills`, `/tools`, `/settings`, `/model`, `/effort`, `/resume`,
`/help`, `/permissions`) open only when stdin and stdout are terminals; otherwise
these commands print a list or name the subcommand that does the same.

### Print mode

`nyq ask` / `nyq -p` answer one prompt: the answer goes to stdout, tool activity and
notices to stderr. Options (after `ask`, or with `-p`):

| Option | |
|---|---|
| `--json` | print one JSON object instead of streaming: `output` (all assistant text), `final_output` (the text after the last tool call), `exit_code`, `model`, `session_id`, `steps` (model requests), `tool_calls` (`name`, `summary`, `status` ok/error/denied, `output`), `notices` (e.g. reaching the step limit), and `error` when it failed — also for failures before the model runs, such as missing credentials |
| `--quiet` | don't print the answer (with `--json`, only the JSON) |
| `--system TEXT` | replace the built-in base system prompt for this run, ignoring `system.md`/`append-system.md` ([system prompt](#system-prompt)); tools, skills, project instructions and environment details still follow |
| `--no-save` | don't create or save a session (not with `-r`/`-c`) |
| `--prompt-permissions` | when stdin is a terminal, ask `y/N/a` on stderr for each permission request instead of denying it |
| `--image PATH` | attach an image (repeatable), like `@path` in the prompt |
| `--continue-recovery` | with `-r ID` or `-c`: continue that session's interrupted or cut-off answer |

The exit status is 0 on success, 1 on errors, 130 after Ctrl-C and 143 after
SIGTERM, including while waiting for piped stdin to finish; background processes
and MCP servers are stopped either way.

## Settings

`/settings` opens a menu like the model switcher (`↑↓` navigate, `Tab` category,
`Enter`/`←→` change, type to filter, `Esc` close). Changes are saved to
`~/.nyq/settings.toml` (only the changed keys; other keys, comments and
formatting in that file are kept). You can also edit it by hand:

```toml
sound = "on"
auto_compact = 70

[statusline]
tokens = false
```

Two layers can sit on top of that file, later ones winning:

- the project's `.nyq/settings.toml` (effort, fast mode, auto-compact, sound,
  notifications, status line, prompt history, slash menu categories, disabled tools and
  theme colors only; permission mode, sandbox and providers are ignored there, with a warning);
- your own override for one folder, under `workspaces` in `~/.nyq/settings.toml`:

  ```toml
  [workspaces."~/src/app"]
  sound = "off"
  ```

`/settings` marks values that come from a layer (`· project`, `· workspace`), and
a change that a layer overrides is saved with a notice saying which layer wins.

`disabled_tools` lists tools that are never offered to the model or run, and
`disabled_skills` skills (by the id `/skills` shows) that are neither offered to the
model nor completed after `$` or `/` (the `/skills` menu still lists them, dimmed).
Neither is in the settings menu: `/skills` (`Ctrl+T`) and `/tools` (with `--debug`)
switch them, writing only your own list, or edit the file:

```toml
# ~/.nyq/settings.toml
disabled_tools = ["web_search", "web_fetch", "mcp__github__create_issue"]
disabled_skills = ["pdf", "review@.claude/skills"]
```

Tool names are built-in tools (`/tools list` shows them), MCP tools as `mcp__server__tool`,
or extension tools (`ext__…`). A project's lists add to yours rather than
replacing them, so a repository can't re-enable a tool or skill you turned off. Unknown
built-in names are reported at startup and by `nyq doctor`, then ignored; MCP and
extension names aren't checked. Subagents get the same restriction. `nyq status`
shows both lists.

| Setting | Values | |
|---|---|---|
| Model, reasoning effort | | the effort is also the default for new sessions |
| Fast mode (`/fast`) | on/off | Codex priority processing (`service_tier: "priority"`) for models whose listing offers it; shown as `fast` in the status line |
| Permission mode | ask/auto/yolo | ask prompts, auto lets a classifier decide, yolo never asks. Also `--permission-mode` and `/permissions`. Shown on the status line. A saved `yolo` applies only to interactive sessions; print/piped runs and ACP need an explicit flag or `NYQ_PERMISSION_MODE=yolo` |
| Classifier model (`classifier_model`) | auto, or a listed model | the model that reviews calls in auto mode; `auto` uses a Codex mini model on Codex, else the session model. Any id can be set in `settings.toml` |
| Default sandbox | workspace-write/read-only/off | used when `--sandbox` isn't given, from the next start |
| Auto-compact at | off, 50–90% | share of the context window that triggers compaction |
| Sound level (`/sound [on\|off\|max]`) | off/on/max | a system chime (macOS sounds, or the freedesktop theme via `paplay` in a local desktop session; otherwise the terminal bell) when a long turn ends or nyq waits for an approval or answer, which also always rings the bell; `max` sounds after every turn |
| Desktop notifications (`/notifications [on\|off\|after N]`) | on/off | `notify-send`/`osascript` in a local desktop session, otherwise the terminal's OSC 9 / OSC 777 notification (passed through tmux) |
| Long turn after | seconds | threshold for the bell and notifications |
| Status line segments (`/statusline [segment…]`) | on/off | effort · model, session title, provider, context, tokens, sandbox and permission mode, folder, branch |
| Theme (`theme`) | auto/dark/light | colors for dark or light terminals; `auto` follows the background. `NYQ_THEME` overrides it |
| Theme colors (`theme_colors`, file only) | role = `"#rrggbb"` or 0-255 | replace single role colors (below); a project may set these too |
| UI style (`ui_style`) | classic/fancy | `classic` (default): plain bars and bullets; `fancy`: logo banner, rounded input box, colored status line with a context gauge, ✓/✗ tool marks and times |
| Collapse tool calls (`collapse_tool_calls`) | on/off | show finished tool groups as their summary line only (`Ctrl+O` shows everything) |
| Start at top of screen (`startup_scrollback`) | on/off | at start, move the shell output on screen into the scrollback |
| Prompt history | on/off | save each prompt to `~/.nyq/history` as it is sent |
| Auto-upgrade (`auto_upgrade`) | on/off | install new releases in the background, then `Ctrl+G` reloads (default on; `NYQ_AUTO_UPGRADE=0` also turns it off). Turning it off stops the background check at once |
| Slash menu categories (`slash_menu_categories`) | on/off | show categories and skill sources in slash-command results (default on) |

`theme_colors` overrides the UI's role colors. Top-level entries apply to both
themes; a `dark` or `light` table wins for that theme. Values are `"#rrggbb"` or
an xterm color index (0-255), and terminals without truecolor get the nearest
256-color match. Unknown roles and bad values are skipped with a warning at start
and in `nyq doctor`. In `~/.nyq/settings.toml`:

```toml
[theme_colors]
blue = "#5fafd7"
[theme_colors.light]
blue = "#005f87"
```

| Role | Used for |
|---|---|
| `bright` | the logo, headings, tool names, your messages, selected menu items, keys in hints |
| `mid` | body text, code without a highlight, menu items |
| `gray` | secondary text: summaries, tool output, thinking, info lines, comments in code, the input box border |
| `faint` | rules, borders, gutters and line numbers, placeholders, "(no output)" |
| `red` | errors and failed tools, removed diff lines, `yolo`, a nearly full context |
| `yellow` | warnings, hints, notes, approvals, the current search match, no sandbox |
| `green` | strings in code, finished tools, added diff lines, `ask`, the context gauge |
| `blue` | the accent: the prompt mark and spinner; links, functions and types in code, `auto` |
| `magenta` | keywords in code, the effort, the branch mark |
| `orange` | numbers in code |

## Key bindings

The global and input keys above can be changed in `~/.nyq/keybindings.toml`
(the profile directory, `$NYQ_HOME` if set). Each line binds an action to a key
or a list of keys; the list replaces that action's defaults, and an empty list
unbinds it:

```toml
expand_tools = "ctrl+t"
history_search = ["ctrl+r", "alt+r"]
newline = "shift+enter"
paste_image = []
```

Keys are written like `ctrl+x`, `alt+enter`, `shift+tab`, `ctrl+shift+z`,
`super+left`, `f5`, `esc` (modifiers `ctrl`, `alt`, `shift`, `super`; keys
`enter`, `tab`, `esc`, `backspace`, `delete`, `insert`, `home`, `end`, `pageup`,
`pagedown`, `up`, `down`, `left`, `right`, `space`, `plus`, `f1`–`f24`, or one
character). A plain character can't be bound, since it types text. A key you bind
leaves any action that only had it by default (silently; `/keys` then lists it
under your action only). Unknown actions, unreadable keys
and the same key bound to two actions are reported at startup and by `nyq doctor`
(the rest of the file still applies; of two actions sharing a key, the earlier one
in the table below wins). `/keys` lists every action with its current keys, and
help text and hints such as `ctrl+o to view` show your keys. Changes apply on the
next start.

| Action | Default | |
|---|---|---|
| `reload` | `ctrl+g` | reload into an installed upgrade |
| `expand_tools` | `ctrl+o` | full transcript with every tool call (and the review of a pending approval); the same key closes it |
| `paste_image` | `ctrl+v` | attach an image from the clipboard |
| `model_menu` | `ctrl+l` | model switcher |
| `cycle_effort` | `shift+tab` | next reasoning effort |
| `suspend` | `ctrl+z` | suspend to the shell (Unix) |
| `submit` | `enter` | send (steers while working); after a trailing `\` it inserts a newline |
| `newline` | `shift+enter`, `ctrl+j` | newline |
| `follow_up` | `alt+enter` | while working, queue a follow-up; otherwise newline |
| `interrupt` | `esc` | interrupt; twice on an idle input clears it |
| `clear` | `ctrl+c` | clear the input; on an empty input interrupt, or twice to quit |
| `exit` | `ctrl+d` | quit on an empty input; otherwise delete the next character |
| `dequeue` | `alt+up` | pull the newest queued message back into the input |
| `history_search` | `ctrl+r` | reverse history search (again for older matches) |
| `history_prev`, `history_next` | `up`, `ctrl+p` / `down`, `ctrl+n` | move by row; history on the first/last row |
| `complete` | `tab` | complete a command |
| `undo` | `ctrl+_`, `ctrl+7`, `ctrl+-`, `super+z` | undo |
| `redo` | `alt+_`, `ctrl+shift+z`, `super+shift+z` | redo |
| `char_left`, `char_right` | `left`, `ctrl+b` / `right`, `ctrl+f` | character left / right |
| `word_left`, `word_right` | `ctrl+left`, `alt+left`, `alt+b` / `ctrl+right`, `alt+right`, `alt+f` | word left / right |
| `line_start`, `line_end` | `home`, `ctrl+a`, `super+left` / `end`, `ctrl+e`, `super+right` | start / end of the line |
| `input_start`, `input_end` | `ctrl+home`, `super+up`, `super+home`, `alt+<` / `ctrl+end`, `super+down`, `super+end`, `alt+>` | start / end of the input |
| `delete_word` | `alt+backspace`, `ctrl+w` | delete the word before the cursor |
| `delete_word_forward` | `alt+d`, `ctrl+delete`, `alt+delete` | delete the word after the cursor |
| `delete_to_line_start` | `ctrl+u`, `super+backspace` | delete to the start of the line |
| `delete_to_line_end` | `ctrl+k` | delete to the end of the line |
| `yank` | `ctrl+y` | paste back the last deleted text |

Bindings match their modifiers exactly (`ctrl+o` is not `ctrl+alt+o`). The
defaults also include the extra-modifier variants the keys have always accepted
(`ctrl+enter` sends, `alt+home` goes to the line start, `ctrl+shift+o` opens the
transcript); `/keys` counts them as `+N with extra modifiers`. Setting an action
in the file replaces its variants too, and a default key you move to another
action takes its variants with it. `Shift` with an arrow, `Home` or `End` still
selects while moving like the bound key.
Some keys stay fixed: approval prompts (`y`/`a`/`s`/`l`/`n`, digits, `Enter`,
`Tab` for a note, `v` to review, `Esc`/`Ctrl+C`/`Ctrl+D` to deny), questions and
secret prompts, everything inside menus and pickers (`↑↓`, `Enter`, `Tab`, `Esc`
in `/`, `@`, `/model`, `/settings`, `/help`, the transcript viewer), `Esc`/`Ctrl+G`/`Ctrl+C`
during a history search, `Backspace`/`Delete`, and with a selection `Ctrl+C`/`Ctrl+X`
(copy/cut) and `Super+A` (select all). Modified keys never answer an approval,
whatever they are bound to.

## System prompt

`~/.nyq/system.md` replaces nyq's built-in base prompt, and
`~/.nyq/append-system.md` is added after it. A project's `.nyq/system.md` and
`.nyq/append-system.md` work the same way: the project's `system.md` wins over
yours, and both append files are used (yours first). Blank files are ignored, and
each file is capped by `project_instruction_file_bytes` (see
[limits](#environment-and-limits)). Sandbox guidance, project instructions
(`AGENTS.md`, ...), skills, tools and environment details are still added.
`ask --system TEXT` replaces the base prompt for one run and skips all four files.
`nyq status` lists the files in use (`system_prompt_files`).

## Prompt templates

Markdown files in `~/.nyq/prompts/` and the project's `.nyq/prompts/` are prompt
templates: `NAME.md` becomes `/NAME` in the command menu, and `/NAME args` sends the
file's text with `$ARGUMENTS` (everything after the name) and `$1`…`$9` (quoted
words count as one) filled in; `$$` is a literal `$`. Without placeholders the
arguments are appended. An optional front matter `description:` shows in the menu.
A project template overrides a profile one with the same name; built-in commands
win. `/alias` lists them, `/alias NAME` shows one.

## Tools and approvals

Built-in tools: `read_file`, `list_dir`, `glob`, `grep` (gitignore-aware; `grep` can
count matches per file or show context lines, and both page with `offset`/`limit`),
`write_file`, `edit_file`, `bash`, `view_image`, `web_fetch` (public URLs only, connected
directly with no HTTP proxy; private and local addresses are blocked; marked as untrusted
content and reused for 15 minutes within a session; PDFs, images and other non-text
responses up to 50 MB are saved under `~/.nyq/artifacts/<session>/` and the model gets
the path, plus the image itself for PNG/JPEG/GIF/WebP), `web_search` (Codex's native search,
DuckDuckGo otherwise, with optional allowed/blocked domains), `subagent` (delegates a
self-contained task to a fresh agent: read-only `explore`, or `general` after approval),
`skill`, `install_skill`, and `ask_user_question` (up to four multiple-choice questions,
each option with a short description, shown as menus). `/undo` reverts nyq's file
changes one at a time; `/copy` copies the last answer.

**Tools menu** (with `--debug` or `NYQ_DEBUG=1`; otherwise `/tools` only says so and
stays out of `/`, `/help` and completion). `/tools` lists every tool in tabs: built-in,
each MCP server, and extensions, with a one-line description and badges: `off`
(`disabled_tools`) and how a call is approved, worked out by the same rules as a real
call: `auto` (runs without asking, e.g. reads in the workspace), `always allowed` (an
allow rule), `allowed (session)`, `asks` (or `asks (rule)`), `classifier` (auto mode),
`yolo`, or `denied` (a deny rule). Type to filter (badges too: `off`, `denied`),
`Tab` switches group, `Enter` or `→` shows the full description and the parameters
(name, type, required, description). `Ctrl+T` turns a tool off or on for good
(your settings.toml, applied at once); `Ctrl+A` always allows a tool after you press
`y` (`/allowlist user add tool NAME`), or takes back an exact entry; deny and ask rules
can't be overridden from here. Without a terminal, `/tools` prints the same list as
text; `/tools list|info NAME|disable NAME|enable NAME|trust NAME|untrust NAME` do the
rest.

Consecutive read-only calls in one reply (reads, searches, web fetches, skills) and
subagent calls run concurrently; results keep their order. `bash` keeps the beginning
and end of long output and stores the whole of it for `read_tool_result`; `grep`/`rg`
finding nothing, `diff`/`cmp` finding differences and a false `test` (exit code 1) are
not reported as failures. API keys, GitHub/GitLab/Slack/AWS tokens, bearer tokens,
`*_TOKEN=`/`*_SECRET=`-style assignments and private key blocks are replaced with
`[redacted]` in every tool result before the model or the session file sees it. When
the same call fails three times, the model is told to change approach. Each request also
ends with a short note telling the model the permission mode, whether it can ask questions, and which background
processes are running (with any server URL they printed). Pressing Esc while a reply
streams keeps the partial text, marked `[interrupted by the user]`, and a turn that
reaches the step limit (100, or `max_agent_steps` / `NYQ_MAX_AGENT_STEPS`) stops with a
notice instead of an error. If a file changes
while its edit or write waits for approval, the call is refused rather than applying a
diff you didn't see.

**Memory.** When you ask nyq to remember something, the `memory` tool (`add`, `list`,
`remove`) saves it to `~/.nyq/memory.json`; saved facts are listed in the system
prompt of later sessions (capped at 50 facts / 4000 characters). `/memory` lists
them and `/memory remove N` forgets one.

**Background processes.** `bash` with `run_in_background` starts a server, watcher
or other long-running command (sandboxed like any other command) and returns an id at
once; `bash_output` returns what it printed since the last read plus its status or
exit code, `process_wait` blocks until new output matches a regex, a localhost port
accepts connections, or the process exits (with a timeout), and `kill_shell` stops it.
Started with `interactive`, a process keeps its stdin open and `process_write` (which
asks first, like `bash`) sends it input, e.g. REPL lines or answers to prompts (pipes,
not a terminal). A dev-server URL a process prints (`http://localhost:5173/`) is shown
in its status. Each process keeps the last 1 MiB of output in memory, and all of them
are killed when nyq exits. `/background` lists them (id, status, runtime, command and
any local server URL they printed); `/background logs ID|last` shows the first and last
20 lines of output, `/background open ID|last` opens the `http://localhost:…` URL a
server printed (with a warning when the process has stopped), and
`/background kill ID|last` stops one.

**Child orchestration.** The model can call `subagent` with `action: "spawn"`,
`name` and `task` to schedule independent work, and `action: "send"` to queue a
follow-up for that name. Up to three children execute concurrently; each child
processes its inbox in order. Optional `request_id` makes exact retries return
the existing task. Children cannot create or message other children.

The parent can keep working. Completed reports enter its context between model
requests; before finishing it waits for outstanding child work, without model
status polling. `/agents` lists task IDs, queue states and pending results;
`/agents show NAME` inspects the child's saved transcript,
`/agents send NAME MESSAGE` queues a follow-up for the next parent turn,
`/agents resume NAME` reopens a stopped child, and `/agents stop NAME`
cancels its active and queued work. Existing synchronous `run` and named-agent
follow-ups remain supported.

The session's `orchestration.jsonl` journal records accepted tasks and results
with synchronized writes and an exclusive process lock. It requires a saved
session (`--no-save` cannot use asynchronous children). Children stop with nyq;
queued tasks resume on the next parent turn after reopening the session, unless
they follow an interrupted run of the same child (those are held for inspection). Work
that was running at a crash is marked interrupted and reported, never blindly
replayed. Inspect its transcript and workspace before sending a replacement task.
A truncated last journal record is preserved separately before recovery; earlier
corruption is reported rather than discarded. Journals are bounded to 1000 tasks,
16 MiB per transition and 128 MiB total; start a fresh session at those limits.

Workspace mutations by the parent and children share a writer lock; read-only
children continue concurrently. This does not lock external editors or provide
a snapshot of files. Background shell starts are unavailable once orchestration
is active, because returning early would release the writer lock while a command
could still change files. Use bounded foreground commands. Child results are
model-generated evidence and never grant permissions.

**Large tool results.** Output over 16 KiB (other than `skill`, which comes in parts) is stored in full under
`~/.nyq/artifacts/<session>/<id>.txt`; the model sees its head and tail plus the id,
and pages through the rest (or searches it) with `read_tool_result`.

Skills follow the `SKILL.md` convention and are found in `.agents/skills`,
`.codex/skills`, `.claude/skills`, `.nyq/skills`, `.opencode/skills`, `skills/`,
`.claw/skills` (project) and `~/.nyq/skills`, `~/.agents/skills`, `~/.codex/skills`,
`~/.claude/skills`, `~/.config/opencode/skills`, `~/.claw/skills`. Skills with the
same name in different directories are all kept: the first keeps the name, later ones
are called `name@location` (e.g. `review@.claude/skills`).

`/skills` opens the skills menu: every skill grouped by where it was found, with
tabs that count the matches in each source (nyq, Agents, Claude Code, Codex,
OpenCode, Claw, Workspace), and a preview of the selected skill under the list
(description, source, scope, folder, how many files and its path). Type to filter
(matches are highlighted), `Tab`/`Shift+Tab` picks a source, and:

| Key | |
|---|---|
| `Enter` | run the skill now, on its own (sends `/skill-name`, or `$skill-name` when a command has that name) |
| `Ctrl+E` | put `/skill-name ` in the input so you can add your request |
| `→` | the whole `SKILL.md` with its path and files, full screen |
| `Ctrl+T` | turn the skill off or back on (`disabled_skills`); turned-off skills stay in the menu, dimmed and marked `off` |
| `Ctrl+O` | copy a skill from another tool's folder (Claude Code, Codex, OpenCode, Agents, Claw, `skills/`) into nyq's: `u` for `~/.nyq/skills`, `p` for the project's `.nyq/skills`; an existing copy is only replaced after `y` |
| `Ctrl+N` | install: type a git URL, `owner/repo`, a skills.sh page or a folder (add `--project` for this project) and press `Enter`; progress and errors show in the menu, which then refreshes |
| `Delete` / `Ctrl+X` | remove a skill nyq manages, after `Enter` or `y` |
| `Esc` | close (or leave the install line) |

With no skills at all, the menu says where nyq looks and how to install one. Every
action has a subcommand, which is also what you get where no menu can open (piped
input, editors, CI): `/skills` and `/skills list` print the skills grouped by source
with `[off]` marks, along with problems in malformed `SKILL.md` files (unreadable,
unterminated frontmatter, no description); `/skills enable|disable NAME` and
`/skills copy NAME [--project] [--force]` do what `Ctrl+T` and `Ctrl+O` do.
`/skills install SOURCE [--project] [--skill NAME]` installs from a local directory, a
git URL, GitHub `owner/repo` (`owner/repo@skill` picks one; shallow `git clone`) or a
skills.sh page (`https://skills.sh/owner/repo/skill` means `owner/repo@skill`)
into `~/.nyq/skills`, or the project's `.nyq/skills` with `--project`: every
directory with a valid `SKILL.md` is copied (without `.git`), replacing an older
install of the same name. A pasted `npx skills add owner/repo --skill NAME -g -y`
line (or `bunx`) works too: `-g` installs for you, otherwise into the project.
`/skills remove NAME` deletes one from those two
directories (skills elsewhere are left alone), `/skills info NAME` shows its path,
description and files, `/skills create NAME` writes a template, `/skills path` shows
the directories. The model can install skills too with `install_skill`, which always
asks first (`/allowlist add tool install_skill` trusts it). You can also invoke a
skill yourself: pick it from the menu, the `/` menu or the `$` picker, start a message
with `/skill-name` or include `$skill-name`, or write "use the skill-name skill"; its
`SKILL.md` instructions are sent with that message.

The model loads skills with the `skill` tool. Paths in a skill's instructions are
relative to its directory: `skill` with a `resource` (e.g. `references/forms.md`)
returns that text file of the skill completely; paths that leave the skill directory,
hidden files, links pointing out of it and binary files are refused. Long content comes
in parts of `skill_chunk_bytes` (32 KiB), each ending with a
`<context_limit name="skill_chunk_bytes" ... next_offset="N" />` marker; the model
continues with `offset`. Skill text is untrusted: control characters, zero-width,
bidi-override and tag characters, which can hide instructions from a reader, are
removed from instructions, resources and descriptions, and the model is told
("[context] Skill content was sanitized before delivery."). When MCP tools are
searched instead of listed, `mcp_search_tools` also returns matching skills (name,
location, description). Files of skills installed under `~/.nyq/skills` can be read
with the file tools (the rest of `~/.nyq` stays off limits).

**Named subagents.** Without a `name`, a `subagent` call is one-shot. With one
(`name: "scout"`) the subagent is kept for the session: later calls with that name
send it follow-up messages, and it still has everything it read before. The model
can also `list`, `inspect`, `stop` and `resume` them (resume reopens a stopped one or
finishes a message that was interrupted). They are saved next to the session
(`~/.nyq/sessions/<id>.agents/<name>.json`, mode 0600), so `nyq -r` / `/resume`
brings them back, and deleting the session deletes them. While subagents work, each
shows a live line under the tool rows (`↳ scout: Find parser · 3 tool calls ·
grep fn parse (12s)`). `/agents` lists them (mode, status, turns, tool calls),
`/agents show NAME` prints its recent transcript, `/agents stop NAME` stops it and
`/agents delete NAME` forgets it. A call may pick the subagent's `model`, `effort`
and `permission_mode` (never more permissive than the session's). Subagents get the
session's skills, and `general` ones its MCP tools too. Approval prompts from
subagents running in parallel are shown one at a time.

Reading and searching inside the workspace run freely, and so do read-only shell
commands: pipelines of up to 8 `ls`, `cat`, `head`, `tail`, `wc`, `grep`, `rg`,
`pwd`, `echo`, `printf` and read-only `git` (`status`, `log`, `diff`, `show`,
branch listing, `rev-parse`, `remote -v`, `worktree list`, `ls-files`) whose
paths stay inside the workspace. Git options must be on a per-subcommand
allowlist; unknown options need approval. Common forms such as `git log --oneline -n 5`,
`git diff --stat HEAD~1`, `git show HEAD:path` and `git status --short` run freely.
nyq parses these itself and runs the parsed
words (with Git's fsmonitor, hooks, external diff and textconv turned off);
anything with redirection, `$(…)`, backticks, variables, globs, `~`, assignments,
unknown programs or options still asks. Reads of paths outside the
workspace, every `web_fetch`, writes, edits, other shell commands and MCP tools ask first, showing a diff for file
changes: **y** once, **a** always (this command; for a path outside the
workspace, that directory tree; for `web_fetch`, that site), **s** always
and save the rule to `permissions.json` (see below), **l** always and save it
for this workspace only, **n** deny. Number keys pick a choice too, and `↑↓`
select one for `Enter` to confirm. `PgUp`/`PgDn` scroll the preview and **v**
(or `Ctrl+O`) shows a long one full screen. **Tab** types a
note that goes to the model with your answer ("yes, but only for the staging
config", "no, use the Makefile target"); Enter or Tab returns to the choices.
Letter shortcuts accept plain or Shift-modified keys; Ctrl/Alt combinations
do not choose an answer. Enter alone never approves. `--yolo` (or `/yolo`) never asks; `-p` mode denies mutations unless
the mode is yolo (`--yolo` or `--permission-mode yolo`; a saved `yolo` does not apply to `-p`).

A `general` subagent's own tool calls go through the same rules; those that need
approval show the normal prompt with a `subagent · <description>` line above it,
and "always" grants land in your session. In `-p` mode without `--yolo` they are denied.

### Permissions

Persistent rules live in `~/.nyq/permissions.json`:

```json
{ "allow": ["mcp__deepwiki__*", "bash:cargo test", "web_fetch:https://docs.rs/*"],
  "ask":   ["mcp__github__create_*"],
  "deny":  ["mcp__*__delete_*", "bash:git push", "subagent:general"] }
```

Each entry is a tool name with `*` wildcards (`web_fetch`, `mcp__deepwiki__*`,
`mcp__*`), `subagent:explore` / `subagent:general`, `web_fetch:<url>`, or
`bash:<prefix>`. In `allow`, a bash prefix covers the same plain commands an
"always" grant would (no `;`, `&&`, pipes, redirects, dollar signs or
backticks, including `$'…'` and `$"…"` quoting); in `ask`/`deny` it also
catches the words anywhere in the command (`ls; git push`, `sh -c 'git push'`) —
best effort, the sandbox is the real boundary.

Path-scoped rules limit file tools to paths: `edit_file:src/**`,
`write_file:docs/*.md`, `read_file:secrets/*`, `read:~/notes/**`,
`edit:/tmp/scratch/**`. The part before `:` picks a category — `read_file`,
`list_dir`, `glob`, `grep`, `view_image` and `read` cover every read tool;
`write_file`, `edit_file` and `edit` cover both writing tools — so a
`deny: ["read_file:secrets/*"]` also stops `grep` there. Relative patterns are
relative to the workspace; `*` matches within a path segment and `**` across
segments. Ask and deny rules match the path as written or with symlinks
resolved; an allow matches only where the path really leads, may cover paths
outside the workspace (that is what it names), and never covers edits under
`.git`.

Precedence: **deny > ask > allow > session grants > the permission mode**. Deny blocks the call in every mode,
including yolo and auto. An ask rule still prompts in auto (yolo skips it).
Allow rules and session grants run without asking. Anything still undecided
asks in `ask`, goes to the classifier in `auto`, and runs in `yolo`. An allow
never covers writes under `.git` or outside the workspace, nor reads outside
the workspace — those still ask — unless it is a path-scoped rule naming them.

`auto` forks the session's provider. The classifier model is `classifier_model`
in `settings.toml` (also **Classifier model** in `/settings`); unset (`auto`), it is
a mini model from the live Codex catalog on Codex, and the session model
elsewhere. The
classifier sees the tool call (for `write_file`/`edit_file`: the path, whether
the file existed, lines added and removed and the start of the diff; for MCP
tools also the tool's input schema), recent tool results and your request, and
returns allow, ask or deny with a short reason. `&&` chains of known, reversible
development commands skip the classifier: `git status`, `git fetch` (without
`--prune`), `git remote -v`, `git worktree list`, `npm`/`pnpm`/`bun`
`install`/`ci`/`test`/`run SCRIPT` (not a global install, and not with a config file, `--config.*`, or a directory such as `--prefix`, `--dir`, `-C` or `--modules-dir`), `cargo build`/`test`/`check`/`clippy`,
`cargo fmt --check`, `TOOL --version`, and the read-only commands above. Allow runs the call, ask shows
the normal prompt, and deny refuses it. The same decision is reused for the
rest of the session. If the classifier errors, times out, or returns an
unusable answer, nyq asks instead. `--permission-mode ask|auto|yolo` overrides
`--yolo` and the saved `permission_mode`.

`/permissions` opens a mode menu (`↑↓`, type to filter, Enter, Esc).
`/permissions ask|auto|yolo` switches and saves, `/permissions rules` lists
rules, session grants and session rules, `/permissions reload` re-reads both
files, and `/permissions reset` goes back to `ask` and forgets this session's
grants and rules. `/permissions remember allow|deny TOOL ARGS-JSON` (after a
confirmation) allows or denies one exact call, e.g.
`/permissions remember deny bash {"command":"make deploy"}`; each rule gets a
number, and `/permissions revoke N` drops it. Session grants ("always") and these
rules are saved in the session file and come back when you resume it.

`/allowlist` (or `/allowlist view effective`) lists trusted commands, tools and URLs:
session grants, `allow` entries for every workspace, and local entries for this
workspace only. Local entries are kept in `~/.nyq/permissions.json` under
`workspaces.<path>.allow`, never in the repository. Path patterns
(`edit_file:src/**`, `read:/tmp/data/**`) in that list match the same way as
in `allow`. `/allowlist [local|user] add
command|tool|url PATTERN` trusts one (user scope by default),
`/allowlist view local|user` shows one scope, `/allowlist remove N` or
`/allowlist [local|user] remove command|tool|url PATTERN` drops one, and
`/allowlist [local|user] reset [commands|tools|urls|all]` clears them (without a
scope: session grants and both scopes). At an approval prompt, `s` saves the rule
for every workspace and `l` for this workspace only.
`nyq permissions` prints the mode and the persistent rules (`--json` for JSON).
`/yolo` toggles between yolo and ask and saves the choice. `/mcp` shows each
connected server's resulting default.

A project may add rules in `.nyq/permissions.json`, but only `ask` and `deny`: an
`allow` there is ignored, so a repository can't grant itself permissions. A
malformed file is reported once and ignored. `--sandbox read-only` confines
shell commands on its own; ask, auto and yolo still decide whether a call runs.

Project instructions in `AGENTS.md`, `CLAUDE.md` or `.nyq/instructions.md` are
added to the system prompt, after your own `~/.nyq/AGENTS.md` (for every project)
and any `AGENTS.md`/`CLAUDE.md` in the parent directories up to the repository root.
An `AGENTS.md` or `CLAUDE.md` in a subdirectory is handed to the model, once, with
the first file or search result that touches that directory.

## Sandbox

Shell commands the model runs are sandboxed by default (`--sandbox workspace-write`):
they can read everything but write only inside the workspace and temp dirs.
`--allow-network` lifts the network block, `--sandbox read-only` also protects the
workspace, and `--sandbox off` runs commands with your full permissions. Linux uses
Landlock (kernel 5.13+, TCP rules need 6.7+). Landlock can block TCP bind and
connect only: with the network off, UDP and Unix-domain sockets stay unrestricted,
and the banner says so (a sandboxed command can still reach a host daemon such as
the Docker socket). macOS uses `sandbox-exec`, which denies those sockets when the
network is off. On both, `.git` stays writable so `git commit` works inside the
sandbox — which also means a sandboxed command could change hooks or Git config
that later run with your full permissions; use `--sandbox read-only` if that
matters. `write_file`/`edit_file` always ask before touching anything under `.git`,
even with an "always" grant. Other files that tools may run later
(`.envrc`, `.vscode/tasks.json`, a project `.mcp.json`) are not special-cased: an
"always" grant for `write_file`/`edit_file` covers them, though each still needs
your own action (allowing direnv, running the task, `/mcp connect`) to execute. Where no sandbox is available (e.g. Windows) nyq warns and runs
commands unconfined. `!cmd` commands you type yourself are not sandboxed.
`write_file` and `edit_file` follow the same writable roots (read-only refuses
them).

`--add-dir PATH` (repeatable) adds another directory for this run. It is a
writable sandbox root and counts as inside the workspace for file tools, so
reads there do not ask. `nyq workspace add PATH` (or `/workspace add PATH`)
remembers the directory in `~/.nyq/workspaces.json` for the current folder;
`/workspace` lists them and `/workspace remove PATH` forgets one.
`--no-additional-dirs` ignores the saved list. Relative paths still start from
the working directory, and project instructions are still read only from there.

## Hooks

Hooks are shell commands nyq runs at conversation events, configured in
`~/.nyq/hooks.json`:

```json
{"hooks": [
  {"event": "PreToolUse", "matcher": "bash", "command": "~/bin/guard.sh", "timeout_secs": 10},
  {"event": "UserPromptSubmit", "command": "git status --short --branch"},
  {"event": "Stop", "command": "notify-send 'nyq is done'"}
]}
```

Each hook gets the event as JSON on stdin (`event`, `session_id`, `cwd`, plus
`tool`/`args` for tool events, `output`/`is_error` for PostToolUse, `prompt`,
`assistant_text` or `source`) and runs unsandboxed in the project directory;
its stderr is shown to you. `matcher` is a tool-name glob (`bash`, `write_*`,
`mcp__*`) for the tool events; `timeout_secs` defaults to 60.

- `PreToolUse` runs before permissions; a non-zero exit or
  `{"decision":"block","reason":"…"}` on stdout blocks the call and tells the
  model why. `{"updatedInput":{…}}` replaces the call's arguments (permissions
  then judge the new ones; the last hook that sets it wins). Subagents' calls
  go through the same tool hooks.
- `PostToolUse` sees each call that ran and its output.
- `UserPromptSubmit` runs before a prompt is sent: plain stdout (or
  `{"context":"…"}`) is added to the conversation; `{"decision":"block"}` drops
  the prompt.
- `Stop` runs after each turn; `{"decision":"block","reason":"…"}` keeps the
  turn going once, with the reason passed to the model.
- `SessionStart` runs at startup, `/new` and `/resume`.
- `AttentionRequired` runs in the background whenever nyq waits for you (an
  approval or a question), with `kind` (`approval`/`question`) and `message`.

A hook that fails to start, times out or is interrupted (Ctrl-C) never blocks
anything and never stops the turn. Hooks run in print mode and over ACP too.

A project may add `.nyq/hooks.json`, but it only runs once you trust it:
`/hooks` lists all hooks with their trust state, and `/hooks trust` records the
file's path and content hash in `~/.nyq/trusted-hooks.json`. Editing the file
makes it untrusted again until you re-run `/hooks trust`.

## MCP

Servers in `~/.nyq/mcp.json` connect at startup, all at once and in the
background (the prompt is usable meanwhile; their tools appear as they come up,
and `/status` shows connecting / ready / needs sign-in / failed). Servers in a
project's `.mcp.json` are only started with `/mcp connect NAME` or once trusted
(below). Stdio, streamable HTTP and legacy HTTP+SSE (`"type": "sse"`) servers are
supported:

```json
{ "mcpServers": {
    "fs": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."] },
    "deepwiki": { "url": "https://mcp.deepwiki.com/mcp" },
    "internal": { "url": "https://mcp.example.com/mcp", "headers": { "X-Team": "${TEAM}" }, "bearer_token_env": "INTERNAL_TOKEN" }
} }
```

Per-server keys: `"enabled": false` keeps a server configured but not started;
`"required": true` makes startup fail with a clear error when it cannot connect;
`startup_timeout_ms` (default 30000) and `operation_timeout_ms` (tool calls,
default 600000); `restart_limit` (default 1) restarts a server that exited on the
next call; `header_env` (`{"X-Api-Key": "VAR"}`) sends headers from environment
variables; `"oauth": {"client_id": …, "client_secret_env": …, "scopes": [...]}`
uses a pre-registered OAuth client and scopes. `${VAR}` and `${VAR:-default}` are
expanded in commands, arguments, env values, URLs and headers. For compatibility,
`"mcp"` works as the root key, `"type": "local"`/`"remote"` as stdio/http,
`command` may be an array and `environment` an alias for `env`; near-miss root
keys such as `servers` are reported. `docker run` servers get a `--cidfile`, and
their container is removed (`docker rm -f`) when the server is closed.

Manage them without opening nyq:

```sh
nyq mcp list                                   # profile and project servers (no connecting)
nyq mcp list --connect                         # start trusted servers; show tools or errors (--json)
nyq mcp add fs --env ROOT=. -- npx -y @modelcontextprotocol/server-filesystem .
nyq mcp add deepwiki --url https://mcp.deepwiki.com/mcp --header 'X-Team=${TEAM}'
nyq mcp add --transport http docs https://example.com/mcp   # same as --url
nyq mcp remove fs                              # other entries and keys in mcp.json are kept
nyq mcp login deepwiki / nyq mcp logout deepwiki  # `mcp auth` is an alias of login
nyq mcp trust [list|approve NAME|reject NAME|approve-all|reset]
```

`add` refuses an existing name unless `--force`; `--bearer-token-env VAR` and `--cwd DIR`
are available too. A project's `.mcp.json` server that you trust (`nyq mcp trust
approve NAME`, or `/mcp trust …`) connects at startup like a profile server; its
configuration hash is recorded in `~/.nyq/trusted-mcp.json`, so editing the entry
makes it untrusted again. Until then a project server may not read environment
variables (`${VAR}`) or send credentials (`Authorization` headers,
`bearer_token_env`, `header_env`); such entries are skipped with a hint to trust them.

HTTP servers that require OAuth: run `/mcp login NAME` to sign in through your
browser (dynamic client registration + PKCE); tokens are stored under
`~/.nyq/mcp-auth/` (mode 0600) and refreshed automatically. `/mcp logout NAME`
forgets them and revokes the tokens when the authorization server supports it.
When a server answers that a call needs more access (`insufficient_scope`), the
next `/mcp login` asks for the extra scopes too. Tools appear as
`mcp__<server>__<tool>`; their arguments are checked against the tool's input
schema before the call, and progress the server reports shows on the tool's
status line. Servers that change their tools, prompts or resources
(`list_changed`) are re-listed live. A server's `instructions` are added to the
system prompt and to tool-search results.

Servers may ask you for input (elicitation): in the terminal nyq asks each form
field in the question pane, and opens URL requests in your browser after you
agree, then waits up to 10 minutes for the server to finish. Print mode declines;
over ACP the request is forwarded to the editor when it supports elicitation.

When connected servers bring more than 40 tools (or roughly 12k tokens of
schemas), their tools are not all sent with every request: the model gets
`mcp_search_tools` to find them and `mcp_select_tool` to make one callable for the
rest of the session. Servers that offer resources add `mcp_list_resources` and
`mcp_read_resource`; servers with prompts add `mcp_list_prompts` and
`mcp_get_prompt`. Server prompts become slash commands:
`/mcp:<server>:<prompt> [args]` (arguments in order, or `name=value`) fetches the
prompt and sends its messages as your turn; `/help` lists them. A missing
argument is reported with the server's suggestions (`completion/complete`).

Inside a session, `/mcp` (or `/mcp list`) shows each server's state, permission
default and tool names. `/mcp add NAME COMMAND [ARGS…]` and
`/mcp add [--transport http] NAME URL` add a profile server and connect it at once
(`--force` replaces one; other options are on `nyq mcp add`), `/mcp remove NAME`
removes and disconnects it, `/mcp reload` re-reads both config files (new servers
connect, removed ones disconnect, changed ones reconnect, the rest keep running),
and `/mcp path` prints the config path. To look at what a server offers yourself:
`/mcp resource list|templates SERVER`, `/mcp resource read SERVER URI`,
`/mcp resource complete SERVER URI-TEMPLATE VAR [VALUE]`, `/mcp prompt list SERVER`
(`*` marks required arguments), `/mcp prompt get SERVER NAME [ARGS-JSON]` and
`/mcp prompt complete SERVER NAME ARG [VALUE]`.

## Extensions

Extensions add tools, slash commands, event handlers and status-line text, written
in any language: nyq starts each one as a child process and talks JSON-RPC 2.0 to it
over stdio (one JSON object per line). An extension is a directory with an
`extension.json`:

```json
{"name": "word-tools", "version": "0.1.0", "description": "word_count and an rm -rf guard",
 "command": ["python3", "ext.py"]}
```

in `~/.nyq/extensions/<dir>/` or the project's `.nyq/extensions/<dir>/`. Project
extensions start only once you trust them (`/extensions trust` records a hash of the
directory's files in `~/.nyq/trusted-extensions.json`; any change needs trusting
again). `word-tools = false` under `[extensions]` in `settings.toml` disables one. They
start in the background (print mode and ACP wait for them), run unsandboxed, log
their stderr to `~/.nyq/logs/ext-<name>.log`, and are restarted once if they crash.

In the `initialize` handshake an extension registers:

- **tools**, offered to the model as `ext__<extension>__<tool>`: read-only ones run
  freely, others ask like any change; permission rules such as
  `"allow": ["ext__word-tools__*"]` work as for MCP tools;
- **commands**, `/name args`, listed under Extensions in `/help` and the `/` menu;
  they print a message and/or send a prompt;
- **events**: `session_start`, `user_prompt` (block, rewrite or add context),
  `tool_call` (block or rewrite the input, for every tool including subagents'),
  `tool_result` (rewrite what the model sees), `turn_end`, `before_compact` (extra
  summary instructions) and `session_end`, each with a timeout (5 s by default);
- a **status** text, updated later with `status/set`.

Extensions can also show notices, ask yes/no or text questions, and send messages as
the user (`ui/notify`, `ui/confirm`, `ui/input`, `session/send`). Commands and the
status line are terminal-UI features; tools and events work in print mode and ACP too.

```sh
nyq extensions list                            # alias: nyq ext
nyq extensions install ./examples/extensions   # a directory, git URL or owner/repo
nyq extensions remove word-tools
nyq extensions path
```

An `install` command in the manifest (e.g. `npm install`) is shown and run only after
you confirm (`--yes` skips the question). In a session, `/extensions` lists them with
their state, tools, commands and log path, and `/extensions reload` restarts them.
The protocol, a dependency-free TypeScript/JavaScript helper
(`examples/extensions/sdk/`) and two examples (Python: a `word_count` tool, `/hello`
and an `rm -rf` guard; Node: the Git branch in the status line and repository context
for each prompt) are in [`examples/extensions/`](https://github.com/Znskr/nyq/tree/main/examples/extensions).

## Editors (ACP)

`nyq acp` speaks the [Agent Client Protocol](https://agentclientprotocol.com) over
stdin/stdout, so editors such as Zed can drive it. In Zed's `settings.json`:

```json
{ "agent_servers": { "nyq": { "command": "nyq", "args": ["acp"] } } }
```

Each editor session is a regular nyq session (same model choice, tools, skills,
profile MCP servers, sandbox and saved conversation; MCP servers the editor passes
are connected too, stdio, streamable HTTP and SSE). Answers, reasoning and tool calls
stream into the editor, file edits show as diffs, and approvals appear as the
editor's permission prompts (allow once, always for this session, reject). The
session mode is **Ask**, **Auto** or **Yolo** (a classifier in Auto; Yolo never
asks). A saved Yolo mode starts as Ask here. Start it with `"args": ["acp", "--yolo"]` or `"args": ["acp", "--permission-mode", "auto"]` to set the mode;
flags such as `--sandbox` and `--model` work before or after `acp`. The model picker
lists the same models as `/model`. Saved sessions can be listed and reopened from
the editor, and `session/resume` reconnects without replaying history.
`--add-dir`, `--no-additional-dirs` and the directories saved with `nyq workspace`
apply to each session's folder. `nyq acp --log-file PATH` appends request and error
logs to a file, since stdout carries the protocol. `ask_user_question` is not
available in this mode.

## Sessions

Conversations are saved as JSONL under `~/.nyq/sessions/`, one item per line.
Each session also records its provider, model, reasoning effort and fast mode
(again whenever you switch), and `-r`, `-c` and `/resume` restore them; `-m` or
`--provider` on the command line takes precedence. Listings, the picker and
`-c`/`last` order sessions by their last activity. Sessions are named by their
first real prompt (a lone slash command is skipped; an image-only prompt gives
"Image session"; otherwise "Untitled session"). Each listing is cached in
`<id>.summary` next to the transcript until the file changes.

Prompt history is in `~/.nyq/history`: each prompt is appended as it is sent
(safe with several nyq windows open), tagged with its workspace, and the file
keeps the newest 1000 prompts.

The resume picker (`/resume`, `nyq -r`) shows each session's title,
workspace, turn count and last activity; `Tab` switches between **This
workspace** and **All workspaces**, and typing filters by title, first prompt,
directory or id.

`/rename Title` stores a title on the session (a metadata line in the JSONL) so
`nyq sessions` and the resume picker show it. `/rename` with no title asks the
model for a 3–6 word name. `/continue` asks the model to resume an interrupted
or cut-off response, without adding a new user message. `/reset` clears the
transcript and stops background processes but keeps this session. `/clear` starts a
fresh session the way `/new` does; both keep background processes running.

`nyq session show [last|id]` prints every turn of the transcript with the
model settings, last update and history length, `nyq session resume
[last|id]` is the same as `-r`, and `nyq session delete <last|id>` removes the
session with its artifacts, subagents, usage copy and terminal tape.
`nyq sessions` notes how many saved sessions could not be read; `nyq doctor`
names them (and sessions over 64 MB) with a recovery hint.

## Long conversations

When a request uses 80% of the context window (the "Auto-compact at" setting), nyq asks the model to summarize the
conversation and continues from the summary; `/compact` does it on demand. The
window defaults to 272k tokens for Codex and 128k otherwise; set
`NYQ_CONTEXT_WINDOW` to match your model (0 disables automatic compaction).
Resumed sessions start from the latest summary.

## Images

Mention an image as `@path/to/shot.png` (or paste/drag a path to an existing
image) to attach it to your message. The model can also look at workspace images
with `view_image`. PNG, JPEG, GIF and WebP up to 20 MB. When the models.dev catalog
says the current model can't read images, attached images are saved under
`~/.nyq/artifacts/<session>/images/` and the model gets a `vision` tool instead of
`view_image`: it sends the images and a question to a vision-capable model of the same
provider (`NYQ_VISION_MODEL`, or the catalog model with the most similar name) and
returns its description.

`/image PATH` attaches an image to your next message (as do `@path`, `/paste`
and `Ctrl+V`). An attached image shows as an `[Image #N]` token in the input;
deleting the token drops the image. `/images` lists pending images
(`/images remove N`, `/images clear`).

## Terminal tapes and session repair

Run `nyq --record` in an interactive terminal to save timestamped terminal output
and size changes to `$NYQ_HOME/tapes/<session>.cast` (default `~/.nyq/tapes/`).
The tape covers the whole interactive run, including session switches. Tapes use
asciicast v2: play them with `asciinema play` or `nyq replay <tape> --speed 2`
(default speed: 1). Recordings contain the visible conversation and tool output;
keep them private when that output is sensitive.

`nyq session recover <id>` copies all readable conversation items into a new
session, skipping broken lines and reporting item and skipped-line counts. The
source is left unchanged, even if its header or final line is damaged.
`nyq session migrate <id>` atomically adds the current version to an older JSONL
header, or supplies a missing header, preserving the conversation. Repeating it
leaves a current file unchanged. Migration refuses corrupt items; recover those
first. Both commands accept a unique filename id prefix.

## Pull requests and issues

`nyq pr [context]` drafts a pull request for the current branch: it collects the
commits and diff since the merge base with the default branch (`origin/HEAD`, else
`main`/`master`; `--base BRANCH` overrides) plus your context, asks the configured
model for a title and a body with `## Summary` and `## Testing`, and prints it. In a
terminal you can then **p**ublish, **e**dit it in `$VISUAL`/`$EDITOR`, or **c**ancel.
Publishing runs `gh pr create` (add `--draft` for a draft PR); if the branch has no
upstream or unpushed commits, nyq asks before pushing it to `origin`. `--publish`
skips the review step. Without a terminal the draft is only printed.

`nyq issue [context]` does the same for an issue (`## Summary`, `## Steps to
Reproduce`, `## Expected`, `## Actual`) from your context and a short repository
snapshot, and publishes it with `gh issue create`. Both need the GitHub CLI (`gh`)
to publish.

## Upgrades

`nyq upgrade` installs the newest release of `Znskr/nyq` from GitHub:
`--channel stable` (default) skips prereleases, `--channel latest` includes them; the
choice is remembered. It downloads the archive for your platform, verifies it against
the release's `SHA256SUMS`, checks that the new binary runs, and atomically replaces
the running executable (on Windows the old one is renamed to `nyq.exe.old`).
`--check` only reports. Release checks use GitHub's public API; if you hit its
anonymous rate limit, set `GITHUB_TOKEN`.

Interactive sessions upgrade themselves in the background: 10 seconds after start,
and then every 30 minutes, nyq checks the saved channel and, when a newer release
exists, downloads it, verifies it against `SHA256SUMS`, checks that it runs and
installs it over the executable, the same way `nyq upgrade` does, without asking.
The status line shows `upgrading to X.Y.Z...` meanwhile, then
`update ready: ctrl+g to reload` (or `upgrade failed`, in which case the old binary
keeps running and the next check tries again). Nothing changes in the running
session until you press **Ctrl+G**: nyq then saves the conversation, restores the
terminal and starts the new binary with the same flags and directory, resuming the
same conversation (`--resume <id>`; a prompt given on the command line, `-c`, `-r`
and `--record` are not repeated). Ctrl+G waits while a reply is streaming, prompts
are queued, a question, approval, menu, picker or the transcript viewer is open, or
the input holds a draft. Print mode (`-p`, `nyq ask`), `nyq acp` and binaries run
from a cargo `target/debug` or `target/release` directory never auto-upgrade.

To turn auto-upgrade off, set `auto_upgrade = false` in `~/.nyq/settings.toml`
(or **Auto-upgrade** under `/settings` → Advanced), or set `NYQ_AUTO_UPGRADE=0`
(`NYQ_NO_UPDATE_CHECK=1` also disables it). `nyq upgrade` keeps working either way.

## Usage and diagnostics

`/feedback` opens the [issue page](https://github.com/Znskr/nyq/issues/new).
`/trace` writes a diagnostic bundle to `~/.nyq/traces/` (mode 0600: version,
platform, model, settings and the last 50 conversation events as short excerpts,
with keys, tokens and passwords redacted) and copies its path.

Every response that reports tokens (including compaction and subagents) is recorded
privately in `<NYQ_HOME>/usage.jsonl` (default `~/.nyq/usage.jsonl`). `/usage [24h|7d|30d]`
(also `/cost`) and `nyq usage --period 7d` show provider and model totals with each
model's share of tokens; the default is 30 days, and `/usage session` totals this
conversation.
Costs are estimates using local models.dev data in `~/.nyq/models.json` or
`~/.cache/opencode/models.json`, then a small built-in OpenAI pricing table.
Unknown pricing is reported separately; Codex and Grok subscription requests show
**subscription**, with no dollar cost. Cached input is included in input totals and
priced separately; Anthropic prompt-cache writes are priced at the catalog's
cache-write rate, and reasoning tokens (part of output) are shown when the provider
reports them. The log drops records older than 35 days once it grows past 8 MB, and
reports note how far back it is complete ("tracking since"). Each saved session keeps
a copy of its own records in `sessions/<id>.usage`, so resuming does not rescan the log.

`/stats` shows session turns, successful model requests, input/output/cached
(and reasoning and cache-write) tokens, tool calls by name, and for this run the
wall time, time spent waiting on the model, and lines added and removed by
`write_file`/`edit_file`. Resuming restores recorded usage and
saved transcript counters; historical requests without reported usage cannot be
recovered. `/status` or `nyq status [--json]` shows the selected model, effort,
profile, authentication sources (never credentials), sandbox, MCP servers, skills,
permissions paths and context window. CLI status does not connect MCP servers.
`nyq doctor` checks authentication and expiry, endpoint reachability (at most five
seconds), sandbox, profile writes, MCP configuration, saved sessions, git and terminal capabilities;
failed checks include hints and cause a nonzero exit. HTTP error responses still
prove endpoint reachability; auth is checked separately. `nyq models` lists the
same provider catalog as the picker, and `/version` shows the installed version.
Doctor also reports settings.toml parse problems, bad `theme_colors` entries,
saved sessions that need `nyq session recover` or `migrate`, the permission mode
and step limit, whether the
folder is a git repository and whether `gh` is installed (informational).
`nyq doctor`, `models`, `usage`, `sessions`, `session show`, `session migrate|recover`,
`workspace`, `upgrade` and `mcp list` accept `--json`. `nyq -v` prints the version.

### Environment and limits

| Variable | |
|---|---|
| `NYQ_PERMISSION_MODE=ask\|auto\|yolo` | default permission mode, below `--permission-mode`/`--yolo` and above settings (also applies to `-p`) |
| `NYQ_DEBUG=1` | like `--debug`: developer-only features (the `/tools` menu) for this run |
| `NYQ_MAX_AGENT_STEPS=N` | model requests per turn before nyq stops with a notice (default 100; also `max_agent_steps` in settings.toml) |
| `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL`, `NYQ_ANTHROPIC_BASE_URL`, `ANTHROPIC_MODEL` | the `anthropic` provider: key (`x-api-key`), bearer token, Anthropic-compatible endpoint, default model |
| `NYQ_NO_OPEN_BROWSER=1` | sign-in flows print the URL instead of opening a browser (also when stdin is not a terminal) |
| `NYQ_SOUND=off\|on\|max` | overrides the sound setting |
| `NYQ_RECORD=PATH` | record interactive sessions to this tape, even without `--record`; `NYQ_RECORD_INPUT=1` also records keystrokes (never typed API keys) |
| `NYQ_TRACE=1` or `NYQ_TRACE=provider,tool,mcp,acp` | live debug trace (all or some scopes) to stderr, or appended to `NYQ_TRACE_LOG=PATH`; `nyq status` shows it |

`--context-limit NAME=BYTES|off` (repeatable) overrides a prompt budget for one run;
`NAME = BYTES` or `NAME = "off"` under `[context_limits]` in settings.toml sets it permanently, and
`nyq status --json` reports each value and its source. Names:
`skill_description_bytes` (120), `skill_catalog_bytes` (off), `skill_file_bytes` (off;
a larger SKILL.md or skill resource is not loaded), `skill_chunk_bytes` (32 KiB; the
`skill` tool returns larger content in parts), `mcp_description_bytes`
(off), `mcp_content_block_bytes` (8 KiB), `mcp_content_total_bytes` (32 KiB),
`project_instruction_file_bytes` (off) and `project_instructions_total_bytes` (off).
