use clap::Parser;
use html_escape::encode_text;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser as MdParser, Tag, TagEnd};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
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
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct PromptLog {
    #[serde(default)]
    model_title: String,
    #[serde(default)]
    completion_options: Option<CompletionOptions>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct CompletionOptions {
    #[serde(default)]
    model: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ChatMessage {
    role: String,
    #[serde(default)]
    content: MessageContent,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallDelta>>,
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
    ImageUrl {
        #[allow(dead_code)]
        #[serde(default)]
        url: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ToolCallDelta {
    #[serde(default)]
    function: Option<ToolCallFunction>,
    #[allow(dead_code)]
    #[serde(default)]
    id: Option<String>,
    #[allow(dead_code)]
    #[serde(default, rename = "type")]
    call_type: Option<String>,
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
                // Try to pretty-print JSON arguments
                let formatted = match serde_json::from_str::<serde_json::Value>(&func.arguments) {
                    Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| func.arguments.clone()),
                    Err(_) => func.arguments.clone(),
                };
                html.push_str("<pre class=\"tool-args\"><code>");
                html.push_str(&encode_text(&formatted));
                html.push_str("</code></pre>");
            }
            html.push_str("</div>");
        }
    }
    html
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
            html.push_str("<pre class=\"context-content\"><code>");
            html.push_str(&encode_text(&item.content));
            html.push_str("</code></pre>");
        }
        html.push_str("</div>");
    }
    html.push_str("</details>");
    html
}

