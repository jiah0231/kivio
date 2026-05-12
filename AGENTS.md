# Kivio Agent Guidelines

Kivio (formerly KeyLingo through v2.4.4) is a lightweight desktop screen-level AI assistant for macOS and Windows. Its core focus is a small package size and low runtime footprint, providing instant text translation, screenshot translation, and AI-powered visual Q&A through global shortcuts.

## Tech Stack & Architecture

- **Frontend**: React 18 + TypeScript + Vite + TailwindCSS v4 (ESM)
- **Backend**: Rust + Tauri v2
- **Package Manager**: npm (lockfile: `package-lock.json`)
- **Build Targets**: macOS (DMG), Windows (MSI + NSIS)

The app uses a classic Tauri split architecture: a single-page React frontend invokes Rust backend commands via Tauri's `invoke` bridge. The backend handles global shortcuts, window management, system tray, screenshot capture, HTTP API calls, and settings persistence.

## Project Directory Structure

```
src/                          # Frontend React + TypeScript source
  main.tsx                    # React entry point (mounts to #root)
  App.tsx                     # Root component: switches views by URL hash
  Settings.tsx                # Settings page main component
  Lens.tsx                    # Lens (screenshot translation + AI vision Q&A)
  api/tauri.ts                # Tauri bridge: all invoke calls & event listeners centralized
  settings/                   # Settings UI helper modules
    components.tsx            # Reusable form components (Toggle, Select, Input, etc.)
    i18n.ts                   # Internationalization strings & language utilities
    utils.ts                  # Settings page utilities (hotkey formatting, platform detection)
  index.css                   # Global styles (Tailwind imports, scrollbar, transparent window)
  App.css                     # Component-specific styles

src-tauri/
  src/                        # Rust backend source
    main.rs                   # App entry, Tauri commands, hotkeys, tray, window lifecycle
    api.rs                    # HTTP client, retry/failover, OpenAI-compatible calls, SSE
    state.rs                  # AppState and key failover runtime state helpers
    prompts.rs                # Default and composed prompt templates
    apple_intelligence.rs     # macOS Apple Intelligence sidecar client
    lens.rs                   # Lens window enumeration and screenshot capture
    native_freeze.rs          # Windows native frozen-screen overlay for Lens capture
    screenshot.rs             # Screenshot capture utilities and temp file cleanup
    sck.rs                    # ScreenCaptureKit integration (macOS 14+)
    settings.rs               # Settings data structures, serialization, migration
    windows.rs                # Window creation & retrieval helpers
    utils.rs                  # Language detection, timestamps, etc.
  tauri.conf.json             # Tauri app config (windows, bundling, icons)
  Cargo.toml                  # Rust dependencies
  icons/                      # App icon assets

public/                       # Static assets (icons, SVGs)
.github/workflows/release.yml # GitHub Actions automated release workflow
```

## Core Module Details

### Frontend View Routing (Hash-based)

`App.tsx` switches modes via `window.location.hash`, mapping to different windows/views:

- `''` or `'translator'`: Main translator window (392x152, floating input)
- `'settings'`: Settings page (640x520)
- `'lens'`: Lens window (600x72 select mode / 600x420 answering mode, floating)

### Rust Backend State (`AppState`)

Defined in `state.rs`, the global shared state includes:

- `settings: RwLock<Settings>` — App settings (multiple readers, single writer)
- `explain_images: Mutex<HashMap<String, PathBuf>>` — Temporary image map for Lens
- `current_explain_image_id: Mutex<Option<String>>` — Currently active Lens image
- `lens_busy: AtomicBool` — Concurrency guard for Lens operations
- `explain_stream_generation: AtomicU64` — Stream cancellation token
- `key_cooldowns: Mutex<HashMap<(String, usize), Instant>>` — API key failover cooldown tracking
- `active_key_idx: Mutex<HashMap<String, usize>>` — Currently active API key index per provider
- `http: Client` — Shared HTTP client for API calls

### Settings Persistence & Security

