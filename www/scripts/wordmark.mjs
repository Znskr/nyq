// Samples "nyq" set in Geist into a dot grid for the footer wordmark and writes
// src/wordmark.ts. Needs a Chromium build and playwright-core:
//   node scripts/wordmark.mjs   (CHROME=/path/to/chromium to override)
import { chromium } from "playwright-core";
import { readFileSync, writeFileSync } from "node:fs";

const font = readFileSync("node_modules/@fontsource-variable/geist/files/geist-latin-wght-normal.woff2").toString("base64");
const browser = await chromium.launch({ executablePath: process.env.CHROME ?? "/usr/bin/chromium-browser" });
const page = await browser.newPage();
await page.setContent(`<style>@font-face{font-family:G;src:url(data:font/woff2;base64,${font}) format("woff2");font-weight:100 900}</style><span style="font:600 10px G">nyq</span>`);
await page.evaluate(() => document.fonts.ready);
const cells = await page.evaluate(() => {
  const SIZE = 800, CELL = 12; // font size and dot pitch, in canvas pixels
  const c = document.createElement("canvas");
  c.width = 2000; c.height = 1200;
  const g = c.getContext("2d");
  g.font = `600 ${SIZE}px G`;
  g.letterSpacing = "-20px";
  g.textBaseline = "alphabetic";
  g.fillText("nyq", 40, 820);
  const d = g.getImageData(0, 0, c.width, c.height).data;
  let x0 = c.width, x1 = 0, y0 = c.height, y1 = 0;
  for (let y = 0; y < c.height; y++) for (let x = 0; x < c.width; x++)
    if (d[(y * c.width + x) * 4 + 3] > 0) { x0 = Math.min(x0, x); x1 = Math.max(x1, x); y0 = Math.min(y0, y); y1 = Math.max(y1, y); }
  const cols = Math.ceil((x1 - x0 + 1) / CELL), rows = Math.ceil((y1 - y0 + 1) / CELL);
  const out = [];
  for (let r = 0; r < rows; r++) {
    let line = "";
    for (let k = 0; k < cols; k++) {
      let sum = 0;
      for (let y = 0; y < CELL; y++) for (let x = 0; x < CELL; x++) {
        const px = x0 + k * CELL + x, py = y0 + r * CELL + y;
        if (px < c.width && py < c.height) sum += d[(py * c.width + px) * 4 + 3];
      }
      line += String(Math.min(9, Math.round((sum / (CELL * CELL * 255)) * 9)));
    }
    out.push(line);
  }
  return out;
});
await browser.close();
writeFileSync("src/wordmark.ts", `// "nyq" in Geist 600 as a dot grid, coverage 0 to 9 per cell. Made by scripts/wordmark.mjs.\nexport const WORDMARK = ${JSON.stringify(cells, null, 2)};\n`);
console.log(cells.length, "rows x", cells[0].length, "cols");
console.log(cells.join("\n").replace(/0/g, " "));
