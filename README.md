# continue-transcripts

Convert [continue.dev](https://continue.dev) session files to readable, self-contained HTML transcripts.

![Example HTML output](docs/example-output-cropped.png)

## Features

- Converts continue.dev session JSON files into styled, self-contained HTML pages
- Renders assistant messages as Markdown (code blocks, tables, task lists, etc.)
- Displays context items (attached files/code) in collapsible sections
- Shows tool calls with pretty-printed JSON arguments
- Generates an `index.html` linking all processed sessions when given a directory
- Optional title-based filtering with `--filter`
- Responsive design — works on desktop and mobile

## Installation

Pre-built wheels are available from [GitHub Releases](https://github.com/curtisalexander/continue-transcripts/releases). No Rust toolchain is required — just Python >= 3.9 and `uv`.

### With `uv tool install` (recommended)

Install globally so the `continue-transcripts` command is always available:

```sh
uv tool install continue-transcripts \
  --no-index \
  --find-links https://github.com/curtisalexander/continue-transcripts/releases/expanded_assets/v0.1.0
```

The `continue-transcripts` command is then available on your `PATH`.

To upgrade later (update the version in the URL):

```sh
uv tool install --upgrade continue-transcripts \
  --no-index \
  --find-links https://github.com/curtisalexander/continue-transcripts/releases/expanded_assets/v0.1.0
```

To uninstall:

```sh
uv tool uninstall continue-transcripts
```

### With `uvx` (one-off runs)

Run without installing:

```sh
uvx \
  --no-index \
  --from continue-transcripts \
  --find-links https://github.com/curtisalexander/continue-transcripts/releases/expanded_assets/v0.1.0 \
  continue-transcripts ./sessions
```

This is useful for a quick one-off conversion without permanently installing the tool.

### With `uv pip install`

Install into a specific virtual environment:

```sh
uv venv .venv
source .venv/bin/activate
uv pip install continue-transcripts \
  --no-index \
  --find-links https://github.com/curtisalexander/continue-transcripts/releases/expanded_assets/v0.1.0
```

### Building from source

If you prefer to build from source (or need a platform not covered by the pre-built wheels), you can install directly from the Git repository. This requires a [Rust toolchain](https://rustup.rs/) in addition to Python >= 3.9:

```sh
uv tool install git+https://github.com/curtisalexander/continue-transcripts
```

### Pre-built wheel platforms

Wheels are built in CI for the following targets:

| Platform | Target |
|----------|--------|
| Linux x86_64 | `x86_64-unknown-linux-gnu` |
| Linux aarch64 | `aarch64-unknown-linux-gnu` |
| macOS Apple Silicon | `aarch64-apple-darwin` |
| Windows x86_64 | `x86_64-pc-windows-msvc` |

## Usage

```
continue-transcripts <INPUT> [OPTIONS]
```

### Arguments

| Argument | Description |
|----------|-------------|
| `INPUT` | Path to a single session JSON file or a directory containing session files |

### Options

| Option | Description |
|--------|-------------|
| `-o`, `--output <DIR>` | Output directory for generated HTML (default: `output`) |
| `--filter <STRING>` | Only process sessions whose title contains this string (case-insensitive) |
| `-h`, `--help` | Print help |

### Examples

Convert a single session file:

```sh
continue-transcripts session.json
```

Convert all sessions in a directory:

```sh
continue-transcripts ~/.continue/sessions/ -o ./transcripts
```

Filter sessions by title:

```sh
continue-transcripts ~/.continue/sessions/ --filter "auth" -o ./transcripts
```

The tool writes one HTML file per session to the output directory, plus an `index.html` that links to each transcript.

## Where are continue.dev sessions stored?

Continue stores data in `~/.continue/` on Linux and macOS (`%USERPROFILE%\.continue\` on Windows). Session history files are JSON files within this directory. The exact subdirectory may vary by version — check your `~/.continue/` tree or use `find ~/.continue -name '*.json'` to locate them.

## How it works

1. Reads one or more continue.dev session JSON files
2. Parses the chat history (user messages, assistant messages, tool calls, context items)
3. Renders assistant/system messages from Markdown to HTML using [pulldown-cmark](https://crates.io/crates/pulldown-cmark)
4. Produces a self-contained HTML file per session (all CSS and JavaScript are embedded — no external dependencies)
5. Generates an `index.html` listing all sessions when processing multiple files

## Inspiration

Inspired by Simon Willison's [claude-code-transcripts](https://github.com/simonw/claude-code-transcripts).

## License

MIT
