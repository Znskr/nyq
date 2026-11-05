import { readFileSync } from "node:fs";
import { resolve } from "node:path";

// Serves the repository's install.sh at /install.sh, so the one-line
// installer is `curl -fsSL https://nyq.zalthar.com/install.sh | sh`.
export function GET() {
  return new Response(readFileSync(resolve(process.cwd(), "../install.sh"), "utf8"), {
    headers: { "Content-Type": "text/x-shellscript; charset=utf-8" },
  });
}
