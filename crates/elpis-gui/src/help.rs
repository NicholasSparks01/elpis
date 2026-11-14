//! The Help window: the bundled documentation, searchable and linked, readable offline.
//!
//! The pages are the project's own `docs/*.md`, embedded in the executable, so help and documentation are one set of
//! files and cannot drift apart. They are turned into HTML here (the page then only displays it):
//!
//! * Raw HTML in a document is never passed through; it is shown as text.
//! * Links between documents become in-window navigation (`data-topic="page#anchor"`); links to files elsewhere in
//!   the repository are shown as plain references (the app has no repository to open); external links are shown as
//!   text followed by the address, since nothing in this window opens a browser.
//! * Headings get GitHub-style anchors (`## The data folder` is `#the-data-folder`), so the existing cross-links work.
//!
//! Tests keep the set honest: every file in `docs/` is listed in [`PAGES`], and every link in every page, and every
//! help topic the GUI asks for, resolves to a real page and heading.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

/// A bundled document. `file` is the name inside `docs/`; other documents link to it by that name.
struct Def {
    id: &'static str,
    group: &'static str,
    file: &'static str,
    source: &'static str,
}

/// The contents, in reading order. Add a new `docs/*.md` here (a test fails until you do).
const PAGES: &[Def] = &[
    Def { id: "index", group: "Start here", file: "README.md", source: include_str!("../../../docs/README.md") },
    Def { id: "getting-started", group: "Start here", file: "getting-started.md", source: include_str!("../../../docs/getting-started.md") },
    Def { id: "usage", group: "Using Elpis", file: "usage.md", source: include_str!("../../../docs/usage.md") },
    Def { id: "gui", group: "Using Elpis", file: "gui.md", source: include_str!("../../../docs/gui.md") },
    Def { id: "tui", group: "Using Elpis", file: "tui.md", source: include_str!("../../../docs/tui.md") },
    Def { id: "cli", group: "Using Elpis", file: "cli.md", source: include_str!("../../../docs/cli.md") },
    Def { id: "capsules", group: "Using Elpis", file: "capsules.md", source: include_str!("../../../docs/capsules.md") },
    Def { id: "networking", group: "Networking and security", file: "networking.md", source: include_str!("../../../docs/networking.md") },
    Def { id: "credentials", group: "Networking and security", file: "credentials.md", source: include_str!("../../../docs/credentials.md") },
    Def { id: "security", group: "Networking and security", file: "security.md", source: include_str!("../../../docs/security.md") },
    Def { id: "configuration", group: "Reference", file: "configuration.md", source: include_str!("../../../docs/configuration.md") },
    Def { id: "capsule-manifest", group: "Reference", file: "capsule-manifest.md", source: include_str!("../../../docs/capsule-manifest.md") },
    Def { id: "troubleshooting", group: "Reference", file: "troubleshooting.md", source: include_str!("../../../docs/troubleshooting.md") },
    Def { id: "architecture", group: "Reference", file: "architecture.md", source: include_str!("../../../docs/architecture.md") },
    Def { id: "vmm", group: "Reference", file: "vmm.md", source: include_str!("../../../docs/vmm.md") },
];

// ---------------------------------------------------------------- rendering

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Heading {
    pub level: u8,
    pub id: String,
    pub text: String,
}

/// A stretch of a page between two headings, for search.
#[derive(Clone, Debug)]
struct Section {
    /// The heading's anchor; empty for the text before the first heading
    anchor: String,
    heading: String,
    text: String,
}

struct Rendered {
    id: &'static str,
    group: &'static str,
    title: String,
    html: String,
    headings: Vec<Heading>,
    sections: Vec<Section>,
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A GitHub-style anchor: lowercase letters and digits kept, spaces become hyphens, other punctuation dropped.
fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.trim().chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if c == '-' || c == '_' {
            out.push(c);
        } else if c.is_whitespace() {
            out.push('-');
        }
    }
    if out.is_empty() { "section".to_owned() } else { out }
}

/// Only characters that can't break out of an attribute.
fn safe_token(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '.')).collect()
}