- Settings body is stored in Tauri Store as `settings.json`
- **API Keys are stored directly in `settings.json`** (as of v2.4.0); the `keyring` crate is only used for one-time migration from legacy keyring storage
- On load, `sanitize_settings` cleans data and migrates legacy single-provider configs to the multi-provider system
- `settings.rs` contains full defaults, normalization logic (hotkeys, prompts), and keyring migration helpers

### Multi-Provider Routing

The app supports separate OpenAI-compatible providers for each feature:

- **Text Translation**: `translator_provider_id` + `translator_model`
- **Screenshot Translation / OCR**: `screenshot_translation.provider_id` + `model`
- **Lens (AI Vision)**: `lens.provider_id` + `model`

Each `ModelProvider` has `id`, `name`, `base_url`, `api_keys`, `available_models`, and `enabled_models`.

Provider `base_url` values may point either to a base OpenAI-compatible URL or directly to an endpoint:

- Base URL: `https://api.example.com/v1` -> calls `/chat/completions` and `/models`
- Chat Completions URL: `https://api.example.com/v1/chat/completions`
- OpenAI Responses URL: `https://api.example.com/v1/responses`

When a provider URL ends in `/responses`, `api.rs` converts chat-style messages into Responses API `input` items, parses Responses API normal and streaming output, and resolves model fetching by trimming back to `/models`.

Lens has a `web_search_enabled` setting. It is only applied to Responses API requests, where the backend adds the web search tool and lets the model choose tool use automatically.

### API Key Failover

- Each provider stores multiple API keys (`api_keys: string[]`)
- Primary key is `api_keys[0]`, backups follow
- Backend `send_with_failover` automatically rotates to next key on 401/402/403/429 responses
- Failed key enters 60-second cooldown before retry
- **Test Connection intentionally only probes the primary key**

### Platform-Specific Handling

- **macOS**:
  - Screenshots use ScreenCaptureKit (SCK) for interactive region/window capture (macOS 14+)
  - Auto-paste uses AppleScript to send `Command+V`
  - Permission checks: Accessibility (`AXIsProcessTrustedWithOptions`) and Screen Recording (`CGPreflightScreenCaptureAccess`)
  - Cocoa / Objective-C FFI is used for `NSApplication hide:` and workspace behavior
  - Dock icon is hidden (`ActivationPolicy::Accessory`)
  - `sck.rs` handles SCScreenshotManager integration with prewarming for performance
- **Windows**:
  - Region capture still uses `xcap` for the final crop, but Lens selection can freeze the screen with `native_freeze.rs`
  - `native_freeze.rs` uses Win32 GDI to capture the desktop into a native topmost overlay so large screenshots do not need to be pushed through WebView
  - When Lens is configured not to stay fullscreen after capture, the prompt/result window uses native floating-window movement instead of CSS animation inside a fullscreen WebView
  - `lens_fly_floating` animates the small Lens window with Win32 `SetWindowPos` and `DwmFlush`, avoiding the WebView2 blank/recomposition frame caused by resizing a fullscreen WebView after an in-page fly animation
  - The frontend Lens overlay remains responsible for selection UI and final question/answer flow
  - Auto-paste uses `enigo` to simulate `Ctrl+V`

### HTTP API & Retry Logic

- Backend uses `reqwest` with a uniform 60-second timeout
- All outbound API calls (translate, OCR, vision, fetch models, test connection) go through `send_with_retry`
- Retry policy: exponential backoff for 429 / 5xx / timeout / connection errors; respects `Retry-After` headers
- Retry count is controlled by `retry_enabled` and `retry_attempts` (1-5, default 3)
- `api.rs` supports both Chat Completions SSE (`chat.completion.chunk`) and Responses API SSE events such as output text deltas/completion
- Responses API web search is Lens-only for now and is controlled by `settings.lens.web_search_enabled`

### Lens Flow

1. Hotkey triggered (`Cmd/Ctrl+Shift+G`)
2. Enter `select` mode: fullscreen overlay showing app windows (hover highlight + label on macOS)
3. User clicks a window or drags a region → capture screenshot
4. Generate `image_id`, store temp image in `explain_images` map
5. Open / reuse the `lens` window; frontend reads the image via `explain_read_image`
6. User asks a question via `lens_ask` (streaming supported through `lens-stream`)
7. While answering, the UI shows the active model name so users can see which provider/model is responding
8. Markdown answers support GFM rendering and code block copy buttons
9. Follow-up questions reuse the same `image_id` and recent messages
10. History keeps the most recent 20 records, with thumbnails in `localStorage` and images in `lens-history`
11. Supports pure-text questions without screenshot

