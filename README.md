# nyq

A small, friendly coding agent for your terminal. Use the nyq Pro, ChatGPT or
Grok plan you already pay for, an Anthropic or OpenAI key, or a local model,
with sandboxed shell commands.

**Docs:** [nyq.zalthar.com/docs](https://nyq.zalthar.com/docs) ·
**Changes:** [CHANGELOG.md](CHANGELOG.md)

- **Typed tasks.** `nyq -p "…" --schema result.json --validate build` runs until
  a submitted result passes its checks, prints it as JSON and exits 0 (2 when its
  budget runs out). A background build keeps the model looking at the real state.
- **Edits that hold.** `multi_edit`, `apply_patch` and `edit_range`, with a
  read-before-edit guard; a working set shows open files from disk on every request.
- **Safe by construction.** Sandboxed shell (Landlock and seccomp on Linux),
  permission rules, taint rules (`deny exec when tainted-by:web`), crash-safe
  sessions that never re-run a call by themselves.
- **Orchestration.** Typed and git-worktree subagents, external ACP agents, a
  shared team task graph, and `run_code`/`workflow` scripts.
- **Profiles and tooling.** `--profile minimal|ci|sdk`, `--dump-config`,
  `nyq export --redact`, `nyq bench`.

## Install

macOS and Linux:

```sh
curl -fsSL https://nyq.zalthar.com/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://nyq.zalthar.com/install.ps1 | iex
```

Binaries for macOS, Linux and Windows are on the
[releases page](https://github.com/Znskr/nyq/releases).

## Build from source

```sh
cargo build --release
./target/release/nyq --help
cargo test
```

`run_code` and `workflow` embed QuickJS (the default `scripting` feature);
`cargo build --release --no-default-features --features tls-rustls` leaves it
out for a smaller binary.

## License

nyq is free software under the [GNU Affero General Public License v3.0](LICENSE)
(AGPL-3.0-only).
