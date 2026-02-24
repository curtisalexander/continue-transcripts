# Improve Conversation Display - Implementation Plan

## Goal

Make the HTML transcript output readable as a real conversation — the user should
be able to follow the flow naturally and drill into details (tool calls, thinking,
system prompt, tool descriptions) only when needed.

---

## Progress

| Step | Status | Notes |
|------|--------|-------|
| 1. Rich sample fixture | **Done** | `tests/fixtures/sample-session-rich.json` — system prompt, thinking, ANSI tool results, varied tool calls |
| 2. ANSI → HTML converter | **Done** | `ansi_to_html()` — hand-rolled SGR parser, One Dark palette, handles 30-37/40-47/90-97, bold/italic/underline/dim |
| 3. Bare tool call rendering | **Done** | `render_tool_args()` — single-key shortcut ($ for Bash), key-value pairs, fallback JSON |
| 4. System prompt extraction | **Done** | First `system` message extracted to collapsible `<details>` at top of page |
| 5. Tool call nesting | **Done** | `render_session` groups assistant + subsequent tool messages; results shown inside `assistant-tool-group` div |
| 6. Collapsible thinking & tool results | **Done** | Thinking: `<details>` (collapsed). Tool results: `<details class="tool-result-details">` (collapsed) |
| 7. Tool descriptions reference | **Done** | `extract_tool_descriptions()` parses system prompt; `render_tools_reference()` shows grid panel |
| 8. CSS updates | **Done** | New classes: `.system-prompt`, `.tools-reference`, `.assistant-tool-group`, `.tool-result-details`, `.tool-args-bare`, `.tool-args-kv`, etc. |
| 9. JavaScript updates | **Done** | Expand/Collapse all toggle; truncation for `.tool-result-pre` |
| 10. Tests | **Done** | 30 tests pass — ANSI, bare tool args, system prompt extraction, tool grouping, thinking collapsed, tool descriptions, rich fixture integration, filename truncation/dedup |
| 11. `--workers` CLI flag | **Done** | `--workers N` / `-w N` controls rayon thread pool size; defaults to CPU cores |
| 12. Terminal summary | **Done** | Files discovered/written/skipped/errored, elapsed time, per-file status with check/X emoji, exit code 1 on errors |
| 13. Filename truncation & dedup | **Done** | `sanitize_filename` truncates to 60 chars; `unique_filename` appends `_1`, `_2`, etc. on collision |

---

## Decisions Made

1. **Tool descriptions source:** Parsed from the system prompt using `### ToolName` header patterns. Falls back to just listing tool names when no descriptions found.
2. **Sidebar vs inline reference:** Expandable section at the top (below system prompt, above transcript). Simpler and works on all screen sizes.
3. **ANSI palette:** One Dark palette — matches the dark code theme well.
4. **Hand-rolled ANSI parser:** Covers SGR sequences (colors, bold, italic, underline, dim, reset). No new dependency needed.
5. **Thread pool:** Rayon defaults to number of CPU cores. `--workers` allows override. Reading is I/O bound so more workers than cores can help on large directories.
6. **Filename length:** Capped at 60 characters (before `.html` extension) for readability. Trailing underscores trimmed. Collisions get `_1`, `_2`, etc. suffix.

---

## Requirements (from user)

| # | Requirement | Summary |
|---|-------------|---------|
| 1 | **Bare tool calls** | Show tool name + arguments in a human-readable way, not raw JSON (unless JSON is the best fit for a particular argument). |
| 2 | **ANSI → HTML** | Tool results often contain terminal output with ANSI escape codes. Convert these to equivalent styled HTML (colored `<span>` elements). |
| 3 | **Tool calls nested under assistant** | Tool calls should appear indented/underneath the assistant message that invoked them, not as standalone top-level blocks. |
| 4 | **Thinking & tool results collapsible (default collapsed)** | Wrap thinking blocks and tool result messages in `<details>` that start **closed**. |
| 5 | **System prompt at top** | Extract the system prompt from the session history, display it once at the very top of the transcript in a collapsible section (default collapsed). |
| 6 | **Tool descriptions reference** | Collect descriptions of all tools that were called. Show as a reference panel (sidebar or expandable section) so the reader can look up what each tool does. |
| 7 | **Richer sample data** | Create a sample session JSON that exercises all these features (ANSI codes, system prompt, thinking, multiple tool calls with varied argument shapes). |
| 8 | **Performance** | Speed up reading/writing. Allow user to control worker count. |
| 9 | **Terminal summary** | Show completion summary with file counts, status indicators, and elapsed time. |

---

## Implementation Steps (original, for reference)

### 1. Create a richer sample session fixture

**File:** `tests/fixtures/sample-session-rich.json`

Create a new fixture that includes:
- A `system` role message at index 0 with a realistic system prompt (tool descriptions embedded)
- `thinking` role messages
- Multiple `assistant` messages with `toolCalls` of varied shapes:
  - `Bash` tool with a `command` argument (single string — display bare)
  - `Read` tool with a `file_path` argument (display bare)
  - `Edit` tool with `file_path`, `old_string`, `new_string` (display structured)
  - `Grep`/`Glob` tool calls
