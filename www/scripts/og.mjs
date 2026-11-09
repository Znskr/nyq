// Renders scripts/og.html to public/og.png (1200x630). Needs Chromium:
//   node scripts/og.mjs   (CHROME=/path/to/chromium to override)
import { chromium } from "playwright-core";
import { pathToFileURL } from "node:url";
import { resolve } from "node:path";

const browser = await chromium.launch({ executablePath: process.env.CHROME ?? "/usr/bin/chromium-browser" });
const page = await browser.newPage({ viewport: { width: 1200, height: 630 } });
await page.goto(pathToFileURL(resolve("scripts/og.html")).href, { waitUntil: "networkidle" });
await page.evaluate(() => document.fonts.ready);
await page.screenshot({ path: "public/og.png" });
await browser.close();
console.log("wrote public/og.png");