/// What a link in a document points to.
#[derive(Debug, PartialEq)]
enum Target {
    /// `#anchor` within the same page
    InPage(String),
    /// Another bundled document: page id and optional anchor
    Page(&'static str, Option<String>),
    /// An address on the web
    External(String),
    /// A file elsewhere in the repository (or anything else): shown, not followed
    Reference(String),
}

fn classify(dest: &str) -> Target {
    if let Some(anchor) = dest.strip_prefix('#') {
        return Target::InPage(safe_token(anchor));
    }
    if dest.starts_with("https://") || dest.starts_with("http://") {
        return Target::External(dest.to_owned());
    }
    let (path, frag) = match dest.split_once('#') {
        Some((p, f)) => (p, Some(safe_token(f))),
        None => (dest, None),
    };
    let path = path.strip_prefix("./").unwrap_or(path);
    match PAGES.iter().find(|d| d.file == path) {
        Some(def) => Target::Page(def.id, frag.filter(|f| !f.is_empty())),
        None => Target::Reference(dest.to_owned()),
    }
}

/// Close tag for the link kinds that open a tag we need to balance.
enum LinkEnd {
    Anchor,
    External(String),
    Reference,
}

fn render(def: &'static Def) -> Rendered {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut html = String::new();
    let mut headings: Vec<Heading> = Vec::new();
    let mut sections = vec![Section { anchor: String::new(), heading: String::new(), text: String::new() }];
    let mut used: HashMap<String, usize> = HashMap::new();
    let mut title = String::new();

    // the heading being read: where its tag goes in `html`, its level, and its text so far
    let mut heading: Option<(usize, u8, String)> = None;
    let mut links: Vec<LinkEnd> = Vec::new();
    let mut in_table_head = false;
    let mut in_code = false;

    for event in Parser::new_ext(def.source, options) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    let n = match level {
                        HeadingLevel::H1 => 1,
                        HeadingLevel::H2 => 2,
                        HeadingLevel::H3 => 3,
                        HeadingLevel::H4 => 4,
                        HeadingLevel::H5 => 5,
                        HeadingLevel::H6 => 6,
                    };
                    heading = Some((html.len(), n, String::new()));
                }
                Tag::Paragraph => html.push_str("<p>"),
                Tag::BlockQuote(_) => html.push_str("<blockquote>"),
                Tag::CodeBlock(kind) => {
                    in_code = true;
                    let lang = match kind {
                        CodeBlockKind::Fenced(info) => safe_token(info.split_whitespace().next().unwrap_or("")),
                        CodeBlockKind::Indented => String::new(),
                    };
                    if lang.is_empty() { html.push_str("<pre><code>") } else { html.push_str(&format!("<pre><code class=\"lang-{lang}\">")) }
                }
                Tag::List(None) => html.push_str("<ul>"),
                Tag::List(Some(1)) => html.push_str("<ol>"),
                Tag::List(Some(n)) => html.push_str(&format!("<ol start=\"{n}\">")),
                Tag::Item => html.push_str("<li>"),
                Tag::Emphasis => html.push_str("<em>"),
                Tag::Strong => html.push_str("<strong>"),
                Tag::Strikethrough => html.push_str("<del>"),
                Tag::Link { dest_url, .. } => match classify(&dest_url) {
                    Target::InPage(a) if !a.is_empty() => {
                        html.push_str(&format!("<a class=\"in-page\" href=\"#{a}\">"));
                        links.push(LinkEnd::Anchor);
                    }
                    Target::Page(id, frag) => {
                        let topic = match frag {
                            Some(f) => format!("{id}#{f}"),
                            None => id.to_owned(),
                        };
                        html.push_str(&format!("<a class=\"doc-link\" data-topic=\"{topic}\" href=\"#\">"));
                        links.push(LinkEnd::Anchor);
                    }
                    Target::External(url) => {
                        html.push_str("<span class=\"ext\">");
                        links.push(LinkEnd::External(url));
                    }
                    Target::InPage(_) | Target::Reference(_) => {
                        html.push_str(&format!("<span class=\"ref\" title=\"{}\">", escape(&dest_url)));
                        links.push(LinkEnd::Reference);
                    }
                },
                Tag::Image { .. } => html.push_str("<span class=\"ref\">[image: "),
                Tag::Table(_) => html.push_str("<div class=\"table-wrap\"><table>"),
                Tag::TableHead => {
                    in_table_head = true;
                    html.push_str("<thead><tr>");
                }
                Tag::TableRow => html.push_str("<tr>"),
                Tag::TableCell => html.push_str(if in_table_head { "<th>" } else { "<td>" }),
                // raw HTML blocks are shown as text (the Html events inside are escaped below)
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) => {
                    if let Some((at, level, text)) = heading.take() {
                        let base = slug(&text);
                        let n = used.entry(base.clone()).or_insert(0);
                        let id = if *n == 0 { base.clone() } else { format!("{base}-{n}") };
                        *n += 1;
                        html.insert_str(at, &format!("<h{level} id=\"{id}\">"));
                        html.push_str(&format!("</h{level}>"));
                        if level == 1 && title.is_empty() {
                            title = text.clone();
                        }
                        if (2..=4).contains(&level) {
                            headings.push(Heading { level, id: id.clone(), text: text.clone() });
                        }
                        if level <= 4 {
                            sections.push(Section { anchor: id, heading: text, text: String::new() });
                        }
                    }
                }
                TagEnd::Paragraph => html.push_str("</p>"),
                TagEnd::BlockQuote(_) => html.push_str("</blockquote>"),
                TagEnd::CodeBlock => {
                    in_code = false;
                    html.push_str("</code></pre>");
                }
                TagEnd::List(true) => html.push_str("</ol>"),
                TagEnd::List(false) => html.push_str("</ul>"),
                TagEnd::Item => html.push_str("</li>"),
                TagEnd::Emphasis => html.push_str("</em>"),
                TagEnd::Strong => html.push_str("</strong>"),
                TagEnd::Strikethrough => html.push_str("</del>"),
                TagEnd::Link => match links.pop() {
                    Some(LinkEnd::Anchor) => html.push_str("</a>"),
                    Some(LinkEnd::External(url)) => html.push_str(&format!("</span> <span class=\"ext-url\">({})</span>", escape(&url))),
                    Some(LinkEnd::Reference) => html.push_str("</span>"),
                    None => {}
                },
                TagEnd::Image => html.push_str("]</span>"),
                TagEnd::TableHead => {
                    in_table_head = false;
                    html.push_str("</tr></thead><tbody>");
                }
                TagEnd::TableRow => html.push_str("</tr>"),
                TagEnd::TableCell => html.push_str(if in_table_head { "</th>" } else { "</td>" }),
                TagEnd::Table => html.push_str("</tbody></table></div>"),
                _ => {}
            },
            Event::Text(t) | Event::Html(t) | Event::InlineHtml(t) => {
                let t: &str = &t;
                if let Some((_, _, text)) = heading.as_mut() {
                    text.push_str(t);
                } else if let Some(s) = sections.last_mut() {
                    s.text.push_str(t);
                    s.text.push(' ');
                }
                html.push_str(&escape(t));
            }
            Event::Code(t) => {
                if let Some((_, _, text)) = heading.as_mut() {
                    text.push_str(&t);
                } else if let Some(s) = sections.last_mut() {
                    s.text.push_str(&t);
                    s.text.push(' ');
                }
                if !in_code {
                    html.push_str(&format!("<code>{}</code>", escape(&t)));
                } else {
                    html.push_str(&escape(&t));
                }
            }
            Event::SoftBreak => html.push('\n'),
            Event::HardBreak => html.push_str("<br>"),
            Event::Rule => html.push_str("<hr>"),
            Event::TaskListMarker(done) => html.push_str(if done { "☑ " } else { "☐ " }),
            _ => {}
        }
    }

    if title.is_empty() {
        title = def.id.to_owned();
    }
    Rendered { id: def.id, group: def.group, title, html, headings, sections }
}

