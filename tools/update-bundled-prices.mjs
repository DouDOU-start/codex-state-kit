// Refresh src/pricing_catalog.json from the sub2api model price repo.
//
// Packaging runs this first (`tauri build`'s beforeBuildCommand), so every
// build ships the latest prices and billing works before the first runtime
// sync. The app keeps syncing the same catalog while it runs. Only OpenAI
// chat/responses models and the price fields the billing code reads are
// kept, so the file stays small and reviewable.
//
// Usage: node tools/update-bundled-prices.mjs [--optional]
//   --optional  keep the current file when the price repo cannot be reached,
//               so a network hiccup does not fail the build.
import { createHash } from "node:crypto";
import { writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const BASE = "https://raw.githubusercontent.com/Wei-Shaw/model-price-repo/main/model_prices_and_context_window";
const TIERS = ["", "_priority", "_flex"];
const PRICES = [
  "input_cost_per_token",
  "output_cost_per_token",
  "cache_read_input_token_cost",
  "cache_creation_input_token_cost",
];
// Must stay in sync with `pricing::PRICE_FIELDS` in src/pricing.rs.
const FIELDS = new Set(["litellm_provider", "mode", "long_context_input_token_threshold", "long_context_input_cost_multiplier", "long_context_output_cost_multiplier"]);
for (const price of PRICES) {
  for (const tier of TIERS) {
    FIELDS.add(`${price}${tier}`);
    FIELDS.add(`${price}_above_272k_tokens${tier}`);
  }
}

const optional = process.argv.includes("--optional");

async function fetchText(url) {
  let lastError;
  for (let attempt = 1; attempt <= 3; attempt += 1) {
    try {
      const response = await fetch(url, { signal: AbortSignal.timeout(30_000) });
      if (!response.ok) throw new Error(`${url}: HTTP ${response.status}`);
      return await response.text();
    } catch (error) {
      lastError = error;
      if (attempt < 3) await new Promise((resolve) => setTimeout(resolve, attempt * 2000));
    }
  }
  throw lastError;
}

async function download() {
  // Fetch the hash first: a publish between the two requests then shows up
  // as a mismatch instead of pairing new prices with an old hash.
  const hashFile = await fetchText(`${BASE}.sha256`);
  const raw = await fetchText(`${BASE}.json`);
  const expected = hashFile.trim().split(/\s+/)[0].toLowerCase();
  const actual = createHash("sha256").update(raw).digest("hex");
  if (expected !== actual) throw new Error(`sha256 mismatch: expected ${expected}, got ${actual}`);
  return { raw, actual };
}

let downloaded;
try {
  downloaded = await download();
} catch (error) {
  if (!optional) throw error;
  console.warn(`[prices] 无法下载最新价格，沿用仓库里的内置价格：${error.message ?? error}`);
  process.exit(0);
}
const { raw, actual } = downloaded;

const catalog = JSON.parse(raw);
const kept = {};
for (const name of Object.keys(catalog).sort()) {
  const entry = catalog[name];
  if (!entry || typeof entry !== "object") continue;
  if (entry.litellm_provider !== "openai" || !["chat", "responses"].includes(entry.mode)) continue;
  if (typeof entry.input_cost_per_token !== "number" || typeof entry.output_cost_per_token !== "number") continue;
  kept[name] = Object.fromEntries(Object.entries(entry).filter(([key]) => FIELDS.has(key)).sort(([a], [b]) => a.localeCompare(b)));
}

const out = path.join(root, "src", "pricing_catalog.json");
const fetchedAt = new Date().toISOString();
writeFileSync(out, `${JSON.stringify({ source_sha256: actual, fetched_at: fetchedAt, models: kept }, null, 1)}\n`);
console.log(`wrote ${Object.keys(kept).length} models to ${path.relative(root, out)} (sha256 ${actual.slice(0, 12)}…)`);