If `keepFullscreenAfterCapture` is false, preserve the current UI but keep the motion path native: first resize/reposition Lens to a small floating window at the current prompt position, hide the rebased in-page bar only during that transition frame, then call `lens_fly_floating` to move the native window to the target anchor. Do not reintroduce the old flow where the bar flies in fullscreen CSS and `lens_set_floating` shrinks the WebView afterwards; that path can flash on Windows because WebView2 recomposes its surface.

### Screenshot Translation Flow

1. Hotkey triggered (`Cmd/Ctrl+Shift+A`) → enter Lens `translate` select mode
2. User clicks a window or drags a region → capture screenshot and register `image_id`
3. `lens_translate` handles OCR/translation and emits `lens-translate-stream`
4. If `use_system_ocr` is enabled, Apple Vision OCR runs locally and the configured provider translates text
5. If `direct_translate` is on, only translated output is shown; otherwise the card shows translated text plus original text

## Build & Development Commands

```bash
# Install dependencies
npm install

# Full dev mode (Rust backend + Vite frontend HMR)
npm run dev

# Frontend-only dev (Vite on port 5713)
npm run dev:ui

# Build full app bundle
npm run build

# Build frontend bundle only
npm run build:ui

# Lint (ESLint)
npm run lint

# Type-check TypeScript without emitting files
npm run typecheck

# Rust unit tests
cargo test --manifest-path src-tauri/Cargo.toml

# Preview built frontend bundle
npm run preview
```

Rust dependencies are managed by `cargo`; the Tauri CLI coordinates frontend and backend builds. `tauri.conf.json` defines:

- `beforeDevCommand`: `npm run dev:ui`
- `beforeBuildCommand`: `npm run build:ui`
- `devUrl`: `http://localhost:5713`
- `frontendDist`: `../dist`

On Windows, `npm run build` creates local sidecar stubs in `src-tauri/binaries/` for the macOS-only Swift helpers:

- `kivio-ai-helper-x86_64-pc-windows-msvc.exe`
- `kivio-ocr-helper-x86_64-pc-windows-msvc.exe`

These files satisfy Tauri `externalBin` validation on Windows. They are local build artifacts ignored by git and should not be committed. Release artifacts are written under `src-tauri/target/release/`, with installers under `src-tauri/target/release/bundle/msi/` and `src-tauri/target/release/bundle/nsis/`.

## Coding Style & Naming Conventions

- **Languages**: TypeScript + React frontend; standard Rust style for backend
- **Module format**: ESM (`"type": "module"`)
- **Indentation**: 2 spaces
- **Quotes**: single quotes
- **Semicolons**: omitted
- **Naming**:
  - Component files: `PascalCase.tsx`
  - Utility / service files: `camelCase.ts`
- **Styling**: prefer Tailwind utility classes; shared styles in `src/index.css`, component-specific in `src/App.css`

## Testing Strategy

- No frontend unit/e2e test runner is configured
- Always run `npm run lint`, `npm run typecheck`, and `cargo test --manifest-path src-tauri/Cargo.toml` for code changes when practical
- Manual smoke-test checklist after changes:
  1. `npm run dev` — verify the app launches
  2. Global hotkeys (translator, screenshot translation, Lens)
  3. Translation flow (input -> debounce -> result -> Enter to commit/paste)
  4. Screenshot translation / Lens windows open correctly
  5. Settings save/load and persistence across restarts
  6. Provider connection test and model fetching
  7. Unsaved-changes close guard in settings
  8. Theme switching (light/dark/system)

## Deployment & Release

- GitHub Actions workflow at `.github/workflows/release.yml`
- Triggered on `v*` tags or manual `workflow_dispatch`
- Build matrix:
  - `macos-14` -> DMG bundle
  - `windows-latest` -> MSI + NSIS bundles
- Uses `tauri-apps/tauri-action@v0` to build and publish release assets

## Security & Configuration Guidelines

