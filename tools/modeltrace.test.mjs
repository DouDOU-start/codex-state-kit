import assert from "node:assert/strict";
import crypto from "node:crypto";
import fs from "node:fs";
import test from "node:test";
import { analyzeGlobalOutputs, parseNumbers } from "../frontend/src/modeltrace/fingerprint-core.js";

const bankPath = new URL("../frontend/src/modeltrace/unified_bank.json", import.meta.url);
const scorerPath = new URL("../frontend/src/modeltrace/fingerprint-core.js", import.meta.url);
const provenance = JSON.parse(fs.readFileSync(new URL("../frontend/src/modeltrace/provenance.json", import.meta.url), "utf8"));
const bank = JSON.parse(fs.readFileSync(bankPath, "utf8"));

function sha256(url) {
  return crypto.createHash("sha256").update(fs.readFileSync(url)).digest("hex");
}

function fixedOutput() {
  return Array.from({ length: 300 }, (_, index) => ((index * 47 + 13) % 355) + 1).join(",");
}

test("vendored ModelTrace assets match recorded upstream hashes", () => {
  assert.equal(sha256(bankPath), provenance.bankSha256);
  assert.equal(sha256(scorerPath), provenance.scorerSha256);
});

test("number parser accepts the longest valid run and rejects out-of-range values", () => {
  assert.deepEqual(parseNumbers("1 2 356 3 abc 4 5 6"), [1, 2, 3]);
  assert.deepEqual(parseNumbers("356 0 400"), []);
});

test("fixed sample matches the calibrated scorer contract", () => {
  const text = fixedOutput();
  const result = analyzeGlobalOutputs(
    [1, 2, 3].map(() => ({ text, expected_count: 300 })),
    bank,
  );
  assert.equal(result.prediction, "gpt-5.6-sol");
  assert.equal(result.used_outputs, 3);
  assert.ok(Math.abs(result.probability - 0.6068677137396103) < 1e-12);
  assert.ok(Math.abs(result.results.reduce((sum, item) => sum + item.probability, 0) - 1) < 1e-12);
  assert.ok(Math.abs(result.family_probabilities.reduce((sum, item) => sum + item.probability, 0) - 1) < 1e-12);
});

test("scorer reports no usable output for refusal or truncation", () => {
  assert.throws(
    () => analyzeGlobalOutputs([{ text: "I cannot help with that", expected_count: 300 }], bank),
    /没有可用回答/,
  );
  const partial = Array.from({ length: 80 }, (_, index) => (index % 355) + 1).join(",");
  const result = analyzeGlobalOutputs([{ text: partial, expected_count: 10 }], bank);
  assert.equal(result.used_outputs, 1);
});
