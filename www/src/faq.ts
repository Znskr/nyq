/** FAQ entries, shared by the page and its FAQPage structured data. */
export const faq: { q: string; a: string }[] = [
  {
    q: "Is nyq free?",
    a: "Yes. nyq is open source under the GNU AGPL v3. You pay your model provider through the subscription or API key you already have, and local models through Ollama cost nothing.",
  },
  {
    q: "Which models and providers does it support?",
    a: "ChatGPT subscriptions (the Codex backend), Grok subscriptions (SuperGrok and X Premium), the Anthropic API and Anthropic compatible endpoints such as DeepSeek or Kimi, the xAI API, OpenCode Zen and OpenCode Go, and any OpenAI compatible API, including OpenAI, OpenRouter, vLLM, llama.cpp and Ollama.",
  },
  {
    q: "Can I use my Claude Pro or Max subscription?",
    a: "No. Anthropic's terms do not allow third party tools to use consumer subscriptions, so nyq uses Anthropic API keys and compatible endpoints only.",
  },
  {
    q: "Is it safe to let it run shell commands?",
    a: "Commands run in a sandbox by default: they can read everything but write only inside your workspace and temp folders, with the network off unless you pass --allow-network. Linux uses Landlock and macOS uses sandbox-exec. You also choose how tool calls are approved: ask, auto or yolo. Windows has no sandbox, and nyq warns you about that.",
  },
  {
    q: "Does nyq collect telemetry?",
    a: "No. nyq talks to the model provider you choose, fetches the public models.dev catalog at most once a day for model names and prices, and checks GitHub for new releases. You can turn the upgrade check off with auto_upgrade = false or NYQ_AUTO_UPGRADE=0.",
  },
  {
    q: "Where are my keys stored?",
    a: "In ~/.nyq on your machine, in files only your user can read (mode 0600). nyq logout removes every sign in it stored.",
  },
  {
    q: "Does it work inside my editor?",
    a: "Yes, in any editor that speaks the Agent Client Protocol, such as Zed. Run nyq acp as the agent server and you get streamed answers, diffs and permission prompts inside the editor.",
  },
  {
    q: "Can I use it in scripts and CI?",
    a: "Yes. nyq -p \"prompt\" prints the answer to stdout and exits. nyq ask --json returns one JSON object with the answer, every tool call and an exit code.",
  },
  {
    q: "Can I run it fully offline with a local model?",
    a: "Yes. Point NYQ_BASE_URL at Ollama or llama.cpp and pick a model. Give the model a context of at least 32k tokens, since Ollama's 4k default is too small for an agent.",
  },
  {
    q: "Which platforms does it run on?",
    a: "macOS and Linux on x86_64 and arm64, plus Windows. The Linux build is static, so it needs no particular glibc. On Windows, shell commands use Git Bash when it is installed.",
  },
];
