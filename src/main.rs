use clap::Parser;
use html_escape::encode_text;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser as MdParser, Tag, TagEnd};
use rayon::prelude::*;
use serde::Deserialize;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;
use syntect::highlighting::ThemeSet;
use syntect::html::highlighted_html_for_string;
use syntect::parsing::SyntaxSet;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "continue-transcripts",
    about = "Convert continue.dev session files to readable HTML transcripts"
)]
struct Cli {
    /// Path to a single session JSON file, or a directory of session files
    input: PathBuf,

    /// Output directory for generated HTML files
    #[arg(short, long, default_value = "output")]
    output: PathBuf,

    /// Only process sessions whose title contains this string (case-insensitive)
    #[arg(long)]
    filter: Option<String>,

    /// Only include sessions created on or after this date (YYYY-MM-DD)
    #[arg(long)]
    since: Option<String>,

    /// Only include sessions created before this date (YYYY-MM-DD)
    #[arg(long)]
    before: Option<String>,

    /// Number of parallel worker threads (defaults to number of CPU cores)
    #[arg(short, long)]
    workers: Option<usize>,
}

// ---------------------------------------------------------------------------
// continue.dev session types  (deserialized from JSON)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct Session {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    workspace_directory: String,
    #[serde(default)]
    history: Vec<ChatHistoryItem>,
    #[serde(default)]
    date_created: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ChatHistoryItem {
    message: ChatMessage,
    #[serde(default)]
    context_items: Vec<ContextItem>,
    #[serde(default)]
    prompt_logs: Option<Vec<PromptLog>>,
    #[serde(default)]
    tool_call_states: Option<Vec<ToolCallState>>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct PromptLog {
    #[serde(default)]
    model_title: String,
    #[serde(default)]
    completion_options: Option<CompletionOptions>,
    #[serde(default)]
    prompt: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct CompletionOptions {
    #[serde(default)]
    model: String,
    #[serde(default)]
    tools: Option<Vec<ToolDef>>,
}

// ---------------------------------------------------------------------------
// Tool definitions (from toolCallStates and completionOptions.tools)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ToolCallState {
    #[serde(default)]
    tool: Option<ToolDef>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct ToolDef {
    #[serde(default)]
    function: Option<ToolFunction>,
    #[serde(default)]
    system_message_description: Option<SystemMessageDescription>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct ToolFunction {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    parameters: Option<serde_json::Value>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct SystemMessageDescription {
    #[serde(default)]
    prefix: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ChatMessage {
    role: String,
    #[serde(default)]
    content: MessageContent,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallDelta>>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct Usage {
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens: u64,
}

/// continue.dev `content` can be a plain string or an array of parts.
#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
enum MessageContent {
    Text(String),
    Parts(Vec<MessagePart>),
}

impl Default for MessageContent {
    fn default() -> Self {
        MessageContent::Text(String::new())
    }
}

impl MessageContent {
    fn text(&self) -> String {
        match self {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    MessagePart::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            MessageContent::Text(s) => s.trim().is_empty(),
            MessageContent::Parts(parts) => parts.iter().all(|p| match p {
                MessagePart::Text { text } => text.trim().is_empty(),
                _ => false,
            }),
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "camelCase")]
enum MessagePart {
    #[serde(alias = "text")]
    Text { text: String },
    #[serde(alias = "imageUrl", alias = "image_url")]
    ImageUrl {},
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ToolCallDelta {
    #[serde(default)]
    function: Option<ToolCallFunction>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ToolCallFunction {
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ContextItem {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    content: String,
}

// ---------------------------------------------------------------------------
// Syntax highlighting helpers  (syntect)
// ---------------------------------------------------------------------------

fn get_syntax_set() -> &'static SyntaxSet {
    use std::sync::OnceLock;
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    SS.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn get_theme() -> &'static syntect::highlighting::Theme {
    use std::sync::OnceLock;
    static TH: OnceLock<syntect::highlighting::Theme> = OnceLock::new();
    TH.get_or_init(|| {
        let ts = ThemeSet::load_defaults();
        ts.themes["base16-ocean.dark"].clone()
    })
}

/// Map common language aliases to tokens that syntect's default set recognises.
fn normalize_lang(lang: &str) -> &str {
    match lang {
        "typescript" | "ts" | "tsx" => "javascript",
        "jsx" => "javascript",
        "sh" | "zsh" | "shell" => "bash",
        "yml" => "yaml",
        "dockerfile" => "Dockerfile",
        "md" => "markdown",
        "py" => "python",
        "rb" => "ruby",
        "rs" => "rust",
        "cs" => "c#",
        "cxx" | "cc" | "hpp" => "c++",
        other => other,
    }
}

/// Attempt to syntax-highlight `code` for the given language token.
/// Returns highlighted HTML (with inline styles) or `None` if the
/// language is not recognised.
fn highlight_code(lang: &str, code: &str) -> Option<String> {
    let ss = get_syntax_set();
    let lang = normalize_lang(lang);
    // Try multiple lookup strategies: token, extension, then name substring
    let syntax = ss
        .find_syntax_by_token(lang)
        .or_else(|| ss.find_syntax_by_extension(lang))
        .or_else(|| ss.find_syntax_by_name(lang))?;
    let theme = get_theme();
    highlighted_html_for_string(code, ss, syntax, theme).ok()
}

// ---------------------------------------------------------------------------
// Markdown → HTML  (using pulldown-cmark + syntect)
// ---------------------------------------------------------------------------

fn markdown_to_html(md: &str) -> String {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS;
    let parser = MdParser::new_ext(md, options);

    let mut html_output = String::new();
    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut code_buf = String::new();

    // We collect events and replace code blocks with highlighted HTML
    let events: Vec<Event<'_>> = parser.collect();
    let mut i = 0;

    while i < events.len() {
        match &events[i] {
            Event::Start(Tag::CodeBlock(kind)) => {
                in_code_block = true;
                code_buf.clear();
                code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                i += 1;
                continue;
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                // Try syntax highlighting; fall back to plain escaped code
                if !code_lang.is_empty() {
                    if let Some(highlighted) = highlight_code(&code_lang, &code_buf) {
                        // syntect wraps output in <pre style="background-color:...;">;
                        // strip that and use our CSS variables instead.
                        let patched = if let Some(start) = highlighted.find("style=\"") {
                            let after_style = start + 7; // past style="
                            if let Some(end_quote) = highlighted[after_style..].find('"') {
                                format!(
                                    "{}class=\"highlighted-code\"{}",
                                    &highlighted[..start],
                                    &highlighted[after_style + end_quote + 1..]
                                )
                            } else {
                                highlighted
                            }
                        } else {
                            highlighted
                        };
                        html_output.push_str(&patched);
                    } else {
                        // Unknown language – render without highlighting
                        html_output.push_str(&format!(
                            "<pre><code class=\"language-{}\">{}</code></pre>",
                            encode_text(&code_lang),
                            encode_text(&code_buf)
                        ));
                    }
                } else {
                    html_output.push_str(&format!(
                        "<pre><code>{}</code></pre>",
                        encode_text(&code_buf)
                    ));
                }
                i += 1;
                continue;
            }
            Event::Text(t) if in_code_block => {
                code_buf.push_str(t);
                i += 1;
                continue;
            }
            _ => {}
        }

        // For non-code-block events, render normally via pulldown-cmark
        let single = std::iter::once(events[i].clone());
        pulldown_cmark::html::push_html(&mut html_output, single);
        i += 1;
    }

    html_output
}

// ---------------------------------------------------------------------------
// ANSI escape code → HTML conversion
// ---------------------------------------------------------------------------

/// Convert ANSI SGR escape sequences to styled HTML `<span>` elements.
///
/// Handles:
/// - Standard foreground colors (30-37, 90-97)
/// - Standard background colors (40-47, 100-107)
/// - Bold (1), italic (3), underline (4), dim (2)
/// - Reset (0)
///
/// Text is HTML-escaped. Unrecognised sequences are silently stripped.
fn ansi_to_html(input: &str) -> String {
    // One Dark–inspired terminal palette (dark background)
    const FG_COLORS: [&str; 8] = [
        "#282c34", // 0 black
        "#e06c75", // 1 red
        "#98c379", // 2 green
        "#e5c07b", // 3 yellow
        "#61afef", // 4 blue
        "#c678dd", // 5 magenta
        "#56b6c2", // 6 cyan
        "#abb2bf", // 7 white
    ];
    const BRIGHT_FG: [&str; 8] = [
        "#5c6370", // 0 bright black (gray)
        "#e06c75", // 1 bright red
        "#98c379", // 2 bright green
        "#e5c07b", // 3 bright yellow
        "#61afef", // 4 bright blue
        "#c678dd", // 5 bright magenta
        "#56b6c2", // 6 bright cyan
        "#ffffff", // 7 bright white
    ];
    const BG_COLORS: [&str; 8] = [
        "#282c34", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#abb2bf",
    ];

    struct Style {
        fg: Option<String>,
        bg: Option<String>,
        bold: bool,
        italic: bool,
        underline: bool,
        dim: bool,
    }

    impl Style {
        fn new() -> Self {
            Style {
                fg: None,
                bg: None,
                bold: false,
                italic: false,
                underline: false,
                dim: false,
            }
        }

        fn reset(&mut self) {
            *self = Style::new();
        }

        fn is_active(&self) -> bool {
            self.fg.is_some()
                || self.bg.is_some()
                || self.bold
                || self.italic
                || self.underline
                || self.dim
        }

        fn open_tag(&self) -> String {
            if !self.is_active() {
                return String::new();
            }
            let mut styles = Vec::new();
            if let Some(ref c) = self.fg {
                styles.push(format!("color:{c}"));
            }
            if let Some(ref c) = self.bg {
                styles.push(format!("background-color:{c}"));
            }
            if self.bold {
                styles.push("font-weight:bold".to_string());
            }
            if self.italic {
                styles.push("font-style:italic".to_string());
            }
            if self.underline {
                styles.push("text-decoration:underline".to_string());
            }
            if self.dim {
                styles.push("opacity:0.7".to_string());
            }
            format!("<span style=\"{}\">", styles.join(";"))
        }
    }

    let mut out = String::with_capacity(input.len());
    let mut style = Style::new();
    let mut span_open = false;
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        // Check for ESC [ ... m  (CSI SGR sequence)
        if i + 1 < len && bytes[i] == 0x1b && bytes[i + 1] == b'[' {
            // Find the terminating letter
            let start = i + 2;
            let mut end = start;
            while end < len && bytes[end] != b'm' && bytes[end] != b'A' && bytes[end] != b'B'
                && bytes[end] != b'C' && bytes[end] != b'D' && bytes[end] != b'H'
                && bytes[end] != b'J' && bytes[end] != b'K'
            {
                end += 1;
            }
            if end < len && bytes[end] == b'm' {
                // SGR sequence – parse parameters
                let params = &input[start..end];
                if span_open {
                    out.push_str("</span>");
                    span_open = false;
                }
                if params.is_empty() || params == "0" {
                    style.reset();
                } else {
                    let codes: Vec<u32> = params
                        .split(';')
                        .filter_map(|s| s.parse().ok())
                        .collect();
                    let mut ci = 0;
                    while ci < codes.len() {
                        match codes[ci] {
                            0 => style.reset(),
                            1 => style.bold = true,
                            2 => style.dim = true,
                            3 => style.italic = true,
                            4 => style.underline = true,
                            22 => {
                                style.bold = false;
                                style.dim = false;
                            }
                            23 => style.italic = false,
                            24 => style.underline = false,
                            30..=37 => {
                                style.fg =
                                    Some(FG_COLORS[(codes[ci] - 30) as usize].to_string());
                            }
                            39 => style.fg = None,
                            40..=47 => {
                                style.bg =
                                    Some(BG_COLORS[(codes[ci] - 40) as usize].to_string());
                            }
                            49 => style.bg = None,
                            90..=97 => {
                                style.fg =
                                    Some(BRIGHT_FG[(codes[ci] - 90) as usize].to_string());
                            }
                            100..=107 => {
                                style.bg =
                                    Some(BG_COLORS[(codes[ci] - 100) as usize].to_string());
                            }
                            // 256-color: 38;5;N or 48;5;N — skip for now
                            38 | 48 => {
                                if ci + 2 < codes.len() && codes[ci + 1] == 5 {
                                    ci += 2; // skip the color index
                                }
                            }
                            _ => {} // ignore unknown
                        }
                        ci += 1;
                    }
                }
                if style.is_active() {
                    out.push_str(&style.open_tag());
                    span_open = true;
                }
                i = end + 1;
            } else {
                // Non-SGR CSI sequence (cursor movement, etc.) — strip it
                i = end + 1;
            }
            continue;
        }

        // Regular character — HTML-escape it
        match bytes[i] {
            b'<' => out.push_str("&lt;"),
            b'>' => out.push_str("&gt;"),
            b'&' => out.push_str("&amp;"),
            b'"' => out.push_str("&quot;"),
            _ => out.push(bytes[i] as char),
        }
        i += 1;
    }

    if span_open {
        out.push_str("</span>");
    }

    out
}

// ---------------------------------------------------------------------------
// HTML generation
// ---------------------------------------------------------------------------

fn role_label(role: &str) -> &str {
    match role {
        "user" => "User",
        "assistant" => "Assistant",
        "system" => "System",
        "tool" => "Tool Result",
        "thinking" => "Thinking",
        _ => role,
    }
}

fn role_class(role: &str) -> &str {
    match role {
        "user" => "user",
        "assistant" => "assistant",
        "system" => "system",
        "tool" => "tool-result",
        "thinking" => "thinking",
        _ => "unknown",
    }
}

/// Render a single tool call's arguments in a human-readable way.
///
/// Strategy:
/// 1. If the arguments parse as a JSON object with a single string key
///    (e.g. `{"command": "..."}` for Bash), display the value directly
///    with a contextual prefix.
/// 2. If it's an object with a few string-valued keys, display as
///    key: value pairs (long values get code blocks).
/// 3. Fallback: pretty-print as JSON.
fn render_tool_args(name: &str, arguments: &str) -> String {
    let parsed: serde_json::Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(_) => {
            // Not valid JSON — just show raw
            return format!(
                "<pre class=\"tool-args\"><code>{}</code></pre>",
                encode_text(arguments)
            );
        }
    };

    let obj = match parsed.as_object() {
        Some(o) => o,
        None => {
            let pretty =
                serde_json::to_string_pretty(&parsed).unwrap_or_else(|_| arguments.to_string());
            return format!(
                "<pre class=\"tool-args\"><code>{}</code></pre>",
                encode_text(&pretty)
            );
        }
    };

    // Single-string-key shortcuts
    if obj.len() == 1 {
        let (key, val) = obj.iter().next().unwrap();
        if let Some(s) = val.as_str() {
            // Bash / command  →  $ command
            if name == "Bash" || key == "command" {
                return format!(
                    "<pre class=\"tool-args tool-args-bare\"><code><span class=\"tool-prompt\">$</span> {}</code></pre>",
                    encode_text(s)
                );
            }
            // Read / Glob / file_path / pattern  →  key: value
            let display_val = if is_path_key(key) {
                decode_path(s)
            } else {
                s.to_string()
            };
            return format!(
                "<div class=\"tool-args-kv\"><span class=\"tool-arg-key\">{}</span>: <code>{}</code></div>",
                encode_text(key),
                encode_text(&display_val)
            );
        }
    }

    // Edit tool — render file path + diff-style old/new
    if name == "Edit" || name == "edit" {
        let file_path = obj.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
        let old_str = obj.get("old_string").and_then(|v| v.as_str()).unwrap_or("");
        let new_str = obj.get("new_string").and_then(|v| v.as_str()).unwrap_or("");
        if !file_path.is_empty() && (!old_str.is_empty() || !new_str.is_empty()) {
            let decoded_path = decode_path(file_path);
            let mut html = String::new();
            html.push_str("<div class=\"tool-args-kv-group\">");
            html.push_str(&format!(
                "<div class=\"tool-arg-line\"><span class=\"tool-arg-key\">file_path</span>: <code>{}</code></div>",
                encode_text(&decoded_path)
            ));
            // Show replace_all flag if present and true
            if let Some(replace_all) = obj.get("replace_all").and_then(|v| v.as_bool()) {
                if replace_all {
                    html.push_str("<div class=\"tool-arg-line\"><span class=\"tool-arg-key\">replace_all</span>: <code>true</code></div>");
                }
            }
            html.push_str("<pre class=\"tool-diff\"><code>");
            for line in old_str.lines() {
                html.push_str(&format!(
                    "<span class=\"diff-remove\">- {}</span>\n",
                    encode_text(line)
                ));
            }
            for line in new_str.lines() {
                html.push_str(&format!(
                    "<span class=\"diff-add\">+ {}</span>\n",
                    encode_text(line)
                ));
            }
            html.push_str("</code></pre>");
            html.push_str("</div>");
            return html;
        }
    }

    // Multiple keys — render as key: value pairs
    let all_simple = obj.values().all(|v| v.is_string() || v.is_number() || v.is_boolean());
    if all_simple && obj.len() <= 8 {
        let mut html = String::new();
        html.push_str("<div class=\"tool-args-kv-group\">");
        for (key, val) in obj {
            let display = match val.as_str() {
                Some(s) => {
                    let display_val = if is_path_key(key) {
                        decode_path(s)
                    } else {
                        s.to_string()
                    };
                    // Long strings get a code block
                    if display_val.contains('\n') || display_val.len() > 120 {
                        format!(
                            "<div class=\"tool-arg-key\">{}</div><pre class=\"tool-args\"><code>{}</code></pre>",
                            encode_text(key),
                            encode_text(&display_val)
                        )
                    } else {
                        format!(
                            "<div class=\"tool-arg-line\"><span class=\"tool-arg-key\">{}</span>: <code>{}</code></div>",
                            encode_text(key),
                            encode_text(&display_val)
                        )
                    }
                }
                None => {
                    let s = val.to_string();
                    format!(
                        "<div class=\"tool-arg-line\"><span class=\"tool-arg-key\">{}</span>: <code>{}</code></div>",
                        encode_text(key),
                        encode_text(&s)
                    )
                }
            };
            html.push_str(&display);
        }
        html.push_str("</div>");
        return html;
    }

    // Fallback: pretty-printed JSON
    let pretty = serde_json::to_string_pretty(&parsed).unwrap_or_else(|_| arguments.to_string());
    format!(
        "<pre class=\"tool-args\"><code>{}</code></pre>",
        encode_text(&pretty)
    )
}

fn render_tool_calls(tool_calls: &[ToolCallDelta]) -> String {
    let mut html = String::new();
    for tc in tool_calls {
        if let Some(func) = &tc.function {
            html.push_str("<div class=\"tool-call\">");
            html.push_str(&format!(
                "<div class=\"tool-call-header\">Tool Call: <strong>{}</strong></div>",
                encode_text(&func.name)
            ));
            if !func.arguments.is_empty() {
                html.push_str(&render_tool_args(&func.name, &func.arguments));
            }
            html.push_str("</div>");
        }
    }
    html
}

/// Extract a language token from a filename or path for syntax highlighting.
/// Returns the file extension (without dot) which can be fed to `highlight_code`.
fn lang_from_filename(name: &str) -> &str {
    // Take the portion after the last '/' or '\'
    let basename = name.rsplit(['/', '\\']).next().unwrap_or(name);
    // Special filenames
    match basename {
        "Dockerfile" | "dockerfile" => return "Dockerfile",
        "Makefile" | "makefile" | "GNUmakefile" => return "makefile",
        _ => {}
    }
    // Extract extension
    if let Some(ext_pos) = basename.rfind('.') {
        &basename[ext_pos + 1..]
    } else {
        ""
    }
}

fn render_context_items(items: &[ContextItem]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut html = String::new();
    html.push_str("<details class=\"context-items\"><summary>Context Items (");
    html.push_str(&items.len().to_string());
    html.push_str(")</summary>");
    for item in items {
        html.push_str("<div class=\"context-item\">");
        if !item.name.is_empty() {
            html.push_str(&format!(
                "<div class=\"context-name\">{}</div>",
                encode_text(&item.name)
            ));
        }
        if !item.description.is_empty() {
            html.push_str(&format!(
                "<div class=\"context-desc\">{}</div>",
                encode_text(&item.description)
            ));
        }
        if !item.content.is_empty() {
            let lang = lang_from_filename(&item.name);
            if !lang.is_empty() {
                if let Some(highlighted) = highlight_code(lang, &item.content) {
                    // Strip syntect's inline background style, use our CSS class
                    let patched = if let Some(start) = highlighted.find("style=\"") {
                        let after_style = start + 7;
                        if let Some(end_quote) = highlighted[after_style..].find('"') {
                            format!(
                                "{}class=\"highlighted-code context-content\"{}",
                                &highlighted[..start],
                                &highlighted[after_style + end_quote + 1..]
                            )
                        } else {
                            highlighted
                        }
                    } else {
                        highlighted
                    };
                    html.push_str(&patched);
                } else {
                    html.push_str("<pre class=\"context-content\"><code>");
                    html.push_str(&encode_text(&item.content));
                    html.push_str("</code></pre>");
                }
            } else {
                html.push_str("<pre class=\"context-content\"><code>");
                html.push_str(&encode_text(&item.content));
                html.push_str("</code></pre>");
            }
        }
        html.push_str("</div>");
    }
    html.push_str("</details>");
    html
}

/// Format a token count as a compact human-readable string (e.g. "1,234" or "12.3k").
fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else if n >= 1_000 {
        // Add comma separator
        let s = n.to_string();
        let mut out = String::new();
        for (i, c) in s.chars().rev().enumerate() {
            if i > 0 && i % 3 == 0 {
                out.push(',');
            }
            out.push(c);
        }
        out.chars().rev().collect()
    } else {
        n.to_string()
    }
}

/// Extract model title from a ChatHistoryItem's promptLogs.
fn item_model(item: &ChatHistoryItem) -> Option<&str> {
    item.prompt_logs.as_ref().and_then(|logs| {
        logs.iter().find_map(|log| {
            if !log.model_title.is_empty() {
                Some(log.model_title.as_str())
            } else {
                log.completion_options
                    .as_ref()
                    .filter(|opts| !opts.model.is_empty())
                    .map(|opts| opts.model.as_str())
            }
        })
    })
}

/// Render a single message (user, assistant, system, thinking).
/// Tool-result messages are rendered separately via `render_tool_result_inline`.
/// `running_tokens` is the cumulative token total up to and including this message.
/// `model_name` is the model used for this turn (shown on assistant messages).
fn render_message(
    item: &ChatHistoryItem,
    running_tokens: Option<(u64, u64)>,
    model_name: Option<&str>,
) -> String {
    let msg = &item.message;
    let role = msg.role.as_str();
    let cls = role_class(role);
    let label = role_label(role);
    let content_text = msg.content.text();
    let is_thinking = role == "thinking";

    // Build model badge for assistant messages
    let model_badge = if role == "assistant" {
        model_name
            .filter(|m| !m.is_empty())
            .map(|m| {
                format!(
                    " <span class=\"model-badge\" title=\"Model\">{}</span>",
                    encode_text(m)
                )
            })
            .unwrap_or_default()
    } else {
        String::new()
    };

    // Build token badge HTML
    let token_badge = if let Some(usage) = &msg.usage {
        let prompt_t = usage.prompt_tokens;
        let comp_t = usage.completion_tokens;
        let total = prompt_t + comp_t;
        let mut badge = format!(
            "<span class=\"token-badge\" title=\"Prompt: {} | Completion: {}\">{} tokens</span>",
            format_tokens(prompt_t),
            format_tokens(comp_t),
            format_tokens(total)
        );
        if let Some((run_p, run_c)) = running_tokens {
            badge.push_str(&format!(
                " <span class=\"token-running\" title=\"Running total — Prompt: {} | Completion: {}\">(cumul. {})</span>",
                format_tokens(run_p),
                format_tokens(run_c),
                format_tokens(run_p + run_c)
            ));
        }
        badge
    } else {
        String::new()
    };

    let mut html = String::new();
    html.push_str(&format!("<div class=\"message {cls}\">\n"));

    if is_thinking {
        // Thinking sections are collapsible (default **collapsed**)
        html.push_str("  <details class=\"thinking-details\">\n");
        html.push_str(&format!(
            "    <summary class=\"message-header\"><span class=\"role-label\">{label}</span>{model_badge}{token_badge}</summary>\n"
        ));
    } else {
        html.push_str(&format!(
            "  <div class=\"message-header\"><span class=\"role-label\">{label}</span>{model_badge}{token_badge}</div>\n"
        ));
    }

    // Context items (collapsed by default)
    html.push_str(&render_context_items(&item.context_items));

    // Main content — render all roles as markdown (users write code blocks,
    // lists, and formatting too)
    if !msg.content.is_empty() {
        html.push_str("  <div class=\"message-content\">\n");
        html.push_str(&format!("    {}\n", markdown_to_html(&content_text)));
        html.push_str("  </div>\n");
    }

    // Tool calls (rendered inside the assistant message div)
    if let Some(calls) = &msg.tool_calls {
        if !calls.is_empty() {
            html.push_str("  <div class=\"assistant-tool-group\">\n");
            html.push_str(&render_tool_calls(calls));
            // Placeholder for tool results — they'll be injected by render_session
            html.push_str("  </div>\n");
        }
    }

    if is_thinking {
        html.push_str("  </details>\n");
    }

    html.push_str("</div>\n");
    html
}

/// Render a tool result message as a collapsible `<details>` block.
/// `tool_name` is the name of the tool call this result responds to (if known).
/// `tool_args` is the JSON arguments string from the corresponding tool call,
/// used to infer file extensions for syntax highlighting.
fn render_tool_result_inline(
    item: &ChatHistoryItem,
    tool_name: &str,
    tool_args: &str,
) -> String {
    let content_text = item.message.content.text();
    let label = if tool_name.is_empty() {
        "Tool Result".to_string()
    } else {
        format!("Result: {}", tool_name)
    };

    let mut html = String::new();
    html.push_str("    <details class=\"tool-result-details\">\n");
    html.push_str(&format!(
        "      <summary>{}</summary>\n",
        encode_text(&label)
    ));
    html.push_str("      <div class=\"tool-result-content\">\n");

    if !content_text.is_empty() {
        // Check if content has ANSI escape codes
        let has_ansi = content_text.contains('\x1b');
        if has_ansi {
            html.push_str("        <pre class=\"tool-result-pre\"><code>");
            html.push_str(&ansi_to_html(&content_text));
            html.push_str("</code></pre>\n");
        } else {
            // For Read/read_file results, try to syntax-highlight based on
            // the file extension from the tool arguments.
            let highlighted = if tool_name == "Read" || tool_name == "read_file" {
                try_highlight_read_result(tool_args, &content_text)
            } else {
                None
            };
            if let Some(hl) = highlighted {
                html.push_str("        ");
                html.push_str(&hl);
                html.push('\n');
            } else {
                html.push_str("        <pre class=\"tool-result-pre\"><code>");
                html.push_str(&encode_text(&content_text));
                html.push_str("</code></pre>\n");
            }
        }
    }

    html.push_str("      </div>\n");
    html.push_str("    </details>\n");
    html
}

/// Attempt to syntax-highlight the content of a Read tool result.
/// Extracts the file path from the JSON arguments, strips line-number prefixes
/// (e.g. "     1\t..."), and applies syntax highlighting based on file extension.
fn try_highlight_read_result(tool_args: &str, content: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(tool_args).ok()?;
    let file_path = parsed
        .get("file_path")
        .or_else(|| parsed.get("filepath"))
        .and_then(|v| v.as_str())?;
    let lang = lang_from_filename(file_path);
    if lang.is_empty() {
        return None;
    }

    // Strip line-number prefixes that continue.dev's Read tool adds
    // (pattern: optional spaces + digits + tab)
    let stripped: String = content
        .lines()
        .map(|line| {
            // Match "  123\t..." style line-number prefixes
            if let Some(tab_pos) = line.find('\t') {
                let prefix = &line[..tab_pos];
                if prefix.trim().chars().all(|c| c.is_ascii_digit()) {
                    return &line[tab_pos + 1..];
                }
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n");

    let highlighted = highlight_code(lang, &stripped)?;

    // Patch syntect's inline style to use our CSS class
    let patched = if let Some(start) = highlighted.find("style=\"") {
        let after_style = start + 7;
        if let Some(end_quote) = highlighted[after_style..].find('"') {
            format!(
                "{}class=\"highlighted-code tool-result-pre\"{}",
                &highlighted[..start],
                &highlighted[after_style + end_quote + 1..]
            )
        } else {
            highlighted
        }
    } else {
        highlighted
    };

    Some(patched)
}

/// Extract tool descriptions from a system prompt.
/// Looks for patterns like "### ToolName\nDescription text"
/// Returns (name, short_desc, full_content) tuples.
fn extract_tool_descriptions(system_content: &str) -> Vec<(String, String, String)> {
    let mut tools: Vec<(String, String, String)> = Vec::new();
    let lines: Vec<&str> = system_content.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim();
        // Match ### ToolName headers (tools are listed under ### in system prompts)
        if line.starts_with("### ") && !line.starts_with("#### ") {
            let header = line.trim_start_matches('#').trim();
            // Check if it looks like a tool name: short, starts with uppercase
            if !header.is_empty()
                && header.len() < 40
                && !header.contains('.')
                && header.chars().next().is_some_and(|c| c.is_uppercase())
            {
                // Collect all lines until next ### or ## header
                let mut desc_lines = Vec::new();
                i += 1;
                while i < lines.len() {
                    let next = lines[i].trim();
                    // Stop at next ### or ## header
                    if next.starts_with("### ") || (next.starts_with("## ") && !next.starts_with("### ")) {
                        break;
                    }
                    desc_lines.push(lines[i]);
                    i += 1;
                }
                let full_content = desc_lines.join("\n").trim().to_string();
                // Short description: first non-empty line(s) up to 3 lines
                let short_desc = full_content
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .take(2)
                    .collect::<Vec<_>>()
                    .join(" ")
                    .trim()
                    .to_string();
                if !full_content.is_empty() {
                    tools.push((header.to_string(), short_desc, full_content));
                }
                continue;
            }
        }
        i += 1;
    }

    tools
}

/// Collect unique tool names from all tool calls in the session.
fn collect_tool_names(history: &[ChatHistoryItem]) -> Vec<String> {
    let mut names = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for item in history {
        if let Some(calls) = &item.message.tool_calls {
            for tc in calls {
                if let Some(func) = &tc.function {
                    if !func.name.is_empty() && seen.insert(func.name.clone()) {
                        names.push(func.name.clone());
                    }
                }
            }
        }
    }
    names
}

fn file_modified_date(path: &Path) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let dt: chrono::DateTime<chrono::Local> = modified.into();
    Some(dt.format("%Y-%m-%d %H:%M").to_string())
}

/// Extract the model name from the session history.
/// Looks through prompt logs for the first non-empty model title,
/// falling back to the completion_options.model field.
fn extract_model(session: &Session) -> Option<String> {
    for item in &session.history {
        if let Some(logs) = &item.prompt_logs {
            for log in logs {
                if !log.model_title.is_empty() {
                    return Some(log.model_title.clone());
                }
                if let Some(opts) = &log.completion_options {
                    if !opts.model.is_empty() {
                        return Some(opts.model.clone());
                    }
                }
            }
        }
    }
    None
}

/// Extract the system prompt from the session.
///
/// Prefer an explicit system-role message in the history — this is
/// the cleanest, most direct representation of the system prompt.
///
/// Fall back to `promptLogs[].prompt` which contains the full
/// text-formatted prompt sent to the model (all messages concatenated,
/// not just the system portion).
fn extract_system_prompt(session: &Session) -> (String, Option<usize>) {
    // Primary: first system message in history
    for (idx, item) in session.history.iter().enumerate() {
        if item.message.role == "system" {
            let text = item.message.content.text();
            if !text.is_empty() {
                return (text, Some(idx));
            }
        }
    }

    // Fallback: promptLogs[].prompt — may contain the full conversation
    // text, not just the system prompt, but is better than nothing.
    for item in &session.history {
        if let Some(logs) = &item.prompt_logs {
            for log in logs {
                if !log.prompt.is_empty() {
                    return (log.prompt.clone(), None);
                }
            }
        }
    }

    (String::new(), None)
}

/// Structured tool info extracted from toolCallStates or completionOptions.
struct ExtractedTool {
    name: String,
    description: String,
    system_message_prefix: String,
    parameters: Option<serde_json::Value>,
}

/// Extract tool definitions from toolCallStates[].tool across the session,
/// falling back to completionOptions.tools in promptLogs.
fn extract_tool_defs(session: &Session) -> Vec<ExtractedTool> {
    let mut tools: Vec<ExtractedTool> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // First: collect from toolCallStates
    for item in &session.history {
        if let Some(states) = &item.tool_call_states {
            for state in states {
                if let Some(tool_def) = &state.tool {
                    if let Some(func) = &tool_def.function {
                        if !func.name.is_empty() && seen.insert(func.name.clone()) {
                            tools.push(ExtractedTool {
                                name: func.name.clone(),

                                description: func.description.clone(),
                                system_message_prefix: tool_def
                                    .system_message_description
                                    .as_ref()
                                    .map_or(String::new(), |s| s.prefix.clone()),
                                parameters: func.parameters.clone(),
                            });
                        }
                    }
                }
            }
        }
    }

    // Second: collect from completionOptions.tools in promptLogs
    for item in &session.history {
        if let Some(logs) = &item.prompt_logs {
            for log in logs {
                if let Some(opts) = &log.completion_options {
                    if let Some(opt_tools) = &opts.tools {
                        for tool_def in opt_tools {
                            if let Some(func) = &tool_def.function {
                                if !func.name.is_empty() && seen.insert(func.name.clone()) {
                                    tools.push(ExtractedTool {
                                        name: func.name.clone(),
        
                                        description: func.description.clone(),
                                        system_message_prefix: tool_def
                                            .system_message_description
                                            .as_ref()
                                            .map_or(String::new(), |s| s.prefix.clone()),
                                        parameters: func.parameters.clone(),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    tools
}

/// Render tool info from structured ExtractedTool data (preferred over
/// text-parsing from the system prompt).
fn render_tools_reference_from_defs(
    tool_names: &[String],
    tool_defs: &[ExtractedTool],
    text_descriptions: &[(String, String, String)],
) -> String {
    if tool_names.is_empty() {
        return String::new();
    }

    // Build lookup from structured tool defs
    let def_map: std::collections::HashMap<&str, &ExtractedTool> = tool_defs
        .iter()
        .map(|t| (t.name.as_str(), t))
        .collect();

    // Build fallback lookup from text-parsed descriptions
    let text_short_map: std::collections::HashMap<&str, &str> = text_descriptions
        .iter()
        .map(|(name, short, _)| (name.as_str(), short.as_str()))
        .collect();
    let text_full_map: std::collections::HashMap<&str, &str> = text_descriptions
        .iter()
        .map(|(name, _, full)| (name.as_str(), full.as_str()))
        .collect();

    let mut html = String::new();
    html.push_str("<details class=\"tools-reference\">\n");
    html.push_str("  <summary>Tools Used (");
    html.push_str(&tool_names.len().to_string());
    html.push_str(")</summary>\n");
    html.push_str("  <div class=\"tools-reference-content\">\n");

    for name in tool_names {
        if let Some(tool) = def_map.get(name.as_str()) {
            // Use structured tool definition
            let has_content = !tool.description.is_empty()
                || !tool.system_message_prefix.is_empty()
                || tool.parameters.is_some();

            if has_content {
                html.push_str("    <details class=\"tool-ref-item-details\">\n");
                html.push_str(&format!(
                    "      <summary class=\"tool-ref-item-summary\"><span class=\"tool-ref-name\">{}</span>",
                    encode_text(name)
                ));
                // Short description: prefer systemMessageDescription.prefix, else first line of description
                let short = if !tool.system_message_prefix.is_empty() {
                    &tool.system_message_prefix
                } else if !tool.description.is_empty() {
                    tool.description.lines().next().unwrap_or("")
                } else {
                    ""
                };
                if !short.is_empty() {
                    html.push_str(&format!(
                        " <span class=\"tool-ref-short\">&mdash; {}</span>",
                        encode_text(short)
                    ));
                }
                html.push_str("</summary>\n");
                html.push_str("      <div class=\"tool-ref-full\">\n");

                // Full description
                if !tool.description.is_empty() {
                    html.push_str(&format!(
                        "        {}\n",
                        markdown_to_html(&tool.description)
                    ));
                }

                // Parameters from JSON Schema
                if let Some(params) = &tool.parameters {
                    html.push_str(&render_parameters_schema(params));
                }

                html.push_str("      </div>\n");
                html.push_str("    </details>\n");
            } else {
                // No content, simple item
                html.push_str("    <div class=\"tool-ref-item\">\n");
                html.push_str(&format!(
                    "      <div class=\"tool-ref-name\">{}</div>\n",
                    encode_text(name)
                ));
                html.push_str("    </div>\n");
            }
        } else {
            // Fall back to text-parsed descriptions
            let has_full = text_full_map
                .get(name.as_str())
                .is_some_and(|f| !f.is_empty());
            if has_full {
                html.push_str("    <details class=\"tool-ref-item-details\">\n");
                html.push_str(&format!(
                    "      <summary class=\"tool-ref-item-summary\"><span class=\"tool-ref-name\">{}</span>",
                    encode_text(name)
                ));
                if let Some(short) = text_short_map.get(name.as_str()) {
                    html.push_str(&format!(
                        " <span class=\"tool-ref-short\">&mdash; {}</span>",
                        encode_text(short)
                    ));
                }
                html.push_str("</summary>\n");
                html.push_str("      <div class=\"tool-ref-full\">\n");
                if let Some(full) = text_full_map.get(name.as_str()) {
                    html.push_str(&format!("        {}\n", markdown_to_html(full)));
                }
                html.push_str("      </div>\n");
                html.push_str("    </details>\n");
            } else {
                html.push_str("    <div class=\"tool-ref-item\">\n");
                html.push_str(&format!(
                    "      <div class=\"tool-ref-name\">{}</div>\n",
                    encode_text(name)
                ));
                if let Some(short) = text_short_map.get(name.as_str()) {
                    html.push_str(&format!(
                        "      <div class=\"tool-ref-desc\">{}</div>\n",
                        encode_text(short)
                    ));
                }
                html.push_str("    </div>\n");
            }
        }
    }

    html.push_str("  </div>\n");
    html.push_str("</details>\n");
    html
}

/// Render a JSON Schema parameters object as an HTML list.
fn render_parameters_schema(params: &serde_json::Value) -> String {
    let mut html = String::new();
    if let Some(props) = params.get("properties").and_then(|p| p.as_object()) {
        let required: Vec<&str> = params
            .get("required")
            .and_then(|r| r.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        html.push_str("        <div class=\"tool-params\">\n");
        html.push_str(
            "          <div class=\"tool-params-header\">Parameters</div>\n",
        );
        html.push_str("          <ul class=\"tool-params-list\">\n");
        for (prop_name, prop_val) in props {
            let type_str = prop_val
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("any");
            let is_required = required.contains(&prop_name.as_str());
            let req_label = if is_required { ", required" } else { "" };
            let desc = prop_val
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("");
            html.push_str(&format!(
                "            <li><code>{}</code> <span class=\"tool-param-type\">({type_str}{req_label})</span>",
                encode_text(prop_name)
            ));
            if !desc.is_empty() {
                html.push_str(&format!(": {}", encode_text(desc)));
            }
            // Show enum values if present
            if let Some(enum_vals) = prop_val.get("enum").and_then(|e| e.as_array()) {
                let vals: Vec<String> = enum_vals
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| format!("<code>{}</code>", encode_text(s))))
                    .collect();
                if !vals.is_empty() {
                    html.push_str(&format!(" [{}]", vals.join(", ")));
                }
            }
            html.push_str("</li>\n");
        }
        html.push_str("          </ul>\n");
        html.push_str("        </div>\n");
    }
    html
}

fn render_session(session: &Session, source_path: Option<&Path>) -> String {
    let title = if session.title.is_empty() {
        "Untitled Session"
    } else {
        &session.title
    };

    // --- Extract system prompt (prefer system message, fallback to promptLogs) ---
    let (system_prompt_content, system_prompt_idx) = extract_system_prompt(session);

    let system_prompt_html = if !system_prompt_content.is_empty() {
        format!(
            "<details class=\"system-prompt\">\n  \
                <summary>System Prompt</summary>\n  \
                <div class=\"system-prompt-content\">\n    {}\n  </div>\n\
             </details>\n",
            markdown_to_html(&system_prompt_content)
        )
    } else {
        String::new()
    };

    // --- Collect tool names and descriptions ---
    let tool_names = collect_tool_names(&session.history);

    // Try structured tool definitions first (from toolCallStates / completionOptions)
    let tool_defs = extract_tool_defs(session);

    // Also parse text-based descriptions from system prompt as fallback
    let text_descriptions = if !system_prompt_content.is_empty() {
        extract_tool_descriptions(&system_prompt_content)
    } else {
        Vec::new()
    };

    let tools_reference_html =
        render_tools_reference_from_defs(&tool_names, &tool_defs, &text_descriptions);

    // --- Check if any messages have token usage data ---
    let has_any_usage = session.history.iter().any(|item| item.message.usage.is_some());

    // --- Render messages with tool-call/tool-result grouping ---
    let mut messages_html = String::new();
    let mut user_count = 0u32;
    let mut assistant_count = 0u32;
    let mut running_prompt_tokens: u64 = 0;
    let mut running_completion_tokens: u64 = 0;
    let mut last_model: Option<String> = None; // tracks latest model from promptLogs
    let history = &session.history;
    let len = history.len();
    let mut i = 0;

    while i < len {
        let item = &history[i];
        let role = item.message.role.as_str();

        // Skip the system prompt we already extracted
        if Some(i) == system_prompt_idx {
            i += 1;
            continue;
        }

        // Track the latest model from promptLogs on any item
        if let Some(m) = item_model(item) {
            last_model = Some(m.to_string());
        }

        // Update running token totals if this message has usage data
        if let Some(usage) = &item.message.usage {
            running_prompt_tokens += usage.prompt_tokens;
            running_completion_tokens += usage.completion_tokens;
        }
        let running = if has_any_usage && item.message.usage.is_some() {
            Some((running_prompt_tokens, running_completion_tokens))
        } else {
            None
        };

        match role {
            "user" => {
                user_count += 1;
                messages_html.push_str(&render_message(item, running, None));
                i += 1;
            }
            "assistant" => {
                assistant_count += 1;
                let mut msg_html =
                    render_message(item, running, last_model.as_deref());

                // Count how many tool calls this assistant message has
                let call_count = item
                    .message
                    .tool_calls
                    .as_ref()
                    .map_or(0, |c| c.len());

                if call_count > 0 {
                    // Collect tool call names and args for matching with results
                    let call_info: Vec<(String, String)> = item
                        .message
                        .tool_calls
                        .as_ref()
                        .unwrap()
                        .iter()
                        .filter_map(|tc| {
                            tc.function
                                .as_ref()
                                .map(|f| (f.name.clone(), f.arguments.clone()))
                        })
                        .collect();

                    // Look ahead for consecutive tool-result messages
                    let mut tool_results_html = String::new();
                    let mut result_idx = 0;
                    let mut j = i + 1;
                    while j < len && history[j].message.role == "tool" {
                        let (tool_name, tool_args) = if result_idx < call_info.len() {
                            (
                                call_info[result_idx].0.as_str(),
                                call_info[result_idx].1.as_str(),
                            )
                        } else {
                            ("", "")
                        };
                        tool_results_html.push_str(&render_tool_result_inline(
                            &history[j],
                            tool_name,
                            tool_args,
                        ));
                        result_idx += 1;
                        j += 1;
                    }

                    // Also swallow any thinking messages that appear between
                    // the tool results and the next non-thinking/tool message
                    // (they belong to this assistant turn)

                    if !tool_results_html.is_empty() {
                        // Inject tool results inside the assistant-tool-group div
                        // (right before the closing </div> of assistant-tool-group)
                        let injection_point = "  </div>\n</div>\n";
                        if let Some(pos) = msg_html.rfind(injection_point) {
                            msg_html.insert_str(pos + 8, &tool_results_html);
                            // pos + 8 = just inside the assistant-tool-group </div>
                        } else {
                            // Fallback: append before closing message div
                            let close = "</div>\n";
                            if let Some(pos) = msg_html.rfind(close) {
                                msg_html.insert_str(pos, &tool_results_html);
                            }
                        }
                    }

                    i = j; // skip past consumed tool messages
                } else {
                    i += 1;
                }

                messages_html.push_str(&msg_html);
            }
            "thinking" => {
                messages_html.push_str(&render_message(item, running, None));
                i += 1;
            }
            "tool" => {
                // Orphaned tool result (not preceded by an assistant with tool calls)
                // Render standalone with collapsible wrapper
                let mut html = String::new();
                html.push_str("<div class=\"message tool-result\">\n");
                html.push_str(&render_tool_result_inline(item, "", ""));
                html.push_str("</div>\n");
                messages_html.push_str(&html);
                i += 1;
            }
            _ => {
                // system (non-first) or other roles — render normally
                messages_html.push_str(&render_message(item, running, None));
                i += 1;
            }
        }
    }

    let date_str = session
        .date_created
        .as_deref()
        .map(|s| s.to_string())
        .or_else(|| source_path.and_then(file_modified_date))
        .unwrap_or_else(|| "Unknown date".to_string());

    let workspace_decoded = decode_path(&session.workspace_directory);
    let workspace = if workspace_decoded.is_empty() {
        "N/A".to_string()
    } else {
        workspace_decoded
    };

    let model = extract_model(session);
    let model_meta = match &model {
        Some(m) => format!(
            "\n      <span class=\"meta-item\">Model: <code>{}</code></span>",
            encode_text(m)
        ),
        None => String::new(),
    };

    let tokens_meta = if has_any_usage {
        let total = running_prompt_tokens + running_completion_tokens;
        format!(
            "\n      <span class=\"meta-item\">Tokens: {} total (prompt: {} &middot; completion: {})</span>",
            format_tokens(total),
            format_tokens(running_prompt_tokens),
            format_tokens(running_completion_tokens)
        )
    } else {
        String::new()
    };

    format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
{CSS}
</head>
<body>
<button class="theme-toggle" aria-label="Toggle theme"></button>
<div class="container">
  <header class="session-header">
    <h1>{title}</h1>
    <div class="session-meta">
      <span class="meta-item">Session: <code>{session_id}</code></span>
      <span class="meta-item">Date: {date}</span>{model_meta}
      <span class="meta-item">Workspace: <code>{workspace}</code></span>
      <span class="meta-item">{user_count} user messages &middot; {assistant_count} assistant messages</span>{tokens_meta}
    </div>
  </header>
{system_prompt}
{tools_reference}
  <main class="transcript">
{messages}
  </main>
  <footer>
    <p>Generated by <strong>continue-transcripts</strong></p>
  </footer>
</div>
{JS}
</body>
</html>"##,
        title = encode_text(title),
        CSS = CSS,
        JS = JS,
        session_id = encode_text(&session.session_id),
        date = encode_text(&date_str),
        model_meta = model_meta,
        tokens_meta = tokens_meta,
        workspace = encode_text(&workspace),
        user_count = user_count,
        assistant_count = assistant_count,
        system_prompt = system_prompt_html,
        tools_reference = tools_reference_html,
        messages = messages_html,
    )
}

// ---------------------------------------------------------------------------
// Embedded CSS
// ---------------------------------------------------------------------------

const CSS: &str = r#"<style>
:root {
  --bg: #f3f4f6;
  --card-bg: #ffffff;
  --text: #1f2937;
  --text-muted: #6b7280;
  --border: #e5e7eb;

  --user-bg: #dbeafe;
  --user-border: #2563eb;
  --user-label: #1d4ed8;

  --assistant-bg: #ffffff;
  --assistant-border: #6b7280;
  --assistant-label: #374151;

  --system-bg: #fef3c7;
  --system-border: #d97706;
  --system-label: #92400e;

  --tool-result-bg: #d1fae5;
  --tool-result-border: #059669;
  --tool-result-label: #065f46;

  --thinking-bg: #fef9c3;
  --thinking-border: #eab308;
  --thinking-label: #854d0e;

  --tool-call-bg: #ede9fe;
  --tool-call-border: #7c3aed;

  --code-bg: #1e293b;
  --code-text: #e2e8f0;
  --code-border: #334155;

  --inline-code-bg: #f1f5f9;
  --inline-code-text: #0f172a;

  --diff-add-bg: #1a2e1a;
  --diff-add-text: #7ee787;
  --diff-remove-bg: #2e1a1a;
  --diff-remove-text: #f47067;
}

* { box-sizing: border-box; margin: 0; padding: 0; }

body {
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
  background: var(--bg);
  color: var(--text);
  line-height: 1.6;
  font-size: 15px;
}

.container {
  max-width: 860px;
  margin: 0 auto;
  padding: 24px 16px;
}

/* ----- Header ----- */
.session-header {
  background: var(--card-bg);
  border: 1px solid var(--border);
  border-radius: 12px;
  padding: 24px;
  margin-bottom: 24px;
}

.session-header h1 {
  font-size: 1.5rem;
  margin-bottom: 12px;
  color: var(--text);
}

.session-meta {
  display: flex;
  flex-wrap: wrap;
  gap: 8px 20px;
  font-size: 0.85rem;
  color: var(--text-muted);
}

.session-meta code {
  background: var(--inline-code-bg);
  padding: 1px 5px;
  border-radius: 4px;
  font-size: 0.82rem;
}

/* ----- Messages ----- */
.message {
  border-radius: 10px;
  padding: 16px 20px;
  margin-bottom: 16px;
  border-left: 4px solid transparent;
  position: relative;
}

.message.user {
  background: var(--user-bg);
  border-left-color: var(--user-border);
}
.message.user .role-label { color: var(--user-label); }

.message.assistant {
  background: var(--assistant-bg);
  border: 1px solid var(--border);
  border-left: 4px solid var(--assistant-border);
}
.message.assistant .role-label { color: var(--assistant-label); }

.message.system {
  background: var(--system-bg);
  border-left-color: var(--system-border);
}
.message.system .role-label { color: var(--system-label); }

.message.tool-result {
  background: var(--tool-result-bg);
  border-left-color: var(--tool-result-border);
}
.message.tool-result .role-label { color: var(--tool-result-label); }

.message.thinking {
  background: var(--thinking-bg);
  border-left-color: var(--thinking-border);
}
.message.thinking .role-label { color: var(--thinking-label); }

.thinking-details { width: 100%; }
.thinking-details > summary {
  cursor: pointer;
  user-select: none;
  list-style: none;
}
.thinking-details > summary::-webkit-details-marker { display: none; }
.thinking-details > summary .role-label::before {
  content: '\25BC  ';
  font-size: 0.65rem;
  vertical-align: 1px;
}
.thinking-details:not([open]) > summary .role-label::before {
  content: '\25B6  ';
}

.message-header {
  margin-bottom: 10px;
}

.role-label {
  font-weight: 700;
  font-size: 0.78rem;
  text-transform: uppercase;
  letter-spacing: 0.05em;
}

/* ----- Model badge ----- */
.model-badge {
  display: inline-block;
  font-size: 0.7rem;
  font-weight: 500;
  color: var(--text-muted);
  background: rgba(0,0,0,0.05);
  padding: 1px 8px;
  border-radius: 10px;
  margin-left: 8px;
  vertical-align: middle;
  font-style: italic;
}

/* ----- Token badges ----- */
.token-badge {
  display: inline-block;
  font-size: 0.72rem;
  font-weight: 500;
  color: var(--text-muted);
  background: rgba(0,0,0,0.06);
  padding: 1px 8px;
  border-radius: 10px;
  margin-left: 8px;
  vertical-align: middle;
  cursor: help;
}

.token-running {
  font-size: 0.7rem;
  color: var(--text-muted);
  opacity: 0.7;
  cursor: help;
}

/* ----- Content ----- */
.message-content {
  overflow-wrap: break-word;
  word-break: break-word;
}

.message-content p {
  margin-bottom: 0.75em;
  white-space: pre-wrap;
}

.message-content p:last-child { margin-bottom: 0; }

.message-content h1,
.message-content h2,
.message-content h3,
.message-content h4 {
  margin-top: 1em;
  margin-bottom: 0.5em;
}

.message-content h1 { font-size: 1.3rem; }
.message-content h2 { font-size: 1.15rem; }
.message-content h3 { font-size: 1.05rem; }

.message-content ul,
.message-content ol {
  margin: 0.5em 0 0.75em 1.5em;
}

.message-content li { margin-bottom: 0.3em; }

.message-content blockquote {
  border-left: 3px solid var(--border);
  padding-left: 12px;
  color: var(--text-muted);
  margin: 0.75em 0;
}

.message-content table {
  border-collapse: collapse;
  margin: 0.75em 0;
  font-size: 0.9em;
  width: 100%;
}

.message-content th,
.message-content td {
  border: 1px solid var(--border);
  padding: 6px 10px;
  text-align: left;
}

.message-content th {
  background: var(--bg);
  font-weight: 600;
}

/* ----- Code blocks ----- */
.message-content pre {
  background: var(--code-bg);
  color: var(--code-text);
  padding: 14px 16px;
  border-radius: 8px;
  border: 1px solid var(--code-border);
  overflow-x: auto;
  margin: 0.75em 0;
  font-size: 0.88rem;
  line-height: 1.5;
}

.message-content pre code {
  background: none;
  color: inherit;
  padding: 0;
  border-radius: 0;
  font-size: inherit;
}

pre.highlighted-code {
  background: var(--code-bg);
  color: var(--code-text);
  padding: 14px 16px;
  border-radius: 8px;
  border: 1px solid var(--code-border);
  overflow-x: auto;
  margin: 0.75em 0;
  font-size: 0.88rem;
  line-height: 1.5;
  font-family: "SF Mono", "Cascadia Code", "Fira Code", Menlo, Consolas, monospace;
}

.message-content code {
  background: var(--inline-code-bg);
  color: var(--inline-code-text);
  padding: 2px 6px;
  border-radius: 4px;
  font-size: 0.88em;
  font-family: "SF Mono", "Cascadia Code", "Fira Code", Menlo, Consolas, monospace;
}

/* ----- System prompt (top of page, collapsed) ----- */
.system-prompt {
  background: var(--system-bg);
  border: 1px solid var(--system-border);
  border-radius: 10px;
  padding: 16px 20px;
  margin-bottom: 16px;
}

.system-prompt > summary {
  cursor: pointer;
  font-weight: 700;
  font-size: 0.85rem;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--system-label);
  user-select: none;
  list-style: none;
}

.system-prompt > summary::-webkit-details-marker { display: none; }
.system-prompt > summary::before {
  content: '\25B6  ';
  font-size: 0.65rem;
  vertical-align: 1px;
}
.system-prompt[open] > summary::before {
  content: '\25BC  ';
}

.system-prompt-content {
  margin-top: 12px;
  font-size: 0.88rem;
  max-height: 500px;
  overflow-y: auto;
}

/* ----- Tools reference panel ----- */
.tools-reference {
  background: var(--card-bg);
  border: 1px solid var(--border);
  border-radius: 10px;
  padding: 16px 20px;
  margin-bottom: 16px;
}

.tools-reference > summary {
  cursor: pointer;
  font-weight: 700;
  font-size: 0.85rem;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--tool-call-border);
  user-select: none;
  list-style: none;
}

.tools-reference > summary::-webkit-details-marker { display: none; }
.tools-reference > summary::before {
  content: '\25B6  ';
  font-size: 0.65rem;
  vertical-align: 1px;
}
.tools-reference[open] > summary::before {
  content: '\25BC  ';
}

.tools-reference-content {
  margin-top: 12px;
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.tool-ref-item {
  background: var(--bg);
  border: 1px solid var(--border);
  border-radius: 6px;
  padding: 10px 12px;
}

.tool-ref-name {
  font-weight: 700;
  font-size: 0.88rem;
  color: var(--tool-call-border);
  margin-bottom: 4px;
}

.tool-ref-desc {
  font-size: 0.82rem;
  color: var(--text-muted);
  line-height: 1.4;
}

/* Individual tool detail collapsible sections */
.tool-ref-item-details {
  background: var(--bg);
  border: 1px solid var(--border);
  border-radius: 6px;
  overflow: hidden;
}

.tool-ref-item-details > summary {
  cursor: pointer;
  padding: 10px 12px;
  user-select: none;
  list-style: none;
}

.tool-ref-item-details > summary::-webkit-details-marker { display: none; }
.tool-ref-item-details > summary::before {
  content: '\25B6  ';
  font-size: 0.6rem;
  vertical-align: 1px;
  color: var(--text-muted);
}
.tool-ref-item-details[open] > summary::before {
  content: '\25BC  ';
}

.tool-ref-short {
  font-size: 0.82rem;
  color: var(--text-muted);
  font-weight: 400;
}

.tool-ref-full {
  padding: 0 12px 12px;
  font-size: 0.85rem;
  line-height: 1.5;
  border-top: 1px solid var(--border);
}

.tool-ref-full p { margin-bottom: 0.5em; }
.tool-ref-full p:last-child { margin-bottom: 0; }
.tool-ref-full ul, .tool-ref-full ol { margin: 0.3em 0 0.5em 1.5em; }
.tool-ref-full li { margin-bottom: 0.2em; }
.tool-ref-full code {
  background: var(--inline-code-bg);
  padding: 1px 5px;
  border-radius: 3px;
  font-size: 0.85em;
}
.tool-ref-full pre {
  background: var(--code-bg);
  color: var(--code-text);
  padding: 10px 14px;
  border-radius: 6px;
  border: 1px solid var(--code-border);
  overflow-x: auto;
  margin: 0.5em 0;
  font-size: 0.82rem;
}

/* ----- Tool parameters (from JSON Schema) ----- */
.tool-params {
  margin-top: 0.5em;
  padding-top: 0.5em;
  border-top: 1px solid var(--border);
}

.tool-params-header {
  font-weight: 600;
  font-size: 0.82rem;
  margin-bottom: 0.3em;
  color: var(--text);
}

.tool-params-list {
  margin: 0.2em 0 0.5em 1.5em;
  font-size: 0.82rem;
  line-height: 1.5;
}

.tool-params-list li { margin-bottom: 0.2em; }
.tool-params-list code {
  background: var(--inline-code-bg);
  padding: 1px 5px;
  border-radius: 3px;
  font-size: 0.85em;
}

.tool-param-type {
  color: var(--text-muted);
  font-size: 0.8em;
}

/* ----- Tool calls ----- */
.tool-call {
  background: var(--tool-call-bg);
  border: 1px solid var(--tool-call-border);
  border-radius: 8px;
  padding: 12px 16px;
  margin-top: 12px;
}

.tool-call-header {
  font-size: 0.82rem;
  color: var(--tool-call-border);
  margin-bottom: 8px;
  text-transform: uppercase;
  letter-spacing: 0.04em;
}

.tool-args {
  background: var(--code-bg) !important;
  color: var(--code-text) !important;
  padding: 10px 14px !important;
  border-radius: 6px;
  font-size: 0.82rem !important;
  max-height: 300px;
  overflow-y: auto;
}

.tool-args-bare {
  font-family: "SF Mono", "Cascadia Code", "Fira Code", Menlo, Consolas, monospace;
}

.tool-prompt {
  color: var(--tool-call-border);
  font-weight: bold;
  user-select: none;
}

.tool-args-kv {
  font-size: 0.88rem;
  padding: 4px 0;
  font-family: "SF Mono", "Cascadia Code", "Fira Code", Menlo, Consolas, monospace;
}

.tool-args-kv code {
  background: var(--inline-code-bg);
  padding: 1px 5px;
  border-radius: 3px;
  font-size: 0.85em;
}

.tool-args-kv-group {
  font-size: 0.88rem;
  padding: 4px 0;
}

.tool-arg-key {
  font-weight: 600;
  color: var(--tool-call-border);
  font-size: 0.82rem;
}

.tool-arg-line {
  padding: 2px 0;
  font-family: "SF Mono", "Cascadia Code", "Fira Code", Menlo, Consolas, monospace;
}

.tool-arg-line code {
  background: var(--inline-code-bg);
  padding: 1px 5px;
  border-radius: 3px;
  font-size: 0.85em;
}

/* ----- Diff rendering for Edit tool calls ----- */
.tool-diff {
  background: var(--code-bg) !important;
  color: var(--code-text) !important;
  padding: 10px 14px !important;
  border-radius: 6px;
  font-size: 0.82rem !important;
  max-height: 400px;
  overflow-y: auto;
  margin-top: 6px;
}

.diff-remove {
  display: block;
  color: var(--diff-remove-text);
  background: var(--diff-remove-bg);
  padding: 0 4px;
  margin: 0 -4px;
}

.diff-add {
  display: block;
  color: var(--diff-add-text);
  background: var(--diff-add-bg);
  padding: 0 4px;
  margin: 0 -4px;
}

/* ----- Assistant tool group (nesting) ----- */
.assistant-tool-group {
  margin-top: 12px;
  margin-left: 16px;
  padding-left: 12px;
  border-left: 2px solid var(--tool-call-border);
}

/* ----- Tool result details (collapsible, default collapsed) ----- */
.tool-result-details {
  margin-top: 10px;
  border: 1px solid var(--tool-result-border);
  border-radius: 6px;
  overflow: hidden;
}

.tool-result-details > summary {
  cursor: pointer;
  padding: 8px 12px;
  background: var(--tool-result-bg);
  font-weight: 600;
  font-size: 0.82rem;
  color: var(--tool-result-label);
  user-select: none;
  list-style: none;
}

.tool-result-details > summary::-webkit-details-marker { display: none; }
.tool-result-details > summary::before {
  content: '\25B6  ';
  font-size: 0.6rem;
  vertical-align: 1px;
}
.tool-result-details[open] > summary::before {
  content: '\25BC  ';
}

.tool-result-content {
  padding: 0;
}

.tool-result-pre {
  background: var(--code-bg) !important;
  color: var(--code-text) !important;
  padding: 10px 14px !important;
  margin: 0 !important;
  border-radius: 0;
  border: none !important;
  font-size: 0.82rem !important;
  max-height: 400px;
  overflow-y: auto;
}

/* ----- Context items ----- */
.context-items {
  margin-bottom: 10px;
  font-size: 0.85rem;
}

.context-items summary {
  cursor: pointer;
  color: var(--text-muted);
  font-weight: 600;
  user-select: none;
}

.context-item {
  background: var(--bg);
  border: 1px solid var(--border);
  border-radius: 6px;
  padding: 10px 12px;
  margin-top: 8px;
}

.context-name {
  font-weight: 600;
  font-size: 0.85rem;
  margin-bottom: 2px;
}

.context-desc {
  color: var(--text-muted);
  font-size: 0.82rem;
  margin-bottom: 6px;
}

.context-content {
  background: var(--code-bg) !important;
  color: var(--code-text) !important;
  padding: 8px 12px !important;
  border-radius: 6px;
  font-size: 0.8rem !important;
  max-height: 500px;
  overflow-y: auto;
}

/* ----- Footer ----- */
footer {
  text-align: center;
  padding: 24px 0 8px;
  font-size: 0.8rem;
  color: var(--text-muted);
}

/* ----- Dark mode ----- */
@media (prefers-color-scheme: dark) {
  :root:not([data-theme="light"]) {
    --bg: #111827;
    --card-bg: #1f2937;
    --text: #f3f4f6;
    --text-muted: #9ca3af;
    --border: #374151;

    --user-bg: #1e3a5f;
    --user-border: #3b82f6;
    --user-label: #93c5fd;

    --assistant-bg: #1f2937;
    --assistant-border: #9ca3af;
    --assistant-label: #d1d5db;

    --system-bg: #422006;
    --system-border: #f59e0b;
    --system-label: #fcd34d;

    --tool-result-bg: #064e3b;
    --tool-result-border: #10b981;
    --tool-result-label: #6ee7b7;

    --thinking-bg: #422006;
    --thinking-border: #eab308;
    --thinking-label: #fde68a;

    --tool-call-bg: #2e1065;
    --tool-call-border: #a78bfa;

    --code-bg: #0f172a;
    --code-text: #e2e8f0;
    --code-border: #1e293b;

    --inline-code-bg: #374151;
    --inline-code-text: #e5e7eb;

    --diff-add-bg: #1a2e1a;
    --diff-add-text: #7ee787;
    --diff-remove-bg: #2e1a1a;
    --diff-remove-text: #f47067;
  }
}

[data-theme="dark"] {
    --bg: #111827;
    --card-bg: #1f2937;
    --text: #f3f4f6;
    --text-muted: #9ca3af;
    --border: #374151;

    --user-bg: #1e3a5f;
    --user-border: #3b82f6;
    --user-label: #93c5fd;

    --assistant-bg: #1f2937;
    --assistant-border: #9ca3af;
    --assistant-label: #d1d5db;

    --system-bg: #422006;
    --system-border: #f59e0b;
    --system-label: #fcd34d;

    --tool-result-bg: #064e3b;
    --tool-result-border: #10b981;
    --tool-result-label: #6ee7b7;

    --thinking-bg: #422006;
    --thinking-border: #eab308;
    --thinking-label: #fde68a;

    --tool-call-bg: #2e1065;
    --tool-call-border: #a78bfa;

    --code-bg: #0f172a;
    --code-text: #e2e8f0;
    --code-border: #1e293b;

    --inline-code-bg: #374151;
    --inline-code-text: #e5e7eb;

    --diff-add-bg: #1a2e1a;
    --diff-add-text: #7ee787;
    --diff-remove-bg: #2e1a1a;
    --diff-remove-text: #f47067;
}

/* ----- Theme toggle button ----- */
.theme-toggle {
  position: fixed;
  top: 12px;
  right: 12px;
  z-index: 100;
  background: var(--card-bg);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 6px 10px;
  cursor: pointer;
  font-size: 1rem;
  line-height: 1;
  color: var(--text);
  box-shadow: 0 1px 3px rgba(0,0,0,0.1);
}
.theme-toggle:hover { opacity: 0.8; }

/* ----- Responsive ----- */
@media (max-width: 600px) {
  .container { padding: 12px 8px; }
  .session-header { padding: 16px; }
  .message { padding: 12px 14px; }
  .session-meta { flex-direction: column; gap: 4px; }
}
</style>"#;

// ---------------------------------------------------------------------------
// Embedded JS  (syntax highlighting for code blocks)
// ---------------------------------------------------------------------------

const JS: &str = r#"<script>
document.addEventListener('DOMContentLoaded', function() {
  // Theme toggle
  (function() {
    var html = document.documentElement;
    var stored = localStorage.getItem('ct-theme');
    if (stored) html.setAttribute('data-theme', stored);
    var btn = document.querySelector('.theme-toggle');
    if (btn) {
      function updateIcon() {
        var theme = html.getAttribute('data-theme');
        if (!theme) {
          // auto — check OS preference
          var dark = window.matchMedia('(prefers-color-scheme: dark)').matches;
          btn.textContent = dark ? '\u2600\uFE0F' : '\uD83C\uDF19';
          btn.title = 'Theme: auto (' + (dark ? 'dark' : 'light') + '). Click to toggle.';
        } else if (theme === 'dark') {
          btn.textContent = '\u2600\uFE0F';
          btn.title = 'Theme: dark. Click for light.';
        } else {
          btn.textContent = '\uD83C\uDF19';
          btn.title = 'Theme: light. Click for dark.';
        }
      }
      btn.addEventListener('click', function() {
        var current = html.getAttribute('data-theme');
        if (!current) {
          var dark = window.matchMedia('(prefers-color-scheme: dark)').matches;
          var next = dark ? 'light' : 'dark';
        } else if (current === 'dark') {
          var next = 'light';
        } else {
          var next = 'dark';
        }
        html.setAttribute('data-theme', next);
        localStorage.setItem('ct-theme', next);
        updateIcon();
      });
      updateIcon();
      window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', updateIcon);
    }
  })();

  // Add language labels to fenced code blocks
  document.querySelectorAll('pre > code[class*="language-"]').forEach(function(el) {
    var lang = el.className.match(/language-(\S+)/);
    if (lang) {
      var label = document.createElement('div');
      label.textContent = lang[1];
      label.style.cssText = 'position:absolute;top:6px;right:10px;font-size:0.7rem;color:#94a3b8;text-transform:uppercase;letter-spacing:0.05em;';
      el.parentElement.style.position = 'relative';
      el.parentElement.appendChild(label);
    }
  });

  // Truncation for long blocks
  document.querySelectorAll('.message-content pre, .tool-args, .context-content, .tool-result-pre').forEach(function(el) {
    if (el.scrollHeight > 350) {
      el.style.maxHeight = '300px';
      el.style.overflow = 'hidden';
      el.style.position = 'relative';

      var btn = document.createElement('button');
      btn.textContent = 'Show more';
      btn.style.cssText = 'display:block;margin:6px auto 0;padding:4px 16px;border:1px solid var(--border);border-radius:6px;background:var(--card-bg);color:var(--text-muted);cursor:pointer;font-size:0.8rem;';
      btn.addEventListener('click', function() {
        if (el.style.maxHeight === '300px') {
          el.style.maxHeight = 'none';
          el.style.overflow = 'auto';
          btn.textContent = 'Show less';
        } else {
          el.style.maxHeight = '300px';
          el.style.overflow = 'hidden';
          btn.textContent = 'Show more';
        }
      });
      el.parentElement.insertBefore(btn, el.nextSibling);
    }
  });

  // Expand/Collapse all toggle for tool details
  var transcript = document.querySelector('.transcript');
  if (transcript) {
    var detailsEls = transcript.querySelectorAll('details.tool-result-details, details.thinking-details');
    if (detailsEls.length > 2) {
      var toggleBtn = document.createElement('button');
      toggleBtn.textContent = 'Expand all details';
      toggleBtn.className = 'expand-collapse-toggle';
      toggleBtn.style.cssText = 'display:block;margin:0 auto 16px;padding:6px 20px;border:1px solid var(--border);border-radius:6px;background:var(--card-bg);color:var(--text-muted);cursor:pointer;font-size:0.82rem;';
      var allExpanded = false;
      toggleBtn.addEventListener('click', function() {
        allExpanded = !allExpanded;
        detailsEls.forEach(function(d) {
          d.open = allExpanded;
        });
        toggleBtn.textContent = allExpanded ? 'Collapse all details' : 'Expand all details';
      });
      transcript.parentElement.insertBefore(toggleBtn, transcript);
    }
  }
});
</script>"#;

// ---------------------------------------------------------------------------
// File discovery & main
// ---------------------------------------------------------------------------

fn discover_session_files(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        return vec![path.to_path_buf()];
    }

    let mut files: Vec<PathBuf> = Vec::new();
    if path.is_dir() {
        // Recursively find .json files
        let pattern = format!("{}/**/*.json", path.display());
        for p in glob::glob(&pattern).expect("Failed to read glob pattern").flatten() {
            files.push(p);
        }
    }
    files.sort();
    files
}

/// Parse a date string (YYYY-MM-DD) into a chrono NaiveDate.
fn parse_date_filter(s: &str) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

/// Extract a NaiveDate from a session date string (ISO 8601 or YYYY-MM-DD).
fn parse_session_date(date_str: &str) -> Option<chrono::NaiveDate> {
    // Try full ISO 8601 datetime first
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(date_str) {
        return Some(dt.date_naive());
    }
    // Try plain date
    if let Ok(d) = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
        return Some(d);
    }
    // Try extracting YYYY-MM-DD prefix
    if date_str.len() >= 10 {
        if let Ok(d) = chrono::NaiveDate::parse_from_str(&date_str[..10], "%Y-%m-%d") {
            return Some(d);
        }
    }
    None
}

/// Maximum length for the base filename (before `.html` extension).
const MAX_FILENAME_LEN: usize = 60;

fn sanitize_filename(s: &str) -> String {
    let sanitized: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();

    // Trim trailing underscores after truncation for cleaner names
    let truncated: String = sanitized.chars().take(MAX_FILENAME_LEN).collect();
    truncated.trim_end_matches('_').to_string()
}

/// Extract a YYYY-MM-DD date prefix from a date string.
/// Handles ISO 8601 (e.g. "2025-06-15T10:30:00Z") and plain date strings.
fn extract_date_prefix(date: &str) -> Option<String> {
    // Try to extract YYYY-MM-DD from the beginning of the string
    if date.len() >= 10 {
        let prefix = &date[..10];
        // Validate it looks like a date: YYYY-MM-DD
        let parts: Vec<&str> = prefix.split('-').collect();
        if parts.len() == 3
            && parts[0].len() == 4
            && parts[1].len() == 2
            && parts[2].len() == 2
            && parts[0].chars().all(|c| c.is_ascii_digit())
            && parts[1].chars().all(|c| c.is_ascii_digit())
            && parts[2].chars().all(|c| c.is_ascii_digit())
        {
            return Some(prefix.to_string());
        }
    }
    None
}

/// Generate a unique filename from a title and optional date, appending a
/// numeric suffix if needed. When a date is available, the filename is
/// prefixed with `YYYY-MM-DD_` for easy chronological sorting.
/// The `used` set tracks filenames already claimed in this run.
fn unique_filename(
    title: &str,
    date: Option<&str>,
    used: &mut std::collections::HashSet<String>,
) -> String {
    let base = sanitize_filename(title);
    let base = if base.is_empty() {
        "untitled".to_string()
    } else {
        base
    };

    // Prepend date prefix if available
    let base = match date.and_then(extract_date_prefix) {
        Some(prefix) => format!("{prefix}_{base}"),
        None => base,
    };

    let candidate = format!("{base}.html");
    if used.insert(candidate.clone()) {
        return candidate;
    }

    // Collision — append numeric suffix
    for n in 1u32.. {
        let candidate = format!("{base}_{n}.html");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }

    unreachable!()
}

/// Returns true if a JSON key name represents a file/directory path,
/// meaning its value should be percent-decoded for display.
fn is_path_key(key: &str) -> bool {
    matches!(
        key,
        "file_path"
            | "filepath"
            | "path"
            | "file"
            | "directory"
            | "dir"
            | "notebook_path"
            | "cwd"
    )
}

/// Decode a value for display: strips `file:///` URI prefixes and
/// decodes percent-encoded bytes (e.g. `%3A` → `:`).
fn decode_path(input: &str) -> String {
    let rest = input.strip_prefix("file:///").unwrap_or(input);
    let decoded = percent_decode(rest);
    // After stripping file:/// and decoding, decide whether to re-add
    // the leading slash.  file:///home/user → /home/user (Unix),
    // file:///C:/Users → C:/Users (Windows drive letter).
    if input.starts_with("file:///") {
        if decoded.starts_with('/') || decoded.chars().nth(1) == Some(':') {
            decoded
        } else {
            format!("/{decoded}")
        }
    } else {
        decoded
    }
}

/// Decode percent-encoded strings (e.g. `%3A` → `:`).
/// Handles UTF-8 sequences that span multiple percent-encoded bytes.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""),
                16,
            ) {
                decoded.push(byte);
                i += 3;
                continue;
            }
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(decoded).unwrap_or_else(|_| input.to_string())
}

/// Result of processing a single session file.
enum ProcessResult {
    /// Successfully processed: (html, title, date, source_path)
    Success(String, String, String, PathBuf),
    /// Skipped (filtered out or empty history)
    Skipped,
    /// Failed to read or parse
    Error(PathBuf, String),
}

fn main() {
    let cli = Cli::parse();
    let start_time = Instant::now();

    let input = &cli.input;
    let output_dir = &cli.output;

    if !input.exists() {
        eprintln!("\u{274c} Error: input path does not exist: {}", input.display());
        std::process::exit(1);
    }

    // Validate date filters
    let since_date = cli.since.as_deref().map(|s| {
        parse_date_filter(s).unwrap_or_else(|| {
            eprintln!("\u{274c} Invalid --since date: {:?} (expected YYYY-MM-DD)", s);
            std::process::exit(1);
        })
    });
    let before_date = cli.before.as_deref().map(|s| {
        parse_date_filter(s).unwrap_or_else(|| {
            eprintln!("\u{274c} Invalid --before date: {:?} (expected YYYY-MM-DD)", s);
            std::process::exit(1);
        })
    });

    // Configure rayon thread pool if --workers is specified
    if let Some(workers) = cli.workers {
        let workers = workers.max(1); // at least 1
        rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .build_global()
            .unwrap_or_else(|e| {
                eprintln!("Warning: could not set thread pool size: {}", e);
            });
    }

    let num_workers = rayon::current_num_threads();
    eprintln!("Using {} worker thread(s)", num_workers);

    let files = discover_session_files(input);
    if files.is_empty() {
        eprintln!("\u{274c} No session JSON files found at: {}", input.display());
        std::process::exit(1);
    }

    let files_discovered = files.len();
    eprintln!("Discovered {} JSON file(s)", files_discovered);

    fs::create_dir_all(output_dir).expect("Failed to create output directory");

    // Process files in parallel: read, parse, filter, and render HTML concurrently
    let results: Vec<ProcessResult> = files
        .par_iter()
        .map(|file| {
            let raw = match fs::read_to_string(file) {
                Ok(s) => s,
                Err(e) => {
                    return ProcessResult::Error(
                        file.clone(),
                        format!("could not read: {}", e),
                    );
                }
            };

            let session: Session = match serde_json::from_str(&raw) {
                Ok(s) => s,
                Err(e) => {
                    return ProcessResult::Error(
                        file.clone(),
                        format!("could not parse: {}", e),
                    );
                }
            };

            // Apply title filter if provided
            if let Some(ref filter) = cli.filter {
                if !session
                    .title
                    .to_lowercase()
                    .contains(&filter.to_lowercase())
                {
                    return ProcessResult::Skipped;
                }
            }

            // Apply date range filters
            if since_date.is_some() || before_date.is_some() {
                let session_date = session
                    .date_created
                    .as_deref()
                    .and_then(parse_session_date)
                    .or_else(|| {
                        // Fall back to file modification time
                        file.metadata().ok()
                            .and_then(|m| m.modified().ok())
                            .map(|t| {
                                let dt: chrono::DateTime<chrono::Local> = t.into();
                                dt.date_naive()
                            })
                    });
                if let Some(sd) = session_date {
                    if let Some(since) = since_date {
                        if sd < since {
                            return ProcessResult::Skipped;
                        }
                    }
                    if let Some(before) = before_date {
                        if sd >= before {
                            return ProcessResult::Skipped;
                        }
                    }
                } else {
                    // No date available — skip if we're filtering by date
                    return ProcessResult::Skipped;
                }
            }

            if session.history.is_empty() {
                return ProcessResult::Skipped;
            }

            let html = render_session(&session, Some(file.as_path()));

            let title = if session.title.is_empty() {
                session.session_id.clone()
            } else {
                session.title.clone()
            };
            let date = session
                .date_created
                .clone()
                .or_else(|| file_modified_date(file.as_path()))
                .unwrap_or_default();

            ProcessResult::Success(html, title, date, file.clone())
        })
        .collect();

    // Write output files and build the index
    let mut index_entries: Vec<(String, String, String)> = Vec::new();
    let mut used_filenames = std::collections::HashSet::new();
    let mut files_written = 0u32;
    let mut files_unchanged = 0u32;
    let mut files_skipped = 0u32;
    let mut files_errored = 0u32;
    let mut errors: Vec<(PathBuf, String)> = Vec::new();

    for result in &results {
        match result {
            ProcessResult::Success(html, title, date, source) => {
                let filename = unique_filename(
                    title,
                    if date.is_empty() { None } else { Some(date.as_str()) },
                    &mut used_filenames,
                );
                let out_path = output_dir.join(&filename);

                // Skip writing if the file already exists with identical content.
                // Check file size first to avoid reading large files unnecessarily.
                let needs_write = match fs::metadata(&out_path) {
                    Ok(meta) if meta.len() == html.len() as u64 => {
                        match fs::read(&out_path) {
                            Ok(existing) => existing != html.as_bytes(),
                            Err(_) => true,
                        }
                    }
                    Ok(_) => true,  // size differs — must rewrite
                    Err(_) => true, // file doesn't exist
                };

                if needs_write {
                    match fs::File::create(&out_path) {
                        Ok(file) => {
                            let mut writer = BufWriter::new(file);
                            match writer.write_all(html.as_bytes()).and_then(|_| writer.flush()) {
                                Ok(_) => {
                                    eprintln!("  \u{2705} {}", out_path.display());
                                    index_entries.push((title.clone(), filename, date.clone()));
                                    files_written += 1;
                                }
                                Err(e) => {
                                    eprintln!(
                                        "  \u{274c} Failed to write {}: {}",
                                        out_path.display(),
                                        e
                                    );
                                    errors.push((source.clone(), format!("write failed: {}", e)));
                                    files_errored += 1;
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "  \u{274c} Failed to create {}: {}",
                                out_path.display(),
                                e
                            );
                            errors.push((source.clone(), format!("write failed: {}", e)));
                            files_errored += 1;
                        }
                    }
                } else {
                    // File exists and is unchanged — skip writing
                    index_entries.push((title.clone(), filename, date.clone()));
                    files_unchanged += 1;
                }
            }
            ProcessResult::Skipped => {
                files_skipped += 1;
            }
            ProcessResult::Error(path, msg) => {
                eprintln!("  \u{274c} {}: {}", path.display(), msg);
                errors.push((path.clone(), msg.clone()));
                files_errored += 1;
            }
        }
    }

    // Generate index.html (also skip if unchanged)
    let mut index_written = false;
    if !index_entries.is_empty() {
        let index_html = render_index(&index_entries);
        let index_path = output_dir.join("index.html");
        let index_needs_write = match fs::metadata(&index_path) {
            Ok(meta) if meta.len() == index_html.len() as u64 => {
                match fs::read(&index_path) {
                    Ok(existing) => existing != index_html.as_bytes(),
                    Err(_) => true,
                }
            }
            Ok(_) => true,
            Err(_) => true,
        };
        if index_needs_write {
            match fs::File::create(&index_path) {
                Ok(file) => {
                    let mut writer = BufWriter::new(file);
                    match writer
                        .write_all(index_html.as_bytes())
                        .and_then(|_| writer.flush())
                    {
                        Ok(_) => {
                            eprintln!("  \u{2705} {}", index_path.display());
                            index_written = true;
                        }
                        Err(e) => {
                            eprintln!("  \u{274c} Failed to write index: {}", e);
                            files_errored += 1;
                        }
                    }
                }
                Err(e) => {
                    eprintln!("  \u{274c} Failed to create index: {}", e);
                    files_errored += 1;
                }
            }
        } else {
            index_written = true; // exists and unchanged
        }
    }

    let elapsed = start_time.elapsed();

    // --- Summary ---
    eprintln!();
    eprintln!("═══════════════════════════════════════════");
    eprintln!("  Summary");
    eprintln!("═══════════════════════════════════════════");
    eprintln!("  Files discovered:  {}", files_discovered);
    eprintln!(
        "  Sessions written:  {} (+ {} index)",
        files_written,
        if index_written { 1 } else { 0 }
    );
    if files_unchanged > 0 {
        eprintln!("  Unchanged:         {}", files_unchanged);
    }
    if files_skipped > 0 {
        eprintln!("  Skipped:           {}", files_skipped);
    }
    if files_errored > 0 {
        eprintln!("  Errors:            {}", files_errored);
        for (path, msg) in &errors {
            eprintln!("    \u{274c} {}: {}", path.display(), msg);
        }
    }
    eprintln!("  Workers:           {}", num_workers);
    eprintln!("  Elapsed:           {:.2?}", elapsed);
    eprintln!("───────────────────────────────────────────");

    if files_errored == 0 {
        eprintln!("  \u{2705} All files processed successfully!");
    } else {
        eprintln!(
            "  \u{274c} Completed with {} error(s)",
            files_errored
        );
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Index page
// ---------------------------------------------------------------------------

fn render_index(entries: &[(String, String, String)]) -> String {
    let mut rows = String::new();
    for (title, filename, date) in entries {
        rows.push_str(&format!(
            "    <tr>\n      <td><a href=\"{filename}\">{title}</a></td>\n      <td style=\"white-space:nowrap\">{date}</td>\n    </tr>\n",
            filename = encode_text(filename),
            title = encode_text(title),
            date = encode_text(date),
        ));
    }

    format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Continue.dev Transcripts</title>
{CSS}
<style>
  table {{ width: 100%; border-collapse: collapse; }}
  th, td {{ text-align: left; padding: 10px 14px; border-bottom: 1px solid var(--border); }}
  th {{ font-size: 0.82rem; text-transform: uppercase; letter-spacing: 0.05em; color: var(--text-muted); }}
  td a {{ color: var(--user-border); text-decoration: none; font-weight: 500; }}
  td a:hover {{ text-decoration: underline; }}
  .search-box {{ width: 100%; padding: 10px 14px; border: 1px solid var(--border); border-radius: 8px; font-size: 0.95rem; margin-bottom: 16px; background: var(--card-bg); color: var(--text); }}
  .search-box::placeholder {{ color: var(--text-muted); }}
  .no-results {{ text-align: center; padding: 24px; color: var(--text-muted); display: none; }}
</style>
</head>
<body>
<button class="theme-toggle" aria-label="Toggle theme"></button>
<div class="container">
  <header class="session-header">
    <h1>Continue.dev Transcripts</h1>
    <div class="session-meta">
      <span class="meta-item">{count} session(s)</span>
    </div>
  </header>
  <input type="text" class="search-box" placeholder="Filter sessions by title or date\u2026" autocomplete="off">
  <table>
    <thead><tr><th>Session</th><th style="width: 22ch;">Date</th></tr></thead>
    <tbody>
{rows}
    </tbody>
  </table>
  <div class="no-results">No sessions match your filter.</div>
  <footer>
    <p>Generated by <strong>continue-transcripts</strong></p>
  </footer>
</div>
<script>
document.addEventListener('DOMContentLoaded', function() {{
  // Theme toggle
  (function() {{
    var html = document.documentElement;
    var stored = localStorage.getItem('ct-theme');
    if (stored) html.setAttribute('data-theme', stored);
    var btn = document.querySelector('.theme-toggle');
    if (btn) {{
      function updateIcon() {{
        var theme = html.getAttribute('data-theme');
        if (!theme) {{
          var dark = window.matchMedia('(prefers-color-scheme: dark)').matches;
          btn.textContent = dark ? '\u2600\uFE0F' : '\uD83C\uDF19';
          btn.title = 'Theme: auto (' + (dark ? 'dark' : 'light') + '). Click to toggle.';
        }} else if (theme === 'dark') {{
          btn.textContent = '\u2600\uFE0F';
          btn.title = 'Theme: dark. Click for light.';
        }} else {{
          btn.textContent = '\uD83C\uDF19';
          btn.title = 'Theme: light. Click for dark.';
        }}
      }}
      btn.addEventListener('click', function() {{
        var current = html.getAttribute('data-theme');
        if (!current) {{
          var dark = window.matchMedia('(prefers-color-scheme: dark)').matches;
          var next = dark ? 'light' : 'dark';
        }} else if (current === 'dark') {{
          var next = 'light';
        }} else {{
          var next = 'dark';
        }}
        html.setAttribute('data-theme', next);
        localStorage.setItem('ct-theme', next);
        updateIcon();
      }});
      updateIcon();
      window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', updateIcon);
    }}
  }})();

  // Search / filter
  var input = document.querySelector('.search-box');
  var rows = document.querySelectorAll('tbody tr');
  var noResults = document.querySelector('.no-results');
  if (input && rows.length) {{
    input.addEventListener('input', function() {{
      var q = input.value.toLowerCase();
      var visible = 0;
      rows.forEach(function(row) {{
        var text = row.textContent.toLowerCase();
        var match = !q || text.indexOf(q) !== -1;
        row.style.display = match ? '' : 'none';
        if (match) visible++;
      }});
      noResults.style.display = visible === 0 ? 'block' : 'none';
    }});
  }}
}});
</script>
</body>
</html>"##,
        CSS = CSS,
        count = entries.len(),
        rows = rows,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_content_text() {
        let mc = MessageContent::Text("hello world".to_string());
        assert_eq!(mc.text(), "hello world");
        assert!(!mc.is_empty());
    }

    #[test]
    fn test_message_content_empty() {
        let mc = MessageContent::Text("   ".to_string());
        assert!(mc.is_empty());
    }

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(sanitize_filename("Hello World!"), "Hello_World");
        assert_eq!(sanitize_filename("test/file:name"), "test_file_name");
    }

    #[test]
    fn test_sanitize_filename_truncation() {
        let long_title = "A".repeat(100);
        let result = sanitize_filename(&long_title);
        assert_eq!(result.len(), MAX_FILENAME_LEN);
    }

    #[test]
    fn test_unique_filename_no_collision() {
        let mut used = std::collections::HashSet::new();
        let f1 = unique_filename("Session One", None, &mut used);
        let f2 = unique_filename("Session Two", None, &mut used);
        assert_eq!(f1, "Session_One.html");
        assert_eq!(f2, "Session_Two.html");
        assert_ne!(f1, f2);
    }

    #[test]
    fn test_unique_filename_collision() {
        let mut used = std::collections::HashSet::new();
        let f1 = unique_filename("Same Title", None, &mut used);
        let f2 = unique_filename("Same Title", None, &mut used);
        let f3 = unique_filename("Same Title", None, &mut used);
        assert_eq!(f1, "Same_Title.html");
        assert_eq!(f2, "Same_Title_1.html");
        assert_eq!(f3, "Same_Title_2.html");
    }

    #[test]
    fn test_unique_filename_empty_title() {
        let mut used = std::collections::HashSet::new();
        let f = unique_filename("", None, &mut used);
        assert_eq!(f, "untitled.html");
    }

    #[test]
    fn test_unique_filename_with_date_prefix() {
        let mut used = std::collections::HashSet::new();
        let f = unique_filename("My Session", Some("2025-06-15T10:30:00Z"), &mut used);
        assert_eq!(f, "2025-06-15_My_Session.html");
    }

    #[test]
    fn test_unique_filename_date_prefix_collision() {
        let mut used = std::collections::HashSet::new();
        let f1 = unique_filename("Chat", Some("2025-06-15T10:00:00Z"), &mut used);
        let f2 = unique_filename("Chat", Some("2025-06-15T14:00:00Z"), &mut used);
        assert_eq!(f1, "2025-06-15_Chat.html");
        assert_eq!(f2, "2025-06-15_Chat_1.html");
    }

    #[test]
    fn test_unique_filename_invalid_date_no_prefix() {
        let mut used = std::collections::HashSet::new();
        let f = unique_filename("Session", Some("not-a-date"), &mut used);
        assert_eq!(f, "Session.html");
    }

    #[test]
    fn test_extract_date_prefix() {
        assert_eq!(
            extract_date_prefix("2025-06-15T10:30:00Z"),
            Some("2025-06-15".to_string())
        );
        assert_eq!(
            extract_date_prefix("2025-01-01"),
            Some("2025-01-01".to_string())
        );
        assert_eq!(extract_date_prefix("not-a-date"), None);
        assert_eq!(extract_date_prefix("short"), None);
    }

    // -----------------------------------------------------------------------
    // Date filter parsing tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_date_filter() {
        let d = parse_date_filter("2025-06-15");
        assert!(d.is_some());
        assert_eq!(d.unwrap().to_string(), "2025-06-15");

        assert!(parse_date_filter("not-a-date").is_none());
        assert!(parse_date_filter("2025/06/15").is_none());
    }

    #[test]
    fn test_parse_session_date_iso() {
        let d = parse_session_date("2025-06-15T10:30:00+00:00");
        assert!(d.is_some());
        assert_eq!(d.unwrap().to_string(), "2025-06-15");
    }

    #[test]
    fn test_parse_session_date_plain() {
        let d = parse_session_date("2025-06-15");
        assert!(d.is_some());
        assert_eq!(d.unwrap().to_string(), "2025-06-15");
    }

    #[test]
    fn test_parse_session_date_prefix() {
        // Handles "2025-06-15T10:30:00Z" (non-standard but with T prefix)
        let d = parse_session_date("2025-06-15T10:30:00Z");
        assert!(d.is_some());
        assert_eq!(d.unwrap().to_string(), "2025-06-15");
    }

    #[test]
    fn test_parse_session_date_invalid() {
        assert!(parse_session_date("not-a-date").is_none());
        assert!(parse_session_date("").is_none());
    }

    // -----------------------------------------------------------------------
    // Percent decode tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_percent_decode_basic() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("%2Fhome%2Fuser"), "/home/user");
    }

    #[test]
    fn test_percent_decode_windows_path() {
        assert_eq!(
            percent_decode("C%3A%5CUsers%5Ctest"),
            "C:\\Users\\test"
        );
    }

    #[test]
    fn test_percent_decode_no_encoding() {
        assert_eq!(percent_decode("/home/user/project"), "/home/user/project");
    }

    #[test]
    fn test_percent_decode_mixed() {
        assert_eq!(
            percent_decode("/path/to%20my%20file.txt"),
            "/path/to my file.txt"
        );
    }

    #[test]
    fn test_is_path_key() {
        assert!(is_path_key("file_path"));
        assert!(is_path_key("filepath"));
        assert!(is_path_key("path"));
        assert!(is_path_key("directory"));
        assert!(is_path_key("notebook_path"));
        assert!(!is_path_key("pattern"));
        assert!(!is_path_key("command"));
        assert!(!is_path_key("old_string"));
        assert!(!is_path_key("content"));
    }

    #[test]
    fn test_decode_path_file_uri() {
        // Unix file URI
        assert_eq!(decode_path("file:///home/user/file.rs"), "/home/user/file.rs");
        // Windows file URI
        assert_eq!(decode_path("file:///C%3A/Users/test"), "C:/Users/test");
        // Percent-encoded without file:/// prefix
        assert_eq!(decode_path("C%3A%5CUsers%5Ctest"), r"C:\Users\test");
        // Plain path — no change
        assert_eq!(decode_path("/home/user/file.rs"), "/home/user/file.rs");
    }

    #[test]
    fn test_markdown_to_html() {
        let html = markdown_to_html("**bold** text");
        assert!(html.contains("<strong>bold</strong>"));
    }

    #[test]
    fn test_markdown_code_block() {
        let md = "```rust\nfn main() {}\n```";
        let html = markdown_to_html(md);
        assert!(html.contains("<pre"));
        // Syntax-highlighted blocks use inline <span> styles
        assert!(html.contains("fn"));
    }

    #[test]
    fn test_parse_simple_session() {
        let json = r#"{
            "sessionId": "abc-123",
            "title": "Test Session",
            "workspaceDirectory": "/tmp",
            "history": [
                {
                    "message": {
                        "role": "user",
                        "content": "Hello"
                    },
                    "contextItems": []
                },
                {
                    "message": {
                        "role": "assistant",
                        "content": "Hi there! How can I help?"
                    },
                    "contextItems": []
                }
            ]
        }"#;

        let session: Session = serde_json::from_str(json).unwrap();
        assert_eq!(session.session_id, "abc-123");
        assert_eq!(session.title, "Test Session");
        assert_eq!(session.history.len(), 2);
        assert_eq!(session.history[0].message.role, "user");
        assert_eq!(session.history[1].message.role, "assistant");
    }

    #[test]
    fn test_parse_parts_content() {
        let json = r#"{
            "sessionId": "xyz",
            "title": "",
            "workspaceDirectory": "",
            "history": [
                {
                    "message": {
                        "role": "user",
                        "content": [
                            {"type": "text", "text": "hello "},
                            {"type": "text", "text": "world"}
                        ]
                    },
                    "contextItems": []
                }
            ]
        }"#;

        let session: Session = serde_json::from_str(json).unwrap();
        assert_eq!(session.history[0].message.content.text(), "hello world");
    }

    #[test]
    fn test_render_session_produces_html() {
        let session = Session {
            session_id: "test".to_string(),
            title: "Test".to_string(),
            workspace_directory: "/tmp".to_string(),
            history: vec![ChatHistoryItem {
                message: ChatMessage {
                    role: "user".to_string(),
                    content: MessageContent::Text("Hello".to_string()),
                    tool_calls: None,
                    usage: None,
                },
                context_items: vec![],
                prompt_logs: None,
                tool_call_states: None,
            }],
            date_created: Some("2025-01-01".to_string()),
        };

        let html = render_session(&session, None);
        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("Test"));
        assert!(html.contains("User"));
        assert!(html.contains("Hello"));
    }

    // -----------------------------------------------------------------------
    // ANSI → HTML tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_ansi_plain_text() {
        assert_eq!(ansi_to_html("hello world"), "hello world");
    }

    #[test]
    fn test_ansi_html_entities_escaped() {
        assert_eq!(ansi_to_html("<b>&</b>"), "&lt;b&gt;&amp;&lt;/b&gt;");
    }

    #[test]
    fn test_ansi_green_text() {
        let result = ansi_to_html("\x1b[32mpass\x1b[0m");
        assert!(result.contains("color:#98c379"));
        assert!(result.contains("pass"));
        assert!(result.contains("</span>"));
    }

    #[test]
    fn test_ansi_red_text() {
        let result = ansi_to_html("\x1b[31mfail\x1b[0m");
        assert!(result.contains("color:#e06c75"));
        assert!(result.contains("fail"));
    }

    #[test]
    fn test_ansi_bold() {
        let result = ansi_to_html("\x1b[1mbold text\x1b[0m");
        assert!(result.contains("font-weight:bold"));
        assert!(result.contains("bold text"));
    }

    #[test]
    fn test_ansi_multiple_sequences() {
        let input = "\x1b[32m✓\x1b[0m pass  \x1b[31m✕\x1b[0m fail";
        let result = ansi_to_html(input);
        // Should have green span for checkmark and red for X
        assert!(result.contains("#98c379"));
        assert!(result.contains("#e06c75"));
    }

    #[test]
    fn test_ansi_reset_closes_span() {
        let result = ansi_to_html("\x1b[1m\x1b[31mred bold\x1b[0m normal");
        assert!(result.contains("</span>"));
        assert!(result.contains("normal"));
    }

    #[test]
    fn test_ansi_bright_colors() {
        let result = ansi_to_html("\x1b[90mgray\x1b[0m");
        assert!(result.contains("color:#5c6370"));
    }

    // -----------------------------------------------------------------------
    // Bare tool call rendering tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_render_tool_args_bash_command() {
        let result = render_tool_args("Bash", r#"{"command": "npm test"}"#);
        assert!(result.contains("$"));
        assert!(result.contains("npm test"));
        assert!(result.contains("tool-args-bare"));
    }

    #[test]
    fn test_render_tool_args_read_file() {
        let result = render_tool_args("Read", r#"{"file_path": "/src/main.rs"}"#);
        assert!(result.contains("file_path"));
        assert!(result.contains("/src/main.rs"));
        assert!(result.contains("tool-args-kv"));
    }

    #[test]
    fn test_render_tool_args_edit_diff_style() {
        let result = render_tool_args(
            "Edit",
            r#"{"file_path": "test.rs", "old_string": "foo", "new_string": "bar"}"#,
        );
        // Edit renders as a diff with file_path and old/new lines
        assert!(result.contains("file_path"));
        assert!(result.contains("test.rs"));
        assert!(result.contains("diff-remove"));
        assert!(result.contains("diff-add"));
        assert!(result.contains("- foo"));
        assert!(result.contains("+ bar"));
    }

    #[test]
    fn test_render_tool_args_edit_multiline_diff() {
        let result = render_tool_args(
            "Edit",
            r#"{"file_path": "app.ts", "old_string": "line1\nline2", "new_string": "new1\nnew2\nnew3"}"#,
        );
        assert!(result.contains("tool-diff"));
        assert!(result.contains("- line1"));
        assert!(result.contains("- line2"));
        assert!(result.contains("+ new1"));
        assert!(result.contains("+ new2"));
        assert!(result.contains("+ new3"));
    }

    #[test]
    fn test_render_tool_args_invalid_json() {
        let result = render_tool_args("Unknown", "not json at all");
        assert!(result.contains("not json at all"));
    }

    #[test]
    fn test_render_tool_args_percent_decodes_file_paths() {
        // Edit tool: file_path with percent-encoded colons (e.g. Windows C: drive)
        let result = render_tool_args(
            "Edit",
            r#"{"file_path": "C%3A%5CUsers%5Ctest%5Cfile.rs", "old_string": "a", "new_string": "b"}"#,
        );
        assert!(
            result.contains(r"C:\Users\test\file.rs"),
            "Edit file_path should be percent-decoded: {}",
            result
        );
        assert!(!result.contains("%3A"), "Should not contain encoded colons");

        // Single-key (Read tool): file_path with percent-encoded colon
        let result = render_tool_args(
            "Read",
            r#"{"file_path": "/home/user%3Aname/file.rs"}"#,
        );
        assert!(
            result.contains("/home/user:name/file.rs"),
            "Read file_path should be percent-decoded: {}",
            result
        );

        // Multi-key: path key should be percent-decoded
        let result = render_tool_args(
            "Grep",
            r#"{"pattern": "hello", "path": "/home/user%3Aname/project"}"#,
        );
        assert!(
            result.contains("/home/user:name/project"),
            "Multi-key path values should be percent-decoded: {}",
            result
        );

        // Multi-key: non-path keys should NOT be decoded
        let result = render_tool_args(
            "Grep",
            r#"{"pattern": "match%3Athis", "path": "/home/user/project"}"#,
        );
        assert!(
            result.contains("match%3Athis"),
            "Non-path keys should not be percent-decoded: {}",
            result
        );
    }

    #[test]
    fn test_render_tool_args_file_uri_stripped() {
        // file:/// URI with percent-encoded path
        let result = render_tool_args(
            "Read",
            r#"{"file_path": "file:///home/user%3Aname/file.rs"}"#,
        );
        assert!(
            result.contains("/home/user:name/file.rs"),
            "file:/// prefix should be stripped and path decoded: {}",
            result
        );
        assert!(
            !result.contains("file:///"),
            "file:/// prefix should be removed"
        );

        // file:/// URI with Windows path
        let result = render_tool_args(
            "Edit",
            r#"{"file_path": "file:///C%3A/Users/test/file.rs", "old_string": "a", "new_string": "b"}"#,
        );
        assert!(
            result.contains("C:/Users/test/file.rs"),
            "Windows file:/// URI should be decoded: {}",
            result
        );
    }

    // -----------------------------------------------------------------------
    // Language detection from filenames
    // -----------------------------------------------------------------------

    #[test]
    fn test_lang_from_filename() {
        assert_eq!(lang_from_filename("src/auth/middleware.ts"), "ts");
        assert_eq!(lang_from_filename("main.rs"), "rs");
        assert_eq!(lang_from_filename("/home/user/project/app.py"), "py");
        assert_eq!(lang_from_filename("Dockerfile"), "Dockerfile");
        assert_eq!(lang_from_filename("Makefile"), "makefile");
        assert_eq!(lang_from_filename("no-extension"), "");
    }

    #[test]
    fn test_lang_from_filename_windows_path() {
        assert_eq!(lang_from_filename("C:\\Users\\test\\file.tsx"), "tsx");
    }

    // -----------------------------------------------------------------------
    // Context item syntax highlighting
    // -----------------------------------------------------------------------

    #[test]
    fn test_context_items_syntax_highlighted() {
        let items = vec![ContextItem {
            name: "src/app.ts".to_string(),
            description: "Main app".to_string(),
            content: "const x: number = 42;".to_string(),
        }];
        let html = render_context_items(&items);
        // Should use highlighted-code class for TypeScript
        assert!(html.contains("highlighted-code") || html.contains("context-content"));
        assert!(html.contains("src/app.ts"));
    }

    // -----------------------------------------------------------------------
    // Read result syntax highlighting
    // -----------------------------------------------------------------------

    #[test]
    fn test_try_highlight_read_result() {
        let args = r#"{"file_path": "/home/user/test.rs"}"#;
        let content = "     1\tfn main() {\n     2\t    println!(\"hello\");\n     3\t}";
        let result = try_highlight_read_result(args, content);
        assert!(result.is_some());
        let hl = result.unwrap();
        assert!(hl.contains("highlighted-code"));
    }

    #[test]
    fn test_try_highlight_read_result_no_extension() {
        let args = r#"{"file_path": "/home/user/README"}"#;
        let content = "just some text";
        let result = try_highlight_read_result(args, content);
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // Model badge
    // -----------------------------------------------------------------------

    #[test]
    fn test_model_badge_on_assistant() {
        let item = ChatHistoryItem {
            message: ChatMessage {
                role: "assistant".to_string(),
                content: MessageContent::Text("Hello".to_string()),
                tool_calls: None,
                usage: None,
            },
            context_items: vec![],
            prompt_logs: None,
            tool_call_states: None,
        };
        let html = render_message(&item, None, Some("Claude 3.5 Sonnet"));
        assert!(html.contains("model-badge"));
        assert!(html.contains("Claude 3.5 Sonnet"));
    }

    #[test]
    fn test_no_model_badge_on_user() {
        let item = ChatHistoryItem {
            message: ChatMessage {
                role: "user".to_string(),
                content: MessageContent::Text("Hello".to_string()),
                tool_calls: None,
                usage: None,
            },
            context_items: vec![],
            prompt_logs: None,
            tool_call_states: None,
        };
        let html = render_message(&item, None, Some("Claude 3.5 Sonnet"));
        // Model badge should not appear on user messages
        assert!(!html.contains("model-badge"));
    }

    // -----------------------------------------------------------------------
    // System prompt extraction tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_system_prompt_extracted_to_top() {
        let session = Session {
            session_id: "sp-test".to_string(),
            title: "System Prompt Test".to_string(),
            workspace_directory: "/tmp".to_string(),
            history: vec![
                ChatHistoryItem {
                    message: ChatMessage {
                        role: "system".to_string(),
                        content: MessageContent::Text("You are a helpful assistant.".to_string()),
                        tool_calls: None,
                        usage: None,
                    },
                    context_items: vec![],
                    prompt_logs: None,
                    tool_call_states: None,
                },
                ChatHistoryItem {
                    message: ChatMessage {
                        role: "user".to_string(),
                        content: MessageContent::Text("Hello".to_string()),
                        tool_calls: None,
                        usage: None,
                    },
                    context_items: vec![],
                    prompt_logs: None,
                    tool_call_states: None,
                },
            ],
            date_created: Some("2025-01-01".to_string()),
        };

        let html = render_session(&session, None);
        // System prompt should be in a collapsed details element
        assert!(html.contains("class=\"system-prompt\""));
        assert!(html.contains("System Prompt"));
        assert!(html.contains("helpful assistant"));
        // System message should NOT also appear as a standalone message in the transcript
        // (it's extracted to the top)
        let transcript_start = html.find("class=\"transcript\"").unwrap();
        let transcript_section = &html[transcript_start..];
        assert!(!transcript_section.contains("class=\"message system\""));
    }

    // -----------------------------------------------------------------------
    // Tool call + result grouping tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_tool_results_nested_under_assistant() {
        let session = Session {
            session_id: "group-test".to_string(),
            title: "Grouping Test".to_string(),
            workspace_directory: "/tmp".to_string(),
            history: vec![
                ChatHistoryItem {
                    message: ChatMessage {
                        role: "assistant".to_string(),
                        content: MessageContent::Text("Let me check.".to_string()),
                        tool_calls: Some(vec![ToolCallDelta {
                            function: Some(ToolCallFunction {
                                name: "Bash".to_string(),
                                arguments: r#"{"command": "ls"}"#.to_string(),
                            }),
                        }]),
                        usage: None,
                    },
                    context_items: vec![],
                    prompt_logs: None,
                    tool_call_states: None,
                },
                ChatHistoryItem {
                    message: ChatMessage {
                        role: "tool".to_string(),
                        content: MessageContent::Text("file1.txt\nfile2.txt".to_string()),
                        tool_calls: None,
                        usage: None,
                    },
                    context_items: vec![],
                    prompt_logs: None,
                    tool_call_states: None,
                },
            ],
            date_created: Some("2025-01-01".to_string()),
        };

        let html = render_session(&session, None);
        // Tool result should be inside a details element with label
        assert!(html.contains("tool-result-details"));
        assert!(html.contains("Result: Bash"));
        // Tool result should be nested inside the assistant message
        // (inside assistant-tool-group)
        assert!(html.contains("assistant-tool-group"));
        // Should NOT have a standalone tool-result message
        assert!(!html.contains("class=\"message tool-result\""));
    }

    // -----------------------------------------------------------------------
    // Thinking blocks default collapsed
    // -----------------------------------------------------------------------

    #[test]
    fn test_thinking_default_collapsed() {
        let item = ChatHistoryItem {
            message: ChatMessage {
                role: "thinking".to_string(),
                content: MessageContent::Text("I should check the tests.".to_string()),
                tool_calls: None,
                usage: None,
            },
            context_items: vec![],
            prompt_logs: None,
            tool_call_states: None,
        };

        let html = render_message(&item, None, None);
        // Should be <details class="thinking-details"> WITHOUT the "open" attribute
        assert!(html.contains("<details class=\"thinking-details\">"));
        assert!(!html.contains("<details open"));
    }

    // -----------------------------------------------------------------------
    // Tool descriptions extraction
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_tool_descriptions() {
        let system = "## Tools\n\n### Bash\nExecute a shell command and return its output.\n\n### Read\nRead the contents of a file from the local filesystem.\n\n### Edit\nPerform exact string replacements in files.\n";
        let descs = extract_tool_descriptions(system);
        assert!(descs.len() >= 3);
        assert!(descs.iter().any(|(name, _, _)| name == "Bash"));
        assert!(descs.iter().any(|(name, _, _)| name == "Read"));
        assert!(descs.iter().any(|(name, _, _)| name == "Edit"));
    }

    #[test]
    fn test_collect_tool_names() {
        let history = vec![
            ChatHistoryItem {
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: MessageContent::Text(String::new()),
                    tool_calls: Some(vec![
                        ToolCallDelta {
                            function: Some(ToolCallFunction {
                                name: "Bash".to_string(),
                                arguments: String::new(),
                            }),
                        },
                        ToolCallDelta {
                            function: Some(ToolCallFunction {
                                name: "Read".to_string(),
                                arguments: String::new(),
                            }),
                        },
                    ]),
                    usage: None,
                },
                context_items: vec![],
                prompt_logs: None,
                tool_call_states: None,
            },
            ChatHistoryItem {
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: MessageContent::Text(String::new()),
                    tool_calls: Some(vec![ToolCallDelta {
                        function: Some(ToolCallFunction {
                            name: "Bash".to_string(), // duplicate
                            arguments: String::new(),
                        }),
                    }]),
                    usage: None,
                },
                context_items: vec![],
                prompt_logs: None,
                tool_call_states: None,
            },
        ];

        let names = collect_tool_names(&history);
        assert_eq!(names, vec!["Bash", "Read"]); // deduplicated, ordered
    }

    // -----------------------------------------------------------------------
    // Rich fixture integration test
    // -----------------------------------------------------------------------

    #[test]
    fn test_render_rich_fixture() {
        let raw = fs::read_to_string("tests/fixtures/sample-session-rich.json")
            .expect("rich fixture should exist");
        let session: Session = serde_json::from_str(&raw).expect("should parse");
        let html = render_session(&session, None);

        // System prompt extracted
        assert!(html.contains("class=\"system-prompt\""));
        // Tools reference panel present
        assert!(html.contains("class=\"tools-reference\""));
        // ANSI colors converted (green check mark from test output)
        assert!(html.contains("color:#98c379"));
        // Thinking block present and collapsed
        assert!(html.contains("thinking-details"));
        // Tool calls present
        assert!(html.contains("tool-call-header"));
        // Bare Bash rendering
        assert!(html.contains("tool-prompt"));
        // Tool details should have collapsible sections
        assert!(html.contains("tool-ref-item-details"));
    }

    // -----------------------------------------------------------------------
    // Token usage display tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_token_usage_displayed() {
        let session = Session {
            session_id: "tokens-test".to_string(),
            title: "Token Test".to_string(),
            workspace_directory: "/tmp".to_string(),
            history: vec![
                ChatHistoryItem {
                    message: ChatMessage {
                        role: "user".to_string(),
                        content: MessageContent::Text("Hello".to_string()),
                        tool_calls: None,
                        usage: None,
                    },
                    context_items: vec![],
                    prompt_logs: None,
                    tool_call_states: None,
                },
                ChatHistoryItem {
                    message: ChatMessage {
                        role: "assistant".to_string(),
                        content: MessageContent::Text("Hi!".to_string()),
                        tool_calls: None,
                        usage: Some(Usage {
                            prompt_tokens: 100,
                            completion_tokens: 50,
                        }),
                    },
                    context_items: vec![],
                    prompt_logs: None,
                    tool_call_states: None,
                },
            ],
            date_created: Some("2025-01-01".to_string()),
        };

        let html = render_session(&session, None);
        // Token badge should appear
        assert!(html.contains("token-badge"));
        assert!(html.contains("150 tokens"));
        // Running total should appear
        assert!(html.contains("token-running"));
        assert!(html.contains("cumul."));
        // Session header should show total tokens
        assert!(html.contains("Tokens:"));
    }

    #[test]
    fn test_no_token_display_when_no_usage() {
        let session = Session {
            session_id: "no-tokens".to_string(),
            title: "No Tokens".to_string(),
            workspace_directory: "/tmp".to_string(),
            history: vec![ChatHistoryItem {
                message: ChatMessage {
                    role: "user".to_string(),
                    content: MessageContent::Text("Hello".to_string()),
                    tool_calls: None,
                    usage: None,
                },
                context_items: vec![],
                prompt_logs: None,
                tool_call_states: None,
            }],
            date_created: Some("2025-01-01".to_string()),
        };

        let html = render_session(&session, None);
        // No token badges when no usage data
        // (token-badge appears in CSS, so check for actual badge markup)
        assert!(!html.contains("class=\"token-badge\""));
        // Session header should not show total tokens line
        assert!(!html.contains("Tokens: "));
    }

    #[test]
    fn test_format_tokens() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1000), "1,000");
        assert_eq!(format_tokens(1234), "1,234");
        assert_eq!(format_tokens(9999), "9,999");
        assert_eq!(format_tokens(10000), "10.0k");
        assert_eq!(format_tokens(123456), "123.5k");
        assert_eq!(format_tokens(1000000), "1.0M");
    }

    // -----------------------------------------------------------------------
    // Full tool descriptions extraction tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_tool_descriptions_full_content() {
        let system = "## Tools\n\n### Bash\nExecute a shell command.\n\nParameters:\n- `command` (string, required): The command\n- `timeout` (number, optional): Timeout in ms\n\n### Read\nRead a file.\n\nParameters:\n- `file_path` (string, required): Path\n";
        let descs = extract_tool_descriptions(system);
        assert_eq!(descs.len(), 2);

        let (name, _short, full) = &descs[0];
        assert_eq!(name, "Bash");
        // Full content should include parameters
        assert!(full.contains("Parameters:"));
        assert!(full.contains("`command`"));
        assert!(full.contains("`timeout`"));

        let (name2, _short2, full2) = &descs[1];
        assert_eq!(name2, "Read");
        assert!(full2.contains("`file_path`"));
    }

    // -----------------------------------------------------------------------
    // System prompt extraction
    // -----------------------------------------------------------------------

    #[test]
    fn test_system_prompt_prefers_system_message() {
        // System messages in history are the primary source
        let session = Session {
            session_id: "sys-msg-test".to_string(),
            title: "System Message Test".to_string(),
            workspace_directory: "/tmp".to_string(),
            history: vec![
                ChatHistoryItem {
                    message: ChatMessage {
                        role: "system".to_string(),
                        content: MessageContent::Text("You are a helpful assistant.".to_string()),
                        tool_calls: None,
                        usage: None,
                    },
                    context_items: vec![],
                    prompt_logs: None,
                    tool_call_states: None,
                },
                ChatHistoryItem {
                    message: ChatMessage {
                        role: "user".to_string(),
                        content: MessageContent::Text("Hello".to_string()),
                        tool_calls: None,
                        usage: None,
                    },
                    context_items: vec![],
                    prompt_logs: None,
                    tool_call_states: None,
                },
            ],
            date_created: Some("2025-01-01".to_string()),
        };

        let (prompt, idx) = extract_system_prompt(&session);
        assert_eq!(prompt, "You are a helpful assistant.");
        assert_eq!(idx, Some(0));
    }

    #[test]
    fn test_system_prompt_falls_back_to_prompt_logs() {
        // When there is no system message, fall back to promptLogs[].prompt
        let raw = fs::read_to_string("tests/fixtures/sample-session-prompt-logs.json")
            .expect("prompt-logs fixture should exist");
        let session: Session = serde_json::from_str(&raw).expect("should parse");

        let (prompt, idx) = extract_system_prompt(&session);
        assert!(!prompt.is_empty());
        assert!(prompt.contains("expert software engineer"));
        assert!(prompt.contains("investigate before making changes"));
        // No system message index since it came from promptLogs
        assert!(idx.is_none());
    }

    // -----------------------------------------------------------------------
    // Tool definition extraction from toolCallStates
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_tool_defs_from_tool_call_states() {
        let raw = fs::read_to_string("tests/fixtures/sample-session-prompt-logs.json")
            .expect("prompt-logs fixture should exist");
        let session: Session = serde_json::from_str(&raw).expect("should parse");

        let defs = extract_tool_defs(&session);
        // Should find Bash and Read from toolCallStates
        assert!(defs.iter().any(|t| t.name == "Bash"));
        assert!(defs.iter().any(|t| t.name == "Read"));

        // Check Bash has correct data
        let bash = defs.iter().find(|t| t.name == "Bash").unwrap();
        assert!(bash.description.contains("Execute a shell command"));
        assert_eq!(bash.system_message_prefix, "Execute shell commands and return output");
        assert!(bash.parameters.is_some());

        // Check parameters have property descriptions
        let params = bash.parameters.as_ref().unwrap();
        let command_desc = params
            .get("properties")
            .and_then(|p| p.get("command"))
            .and_then(|c| c.get("description"))
            .and_then(|d| d.as_str());
        assert_eq!(command_desc, Some("The shell command to execute"));
    }

    #[test]
    fn test_extract_tool_defs_from_completion_options() {
        // Tools from completionOptions.tools (in promptLogs) when no toolCallStates
        let raw = fs::read_to_string("tests/fixtures/sample-session-prompt-logs.json")
            .expect("prompt-logs fixture should exist");
        let session: Session = serde_json::from_str(&raw).expect("should parse");

        let defs = extract_tool_defs(&session);
        // Grep and Edit are only in completionOptions.tools (not in toolCallStates)
        // since they were available but not used via toolCallStates in this fixture
        assert!(defs.iter().any(|t| t.name == "Grep"));
        assert!(defs.iter().any(|t| t.name == "Edit"));

        let grep = defs.iter().find(|t| t.name == "Grep").unwrap();
        assert!(grep.description.contains("Search for patterns"));
        assert!(grep.parameters.is_some());

        // Grep has an enum parameter
        let params = grep.parameters.as_ref().unwrap();
        let output_mode = params
            .get("properties")
            .and_then(|p| p.get("output_mode"));
        assert!(output_mode.is_some());
        let enum_vals = output_mode.unwrap().get("enum");
        assert!(enum_vals.is_some());
    }

    #[test]
    fn test_extract_tool_defs_deduplicates() {
        let raw = fs::read_to_string("tests/fixtures/sample-session-prompt-logs.json")
            .expect("prompt-logs fixture should exist");
        let session: Session = serde_json::from_str(&raw).expect("should parse");

        let defs = extract_tool_defs(&session);
        // Bash appears in multiple toolCallStates but should only be extracted once
        let bash_count = defs.iter().filter(|t| t.name == "Bash").count();
        assert_eq!(bash_count, 1);
    }

    // -----------------------------------------------------------------------
    // Render parameters from JSON Schema
    // -----------------------------------------------------------------------

    #[test]
    fn test_render_parameters_schema() {
        let schema: serde_json::Value = serde_json::from_str(r#"{
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "timeout": {
                    "type": "number",
                    "description": "Timeout in milliseconds"
                }
            },
            "required": ["command"]
        }"#).unwrap();

        let html = render_parameters_schema(&schema);
        assert!(html.contains("Parameters"));
        assert!(html.contains("command"));
        assert!(html.contains("string, required"));
        assert!(html.contains("The shell command to execute"));
        assert!(html.contains("timeout"));
        assert!(html.contains("number"));
        assert!(!html.contains("number, required")); // timeout is not required
    }

    #[test]
    fn test_render_parameters_schema_with_enum() {
        let schema: serde_json::Value = serde_json::from_str(r#"{
            "type": "object",
            "properties": {
                "output_mode": {
                    "type": "string",
                    "description": "Output mode",
                    "enum": ["content", "files_with_matches", "count"]
                }
            },
            "required": []
        }"#).unwrap();

        let html = render_parameters_schema(&schema);
        assert!(html.contains("output_mode"));
        assert!(html.contains("content"));
        assert!(html.contains("files_with_matches"));
        assert!(html.contains("count"));
    }

    // -----------------------------------------------------------------------
    // Full integration: promptLogs fixture renders correctly
    // -----------------------------------------------------------------------

    #[test]
    fn test_render_prompt_logs_fixture() {
        let raw = fs::read_to_string("tests/fixtures/sample-session-prompt-logs.json")
            .expect("prompt-logs fixture should exist");
        let session: Session = serde_json::from_str(&raw).expect("should parse");
        let html = render_session(&session, None);

        // System prompt from promptLogs should appear
        assert!(html.contains("class=\"system-prompt\""));
        assert!(html.contains("expert software engineer"));

        // Tools reference panel should show structured tool info
        assert!(html.contains("class=\"tools-reference\""));
        assert!(html.contains("Bash"));
        assert!(html.contains("Read"));

        // Parameters should be rendered
        assert!(html.contains("tool-params"));
        assert!(html.contains("command"));
        assert!(html.contains("file_path"));

        // systemMessageDescription prefix should appear as short description
        assert!(html.contains("Execute shell commands and return output"));

        // Tool calls should appear
        assert!(html.contains("tool-call-header"));

        // Token usage should appear
        assert!(html.contains("token-badge"));

        // Model should be extracted from promptLogs
        assert!(html.contains("Claude 3.5 Sonnet"));
    }
}
