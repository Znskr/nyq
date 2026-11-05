import { readFileSync } from "node:fs";
import { resolve } from "node:path";

/** nyq's version, read from the crate so the site never goes stale. */
export const VERSION =
  readFileSync(resolve(process.cwd(), "../Cargo.toml"), "utf8").match(
    /^version\s*=\s*"([^"]+)"/m,
  )?.[1] ?? "";

/** A path under the site's base (works on GitHub Pages and at the root). */
export function href(path = ""): string {
  const base = import.meta.env.BASE_URL.replace(/\/$/, "");
  return `${base}/${path.replace(/^\//, "")}`;
}

export const REPO = "https://github.com/Znskr/nyq";
export const INSTALL =
  "curl -fsSL https://nyq.zalthar.com/install.sh | sh";
