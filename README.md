# nyq

A small, friendly coding agent for your terminal. Use the ChatGPT or Grok plan
you already pay for, an Anthropic or OpenAI key, or a local model, with
sandboxed shell commands.

**Docs:** [nyq.zalthar.com/docs](https://nyq.zalthar.com/docs)

## Install

```sh
curl -fsSL https://nyq.zalthar.com/install.sh | sh
```

Binaries for macOS, Linux and Windows are on the
[releases page](https://github.com/Znskr/nyq/releases).

## Build from source

```sh
cargo build --release
./target/release/nyq --help
cargo test
```

## License

nyq is free software under the [GNU Affero General Public License v3.0](LICENSE)
(AGPL-3.0-only).
