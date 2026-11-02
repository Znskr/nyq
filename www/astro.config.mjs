// @ts-check
import { defineConfig } from "astro/config";
import tailwindcss from "@tailwindcss/vite";

// The site is served from the root of its domain on Cloudflare Workers.
// SITE_URL is its public address (for canonical links and social cards); the
// deploy workflow sets it from the SITE_URL repository variable.
export default defineConfig({
  site: process.env.SITE_URL ?? "http://localhost:4321",
  trailingSlash: "ignore",
  vite: { plugins: [tailwindcss()] },
});
