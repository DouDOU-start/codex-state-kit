# Localization / 本地化 / Локализация

[简体中文](../README.md) · [Русский](../README.ru.md)

## Scope

The title-bar language selector switches the application-owned frontend text
and native tray between Simplified Chinese (`zh-CN`) and Russian (`ru`). Chinese
remains the default for existing installations and unknown saved preferences.
The preference is stored under `codex-state-kit.language` in the webview's local
storage. With storage disabled, switching still works until the next launch.

This is a presentation setting, separate from the virtual device's environment
locale and timezone. It does not change requests, credentials, model IDs,
account names, proxy URLs or user-defined node names. Raw backend/upstream
diagnostic details and logs remain verbatim; their UI headings are translated.
The desktop frontend synchronizes the tray with `set_ui_language` on mount and
on changes; the native menu uses Chinese until that initial synchronization.
Existing one-shot action results retain their original language. Active
state-driven notices are rendered in the selected language without reopening
dismissed notices or resetting dismissal timers.

## Messages

- `frontend/src/lib/i18n.ts`: supported locales, persistence, subscriptions and
  `t(source, values)` interpolation. Unknown messages fall back to the source.
- `frontend/src/locales/ru.ts`: Russian translations keyed by Chinese source
  messages. Keeping source messages as keys preserves the upstream wording and
  avoids a second, duplicated Chinese dictionary.
- `frontend/src/hooks/useLocale.ts`: React subscription. Localized components
  subscribe without changing their keys or remounting forms.
- `src-tauri/src/ui_language.rs`: validated, presentation-only native language.
- `src-tauri/src/tray.rs`: native tray strings, with unchanged menu IDs/actions.

Use whole messages with positional placeholders for variable text:

```tsx
t("共 {0} 条", [total])
```

Translate option labels at render time rather than module initialization.
Do not translate protocol values or arbitrary user data. Format dates/numbers
with `getLocale()`; amounts stay in USD regardless of UI language. To add a
language, extend `Locale`, validation/persistence, the selector, dictionaries,
native labels and tests together. No i18n runtime dependency is required.

## Verification

```sh
node --test tools/i18n.test.mjs
corepack pnpm build:renderer
cargo check --workspace --locked
cargo test -p codex-state-kit-desktop ui_language::tests --lib --locked
```

In browser preview (`corepack pnpm dev:renderer`), check all six tabs, the login
dialog, pagination, refresh choices, confirmations and notices. Switch using
the keyboard, retain a search query while switching, reload to confirm
persistence, and switch back to Chinese. Check the desktop minimum size of
900 × 680: translated tabs must wrap rather than overlap the account selector.
The browser cannot validate native tray rendering; check that separately in a
desktop session with a disposable Codex configuration.