fn rendered() -> &'static [Rendered] {
    static CACHE: OnceLock<Vec<Rendered>> = OnceLock::new();
    CACHE.get_or_init(|| PAGES.iter().map(render).collect())
}

// ---------------------------------------------------------------- what the page asks for

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PageMeta {
    pub id: &'static str,
    pub title: String,
    pub group: &'static str,
    pub headings: Vec<Heading>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PageHtml {
    pub id: &'static str,
    pub title: String,
    pub html: String,
}

/// The contents: every page, in order, with its headings.
pub fn pages() -> Vec<PageMeta> {
    rendered().iter().map(|p| PageMeta { id: p.id, title: p.title.clone(), group: p.group, headings: p.headings.clone() }).collect()
}

pub fn page(id: &str) -> Option<PageHtml> {
    rendered().iter().find(|p| p.id == id).map(|p| PageHtml { id: p.id, title: p.title.clone(), html: p.html.clone() })
}

/// Check a topic (`page` or `page#anchor`) against the bundled pages: the page's id, and the anchor if it has one.
pub fn resolve(topic: &str) -> Option<(&'static str, Option<String>)> {
    let (id, anchor) = match topic.split_once('#') {
        Some((i, a)) => (i, Some(a)),
        None => (topic, None),
    };
    let p = rendered().iter().find(|p| p.id == id)?;
    match anchor.filter(|a| !a.is_empty()) {
        None => Some((p.id, None)),
        Some(a) if p.headings.iter().any(|h| h.id == a) || a == "top" => Some((p.id, Some(a.to_owned()))),
        Some(_) => None,
    }
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Hit {
    pub page: &'static str,
    pub page_title: String,
    pub group: &'static str,
    /// The section's anchor (empty for a page's opening text)
    pub anchor: String,
    pub section: String,
    pub snippet: String,
}

/// About `width` characters of `text` around the first occurrence of any of `words` (lowercase).
fn snippet(text: &str, words: &[String], width: usize) -> String {
    let chars: Vec<char> = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().collect();
    let lower: Vec<char> = chars.iter().map(|c| c.to_lowercase().next().unwrap_or(*c)).collect();
    let at = words
        .iter()
        .filter_map(|w| {
            let w: Vec<char> = w.chars().collect();
            (!w.is_empty() && lower.len() >= w.len()).then(|| (0..=lower.len() - w.len()).find(|&i| lower[i..i + w.len()] == w[..])).flatten()
        })
        .min()
        .unwrap_or(0);
    let start = at.saturating_sub(width / 3);
    let end = (start + width).min(chars.len());
    let mut out: String = chars[start..end].iter().collect();
    if start > 0 {
        out.insert(0, '…');
    }
    if end < chars.len() {
        out.push('…');
    }
    out
}

/// Sections containing every word of `query` (case-insensitive), best first: a word in a heading counts most.
pub fn search(query: &str, limit: usize) -> Vec<Hit> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<(u32, usize, usize, Hit)> = Vec::new(); // score, page order, section order
    for (page_order, p) in rendered().iter().enumerate() {
        let title = p.title.to_lowercase();
        for (section_order, s) in p.sections.iter().enumerate() {
            let (heading, body) = (s.heading.to_lowercase(), s.text.to_lowercase());
            if !words.iter().all(|w| heading.contains(w.as_str()) || body.contains(w.as_str()) || title.contains(w.as_str())) {
                continue;
            }
            let mut score = 0;
            for w in &words {
                if heading.contains(w.as_str()) {
                    score += 10;
                }
                if title.contains(w.as_str()) {
                    score += 3;
                }
                score += body.matches(w.as_str()).count().min(5) as u32;
            }
            let shown = if s.text.trim().is_empty() { s.heading.clone() } else { snippet(&s.text, &words, 170) };
            hits.push((score, page_order, section_order, Hit { page: p.id, page_title: p.title.clone(), group: p.group, anchor: s.anchor.clone(), section: s.heading.clone(), snippet: shown }));
        }
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    hits.into_iter().take(limit).map(|h| h.3).collect()
}

// ---------------------------------------------------------------- the window

/// The topic a newly opened Help window should show first (the window asks for it once, when it loads).
#[derive(Default)]
pub struct HelpState {
    pending: Mutex<Option<String>>,
}

/// Open the Help window, or bring it forward, at `topic` (`page` or `page#anchor`; anything unknown is the contents page).
pub async fn show(app: &AppHandle, topic: Option<String>) -> Result<(), String> {
    let topic = topic.filter(|t| resolve(t).is_some());
    *app.state::<HelpState>().pending.lock().unwrap() = topic.clone();
    if let Some(window) = app.get_webview_window("help") {
        let _ = window.unminimize();
        window.show().map_err(|e| e.to_string())?;
        window.set_focus().map_err(|e| e.to_string())?;
        // an open window is already loaded, so it is told rather than asked
        app.emit_to("help", "help-open", topic).map_err(|e| e.to_string())?;
        return Ok(());
    }
    WebviewWindowBuilder::new(app, "help", WebviewUrl::App("help.html".into()))
        .title("Elpis Help")
        .inner_size(1040.0, 760.0)
        .min_inner_size(640.0, 420.0)
        .build()
        .map_err(|e| format!("couldn't open the Help window: {e}"))?;
    Ok(())
}

#[tauri::command]
pub async fn open_help(app: AppHandle, topic: Option<String>) -> Result<(), String> {
    show(&app, topic).await
}

#[tauri::command]
pub fn help_pages() -> Vec<PageMeta> {
    pages()
}

#[tauri::command]
pub fn help_page(id: String) -> Result<PageHtml, String> {
    page(&id).ok_or_else(|| format!("there is no help page {id:?}"))
}

#[tauri::command]
pub fn help_search(query: String) -> Vec<Hit> {
    search(&query, 40)
}

/// What the Help window should show when it first loads (once; later requests arrive as `help-open` events).
#[tauri::command]
pub fn help_take_topic(state: tauri::State<'_, HelpState>) -> Option<String> {
    state.pending.lock().unwrap().take()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(source: &'static str) -> &'static Def {
        Box::leak(Box::new(Def { id: "t", group: "G", file: "t.md", source }))
    }

    fn html(md: &'static str) -> String {
        render(def(md)).html
    }

    #[test]
    fn slugs_follow_the_github_rule() {
        assert_eq!(slug("Capsules (preview)"), "capsules-preview");
        assert_eq!(slug("1. Check the prerequisites"), "1-check-the-prerequisites");
        assert_eq!(slug("The data folder"), "the-data-folder");
        assert_eq!(slug("Things to know"), "things-to-know");
        assert_eq!(slug("A `code` heading!"), "a-code-heading");
        assert_eq!(slug("Über uns"), "über-uns");
        assert_eq!(slug("???"), "section");
    }

    #[test]
    fn headings_get_anchors_and_duplicates_are_numbered() {
        let r = render(def("# Title\n\n## Same\n\ntext\n\n## Same\n\n### `code` in heading\n"));
        assert_eq!(r.title, "Title");
        let ids: Vec<&str> = r.headings.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, ["same", "same-1", "code-in-heading"]);
        assert!(r.html.contains("<h1 id=\"title\">Title</h1>") && r.html.contains("<h2 id=\"same-1\">Same</h2>"));
        assert!(r.html.contains("<h3 id=\"code-in-heading\"><code>code</code> in heading</h3>"), "{}", r.html);
        assert_eq!((r.headings[0].level, r.headings[2].level), (2, 3));
    }

    #[test]
    fn raw_html_is_shown_as_text_never_passed_through() {
        let out = html("# T\n\n<script>alert(1)</script>\n\nInline <img src=x onerror=alert(1)> here.\n\n<div onclick=\"x\">block</div>\n");
        assert!(!out.contains("<script") && !out.contains("<img") && !out.contains("<div onclick"), "{out}");
        assert!(out.contains("&lt;script&gt;alert(1)&lt;/script&gt;") && out.contains("&lt;img src=x onerror=alert(1)&gt;"), "{out}");
        // and nothing in text or code can close an attribute or tag
        let out = html("# T\n\n`<b>` and \"quotes\" & 'apos'\n\n```\n</code></pre><script>\n```\n");
        assert!(out.contains("<code>&lt;b&gt;</code>") && out.contains("&quot;quotes&quot; &amp; &#39;apos&#39;"), "{out}");
        assert!(out.contains("&lt;/code&gt;&lt;/pre&gt;&lt;script&gt;"), "{out}");
    }

    #[test]
    fn links_between_documents_become_navigation() {
        let out = html("[usage](usage.md) and [the TUI](usage.md#the-tui) and [here](#section-one) and [dot](./gui.md)\n");
        assert!(out.contains("<a class=\"doc-link\" data-topic=\"usage\" href=\"#\">usage</a>"), "{out}");
        assert!(out.contains("data-topic=\"usage#the-tui\""), "{out}");
        assert!(out.contains("<a class=\"in-page\" href=\"#section-one\">here</a>"), "{out}");
        assert!(out.contains("data-topic=\"gui\""), "./ is allowed");
        // the docs index is the README
        assert!(html("[docs](README.md)").contains("data-topic=\"index\""));
    }

    #[test]
    fn other_links_are_shown_not_followed() {
        let out = html("[crate](../crates/elpis-gui/README.md) and [site](https://example.com/x?a=1&b=2) and [js](javascript:alert(1)) and [unknown](nothere.md)\n");
        assert!(out.contains("<span class=\"ref\" title=\"../crates/elpis-gui/README.md\">crate</span>"), "{out}");
        assert!(out.contains("<span class=\"ext\">site</span> <span class=\"ext-url\">(https://example.com/x?a=1&amp;b=2)</span>"), "{out}");
        assert!(out.contains("<span class=\"ref\" title=\"javascript:alert(1)\">js</span>"), "a script URL is never a link: {out}");
        assert!(out.contains("title=\"nothere.md\""));
        assert!(!out.contains("href=\"javascript") && !out.contains("href=\"../") && !out.contains("href=\"https"));
        // an anchor can't carry markup
        assert!(!html("[x](#a\"onmouseover=\"alert(1))").contains("onmouseover=\""));
    }

    #[test]
    fn blocks_are_rendered() {
        let out = html("# T\n\nA **bold**, *italic* and ~~gone~~ word.\n\n- one\n- two\n\n3. three\n4. four\n\n> a note\n\n```bash\nelpis-run ls\n```\n\n---\n\n- [x] done\n- [ ] todo\n");
        for want in ["<strong>bold</strong>", "<em>italic</em>", "<del>gone</del>", "<ul><li>one</li><li>two</li></ul>", "<ol start=\"3\">", "<blockquote><p>a note</p></blockquote>", "<pre><code class=\"lang-bash\">elpis-run ls\n</code></pre>", "<hr>", "☑ done", "☐ todo"] {
            assert!(out.contains(want), "missing {want:?} in {out}");
        }
        assert!(html("```we ird\"x\ncode\n```").contains("class=\"lang-we\""), "the language is reduced to safe characters");
    }

    #[test]
    fn tables_have_a_head_and_a_body() {
        let out = html("| A | B |\n|---|---|\n| `x` | y |\n| z | w |\n");
        assert!(out.contains("<div class=\"table-wrap\"><table><thead><tr><th>A</th><th>B</th></tr></thead><tbody><tr><td><code>x</code></td><td>y</td></tr><tr><td>z</td><td>w</td></tr></tbody></table></div>"), "{out}");
    }

    #[test]
    fn sections_split_at_headings_and_search_ranks_by_heading() {
        // the real pages: a word in a heading beats the same word in body text
        let hits = search("published ports", 10);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].page, "usage");
        assert!(hits[0].section.to_lowercase().contains("published ports"), "{:?}", hits[0].section);
        assert!(search("", 10).is_empty() && search("   ", 10).is_empty());
        assert!(search("zzzqqqxxx", 10).is_empty());
        // every word must be present, in any order and case
        assert_eq!(search("PORTS published", 10)[0].section, hits[0].section);
        assert!(search("published zzzqqqxxx", 10).is_empty());
        assert!(search("ports", 3).len() <= 3);
    }

    #[test]
    fn snippets_are_cut_on_character_boundaries_around_the_match() {
        let text = format!("{} needle {}", "ä".repeat(300), "ö".repeat(300));
        let s = snippet(&text, &["needle".to_owned()], 100);
        assert!(s.contains("needle") && s.starts_with('…') && s.ends_with('…') && s.chars().count() <= 104, "{s}");
        // no match: the start of the text
        assert!(snippet("short text", &["nothing".to_owned()], 100) == "short text");
        assert_eq!(snippet("", &["x".to_owned()], 100), "");
        assert!(snippet("日本語のテキスト needle 終わり", &["needle".to_owned()], 10).contains("needle"));
    }

    #[test]
    fn topics_resolve_only_to_real_pages_and_headings() {
        assert_eq!(resolve("usage"), Some(("usage", None)));
        assert_eq!(resolve("usage#the-tui"), Some(("usage", Some("the-tui".to_owned()))));
        assert_eq!(resolve("usage#top"), Some(("usage", Some("top".to_owned()))));
        assert!(resolve("usage#no-such-heading").is_none() && resolve("nope").is_none() && resolve("").is_none());
        assert!(page("usage").is_some() && page("nope").is_none());
    }

    // ------------------------------------------------------------ the documentation itself

    fn docs_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs")
    }

    #[test]
    fn every_document_in_docs_is_in_the_contents() {
        let mut missing = Vec::new();
        for entry in std::fs::read_dir(docs_dir()).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".md") && !PAGES.iter().any(|d| d.file == name) {
                missing.push(name);
            }
        }
        assert!(missing.is_empty(), "add these to PAGES in crates/elpis-gui/src/help.rs so the Help window shows them: {missing:?}");
        for d in PAGES {
            assert!(docs_dir().join(d.file).is_file(), "PAGES lists docs/{} but it doesn't exist", d.file);
        }
        let mut ids: Vec<&str> = PAGES.iter().map(|d| d.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), PAGES.len(), "page ids must be unique");
    }

    #[test]
    fn every_page_has_a_title_and_no_placeholder_text() {
        for p in rendered() {
            assert!(!p.title.is_empty() && p.title != p.id, "page {} needs a `# Title` heading", p.id);
            assert!(!p.html.contains("(placeholder)"), "page {} is still a placeholder", p.id);
            assert!(p.sections.iter().map(|s| s.text.len()).sum::<usize>() > 100, "page {} is nearly empty", p.id);
        }
    }

    #[test]
    fn every_link_in_every_page_goes_somewhere() {
        let mut problems = Vec::new();
        for def in PAGES {
            let here = rendered().iter().find(|p| p.id == def.id).unwrap();
            for event in Parser::new_ext(def.source, Options::ENABLE_TABLES) {
                let Event::Start(Tag::Link { dest_url, .. }) = event else { continue };
                match classify(&dest_url) {
                    Target::InPage(a) => {
                        if !here.headings.iter().any(|h| h.id == a) {
                            problems.push(format!("docs/{}: #{a} isn't a heading on that page", def.file));
                        }
                    }
                    Target::Page(id, frag) => {
                        let target = format!("{id}{}", frag.map(|f| format!("#{f}")).unwrap_or_default());
                        if resolve(&target).is_none() {
                            problems.push(format!("docs/{}: {dest_url} has no heading to land on", def.file));
                        }
                    }
                    Target::External(url) => {
                        if !url.starts_with("https://") {
                            problems.push(format!("docs/{}: {url} should be https", def.file));
                        }
                    }
                    Target::Reference(path) => {
                        let file = path.split('#').next().unwrap_or_default();
                        if !docs_dir().join(file).exists() {
                            problems.push(format!("docs/{}: {path} doesn't exist in the repository", def.file));
                        }
                    }
                }
            }
        }
        assert!(problems.is_empty(), "broken links:\n{}", problems.join("\n"));
    }

    #[test]
    fn the_help_topics_the_gui_asks_for_exist() {
        // every `helpTopic('page#anchor')` and `help: 'page#anchor'` in the page's scripts
        let mut missing = Vec::new();
        let mut seen = 0;
        for (name, js) in [("app.js", include_str!("../ui/app.js")), ("boxes.js", include_str!("../ui/boxes.js")), ("help.js", include_str!("../ui/help.js"))] {
            for marker in ["helpTopic('", "help: '"] {
                for part in js.split(marker).skip(1) {
                    let topic = part.split('\'').next().unwrap_or_default();
                    seen += 1;
                    if resolve(topic).is_none() {
                        missing.push(format!("{name}: {topic:?}"));
                    }
                }
            }
        }
        assert!(seen >= 5, "expected the GUI to link to help topics (found {seen})");
        assert!(missing.is_empty(), "help topics that don't exist: {missing:?}");
    }

    /// `cargo test -p elpis-gui -- --ignored print_help_fixtures` with GUI_HELP_FIXTURE_OUT set writes the JSON the
    /// Help window receives, for trying the page against a mock backend in a browser.
    #[test]
    #[ignore]
    fn print_help_fixtures() {
        let all: Vec<_> = PAGES.iter().map(|d| page(d.id).unwrap()).collect();
        let fixture = serde_json::json!({ "pages": pages(), "html": all, "search": search("capsule trust", 40) });
        if let Ok(path) = std::env::var("GUI_HELP_FIXTURE_OUT") {
            std::fs::write(path, serde_json::to_string(&fixture).unwrap()).unwrap();
        }
    }
}
