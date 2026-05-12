# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Kivio (formerly KeyLingo through v2.4.4) is a lightweight desktop **screen-level AI assistant** built with **Tauri v2** (Rust backend) and **React 18 + Vite + TailwindCSS v4** (frontend). It runs on macOS and Windows and provides global hotkey-triggered text translation, screenshot OCR/translation, and a Lens overlay for capture-then-ask vision Q&A — all via OpenAI-compatible APIs.

## Common Commands

Use `npm` (lockfile is `package-lock.json`). Rust tooling is managed by Tauri.

- `npm install` — install Node dependencies.
- `npm run dev` — run the full Tauri app (Rust backend + Vite UI). This is the standard dev command.
- `npm run dev:ui` — run the Vite UI dev server only (useful for quick UI iteration without compiling Rust).
- `npm run build` — build the full desktop app bundle via Tauri.
- `npm run build:ui` — build the production UI bundle only (outputs to `dist/`).
- `npm run preview` — preview the built UI bundle locally.
- `npm run lint` — run ESLint on `.ts` and `.tsx` files.
- `npm run typecheck` — run `tsc --noEmit` for strict TypeScript checks.
- `cargo test --manifest-path src-tauri/Cargo.toml` — run Rust unit tests.

There is no frontend unit/e2e test runner configured. Manual smoke testing is required after changes that affect app flows.

## Architecture

### Frontend-Backend Communication

All Tauri `invoke` calls and event listeners are centralized in **`src/api/tauri.ts`**. This is the single source of truth for the frontend-backend contract. When adding new Rust commands, expose them here first.

Key patterns:
- `api.translateText(text)` — debounced 600ms in `App.tsx`.
- `api.commitTranslation(text)` — copies to clipboard, hides window, optionally sends paste shortcut to the previous app.
- `api.closeWindow()` — calls `win.hide()` rather than destroying the window; both `main` and `lens` windows are reused across hotkey triggers.

### Window Modes and Routing

The app uses **two webview windows**:
- **`main`** — translator (default, `392×152`) and Settings panel; switches view via `window.location.hash` (`''` → translator, `'#settings'` → Settings).
- **`lens`** — fullscreen transparent overlay for capture + chat. Created on first hotkey trigger via `ensure_lens_window` in `src-tauri/src/windows.rs`. Subroute via hash query: `#lens` (chat mode, default) vs `#lens?mode=translate` (screenshot translate mode); both modes share the same component (`Lens.tsx`) which reads the query in `readModeFromHash`.

`App.tsx` reads the hash to determine the mode and resizes the main window accordingly. Window behavior and bundle targets are configured in **`src-tauri/tauri.conf.json`**. The capabilities allowlist (`src-tauri/capabilities/default.json`) must contain every webview label any plugin permission applies to (currently `["main", "lens"]`).

### Settings UI Submodules

The settings panel (`src/Settings.tsx`) delegates to helpers in **`src/settings/`**:
- `components.tsx` — reusable UI primitives (Toggle, Select, HotkeyRecorder, etc.).
- `i18n.ts` — bilingual string table (zh/en).
- `utils.ts` — hotkey parsing/formatting and platform detection.

### Multi-Provider System

The app supports multiple OpenAI-compatible providers. Each feature can use a different provider/model:
- **Translator** (`translatorProviderId` + `translatorModel`)
- **Screenshot Translation/OCR** (`screenshotTranslation.providerId` + `model`)
- **Lens** (`lens.providerId` + `lens.model`; both blank ⇒ falls back to translator provider/model)

Providers have `availableModels` (fetched from `/models` endpoint) and `enabledModels` (user-selected subset used in dropdowns). Model selection UI uses colon-delimited values like `providerId:modelName`.

Each provider stores `apiKeys: string[]` (a pool of keys for failover), not a single key. The first entry is the primary; subsequent entries are backups.

### Multi-Key Failover

When a request fails with a quota/rate-limit/auth error, the backend automatically rotates to the next configured key for that provider. Implementation lives across `src-tauri/src/api.rs` and `src-tauri/src/state.rs`:

