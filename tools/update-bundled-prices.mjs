// Refresh src/pricing_catalog.json from the sub2api model price repo.
//
// The app downloads the same catalog at runtime; this bundled copy is the
// offline fallback. Only OpenAI chat/responses models and the price fields the
// billing code reads are kept, so the file stays small and reviewable.
//
// Usage: node tools/update-bundled-prices.mjs
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

async function fetchText(url) {
  const response = await fetch(url);
  if (!response.ok) throw new Error(`${url}: HTTP ${response.status}`);
  return response.text();
}

const [raw, hashFile] = await Promise.all([fetchText(`${BASE}.json`), fetchText(`${BASE}.sha256`)]);
const expected = hashFile.trim().split(/\s+/)[0].toLowerCase();
const actual = createHash("sha256").update(raw).digest("hex");
if (expected !== actual) throw new Error(`sha256 mismatch: expected ${expected}, got ${actual}`);

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
writeFileSync(out, `${JSON.stringify({ source_sha256: actual, models: kept }, null, 1)}\n`);
console.log(`wrote ${Object.keys(kept).length} models to ${path.relative(root, out)} (sha256 ${actual.slice(0, 12)}…)`);
