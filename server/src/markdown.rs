//! Reddit's Markdown, for previews: as the few tags Telegram's Instant View knows, with links
//! written by the caller (`/_/l/` links on the preview host, say), and as plain text for the card.
//! Raw HTML in the text stays text; only http(s) links become links.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::render::{LinkFn, escape_into, linkify_reddit};

fn parser(text: &str) -> Parser<'_> {
    Parser::new_ext(text, Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES)
}

/// Where a link in the text leads: http(s) links as they are, Reddit's own paths (`/r/rust`) on
/// Reddit; anything else (`javascript:`, `mailto:`) nowhere.
fn destination(url: &str) -> Option<String> {
    if url.starts_with("https://") || url.starts_with("http://") {
        Some(url.to_string())
    } else if url.starts_with('/') && !url.starts_with("//") {
        Some(format!("https://www.reddit.com{url}"))
    } else {
        None
    }
}

/// The text as Instant View HTML, escaped.
pub fn to_html(text: &str, link: LinkFn<'_>) -> String {
    let mut out = String::new();
    // Whether each open link became an `<a>`, so its end knows whether to close one.
    let mut links: Vec<bool> = Vec::new();
    // Inside a code block, text is code: no links made in it, as in inline code.
    let mut in_code = false;
    let mut in_cell = false;
    for event in parser(text) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => out.push_str("<p>"),
                Tag::Heading { level, .. } => out.push_str(heading(level).0),
                Tag::BlockQuote(_) => out.push_str("<blockquote>"),
                Tag::CodeBlock(_) => {
                    in_code = true;
                    out.push_str("<pre>");
                }
                Tag::List(Some(_)) => out.push_str("<ol>\n"),
                Tag::List(None) => out.push_str("<ul>\n"),
                Tag::Item => out.push_str("<li>"),
                Tag::Emphasis => out.push_str("<i>"),
                Tag::Strong => out.push_str("<b>"),
                Tag::Strikethrough => out.push_str("<s>"),
                Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                    let to = destination(&dest_url);
                    if let Some(to) = &to {
                        out.push_str("<a href=\"");
                        escape_into(&link(to), &mut out);
                        out.push_str("\">");
                    }
                    links.push(to.is_some());
                }
                Tag::TableHead | Tag::TableRow => {
                    out.push_str("<p>");
                    in_cell = false;
                }
                Tag::TableCell => {
                    if in_cell {
                        out.push_str(" | ");
                    }
                    in_cell = true;
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph | TagEnd::TableHead | TagEnd::TableRow => out.push_str("</p>\n"),
                TagEnd::Heading(level) => {
                    out.push_str(heading(level).1);
                    out.push('\n');
                }
                TagEnd::BlockQuote(_) => out.push_str("</blockquote>\n"),
                TagEnd::CodeBlock => {
                    in_code = false;
                    out.push_str("</pre>\n");
                }
                TagEnd::List(true) => out.push_str("</ol>\n"),
                TagEnd::List(false) => out.push_str("</ul>\n"),
                TagEnd::Item => out.push_str("</li>\n"),
                TagEnd::Emphasis => out.push_str("</i>"),
                TagEnd::Strong => out.push_str("</b>"),
                TagEnd::Strikethrough => out.push_str("</s>"),
                TagEnd::Link | TagEnd::Image if links.pop() == Some(true) => out.push_str("</a>"),
                _ => {}
            },
            // Bare links and `r/`, `u/` names inside a link's text stay its text.
            Event::Text(text) if in_code || links.last() == Some(&true) => escape_into(&text, &mut out),
            Event::Text(text) => linkify_reddit(&text, link, &mut out),
            Event::Code(code) => {
                out.push_str("<code>");
                escape_into(&code, &mut out);
                out.push_str("</code>");
            }
            Event::Html(html) | Event::InlineHtml(html) => escape_into(&html, &mut out),
            Event::SoftBreak => out.push(' '),
            Event::HardBreak => out.push_str("<br>"),
            Event::Rule => out.push_str("<hr>\n"),
            _ => {}
        }
    }
    out
}

