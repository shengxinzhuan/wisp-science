# wisp-science 1.6.1 — Fix Release / 修复版本

All changes are based on `v1.6.0`. Five themes: cross-provider model-switch 400s, silent audit skips, dead-window rendering, browser-extension onboarding, and error readability. No schema migrations; sessions and settings are forward/backward compatible.
所有改动基于 `v1.6.0`。共五个主题：切模型 400、审计静默跳过、窗口死亡、浏览器扩展引导、错误可读性。无数据库迁移；会话与设置双向兼容。

---

## 1. Cross-provider model switch no longer 400s / 切换模型不再 400

**User-facing problem**: switching models mid-conversation (or between providers under the same OpenAI-compatible protocol) frequently failed with a raw 400 JSON error; the audit (reviewer) agent hit the same failure and silently skipped audits.
**用户问题**：会话中途切换模型（甚至在同为 OpenAI 兼容协议的模型间切换）经常报 400 原始 JSON 错误；审计 agent 遇到同样错误后会静默跳过审计。

Root cause verified against production transcripts and live APIs: reasoning-only / interrupted turns persist as empty `assistant` messages; DeepSeek tolerates replaying them, **kimi-k3 rejects them** ("the message at position N with role 'assistant' must not be empty" — reproduced live). Over 3,700 such messages existed in one local session store.
根因经生产会话记录与真实 API 实证：纯思考/中断轮次持久化为空 `assistant` 消息；DeepSeek 容忍重放，**kimi-k3 拒绝**（"must not be empty"，已实测复现）。本机会话库中此类消息超过 3700 条。

- `crates/wisp-llm/src/openai.rs` — `sanitize()` drops bare empty assistant turns (empty text AND no retained tool_calls); empty text alongside tool_calls still replays (accepted by both providers, verified live). 【tagged `#k3-empty-assistant`】
- `crates/wisp-llm/src/anthropic.rs` — history normalization for Anthropic Messages API: leading assistant turns dropped, positional tool_use/tool_result pairing (adjacency enforced, not just id sets), consecutive same-role messages merged at wire level, empty user text and empty transcript padded. 【tagged `#anthropic-strict-roles`】
- Tests: `openai.rs` 2 unit tests; `anthropic.rs` 7 unit tests; `tests/anthropic_wire.rs` 3 integration tests against a local mock enforcing the documented Messages-API constraints (first-message-user, strict alternation, tool_result adjacency, non-empty text).

## 2. Audit (reviewer) agent no longer silent / 审计 agent 不再静默跳过

`src-tauri/src/lib.rs` — five paths that previously only wrote `tracing::warn!` and marked findings `unaddressed` now also emit `AgentEvent::ReviewFailed` so the UI surfaces the failure: HTTP-path correction-turn failure and follow-up-review failure; ACP-path correction-turn failure, follow-up failure, and transcript-load failure. Messages carry a stage prefix (`correction turn failed:` / `follow-up review failed:`).

## 3. API errors are readable / API 错误可读

`crates/wisp-llm/src/provider.rs` — `LlmError::Api` display extracts `error.message` from the provider's JSON envelope instead of dumping the raw body. The raw body stays on the struct, so `is_retriable` / `is_context_overflow` matching is unchanged. The removed `thiserror` derive for this enum dropped an unused dependency.

## 4. Dead-window renderer fixes / 窗口死亡修复

**User-facing problem**: on long sessions with dense tool activity the window froze or went permanently white while backend tasks kept running (reported on 1.5.0; identical code paths in 1.6.0).
**用户问题**：长会话+密集工具调用时窗口卡死或永久白屏，后端任务照常运行（1.5.0 上报告；1.6.0 代码路径相同）。

