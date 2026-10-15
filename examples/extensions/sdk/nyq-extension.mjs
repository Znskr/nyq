// A dependency-free helper for writing nyq extensions in JavaScript: the
// same code as nyq-extension.ts without the types, for any Node version.
// See nyq-extension.ts for the API and docs/extensions.md for the protocol.
//
//   import { extension } from "./nyq-extension.mjs";
//   extension({ tools: [...], commands: [...], on: { tool_call(e) { ... } } });

import { createInterface } from "node:readline";

/** Run `ext` until nyq closes stdin. */
export function extension(ext) {
  const write = (msg) => process.stdout.write(JSON.stringify({ jsonrpc: "2.0", ...msg }) + "\n");
  const pending = new Map();
  let next = 1;
  const request = (method, params) =>
    new Promise((resolve) => {
      const id = next++;
      pending.set(id, resolve);
      write({ id, method, params });
    });
  const notify = (method, params) => write({ method, params });
  const ctx = {
    session: { nyq_version: "", protocol: 1, cwd: process.cwd(), session_id: "", mode: "print" },
    notify: (text, level = "info") => notify("ui/notify", { text, level }),
    confirm: async (text) => (await request("ui/confirm", { text })) === true,
    input: async (prompt) => (await request("ui/input", { prompt })) ?? null,
    send: (text, queue = "follow_up") => notify("session/send", { text, queue }),
    status: (text) => notify("status/set", { text }),
    log: (text) => notify("log", { text }),
  };
  const tools = new Map((ext.tools ?? []).map((t) => [t.name, t]));
  const commands = new Map((ext.commands ?? []).map((c) => [c.name, c]));
  const handlers = { ...ext.on };
  let inflight = 0;
  let closed = false;

  async function handle(method, params) {
    if (method === "initialize") {
      ctx.session = params;
      await ext.init?.(ctx);
      return {
        tools: [...tools.values()].map(({ name, description, input_schema, read_only }) => ({
          name,
          description,
          input_schema: input_schema ?? { type: "object", properties: {} },
          read_only: read_only ?? false,
        })),
        commands: [...commands.values()].map(({ name, description, args_hint }) => ({ name, description, args_hint: args_hint ?? "" })),
        events: Object.keys(handlers).filter((k) => typeof handlers[k] === "function"),
        status: typeof ext.status === "function" ? ext.status(ctx) : ext.status,
      };
    }
    if (method === "tool/call") {
      const tool = tools.get(params.name);
      if (!tool) throw new Error(`unknown tool ${params.name}`);
      try {
        const out = await tool.run(params.input ?? {}, ctx);
        return typeof out === "string" ? { content: [{ type: "text", text: out }] } : out;
      } catch (e) {
        return { content: [{ type: "text", text: String(e instanceof Error ? e.message : e) }], is_error: true };
      }
    }
    if (method === "command/run") {
      const command = commands.get(params.name);
      if (!command) throw new Error(`unknown command ${params.name}`);
      const out = await command.run(params.args ?? "", ctx);
      return typeof out === "string" ? { message: out } : (out ?? {});
    }
    if (method.startsWith("event/")) {
      const handler = handlers[method.slice("event/".length)];
      return (handler ? await handler(params, ctx) : undefined) ?? {};
    }
    throw Object.assign(new Error(`unknown method ${method}`), { code: -32601 });
  }

  const lines = createInterface({ input: process.stdin });
  lines.on("line", (line) => {
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      return;
    }
    if (msg.method === undefined) {
      pending.get(msg.id)?.(msg.error ? null : msg.result);
      pending.delete(msg.id);
      return;
    }
    inflight++;
    handle(msg.method, msg.params ?? {})
      .then(
        (result) => msg.id !== undefined && write({ id: msg.id, result: result ?? null }),
        (e) => msg.id !== undefined && write({ id: msg.id, error: { code: e?.code ?? -32000, message: String(e?.message ?? e) } }),
      )
      .finally(() => --inflight === 0 && closed && process.exit(0));
  });
  // nyq closed stdin: finish what is running (e.g. session_end), then exit.
  lines.on("close", () => {
    closed = true;
    if (inflight === 0) process.exit(0);
  });
}