- **Never commit API keys or base URLs**; they are configured through the app settings UI
- API Keys are stored directly in `settings.json` (as of v2.4.0); the `keyring` crate is only used for one-time migration from legacy keyring storage
- External URLs are validated to start with `https://` before opening (`open_external` command)
- Active explain image paths are validated to reside inside the system temp directory; history images are resolved from the app data `lens-history` directory (`resolve_explain_image_path`)
- If you add new Tauri JS permissions or capabilities, update `src-tauri/capabilities/default.json` and document defaults

## Commit & Pull Request Guidelines

- Git history follows Conventional Commits (`feat:`, `fix:`, `refactor:`, `chore:`)
- Use short, imperative subjects
- PRs should include a concise summary, testing notes, and screenshots/GIFs for UI changes

## Current Work Handoff

This section captures the latest local work so a new Codex context can continue without rediscovering the same details.

### Recent changes in this session (2026-05-10/11)

**Upstream v2.6.0 migration completed** — backend split into focused modules (`commands.rs`, `shortcuts.rs`, `lens_commands.rs`, `updates.rs`, `browser_automation.rs`). `main.rs` is now a slim entry point.

**Reasoning effort (GPT o-series thinking summary)**:
- `ModelProvider` gained `reasoning_effort: Option<String>` (low/medium/high/xhigh).
- `apply_responses_reasoning` injects `{ "effort": "<value>", "summary": "auto" }` for Responses API when set.
- Settings UI shows effort selector in provider card when endpoint is Responses API.

**Tavily web search integration** (client-side tool calling):
- `Settings.tavily_api_key` — global Tavily API key field.
- When key is set, `call_vision_api` injects a `tavily_web_search` function tool into all endpoint formats (Chat Completions / Messages / Responses).
- First request is non-streaming to detect if model calls the tool. If yes: call Tavily, inject results, send second streaming request. If no: emit the non-streaming result directly.
- `extract_tavily_tool_call_query` handles all three response formats.
- Frontend shows "Searched web: {query}" indicator with hover tooltip listing results.
- Tool name is `tavily_web_search` (not `web_search`) to avoid proxy interception.

**Authentication fix for Claude Messages endpoint**:
- `claude_auth_headers` changed from `x-api-key` to `Bearer` auth — third-party proxies (Sub2API, XTOKEN) only accept Bearer tokens.
- `anthropic-version: 2023-06-01` header retained.

**Connection test / model fetch fix**:
- `test_provider_connection` and `fetch_models` now use `models_url_from_provider_url` to correctly strip endpoint suffixes before appending `/models`.
- Claude Messages endpoints use Bearer + `anthropic-version` header for model listing.

**HTTP client timeout fix**:
- Removed global 60s `timeout` that killed SSE streams mid-response.
- Now uses `connect_timeout: 30s` + `read_timeout: 300s`.

**Lens model name display restored**:
- `formatLensAsking` + `activeLensModel` state restored from pre-migration code.
- Shows current model name in "正在回答..." indicator.

**Lens streaming / Tavily / Responses fixes after packaging tests**:
- Ordinary Lens questions now stream again when a Tavily API key is configured. Tavily probing only runs when `query_likely_needs_web_search(...)` detects an explicit search or time-sensitive query.
- Tavily Responses second-turn requests no longer send `previous_response_id`, because some OpenAI-compatible proxies reject it with `previous_response_id is only supported on Responses WebSocket v2`.
- Tavily Responses second-turn replay now copies only the `function_call` item from the first response, not reasoning/message output. This prevents first-turn reasoning summaries from being fed back into the second turn.
- Responses final text parsing now only appends true suffixes from `*.done` events, so completed text is not appended twice.
- Responses SSE parsing now classifies `response.reasoning_summary_text.*`, `summary_text`, and reasoning `content_part.done` events as reasoning, not answer text. This fixes the bug where the thinking summary appeared both in the ThinkingBlock and in the normal answer body.

**Windows Lens floating / drag path restored**:
- `lens_set_floating` clears the interactive region and resizes/repositions the native window instead of relying on clipped fullscreen behavior.
- `Lens.tsx` uses native small-window rebasing on Windows floating mode and an explicit drag handle via `api.startDragging()`.