- `AppState.key_cooldowns` — `(provider_id, key_idx) → Instant` map; failed keys are cooled down for `KEY_COOLDOWN` (60s) before being eligible again.
- `AppState.active_key_idx` — last-known-good idx per provider; subsequent calls start from this idx.
- `send_with_failover(state, label, attempts, provider_id, api_keys, send)` — wraps `send_with_retry`. The `send` closure takes a `&str` (the current key) so the same body builder is reused across keys.
- `is_failover_error(err_msg)` — pattern-matches on HTTP status parsed from the error string. Only 401/402/403/429 trigger key rotation; malformed requests and server/network failures do not burn backup keys.
- Non-failover errors (timeouts, 5xx) still go through `send_with_retry` exponential backoff and don't burn keys.
- `test_provider_connection` deliberately uses only the first key (so users see whether their primary configuration is correct without hidden fallback masking issues).

### Settings Persistence and Security

- Settings are stored via `tauri-plugin-store` in `settings.json`, **including API keys** (in the `providers[].apiKeys` array).
- Older versions (≤ v2.3.x) stored keys in the OS keyring. On first launch under v2.4+, `migrate_legacy_keyring_keys` reads any leftover keyring entries into `settings.api_keys[0]` and deletes the keyring entry. From then on, the keyring is never written.
- The `keyring` crate dependency is retained only for that one-shot migration path and can be removed once all users have upgraded.
- **`sanitize_settings`** in `src-tauri/src/settings.rs` handles migration from legacy single-provider configs to the multi-provider system, validates provider existence, and normalizes hotkeys. It also migrates the legacy single `apiKey` field on each `ModelProvider` (read via the `api_key_legacy` field with `#[serde(rename = "apiKey")]`) into `api_keys[0]`. `normalize_hotkey` canonicalizes modifier aliases to `CommandOrControl`, `Control`, `Alt`, `Shift`, `Super` — use these exact strings when constructing hotkeys.
- Saving settings is transactional: if hotkey registration fails, `restore_runtime_settings` rolls back to the previous state.

### Screenshot Capture (macOS / Windows)

Capture is platform-guarded with `cfg(target_os = ...)`:

- **macOS** — `src-tauri/src/sck.rs` uses ScreenCaptureKit (`screencapturekit` crate, `macos_14_0` feature). No `screencapture` shell-out.
- **Windows** — `xcap` crate captures full-screen / window content (the dependency is `cfg`-gated to Windows in `Cargo.toml`).

Both platforms route through the **Lens overlay** (`Lens.tsx`): the overlay presents hover-highlighted app windows or a draggable region; user click / drag commits via `lens_capture_window` / `lens_capture_region` Tauri commands. The capture commands receive logical-pixel coordinates from the overlay and call the platform-specific module to produce a PNG in `temp_dir`.

A single busy flag (`AppState.lens_busy`, `AtomicBool`) prevents concurrent overlays. `lens_request_internal` swaps it true on entry; `lens_close` resets it. A reactive self-heal in `lens_request_internal` clears a stale flag if the previous run leaked it (e.g. on panic).

### Rust Backend Structure

The backend has been migrated to the upstream v2.6.0 split-module layout. Keep new work in the focused module instead of putting large command bodies back into `main.rs`.

