// @ts-check
import { defineConfig } from "astro/config";
import tailwindcss from "@tailwindcss/vite";

// The site is served from the root of its domain on Cloudflare Workers.
// SITE_URL is its public address (for canonical links and social cards); the
// default is the production address; set SITE_URL to override.
export default defineConfig({
  site: process.env.SITE_URL ?? "https://nyq.zalthar.com",
  trailingSlash: "ignore",
  markdown: {
    // Both themes' colors as CSS variables; global.css picks one per site theme.
    shikiConfig: { themes: { light: "github-light", dark: "github-dark" }, defaultColor: false },
  },
  vite: { plugins: [tailwindcss()] },
});