### API compatibility expectations

Keep all provider endpoint suffixes compatible:
- base OpenAI-compatible URL such as `https://example.com/v1`
- direct `/chat/completions`
- direct `/responses`
- direct Anthropic-style `/messages`

All three endpoint types support Tavily tool calling when `tavily_api_key` is set. The tool is injected as:
- Chat Completions: OpenAI function tool format
- Messages: Claude `input_schema` tool format
- Responses: Responses API function tool format

Native web search (`web_search_20250305` for Claude, `web_search` for Responses API) has been removed in favor of Tavily. This avoids proxy compatibility issues.

### Important files for continuation

- `src-tauri/src/api.rs` - provider routing, Tavily integration, tool call detection, SSE parsing, retries/failover.
- `src-tauri/src/commands.rs` - settings load/save, connection test, model fetch, translation commands.
- `src-tauri/src/settings.rs` - Settings schema including `tavily_api_key`, `ModelProvider.reasoning_effort`, `ModelProvider.web_search_enabled`.
- `src-tauri/src/main.rs` - slim app entry, plugin registration, command registration.
- `src-tauri/src/shortcuts.rs` - global hotkeys, tray, selected-text capture.
- `src-tauri/src/lens_commands.rs` - Lens command surface.
- `src/Lens.tsx` - Lens UI, search indicator, answer streaming, reasoning display.
- `src/App.tsx` - main translator UI.
- `src/api/tauri.ts` - frontend Tauri invoke/event API contract (includes `searchQuery`/`searchResults` in `LensStreamPayload`).
- `src/Settings.tsx` - provider settings UI, Tavily API key input, reasoning effort selector.

### Validation already run recently

- `npm run typecheck` ✓
- `npm run lint` ✓
- `cargo check --manifest-path src-tauri/Cargo.toml` ✓
- `cargo test --manifest-path src-tauri/Cargo.toml` ✓ (latest run: 56 tests passed)
- `npx tauri build --bundles nsis` ✓
- `npm run build` builds the release exe but currently fails during MSI bundling at WiX `light` with `拒绝访问。 (os error 5)`

Current Windows bundle outputs (v2.6.0):
- `D:\Desktop\kivio\src-tauri\target\release\bundle\nsis\Kivio_2.6.0_x64-setup.exe`
- `D:\Desktop\kivio\src-tauri\target\release\kivio.exe`

MSI note:
- `D:\Desktop\kivio\src-tauri\target\release\bundle\msi\Kivio_2.6.0_x64_en-US.msi` may exist, but it is an older artifact unless the WiX permission issue is fixed.

### Known issues / follow-up

- Tavily web-search queries still use a non-streaming first probe to detect tool calls. Non-search Lens questions should bypass the probe and stream normally.
- `ModelProvider.web_search_enabled` field still exists in settings schema but is no longer used by the UI or backend logic (Tavily activates automatically when key is present). Can be removed in a cleanup pass.
- Lens `webSearchEnabled` setting in `LensConfig` is also unused now. Same cleanup opportunity.
- Full `npm run build` still needs MSI/WiX `os error 5` diagnosis if MSI output is required. NSIS packaging succeeds.
- Some Rust warnings remain (unused imports/variables, unused browser_automation helpers). Non-blocking.

### Gotchas

- Third-party proxies (Sub2API, XTOKEN) do NOT support Claude native server tools (`web_search_20250305`). They filter out the `tools` field or return errors. Always use `tavily_web_search` function tool instead.
- These proxies only accept Bearer auth, not `x-api-key`. The `claude_auth_headers` function uses `.bearer_auth(key)`.
- Tool name must not be `web_search` — some proxies intercept that name and try to handle it themselves, causing "无法从消息中提取搜索查询" errors.
- Responses stream events with `reasoning`, `reasoning_summary_text`, or `summary_text` must never be treated as normal answer deltas. Keep `response_stream_is_reasoning_event(...)` and `response_stream_message_item_text(...)` strict.
- Probe/non-streaming approach avoids the complexity of detecting tool calls mid-stream. Keep it limited to likely web-search queries so ordinary Lens answers remain streaming.
