#!/usr/bin/env node
// Checks the design gallery against the design system.
//
//   node design/check.mjs                 # checks design.html
//   node design/check.mjs path/to/file    # checks another gallery
//
// Fails when a card claims a size it does not render, when window content does
// not start on the 16px gutter, or when a window width is not 480 / 760 / 984.
// Values come from design/design-system.yaml; read design/DESIGN.md first.

import { existsSync, readFileSync, readdirSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const target = resolve(process.argv[2] ?? join(here, "..", "design.html"));
const manifestPath = join(here, "design-system.yaml");
const GUTTER = 16;
const TITLE_X = 50; // gutter + 24 icon + 10 gap
const WIDTHS = [480, 760, 984];

async function loadPlaywright() {
  for (const spec of ["playwright", "playwright-core"]) {
    try {
      return await import(spec);
    } catch {}
  }
  const cache = join(homedir(), ".npm", "_npx");
  if (existsSync(cache)) {
    for (const dir of readdirSync(cache).reverse()) {
      for (const file of ["node_modules/playwright/index.mjs", "node_modules/playwright-core/index.mjs"]) {
        const path = join(cache, dir, file);
        if (existsSync(path)) return await import(pathToFileURL(path).href);
      }
    }
  }
  console.error("Playwright not found. Run: npm i -D playwright && npx playwright install chromium");
  process.exit(2);
}

const chrome = [
  process.env.CHROME_PATH,
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  "/usr/bin/google-chrome",
].find((path) => path && existsSync(path));

const { chromium } = await loadPlaywright();
const browser = await chromium.launch(chrome ? { executablePath: chrome } : {});
const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
await page.goto(pathToFileURL(target).href, { waitUntil: "networkidle" });
await page.evaluate(() => document.fonts.ready);

const result = await page.evaluate(({ GUTTER, TITLE_X, WIDTHS }) => {
  const problems = [];
  const cards = [...document.querySelectorAll("figure.card")];
  const push = (card, problem) => problems.push(`${card}: ${problem}`);

  for (const fig of cards) {
    const label = fig.querySelector("figcaption b")?.textContent.trim() ?? "(no caption)";
    const claim = fig.querySelector("figcaption span")?.textContent.trim().replace(/\s+/g, " ");
    const win = fig.querySelector(".win");
    if (!win) continue;

    const box = win.getBoundingClientRect();
    const rendered = `${Math.round(box.width)} × ${Math.round(box.height)}`;
    if (claim && claim !== rendered) push(label, `caption says ${claim}, renders ${rendered}`);
    if (!WIDTHS.includes(Math.round(box.width))) push(label, `window width ${Math.round(box.width)} is not one of ${WIDTHS.join("/")}`);

    const border = parseFloat(getComputedStyle(win).borderLeftWidth) || 0;
    const inner = box.left + border;
    const x = (el) => {
      if (!el) return null;
      const style = getComputedStyle(el);
      return +(el.getBoundingClientRect().left + parseFloat(style.paddingLeft) - inner).toFixed(1);
    };
    const at = (el, want, what) => {
      const got = x(el);
      if (got === null) return;
      if (Math.abs(got - want) > 0.5) push(label, `${what} starts at ${got}, expected ${want}`);
    };

    at(win.querySelector(".bar .field"), GUTTER, "search text");
    at(win.querySelector(".row .ico"), GUTTER, "icon box");
    at(win.querySelector(".err"), GUTTER, "error text");
    at(win.querySelector(".confirm"), GUTTER, "confirm text");
    at(win.querySelector(".foot span"), GUTTER, "footer text");

    // the preview pane is right-aligned: its own gutter is 16 inside a 280 pane
    const preview = win.querySelector(".preview");
    if (preview) {
      const pane = preview.getBoundingClientRect();
      const width = +(box.right - border - pane.left).toFixed(1);
      if (Math.abs(width - 280) > 0.5) push(label, `preview pane is ${width} wide, expected 280`);
      const pad = parseFloat(getComputedStyle(preview).paddingLeft);
      if (pad !== GUTTER) push(label, `preview padding is ${pad}, expected ${GUTTER}`);
    }

    const rows = [...win.querySelectorAll(".row")];
    const firstWithIcon = rows.find((row) => row.querySelector(".ico"));
    if (firstWithIcon) at(firstWithIcon.querySelector(".title"), TITLE_X, "title after an icon");
    const firstTextOnly = rows.find((row) => !row.querySelector(".ico"));
    if (firstTextOnly) at(firstTextOnly, GUTTER, "text-only row content");

    // right edge of the footer count
    const count = win.querySelector(".foot .count");
    if (count) {
      const right = +(box.right - border - parseFloat(getComputedStyle(count).paddingRight) - count.getBoundingClientRect().right).toFixed(1);
      if (Math.abs(right - GUTTER) > 0.5) push(label, `footer count ends ${right} from the edge, expected ${GUTTER}`);
    }

    // a filmstrip keeps 16 of air above and below its focused frame
    for (const strip of win.querySelectorAll(".strip")) {
      const focused = strip.querySelector(".frame.on");
      if (!focused) continue;
      const band = strip.getBoundingClientRect();
      const frame = focused.getBoundingClientRect();
      const top = +(frame.top - band.top).toFixed(1);
      const bottom = +(band.bottom - frame.bottom).toFixed(1);
      if (Math.abs(top - GUTTER) > 0.5) push(label, `focused frame sits ${top} from the band top, expected ${GUTTER}`);
      if (Math.abs(bottom - GUTTER) > 0.5) push(label, `focused frame sits ${bottom} from the band bottom, expected ${GUTTER}`);
    }

    // component invariants
    const bar = win.querySelector(".bar");
    const foot = win.querySelector(".foot");
    if (bar && Math.round(bar.getBoundingClientRect().height) !== 52) push(label, `bar is ${Math.round(bar.getBoundingClientRect().height)}, expected 52`);
    if (foot && Math.round(foot.getBoundingClientRect().height) !== 22) push(label, `footer is ${Math.round(foot.getBoundingClientRect().height)}, expected 22`);
    for (const row of rows) {
      const h = Math.round(row.getBoundingClientRect().height);
      if (h !== 44) push(label, `row "${row.textContent.trim().split("\n")[0]}" is ${h}, expected 44`);
    }
  }
  const rendered = cards.map((fig) => {
    const label = fig.querySelector("figcaption b")?.textContent.trim() ?? "";
    const win = fig.querySelector(".win");
    const box = win?.getBoundingClientRect();
    return { label, size: box ? `${Math.round(box.width)}x${Math.round(box.height)}` : "" };
  });
  return { problems, cards: cards.length, rendered };
}, { GUTTER, TITLE_X, WIDTHS });

// the states inventory in design-system.yaml must describe the cards that exist
if (existsSync(manifestPath)) {
  const yaml = readFileSync(manifestPath, "utf8");
  const listed = new Map();
  for (const m of yaml.matchAll(/-\s*\{\s*card:\s*"([^"]+)",\s*size:\s*\[\s*(\d+)\s*,\s*(\d+)\s*\]/g)) {
    listed.set(m[1], `${m[2]}x${m[3]}`);
  }
  if (listed.size) {
    const seen = new Set();
    for (const { label, size } of result.rendered) {
      seen.add(label);
      if (!listed.has(label)) result.problems.push(`${label}: not listed in design-system.yaml states`);
      else if (listed.get(label) !== size) result.problems.push(`${label}: yaml says ${listed.get(label)}, renders ${size}`);
    }
    for (const [label] of listed) {
      if (!seen.has(label)) result.problems.push(`${label}: listed in design-system.yaml states but missing from the gallery`);
    }
  }
}

await browser.close();

if (result.problems.length) {
  console.error(`✗ ${result.problems.length} problem(s) across ${result.cards} cards\n`);
  for (const line of result.problems) console.error("  " + line);
  process.exit(1);
}
console.log(`✓ ${result.cards} cards: captions, gutters and row metrics match design/design-system.yaml`);