- `tool` role messages (tool results) containing ANSI escape codes for:
  - Colored text (`\x1b[32m` green, `\x1b[31m` red, etc.)
  - Bold (`\x1b[1m`)
  - Reset (`\x1b[0m`)
  - Typical `cargo test`, `git diff`, or `ls --color` output

### 2. ANSI escape code → HTML conversion

**File:** `src/main.rs` (new function `ansi_to_html`)

Add a lightweight ANSI-to-HTML converter:
- Parse SGR sequences (`\x1b[...m`)
- Map color codes to CSS classes or inline styles:
  - 30-37: standard foreground colors
  - 40-47: standard background colors
  - 90-97: bright foreground
  - 1: bold, 3: italic, 4: underline
  - 0: reset all
- Wrap colored spans: `<span style="color:#e06c75">text</span>`
- Escape HTML entities in the text portions
- Use a well-known terminal color palette (e.g., One Dark or match the existing code theme)

This keeps dependencies minimal (no new crate needed for a straightforward SGR parser).

### 3. Bare tool call rendering

**File:** `src/main.rs` — rewrite `render_tool_calls`

Instead of always dumping pretty-printed JSON, present tool calls in a more
human-readable format:

```
Tool Call: Bash
$ npm install express-session connect-redis redis

Tool Call: Read
file_path: src/auth/session.ts

Tool Call: Edit
file_path: src/auth/session.ts
old_string: |
  import jwt from 'jsonwebtoken';
new_string: |
  import session from 'express-session';
```

Logic:
1. Parse the JSON arguments into a `serde_json::Value`.
2. **If it's an object with a single string key** (like `{"command": "..."}` for
   Bash), display the value directly with a contextual prefix (`$` for commands).
3. **If it's an object with a few string keys**, display as `key: value` pairs
   (with long values in a code block).
4. **Fallback**: pretty-print as JSON for complex/nested structures.

### 4. Nest tool calls + results under assistant messages

**File:** `src/main.rs` — restructure `render_session` / `render_message`

Currently each `ChatHistoryItem` is rendered independently. Instead:

- When an `assistant` message has `toolCalls`, render the tool calls *inside*
  the assistant `<div>`, indented (using CSS margin-left or a nested container).
- The next `tool` role message(s) are the **results** of those calls. Pair them
  with the preceding assistant's tool calls by index/position.
- Render tool results as sub-blocks under the assistant message, indented to
  match the tool call they respond to.
- This means `render_session` needs to look ahead in the history to group
  assistant + subsequent tool messages together.

CSS: Add an `.assistant-tool-group` wrapper with left margin/border to show
nesting visually.

### 5. Collapsible thinking & tool results (default collapsed)

**File:** `src/main.rs` — update `render_message`

- **Thinking blocks:** Change `<details open>` → `<details>` (currently expanded
  by default — flip to collapsed).
- **Tool results:** Wrap tool result content in
  `<details><summary>Tool Result: {tool_name}</summary>...</details>`.
  When paired with a tool call (step 4), the summary can reference the
  tool name from the call.

### 6. System prompt extraction to top

**File:** `src/main.rs` — update `render_session`

- Scan `session.history` for the first message with `role == "system"`.
- If found, extract it and render at the top of the page (after the header,
  before the transcript) as:
  ```html
  <details class="system-prompt">
    <summary>System Prompt</summary>
    <div class="system-prompt-content">
      {rendered markdown content}
    </div>
  </details>
  ```
- Remove that system message from the main transcript flow (avoid duplication).
- Only extract the **first** system message; subsequent ones stay inline.

### 7. Tool descriptions reference panel

**File:** `src/main.rs` — new function + CSS

- During rendering, collect all unique tool names that appear in `toolCalls`.
- If the system prompt contains tool descriptions (common in Claude Code
  transcripts — tools are described in the system prompt), parse/extract them.
- Alternatively, build descriptions from the tool call patterns observed.
- Render as a collapsible `<details>` section (or a fixed sidebar on wide
  screens) titled "Tools Used" that lists each tool with its description.
- Place this after the system prompt section, before the transcript.

### 8. CSS updates

- `.tool-call` — adjust for bare rendering (monospace for commands, key-value layout)
- `.assistant-tool-group` — indented sub-block for tool calls + results
- `.system-prompt` — collapsible section styling at top
- `.tool-result-details` — collapsible tool result styling
- `.ansi-*` — ANSI color classes (or rely on inline styles)
- `.tools-reference` — sidebar/panel for tool descriptions
- Responsive adjustments for the sidebar

### 9. JavaScript updates

- Ensure "Show more/less" logic works for the new nested structure
- Possibly add a "Collapse all / Expand all" toggle for tool details

### 10. Tests

- Update existing tests for the new rendering behavior
- Add tests for:
  - `ansi_to_html` conversion
  - Bare tool call rendering for different argument shapes
  - System prompt extraction
  - Tool call + result grouping logic