/// Instant View's headings: a post's own `#` is no bigger than its title's `<h1>`.
const fn heading(level: HeadingLevel) -> (&'static str, &'static str) {
    match level {
        HeadingLevel::H1 | HeadingLevel::H2 => ("<h3>", "</h3>"),
        _ => ("<h4>", "</h4>"),
    }
}

/// The text without its Markdown, for the card: paragraphs apart, list items on lines of their
/// own.
pub fn to_text(text: &str) -> String {
    let mut out = String::new();
    let mut in_cell = false;
    for event in parser(text) {
        match event {
            Event::Start(Tag::Item) => out.push_str("• "),
            Event::Start(Tag::TableCell) => {
                if in_cell {
                    out.push_str(" | ");
                }
                in_cell = true;
            }
            Event::Start(Tag::TableHead | Tag::TableRow) => in_cell = false,
            Event::End(TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::CodeBlock | TagEnd::BlockQuote(_)) => {
                out.push_str("\n\n");
            }
            Event::End(TagEnd::Item | TagEnd::TableHead | TagEnd::TableRow) => out.push('\n'),
            Event::End(TagEnd::List(_) | TagEnd::Table) => out.push('\n'),
            Event::Text(text) | Event::Code(text) | Event::Html(text) | Event::InlineHtml(text) => out.push_str(&text),
            Event::SoftBreak => out.push(' '),
            Event::HardBreak => out.push('\n'),
            _ => {}
        }
    }
    // Items end in one break and paragraphs in two; inside an item, a paragraph's would double.
    let mut text = out.replace("\n\n\n", "\n\n");
    while text.contains("\n\n\n") {
        text = text.replace("\n\n\n", "\n\n");
    }
    text.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html(text: &str) -> String {
        to_html(text, &crate::render::direct)
    }

    #[test]
    fn markdown_as_instant_view() {
        assert_eq!(
            html("**Big** *news* ~~old~~ `x<y`"),
            "<p><b>Big</b> <i>news</i> <s>old</s> <code>x&lt;y</code></p>\n"
        );
        assert_eq!(html("# Title\n\ntext\nsame paragraph"), "<h3>Title</h3>\n<p>text same paragraph</p>\n");
        assert_eq!(html("- one\n- two"), "<ul>\n<li>one</li>\n<li>two</li>\n</ul>\n");
        assert_eq!(html("> quoted"), "<blockquote><p>quoted</p>\n</blockquote>\n");
        assert_eq!(html("a  \nb"), "<p>a<br>b</p>\n", "two spaces break the line");
    }

    #[test]
    fn only_web_links_link() {
        assert_eq!(
            html("[docs](https://example.com/a?b=1&c=2)"),
            "<p><a href=\"https://example.com/a?b=1&amp;c=2\">docs</a></p>\n"
        );
        assert_eq!(html("[sub](/r/rust)"), "<p><a href=\"https://www.reddit.com/r/rust\">sub</a></p>\n");
        assert_eq!(html("[x](javascript:alert(1))"), "<p>x</p>\n");
        assert_eq!(html("<script>alert(1)</script>"), "&lt;script&gt;alert(1)&lt;/script&gt;");
        let bare = html("see https://example.com and r/rust, u/someone");
        assert!(bare.contains("<a href=\"https://example.com\">https://example.com</a>"), "{bare}");
        assert!(bare.contains("<a href=\"https://www.reddit.com/r/rust/\">r/rust</a>"), "{bare}");
        assert!(bare.contains("<a href=\"https://www.reddit.com/user/someone/\">u/someone</a>"), "{bare}");
        let linked = html("[r/rust](https://www.reddit.com/r/rust)");
        assert_eq!(linked.matches("<a ").count(), 1, "no link inside a link: {linked}");
        let code = html("```\nsee r/rust at https://example.com\n```");
        assert!(!code.contains("<a "), "code blocks are code: {code}");
    }

    #[test]
    fn plain_text_for_the_card() {
        assert_eq!(
            to_text("**Big** [news](https://x.example)\n\n- one\n- two\n\nend"),
            "Big news\n\n• one\n• two\n\nend"
        );
        assert_eq!(to_text("# Title\nbody"), "Title\n\nbody");
        assert_eq!(to_text("a | b\n--|--\n1 | 2"), "a | b\n1 | 2");
    }
}