- **4a. Remount storm removed** `ui/src/main.rs`, `ui/src/chat_render.rs`, `ui/src/app_support/messages.rs` — the global artifact fingerprint (`arts_fp`) that was XORed into every assistant row's keyed-For key remounted the entire visible thread (markdown + highlight + image loads) on every artifact event. `AssistantMessage` now receives the shared artifact signal and re-enriches at row scope; Leptos memo string-equality keeps untouched rows' DOM intact. `artifacts_fingerprint()` removed. 【tagged `#remount-storm`】
- **4b. Chat media no longer inlines base64** `ui/src/api.js` — new `media_url` / `media_thumbnail_url`: bytes fetched through the existing preview command family (all four path spellings) and handed to the browser as blob object URLs with a 64-entry LRU cache; thumbnails are canvas-downscaled (max edge 384px) with a 128-entry result cache. `messages.rs` switches all four inline sites (attachment thumbnails, artifact cards, generated images, generated videos — a 64 MB MP4 no longer becomes an ~85 MB string). 【tagged `#blob-media`】
- **4c. OwnerDisposed panics downgraded** `ui/src/main.rs` — leptos 0.6 runs a `create_effect`'s first pass in an owner-bound microtask; a row disposed in between made `with_owner` panic, and under `panic = "abort"` that killed the whole renderer (pre-existing in 1.5.0/1.6.0, not introduced here). A custom panic hook downgrades `OwnerDisposed` to a console warning; `ui/Cargo.toml` adds `[profile.release-wasm]` (`inherits = "release"`, `panic = "unwind"`) and `ui/build.ps1` builds with it so the downgrade survives in release. 【tagged `#owner-disposed-abort`】
- **4d. UI heartbeat watchdog** `src-tauri/src/lib.rs` + `ui/src/main.rs` — frontend beats every 5 s; if the focused main window goes silent past 60 s the backend reloads the webview (cooldown 120 s, fresh beats required after reload; backgrounded/minimized windows never trigger). Sessions live in SQLite, so a reload is the cheapest recovery. Decision function unit-tested in `lib_tests.rs`. 【tagged `#ui-watchdog`】
- **4e. Async media writes on disposed owners** `ui/src/app_support/messages.rs` — async completions now use `try_set` (drops the write instead of panicking); `ArtifactThumb` fills its `<img>` via direct DOM append keyed by a per-mount id (same pattern as the resources effect), with an error handler that removes the img so the kind badge remains the fallback.
- Tests: new `ui-tests/tests/long-session-stress.spec.ts` (idle DOM stability via MutationObserver, rAF responsiveness after dense tool events, media must be `blob:` and never `data:`); regressions `#927` (scroll) and artifact suites all pass.

## 5. Browser-extension one-click onboarding / 浏览器扩展一键引导

**User-facing problem**: the "not live-retrieved" banner told users to manually enable Developer mode and load an unpacked extension; non-technical users gave up (verified on a machine where the extension had never connected once).
**用户问题**："未联网检索"横幅让用户手动开启开发者模式并加载扩展；非技术用户直接放弃（已在一台从未连接成功的机器上实证）。

- `src-tauri/src/browser_bridge/mod.rs` — `open_extension_setup()`: launches the first real browser on its extension-manager page with the correct scheme per browser (`edge://extensions` / `brave://extensions` / `chrome://extensions`); skips the `cmd /C start` fallback. Returns the verified bundled extension path.
- `src-tauri/src/app_commands.rs` + `lib.rs` — new `open_browser_extension_page` command.
- `ui/src/main.rs` banner — "Set up browser" button between Retry and Dismiss: opens the page, copies the extension path to the clipboard, and shows a longer-lived actionable toast with the two remaining manual steps. `ui/src/bindings.rs`, `ui/src/i18n.rs` (en/zh), `ui/mock-bridge.js` updated. 【tagged `#extension-setup`】

## 6. CPU hot-path fixes / CPU 热点修复

Six fixes targeting the high-CPU reports (idle machine verified <1%; the burn happens during use):

- **Streaming markdown append-only rendering** `ui/src/app_support/messages.rs` — a commit that extends the previously rendered text at a block boundary now parses only the new suffix and appends it via DOM, instead of re-parsing and re-replacing the whole prefix (was O(n²) over a long reply). Mid-block growth still full-renders, bounded by the adaptive commit interval.
- **Transfer tray two-layer closure** `ui/src/main.rs` — the tray no longer subscribes to the 1-second clock unconditionally; with no transfer-worthy runs it stops rescanning and JSON-parsing every run record each second.
- **Completion dispatcher idle backoff** `src-tauri/src/delegation_completion.rs` — the 250 ms SQLite polling loop backs off to 5 s after four idle polls; any dispatch resets to fast polling.
- **Pet poll gating** `ui/src/pet.rs` — the 2 s pet poll keeps one cheap settings read while disabled and skips the runtime snapshot (SQLite run query) unless the pet is visible; slow-probes notice re-enabling within seconds.
- **Scroll hot path write-only** `ui/src/scroll.js` — follow snaps no longer read `scrollTop` back after writing it (that round-trip forced a synchronous layout every streaming frame), and the jump pill syncs only when not following.
- Not done (deliberately): keyed-For projection row caching and run-poll event-driven rewrite — architectural changes whose risk outweighed the remaining gain this round.

