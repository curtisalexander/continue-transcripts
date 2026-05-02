# continue-transcripts

## What this project does

Converts [continue.dev](https://continue.dev) session JSON files into self-contained HTML transcripts. The tool reads the `~/.continue/sessions/` directory (or a single JSON file), parses the conversation history, and produces styled HTML pages with syntax-highlighted code, ANSI-colored terminal output, collapsible tool calls, system prompt display, and token usage tracking.

## Architecture

### Language & build system

- **Core application**: Rust (`src/main.rs` — single-file, ~6000 lines)
- **Python wrapper**: Thin shim via [maturin](https://www.maturin.rs/) that packages the Rust binary as a Python wheel
- **Distribution**: Pre-built wheels on GitHub Releases for Linux x86_64/aarch64, macOS ARM, Windows x86_64

### How the Python wrapper works

The Python package exists solely for distribution convenience via `uv` / `pip`. There is **no Python code that does any processing**. The structure is:

```
python/continue_transcripts/
  __init__.py      # Just exports __version__
  __main__.py      # Finds the Rust binary on PATH and exec's it
```

When installed via `uv tool install` or `pip install`, maturin bundles the compiled Rust binary into the wheel. The `__main__.py` simply locates the binary via `shutil.which()` and calls `os.execvp()` to hand off execution entirely.

### Installing for development

```sh
# Build and run from source (Rust only, no Python needed)
cargo build --release
./target/release/continue-transcripts session.json

# Or install as a Python tool via uv (recommended for end users)
uv tool install continue-transcripts \
  --no-index \
  --find-links https://github.com/curtisalexander/continue-transcripts/releases/expanded_assets/v0.8.0
```

### Running tests

```sh
cargo test
```

Tests use fixture files in `tests/fixtures/`:

### Linting and formatting

Before committing, ensure clippy and rustfmt are clean:

```sh
cargo clippy
cargo fmt --check
```

To auto-fix formatting: `cargo fmt`

Fixture files in `tests/fixtures/`:

- `sample-session.json` — basic conversation
- `sample-session-rich.json` — system prompt, tool calls, ANSI output, thinking blocks
- `sample-session-prompt-logs.json` — system prompt only in `promptLogs[].prompt`, tool defs in `toolCallStates[].tool`
- `sample-session-templated-prompt.json` — `promptLogs[].prompt` in templated `<role>\n...\n\n` form (regression for the system-prompt slicing fix)
- `sample-session-compacted.json` — `conversationSummary` + session-level mode/chatModelTitle/totalCost
- `sample-session-reasoning.json` — Anthropic-style structured `reasoning` block
- `sample-session-tool-status.json` — tool call statuses (errored/canceled) and structured `output`
- `sample-session-token-details.json` — `promptTokensDetails.cachedTokens` + `completionTokensDetails.reasoningTokens`
- `sample-session-applied-rules.json` — `appliedRules[]` chip strip
- `sample-session-modifiers-editor.json` — `modifiers` + `editorState` (TipTap JSON with mentions/code blocks)
- `sample-session-context-extras.json` — context item `id`/`uri`/`hidden` fields
- `sample-session-thinking-redacted.json` — `redactedThinking` + `signature` on thinking messages
- `sample-session-showcase.json` — composite fixture used to render the README screenshot (exercises mode/cost/model header, applied rules, modifiers, reasoning, cached tokens, tool status)

## Key data flow

```
Session JSON file
  → serde_json::from_str() → Session struct
  → render_session()
    ├── Header chips: model (chatModelTitle), mode, totalCost, date, tokens
    ├── Extract system prompt
    │     primary: first role:system message
    │     fallback: leading <system> block of promptLogs[].prompt
    │              (continue.dev's chat template wraps every message in
    │               `<role>\n${content}\n\n` — slice only the system part)
    ├── Extract tool defs from toolCallStates[].tool → collapsible tool panel
    │     (completionOptions.tools was removed upstream Aug 2025; no fallback)
    ├── Per turn:
    │     ├── conversationSummary panel (compaction)
    │     ├── appliedRules chip strip + modifier badges (@codebase, no-context)
    │     ├── editorState "Original input" panel (TipTap JSON, mentions, slash)
    │     ├── reasoning block (Anthropic/OpenAI structured chain-of-thought)
    │     ├── context items (with provider/uri/icon chips)
    │     ├── message content (Markdown → syntect-highlighted HTML)
    │     └── tool calls with status badges (errored/canceled/calling/...)
    ├── Pair tool calls to results by toolCallId, fall back to positional
    │     If a call has structured output but no role:tool message,
    │     render toolCallStates[].output instead.
    ├── Token badges show cached/cacheWrite/reasoning details when present
    ├── Convert ANSI escape codes → styled <span> elements
    └── Embed all CSS + JS inline (no external dependencies)
  → Self-contained HTML file
```

## Key types (src/main.rs)

| Struct | Purpose |
|--------|---------|
| `Session` | sessionId, title, workspaceDirectory, history, dateCreated, mode, chatModelTitle, usage (with totalCost) |
| `ChatHistoryItem` | message + contextItems + promptLogs + toolCallStates + conversationSummary + reasoning + appliedRules + editorState + modifiers |
| `ChatMessage` | role, content, toolCalls, usage, toolCallId, signature, redactedThinking |
| `Usage` | promptTokens, completionTokens, promptTokensDetails (cachedTokens, cacheWriteTokens), completionTokensDetails (reasoningTokens) |
| `Reasoning` | active, text, startAt, endAt — Anthropic/OpenAI structured reasoning |
| `ToolCallState` | toolCallId, status, parsedArgs, output: ContextItem[], tool: ToolDef |
| `RuleMetadata` | name/slug/source/description for appliedRules chip strip |
| `InputModifiers` | useCodebase, noContext (per-turn user-input toggles) |
| `MessageContent` | Either a plain String or Vec<MessagePart> |
| `ToolCallDelta` | id + function (name, arguments JSON string) |
| `ContextItem` | name, description, content, icon, uri (file/url), id (provider, itemId), hidden |
| `PromptLog` | modelTitle + prompt (full templated chat) |

## Key functions (src/main.rs)

| Function | Purpose |
|----------|---------|
| `render_session` | Main orchestrator — builds complete HTML document |
| `render_message` | Renders a single message with role label, content, token badge |
| `render_tool_calls` | Renders tool call blocks; takes optional `&[ToolCallState]` for status badges |
| `render_tool_status_badge` | Maps `status` → color-coded chip (errored/canceled/calling/etc.) |
| `render_tool_result_inline` | Renders role:tool result messages as collapsible details |
| `render_structured_tool_output` | Renders `ToolCallState.output: ContextItem[]` when no role:tool message exists |
| `render_conversation_summary` | Compaction summary panel above the post-compaction turn |
| `render_reasoning_block` | Collapsible reasoning panel with duration (from startAt/endAt) |
| `render_applied_rules` | Per-turn chip strip of fired rules with source-specific styling |
| `render_modifiers` | `@codebase` / `no-context` badges |
| `render_editor_state` | Walks ProseMirror/TipTap JSON → faithful HTML (mentions, slash commands, code blocks) |
| `render_tools_reference_from_defs` | Tools panel from extracted ToolCallState.tool |
| `extract_system_from_prompt_log` | Slices the leading `<system>` block out of a templated prompt log |
| `markdown_to_html` / `ansi_to_html` | Markdown rendering + ANSI escape conversion |
| `format_tokens` | Compact token formatting (1,234 / 12.3k / 1.2M) |

## Version management

When asked to "bump version", update **all four** of these locations:

1. `Cargo.toml` — `version = "X.Y.Z"`
2. `pyproject.toml` — `version = "X.Y.Z"`
3. `python/continue_transcripts/__init__.py` — `__version__ = "X.Y.Z"`
4. `README.md` — all `--find-links` URLs containing the release version (e.g. `expanded_assets/vX.Y.Z`)

## continue.dev session format

Sessions are stored as JSON in `~/.continue/sessions/`. Key structure:

- `history[]` contains `ChatHistoryItem` objects
- Messages have roles: `user`, `assistant`, `system`, `tool`, `thinking`
- `assistant` messages may have `toolCalls[]` and `usage` (token counts)
- `tool` messages contain tool results (may include ANSI escape codes); newer sessions also set `toolCallId` on the result
- `system` messages contain the system prompt with tool definitions under `### ToolName` headers
- Token usage is in `message.usage` on assistant messages: `{ promptTokens, completionTokens, promptTokensDetails?, completionTokensDetails? }`

### Schema gotchas worth knowing

- **`promptLogs[].prompt` is the *entire* templated chat**, not just the system prompt. Continue.dev's `_formatChatMessage` wraps every message as `<role>\n${content}\n\n`, so the full string contains `<system>...\n\n<user>...\n\n<assistant>...`. To get just the system prompt out of it, slice up to the first `\n\n<role>\n` boundary.
- **`completionOptions` was removed** from `PromptLog` upstream in commit 00985d3a5 (Aug 2025). Tool defs only come from `toolCallStates[].tool` now — only tools that were actually *called* are recoverable from the JSON.
- **Compaction**: when a long session is compacted, the first post-compaction `ChatHistoryItem` carries a `conversationSummary` string that *replaces* the earlier turns. Render it above the message or the transcript looks like it starts mid-conversation.
- **`reasoning` block**: Anthropic/OpenAI native chain-of-thought lands in `ChatHistoryItem.reasoning: { active, text, startAt, endAt }` — not as a `role: "thinking"` message. Both can coexist.
- **Per-turn extras**: `appliedRules[]` (which rules fired), `modifiers: { useCodebase, noContext }` (input toggles), `editorState` (TipTap JSON of the user's raw input with mentions/slash commands).
- **Session-level**: `mode` (chat/agent/plan/background), `chatModelTitle`, and `usage.totalCost` (USD) are top-level fields.
