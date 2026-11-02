// Renders the PNG icons from public/favicon.svg. Run: node scripts/images.mjs
// The social card, public/og.png, is a 1200x630 screenshot of scripts/og.html.
import sharp from "sharp";
import { readFileSync } from "node:fs";

const icon = readFileSync("public/favicon.svg");
await sharp(icon, { density: 384 }).resize(32, 32).png().toFile("public/favicon-32.png");
await sharp(icon, { density: 768 }).resize(180, 180).png().toFile("public/apple-touch-icon.png");

console.log("wrote favicon-32.png, apple-touch-icon.png (og.png comes from scripts/og.html)");
