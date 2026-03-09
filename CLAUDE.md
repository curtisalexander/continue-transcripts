# continue-transcripts

## What this project does

Converts [continue.dev](https://continue.dev) session JSON files into self-contained HTML transcripts. The tool reads the `~/.continue/sessions/` directory (or a single JSON file), parses the conversation history, and produces styled HTML pages with syntax-highlighted code, ANSI-colored terminal output, collapsible tool calls, system prompt display, and token usage tracking.

## Architecture

### Language & build system

- **Core application**: Rust (`src/main.rs` — single-file, ~2600 lines)
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
- `sample-session.json` — basic conversation
- `sample-session-rich.json` — complex session with system prompt, tool calls, ANSI output, thinking blocks

## Key data flow

```
Session JSON file
  → serde_json::from_str() → Session struct
  → render_session()
    ├── Extract system prompt (first "system" message) → collapsible HTML section
    ├── Extract full tool descriptions from system prompt → collapsible per-tool panels
    ├── Track token usage per message → running cumulative totals
    ├── Group assistant messages with tool calls and tool results
    ├── Convert ANSI escape codes → styled <span> elements
    ├── Render Markdown → HTML (pulldown-cmark + syntect syntax highlighting)
    └── Embed all CSS + JS inline (no external dependencies)
  → Self-contained HTML file
```

## Key types (src/main.rs)

| Struct | Purpose |
|--------|---------|
| `Session` | Root: sessionId, title, workspaceDirectory, history, dateCreated |
| `ChatHistoryItem` | Wraps a ChatMessage + contextItems + promptLogs |
| `ChatMessage` | role, content, toolCalls, usage (token counts) |
| `Usage` | completionTokens, promptTokens, completionTokensDetails |
| `MessageContent` | Either a plain String or Vec<MessagePart> |
| `ToolCallDelta` | Tool call with function name + JSON arguments |
| `ContextItem` | Attached file: name, description, content |
| `PromptLog` | Model title + completion options |

## Key functions (src/main.rs)

| Function | Purpose |
|----------|---------|
| `render_session` | Main orchestrator — builds complete HTML document |
| `render_message` | Renders a single message with role label, content, token badge |
| `render_tool_calls` | Renders tool call blocks with human-readable arguments |
| `render_tool_result_inline` | Renders tool results as collapsible details |
| `render_tools_reference` | Renders the tools panel with full collapsible descriptions |
| `extract_tool_descriptions` | Parses system prompt for `### ToolName` sections |
| `markdown_to_html` | Converts Markdown to HTML with syntax highlighting |
| `ansi_to_html` | Converts ANSI SGR escape codes to styled HTML spans |
| `render_tool_args` | Formats tool arguments ($ command, key:value, JSON) |
| `render_index` | Creates index.html listing all sessions |
| `format_tokens` | Formats token counts as compact strings (1,234 / 12.3k / 1.2M) |

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
- `tool` messages contain tool results (may include ANSI escape codes)
- `system` messages contain the system prompt with tool definitions under `### ToolName` headers
- Token usage is in `message.usage` on assistant messages: `{ promptTokens, completionTokens }`