## 7. view_image double-billing removed / view_image 不再双重计费

Measured on a real 904-message session: every `view_image` forwarded the picture through a vision describer LLM call before the main model saw it — median 18 s, p90 154 s per look (the describer was itself a slow reasoning model), and the follow-up request then paid the image upload too. CLI agents attach the image directly.

- `crates/wisp-core/src/agent.rs` — when the active model supports vision (`ctx.supports_vision`, same flag the message-attachment path already uses), the tool result now carries the image as a native `Content::Parts` image part; the vision describer round-trip only runs for text-only primary models. Existing context machinery (serialization, `age_images` tombstones, text-only downgrade) already handles image parts on tool rows.
- Test: `vision_primary_view_image_attaches_without_describer_round_trip` (scripted primary + recording fallback; asserts zero describer calls and an image part in the follow-up request).
- Latency report that motivated this (same key, live APIs): DeepSeek TTFB ~80 ms; kimi-k3 TTFB 1-15 s (reasoning). Per-tool-step gap in the measured session: median 14 s — mostly k3 thinking; view_image was the one outlier the client could eliminate.

## 8. Responsiveness batch / 响应速度第二批

- **Provenance scan halved** `crates/wisp-core/src/agent.rs` — the after-snapshot of one producing tool call is reused as the next call's before-snapshot, instead of rescanning the whole workspace (≤20k entries) twice per call. Hundreds of ms per tool step on DrvFs/network mounts.
- **Offline banner live recheck** `src-tauri` + `ui/src/main.rs` — the per-turn "extension disconnected" judgment is frozen in the transcript, so a transient disconnect kept the banner forever (and resurrected it on session reload) even after the extension reconnected. The banner now rechecks live connection state before rendering and clears itself when the extension is back.
- **Shared HTTP client pool** `crates/wisp-llm/src/provider.rs` — review / follow-up / memory side calls each built a fresh `reqwest::Client` per call, paying a TLS handshake every time. No-proxy clients now share one process-wide pool; per-proxy clients stay isolated.
- **Streaming commit cap 1200 ms → 400 ms** `ui/src/app_support/messages.rs` — with append-only rendering the per-commit cost dropped sharply, so the adaptive interval cap can be tighter for typing-feel latency; full re-parses only happen mid-block and stay throttled by measured cost.

## Known issues, deliberately not fixed in 1.6.1 / 已知问题，本版不修

- **Agent-workflow latency** (diagnosed, no code change): per-tool provenance does two full workspace scans (≤20k entries) around every producing tool call (`crates/wisp-core/src/agent.rs:498-527`) — the dominant per-step cost on DrvFs/Windows; Windows shells cold-start PowerShell per call; streaming has three stacked buffers (33 ms + 50 ms + adaptive 50–1200 ms markdown commit). Interim mitigations: keep auto-review off; disabling follow-up questions frees the shared API key for the next turn's first token.
- The stale browser-offline banner can reappear on session reload after the extension has reconnected (per-turn judgment is frozen in the transcript); `web_agent_*` tools are not counted by the banner either.
- Reviewer model falls back to the global active model when its `model_id` is unset (`src-tauri/src/specialists.rs:272-290`).

## Verification / 验证

- `cargo test -p wisp-llm`: 62 unit + 3 integration passed.
- `cargo check`: wisp-tauri / wisp-llm / wisp-dto clean; `ui` wasm32 target clean.
- Watchdog decision unit test passed; `cargo fmt --check` clean for all touched files (six untouched files carry pre-existing baseline drift, left alone per AGENTS.md).
- Playwright (mock bridge): long-session-stress 3/3, artifact 21/21, #927 scroll 3/3.
- Live smoke against real DeepSeek + kimi-k3 APIs with a local key: pre-fix body 400s on k3 exactly as reported; fixed code passes (also re-verified after the OpenAI sanitize change).
- Full release-wasm build (`trunk build --release --cargo-profile release-wasm`) succeeds and contains the panic hook.
