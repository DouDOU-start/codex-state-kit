import assert from "node:assert/strict";
import { after, test } from "node:test";
import { mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
import ts from "typescript";

// Compile the small, framework-free translation module using the project's TS.
// No test-runner dependency or real Tauri session is needed.
const output = mkdtempSync(join(tmpdir(), "codex-state-kit-i18n-"));
writeFileSync(join(output, "package.json"), '{"type":"commonjs"}\n');
const program = ts.createProgram([
  "frontend/src/lib/i18n.ts", "frontend/src/locales/ru.ts", "frontend/src/lib/accountNetwork.ts",
], { outDir: output, module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022, skipLibCheck: true });
const emitted = program.emit();
assert.equal(emitted.emitSkipped, false);
const require = createRequire(import.meta.url);
const { t, getLocale, formatNumber, setLocale, subscribeLocale, readLocale, LOCALE_STORAGE_KEY } = require(join(output, "lib/i18n.js"));
const { ru } = require(join(output, "locales/ru.js"));
const { accountNetwork } = require(join(output, "lib/accountNetwork.js"));
after(() => rmSync(output, { recursive: true, force: true }));

test("preserves Chinese by default and switches both ways", () => {
  assert.equal(getLocale(), "zh-CN");
  assert.equal(t("概览"), "概览");
  assert.equal(formatNumber(1.25, { minimumFractionDigits: 2, maximumFractionDigits: 2, useGrouping: false }), "1.25");
  setLocale("ru");
  assert.equal(t("概览"), "Обзор");
  assert.equal(formatNumber(1.25, { minimumFractionDigits: 2, maximumFractionDigits: 2, useGrouping: false }), "1,25");
  setLocale("zh-CN");
  assert.equal(t("概览"), "概览");
});

test("persists language and updates document language without changing device settings", () => {
  const storage = new Map([["device.locale", "ja-JP"]]);
  globalThis.window = { localStorage: { getItem: key => storage.get(key) ?? null, setItem: (key, value) => storage.set(key, value) } };
  globalThis.document = { documentElement: { lang: "zh-CN" } };
  setLocale("ru");
  assert.equal(storage.get(LOCALE_STORAGE_KEY), "ru");
  assert.equal(readLocale(), "ru");
  assert.equal(document.documentElement.lang, "ru");
  assert.equal(storage.get("device.locale"), "ja-JP");
  storage.set(LOCALE_STORAGE_KEY, "unsupported");
  assert.equal(readLocale(), "zh-CN");
  setLocale("unsupported");
  assert.equal(getLocale(), "ru");
  delete globalThis.window;
  delete globalThis.document;
});

test("works with denied storage and notifies/unsubscribes listeners", () => {
  globalThis.window = { get localStorage() { throw new Error("Storage denied"); } };
  let changes = 0;
  const unsubscribe = subscribeLocale(() => changes++);
  setLocale("ru");
  assert.equal(getLocale(), "ru");
  assert.equal(readLocale(), "zh-CN");
  assert.equal(changes, 1);
  unsubscribe();
  setLocale("zh-CN");
  assert.equal(changes, 1);
  delete globalThis.window;
});

test("interpolates without interpreting user values and falls back for unknown messages", () => {
  setLocale("ru");
  assert.equal(t("已重新授权 {0}", ["<test>{1}$&"]), "Повторно авторизован: <test>{1}$&");
  assert.equal(t("new message {0}", [42]), "new message 42");
  assert.equal(t("new message {0}"), "new message {0}");
  assert.equal(t("__proto__"), "__proto__");
});

test("Russian translations are nonempty and preserve every interpolation slot", () => {
  for (const [source, translated] of Object.entries(ru)) {
    assert.ok(translated.trim(), source);
    assert.deepEqual(translated.match(/\{\d+\}/g)?.sort() ?? [], source.match(/\{\d+\}/g)?.sort() ?? [], source);
  }
});

test("all literal translation calls have Russian entries and matching parameters", () => {
  for (const name of readdirSync("frontend/src", { recursive: true })) {
    if (!/\.tsx?$/.test(name)) continue;
    const file = join("frontend/src", name);
    const source = ts.createSourceFile(file, readFileSync(file, "utf8"), ts.ScriptTarget.Latest, true);
    function visit(node) {
      if (ts.isCallExpression(node) && ts.isIdentifier(node.expression) && node.expression.text === "t") {
        const [key, values] = node.arguments;
        if (key && ts.isStringLiteral(key)) {
          assert.ok(Object.hasOwn(ru, key.text), `${file}: ${key.text}`);
          const indices = [...key.text.matchAll(/\{(\d+)\}/g)].map(match => Number(match[1]));
          if (indices.length) {
            assert.ok(values && ts.isArrayLiteralExpression(values), key.text);
            assert.equal(values.elements.length, Math.max(...indices) + 1, key.text);
          }
        }
      }
      ts.forEachChild(node, visit);
    }
    visit(source);
  }
});

test("account network labels preserve node names, addresses and unknown backend formats", () => {
  setLocale("ru");
  assert.equal(accountNetwork("手动代理 · 未配置"), "Ручной прокси · Не настроен");
  assert.equal(accountNetwork("手动代理 · socks5://proxy.example:1234"), "Ручной прокси · socks5://proxy.example:1234");
  assert.equal(accountNetwork("订阅节点 · Kit → 香港 01"), "Узлы подписки · Kit → 香港 01");
  assert.equal(accountNetwork("custom · 中文"), "custom · 中文");
  setLocale("zh-CN");
  assert.equal(accountNetwork("手动代理 · 未配置"), "手动代理 · 未配置");
});