- **`main.rs`** - Tauri builder setup, plugin registration, app state initialization, startup update check, and `generate_handler!` command registration only.
- **`commands.rs`** - general app commands: settings load/save, text translation, commit/paste, provider/model fetching, permission checks, RapidOCR status/install, Apple Intelligence availability, and selection handoff commands.
- **`shortcuts.rs`** - global hotkey registration, tray setup, hotkey error serialization/localization, selected-text capture, main/settings window activation, and runtime hotkey rollback helpers.
- **`lens_commands.rs`** - Lens command surface: request/select flow, screenshot capture, Lens chat/translation commands, floating-window sizing/animation commands, history image persistence, and image path resolution.
- **`updates.rs`** - GitHub release check, update asset download, progress events, and installer launch/quit flow.
- **`api.rs`** - HTTP client setup, provider credential resolution, retry/failover, OpenAI-compatible text/OCR/vision calls, `/chat/completions`/`/responses`/`/messages` routing, and SSE stream parsing.
- **`browser_automation.rs`** - local browser bridge commands and generated Chrome extension support used by browser automation.
- **`state.rs`** - `AppState`, lock helpers, Lens runtime state, pending selection state, and multi-key cooldown / active-key selection.
- **`settings.rs`** - Settings schema, serde defaults, `sanitize_settings` migration/validation, one-shot `migrate_legacy_keyring_keys`, and prompt defaults.
- **`screenshot.rs`** - Temp PNG cleanup helpers (`cleanup_temp_file` for one-shot, `cleanup_orphan_temp_files` for app-startup GC of stale `lens-*.png` / `screenshot-*.png` older than 24 h).
- **`sck.rs`** - macOS-only ScreenCaptureKit wrapper invoked by Lens capture commands.
- **`lens.rs`** - Lens window enumeration and platform capture helpers.
- **`native_freeze.rs`** - Windows native frozen-screen overlay support retained from local work.
- **`windows.rs`** - Window helpers: `ensure_main_window`, `ensure_lens_window`, `get_main_window`, plus `apply_macos_workspace_behavior` for `visibleOnAllWorkspaces`.
- **`utils.rs`** - Language detection, target language resolution, timestamp helper.

Key crate responsibilities from `Cargo.toml`:
- `enigo` — simulates keyboard paste after translation commit.
- `arboard` — clipboard read/write.
- `keyring` — legacy API key storage (read-only; v2.4+ stores keys in `settings.json`, `keyring` is retained only for one-shot migration of pre-v2.4 installs).
- `reqwest` — HTTP client for OpenAI-compatible APIs.
- `screencapturekit` — macOS ScreenCaptureKit binding (used by `sck.rs`).
- `xcap` — Windows screen / window capture.

### Streaming

Lens supports streaming responses via two SSE-relay event channels emitted by stream helpers in `api.rs`:
- `lens-stream` — chat answers; deltas accumulate into the last assistant message in `Lens.tsx`. Supports `delta.reasoning_content` for reasoning-mode models.
- `lens-translate-stream` — screenshot translate; emits `kind="translated"` deltas, then a `<<<ORIGINAL>>>` separator, then `kind="original"` deltas. Frontend splits the stream into translation (top) + original (small grey reference, bottom).

Cancellation is via `AppState.explain_stream_generation` (`AtomicU64`) — each new stream snapshots its generation; the inner chunk loop bails when the global moves past it.

## Release

Releases are built via GitHub Actions (`.github/workflows/release.yml`). Pushing a `v*` tag triggers builds for:
- **macOS** — DMG bundle (`--bundles dmg`)
- **Windows** — MSI + NSIS bundles (`--bundles msi,nsis`)

Manual releases are also supported via `workflow_dispatch`.

## Code Style

- TypeScript + React, ESM (`"type": "module"`).
- 2-space indentation, single quotes, no semicolons.
- Components use `PascalCase.tsx`; utilities/services use `camelCase.ts`.
- Tailwind utility classes for UI; shared styles in `src/index.css`, component-specific in `src/App.css`.
- Dark mode uses a `.dark` class on `document.documentElement` (configured via `@custom-variant dark` in Tailwind v4).
- Git commits follow Conventional Commits (`feat:`, `fix:`, `refactor:`, `chore:`).

## Important Implementation Details

- **macOS**: The app hides its Dock icon (`ActivationPolicy::Accessory`) and uses `visibleOnAllWorkspaces` for all windows.
- **Windows**: Manual launch opens settings by default. Autostart uses a dedicated `--from-autostart` arg to avoid popping up settings. Single-instance guard ensures clicking the app icon focuses the existing instance.
- **LaTeX math**: Both screenshot result and explain use `react-markdown` + `remark-math` + `rehype-katex` for rendering LaTeX formulas.
- **Prompt templates**: Default prompts and prompt composition live in Rust (`prompts.rs` plus defaults exposed through `get_default_prompt_templates`). Custom prompts support `{lang}` and `{text}` placeholders.

## Current Work Handoff

Current branch: `codex/upstream-2.6-migration`. The upstream `ZMGID/kivio` v2.6.0 refactor has been migrated into this worktree, and the resolved changes are staged. A safety stash remains available as `stash@{0}: codex-pre-upstream-2.6-migration`.

