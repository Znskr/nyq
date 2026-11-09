import { readFileSync } from "node:fs";
import { resolve } from "node:path";

// Serves the repository's install.ps1 at /install.ps1 for Windows:
// `irm https://nyq.zalthar.com/install.ps1 | iex`.
export function GET() {
  return new Response(readFileSync(resolve(process.cwd(), "../install.ps1"), "utf8"), {
    headers: { "Content-Type": "text/plain; charset=utf-8" },
  });
}