fn render_message(item: &ChatHistoryItem) -> String {
    let msg = &item.message;
    let role = msg.role.as_str();
    let cls = role_class(role);
    let label = role_label(role);
    let content_text = msg.content.text();
    let is_thinking = role == "thinking";

    let mut html = String::new();
    html.push_str(&format!("<div class=\"message {cls}\">\n"));

    if is_thinking {
        // Thinking sections are collapsible (expanded by default)
        html.push_str("  <details open class=\"thinking-details\">\n");
        html.push_str(&format!(
            "    <summary class=\"message-header\"><span class=\"role-label\">{label}</span></summary>\n"
        ));
    } else {
        html.push_str(&format!(
            "  <div class=\"message-header\"><span class=\"role-label\">{label}</span></div>\n"
        ));
    }

    // Context items (collapsed by default)
    html.push_str(&render_context_items(&item.context_items));

    // Main content
    if !msg.content.is_empty() {
        html.push_str("  <div class=\"message-content\">\n");
        if role == "user" {
            // For user messages render as plain text (they are usually short prompts)
            html.push_str(&format!(
                "    <p>{}</p>\n",
                encode_text(&content_text)
            ));
        } else {
            // For assistant / system / thinking — render markdown
            html.push_str(&format!("    {}\n", markdown_to_html(&content_text)));
        }
        html.push_str("  </div>\n");
    }

    // Tool calls
    if let Some(calls) = &msg.tool_calls {
        if !calls.is_empty() {
            html.push_str(&render_tool_calls(calls));
        }
    }

    if is_thinking {
        html.push_str("  </details>\n");
    }

    html.push_str("</div>\n");
    html
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

fn render_session(session: &Session, source_path: Option<&Path>) -> String {
    let title = if session.title.is_empty() {
        "Untitled Session"
    } else {
        &session.title
    };

    let mut messages_html = String::new();
    let mut user_count = 0u32;
    let mut assistant_count = 0u32;

    for item in &session.history {
        match item.message.role.as_str() {
            "user" => user_count += 1,
            "assistant" => assistant_count += 1,
            _ => {}
        }
        messages_html.push_str(&render_message(item));
    }

    let date_str = session
        .date_created
        .as_deref()
        .map(|s| s.to_string())
        .or_else(|| source_path.and_then(file_modified_date))
        .unwrap_or_else(|| "Unknown date".to_string());

    let workspace = if session.workspace_directory.is_empty() {
        "N/A"
    } else {
        &session.workspace_directory
    };

    let model = extract_model(session);
    let model_meta = match &model {
        Some(m) => format!(
            "\n      <span class=\"meta-item\">Model: <code>{}</code></span>",
            encode_text(m)
        ),
        None => String::new(),
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
<div class="container">
  <header class="session-header">
    <h1>{title}</h1>
    <div class="session-meta">
      <span class="meta-item">Session: <code>{session_id}</code></span>
      <span class="meta-item">Date: {date}</span>{model_meta}
      <span class="meta-item">Workspace: <code>{workspace}</code></span>
      <span class="meta-item">{user_count} user messages &middot; {assistant_count} assistant messages</span>
    </div>
  </header>
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
        workspace = encode_text(workspace),
        user_count = user_count,
        assistant_count = assistant_count,
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
  max-height: 200px;
  overflow-y: auto;
}

/* ----- Footer ----- */
footer {
  text-align: center;
  padding: 24px 0 8px;
  font-size: 0.8rem;
  color: var(--text-muted);
}

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
  document.querySelectorAll('.message-content pre, .tool-args, .context-content').forEach(function(el) {
    if (el.scrollHeight > 350) {
      el.style.maxHeight = '300px';
      el.style.overflow = 'hidden';
      el.style.position = 'relative';

      var btn = document.createElement('button');
      btn.textContent = 'Show more';
      btn.style.cssText = 'display:block;margin:6px auto 0;padding:4px 16px;border:1px solid #cbd5e1;border-radius:6px;background:#fff;color:#475569;cursor:pointer;font-size:0.8rem;';
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
        for entry in glob::glob(&pattern).expect("Failed to read glob pattern") {
            if let Ok(p) = entry {
                files.push(p);
            }
        }
    }
    files.sort();
    files
}

fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect::<String>()
        .chars()
        .take(80)
        .collect()
}

fn main() {
    let cli = Cli::parse();

    let input = &cli.input;
    let output_dir = &cli.output;

    if !input.exists() {
        eprintln!("Error: input path does not exist: {}", input.display());
        std::process::exit(1);
    }

    let files = discover_session_files(input);
    if files.is_empty() {
        eprintln!("No session JSON files found at: {}", input.display());
        std::process::exit(1);
    }

    fs::create_dir_all(output_dir).expect("Failed to create output directory");

    let mut processed = 0u32;
    let mut index_entries: Vec<(String, String, String)> = Vec::new(); // (title, filename, date)

    for file in &files {
        let raw = match fs::read_to_string(file) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Warning: could not read {}: {}", file.display(), e);
                continue;
            }
        };

        let session: Session = match serde_json::from_str(&raw) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Warning: could not parse {}: {}", file.display(), e);
                continue;
            }
        };

        // Apply title filter if provided
        if let Some(ref filter) = cli.filter {
            if !session
                .title
                .to_lowercase()
                .contains(&filter.to_lowercase())
            {
                continue;
            }
        }

        if session.history.is_empty() {
            continue;
        }

        let html = render_session(&session, Some(file.as_path()));

        let title = if session.title.is_empty() {
            &session.session_id
        } else {
            &session.title
        };
        let filename = format!(
            "{}.html",
            sanitize_filename(title)
        );
        let out_path = output_dir.join(&filename);

        fs::write(&out_path, &html).expect("Failed to write HTML file");
        eprintln!("  Wrote: {}", out_path.display());

        let date = session
            .date_created
            .clone()
            .unwrap_or_default();
        index_entries.push((title.to_string(), filename, date));
        processed += 1;
    }

    // Generate index.html
    if !index_entries.is_empty() {
        let index_html = render_index(&index_entries);
        let index_path = output_dir.join("index.html");
        fs::write(&index_path, &index_html).expect("Failed to write index.html");
        eprintln!("  Wrote: {}", index_path.display());
    }

    eprintln!(
        "\nDone. Processed {} session(s) from {} file(s).",
        processed,
        files.len()
    );
}

// ---------------------------------------------------------------------------
// Index page
// ---------------------------------------------------------------------------

fn render_index(entries: &[(String, String, String)]) -> String {
    let mut rows = String::new();
    for (title, filename, date) in entries {
        rows.push_str(&format!(
            "    <tr>\n      <td><a href=\"{filename}\">{title}</a></td>\n      <td>{date}</td>\n    </tr>\n",
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
</style>
</head>
<body>
<div class="container">
  <header class="session-header">
    <h1>Continue.dev Transcripts</h1>
    <div class="session-meta">
      <span class="meta-item">{count} session(s)</span>
    </div>
  </header>
  <table>
    <thead><tr><th>Session</th><th>Date</th></tr></thead>
    <tbody>
{rows}
    </tbody>
  </table>
  <footer>
    <p>Generated by <strong>continue-transcripts</strong></p>
  </footer>
</div>
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
        assert_eq!(sanitize_filename("Hello World!"), "Hello_World_");
        assert_eq!(sanitize_filename("test/file:name"), "test_file_name");
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
                },
                context_items: vec![],
                prompt_logs: None,
            }],
            date_created: Some("2025-01-01".to_string()),
        };

        let html = render_session(&session, None);
        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("Test"));
        assert!(html.contains("User"));
        assert!(html.contains("Hello"));
    }
}