### Completed in this migration

- Merged the upstream v2.6.0 layout: `main.rs` is now a slim app entry, backend commands are split into `commands.rs`, `shortcuts.rs`, `lens_commands.rs`, and `updates.rs`.
- Split Lens frontend helpers into `src/lens/ArrowSvg.tsx`, `annotation.ts`, `history.ts`, `layout.ts`, `markdown.ts`, `ThinkingBlock.tsx`, and `types.ts`.
- Kept local provider compatibility work in `src-tauri/src/api.rs`: base URLs, direct `/chat/completions`, direct `/responses`, and direct Anthropic-style `/messages` provider endpoints should remain supported.
- Replaced native web-search tools with Tavily function calling. When `settings.tavily_api_key` is configured, `call_vision_api` can inject a `tavily_web_search` tool for Chat Completions, Messages, and Responses endpoints.
- Kept browser automation support via `src-tauri/src/browser_automation.rs`, Tauri command registration in `main.rs`, and frontend bridge methods in `src/api/tauri.ts`.
- Kept main translator selected-text handoff: `shortcuts.rs` captures selection before showing the translator, `commands.rs` exposes `take_translator_selection`, and `App.tsx` consumes `api.takeTranslatorSelection()` to prefill and auto-translate.
- Adopted upstream macOS Lens native floating animation via `lens_animate_floating` / `api.lensAnimateFloating`.
- Kept Lens history sizing logic and reasoning display through the new `ThinkingBlock` component.
- Restored normal streaming for ordinary Lens questions with a Tavily key present by only probing Tavily when `query_likely_needs_web_search(...)` detects an explicit or time-sensitive web query.
- Fixed Windows Lens floating mode so `lens_set_floating` clears the interactive region and uses the native small-window path; `Lens.tsx` also has an explicit drag handle that calls `api.startDragging()`.
- Fixed Responses + Tavily second-turn compatibility: do not send `previous_response_id`; replay only the original input, the first response `function_call`, and the `function_call_output`.
- Fixed Responses reasoning leakage: `response.reasoning_summary_text.*`, `summary_text`, and reasoning `content_part.done` events are routed to reasoning or ignored, never appended to the normal answer body.

### Validation and package output

These commands passed recently:

- `npm run typecheck`
- `cargo check --manifest-path src-tauri/Cargo.toml`
- `npm run lint`
- `cargo test --manifest-path src-tauri/Cargo.toml` (latest run: 56 tests passed)
- `npx tauri build --bundles nsis`

Latest Windows NSIS package:

- `D:\Desktop\kivio\src-tauri\target\release\bundle\nsis\Kivio_2.6.0_x64-setup.exe`

Latest release executable:

- `D:\Desktop\kivio\src-tauri\target\release\kivio.exe`

Full `npm run build` currently builds the release exe but fails at the MSI WiX `light` step with `拒绝访问。 (os error 5)`. Any existing MSI under `src-tauri\target\release\bundle\msi\` may be stale until that permission/lock issue is fixed. Rust build currently has warnings only, mostly unused imports/variables and unused browser-tool helper functions. They do not block NSIS packaging.

### Important follow-up notes

- Do not move large command bodies back into `main.rs`; add new backend command logic to the focused module and only register it in `main.rs`.
- Preserve `/responses`, `/messages`, and `/chat/completions` compatibility across text, screenshot translation, and Lens flows.
- Keep Tavily tool name as `tavily_web_search`; some proxies intercept generic `web_search` and fail to extract the query.
- Keep Tavily probing limited to likely search queries so non-search Lens answers continue streaming.
- Do not reintroduce `previous_response_id` for Responses Tavily second turns unless the target server is known to support that continuation mode.
- Keep Responses stream parsing strict: reasoning events must stay out of normal answer deltas.
- Keep selectable text protected from window dragging in the main translator, Lens answer panels, and screenshot translation cards.
- Be careful with `native_freeze.rs`: upstream deleted it, but the local Windows capture/floating work may still depend on that path.
- The staged migration is intentionally not committed yet. Review staged changes before committing or dropping the safety stash.
