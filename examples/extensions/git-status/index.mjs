// A nyq extension in Node.js (no dependencies): shows the Git branch in the
// status line and gives the model the repository state with every prompt.
import { execFileSync } from "node:child_process";
import { extension } from "./nyq-extension.mjs";

let cwd = process.cwd();

/** Output of a git command in the session's directory, or null. */
function git(...args) {
  try {
    return execFileSync("git", args, { cwd, encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] }).trimEnd();
  } catch {
    return null;
  }
}

const branch = () => git("rev-parse", "--abbrev-ref", "HEAD");
const label = () => (branch() ? `git:${branch()}` : "");

extension({
  init(ctx) {
    cwd = ctx.session.cwd;
  },
  status: label,
  on: {
    // The branch may change during a turn (the model ran git checkout).
    turn_end(_e, ctx) {
      ctx.status(label());
    },
    user_prompt() {
      const b = branch();
      if (!b) return;
      const changed = (git("status", "--porcelain") ?? "").split("\n").filter(Boolean);
      const list = changed.slice(0, 10).map((l) => `  ${l}`).join("\n");
      const more = changed.length > 10 ? `\n  … and ${changed.length - 10} more` : "";
      return {
        context: `Git: on branch ${b}, ${changed.length} changed file(s)${changed.length ? `:\n${list}${more}` : "."}`,
      };
    },
  },
});
