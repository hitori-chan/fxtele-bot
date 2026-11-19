//! A post's text as Instant View's HTML: paragraphs, links, and each site's tags and names.

use super::LinkFn;
use super::site::TextStyle;

/// Paragraphs on blank lines, line breaks within them, clickable http(s) links, hashtags and (on
/// Instagram) `@names`; all escaped.
pub(super) fn text_to_html(text: &str, style: &TextStyle, link: LinkFn<'_>) -> String {
    if matches!(style, TextStyle::Markdown) {
        return crate::markdown::to_html(text, link);
    }
    let mut html = String::new();
    let normalized = text.replace("\r\n", "\n");
    for paragraph in normalized.split("\n\n").map(str::trim).filter(|p| !p.is_empty()) {
        html.push_str("<p>");
        for (i, line) in paragraph.lines().enumerate() {
            if i > 0 {
                html.push_str("<br>");
            }
            linkify(line, style, link, &mut html);
        }
        html.push_str("</p>\n");
    }
    html
}

/// Reddit's text: bare links, and `r/name` and `u/name` to the community and the person.
pub fn linkify_reddit(text: &str, link_to: LinkFn<'_>, out: &mut String) {
    linkify(text, &TextStyle::Markdown, link_to, out);
}

fn linkify(line: &str, style: &TextStyle, link_to: LinkFn<'_>, out: &mut String) {
    let mut rest = line;
    while let Some(start) = find_link(rest) {
        tags_into(&rest[..start], style, link_to, out);
        let link_end = rest[start..].find(char::is_whitespace).map_or(rest.len(), |e| start + e);
        let mut link = &rest[start..link_end];
        // Trailing punctuation belongs to the sentence, not the link.
        while let Some(last) = link.chars().last()
            && matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\'')
        {
            link = &link[..link.len() - last.len_utf8()];
        }
        out.push_str("<a href=\"");
        escape_into(&link_to(link), out);
        out.push_str("\">");
        escape_into(link, out);
        out.push_str("</a>");
        rest = &rest[start + link.len()..];
    }
    tags_into(rest, style, link_to, out);
}

/// `#tag` at the start of a word links to the site's page for it, and `@name` to that account
/// where the site has them; `#1`, `a#b` and `me@example.com` stay text.
fn tags_into(text: &str, style: &TextStyle, link_to: LinkFn<'_>, out: &mut String) {
    let TextStyle::Plain { hashtag, mention } = style else {
        return names_into(text, link_to, out);
    };
    let marks: &[char] = if mention.is_some() { &['#', '@'] } else { &['#'] };
    // Positions in the whole text, so what came before a mark is always the text's own: after
    // `a#` is skipped, the `#tag` in `a##tag` still doesn't start a word.
    let (mut written, mut from) = (0, 0);
    while let Some(found) = text[from..].find(marks) {
        let at = from + found;
        let mark = text[at..].chars().next().unwrap_or('#');
        let after = &text[at + 1..];
        let allowed = |c: char| c.is_alphanumeric() || c == '_' || (mark == '@' && c == '.');
        let name = after[..after.find(|c: char| !allowed(c)).unwrap_or(after.len())].trim_end_matches('.');
        let starts_word = text[..at].chars().last().is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '['));
        if !starts_word || name.is_empty() || (mark == '#' && name.chars().all(|c| c.is_ascii_digit())) {
            from = at + 1;
            continue;
        }
        escape_into(&text[written..at], out);
        let escaped: String = url::form_urlencoded::byte_serialize(name.as_bytes()).collect();
        let page = match (mark, mention) {
            ('@', Some(mention)) => mention(&escaped),
            _ => hashtag(&escaped),
        };
        out.push_str("<a href=\"");
        escape_into(&link_to(&page), out);
        out.push_str("\">");
        out.push(mark);
        escape_into(name, out);
        out.push_str("</a>");
        written = at + 1 + name.len();
        from = written;
    }
    escape_into(&text[written..], out);
}

/// Reddit's `r/name` and `u/name` (or `/r/name`) at the start of a word link to the community and
/// the person.
fn names_into(text: &str, link_to: LinkFn<'_>, out: &mut String) {
    // Positions in the whole text, as in `tags_into`.
    let (mut written, mut from) = (0, 0);
    while let Some(found) = text[from..].find(['r', 'u']) {
        let at = from + found;
        let starts_word = text[..at].chars().last().is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '[' | '/'));
        let slash = text[..at].ends_with('/');
        let kind = &text[at..=at];
        let after = text[at + 1..].strip_prefix('/');
        let name = after.map_or("", |after| {
            &after[..after.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')).unwrap_or(after.len())]
        });
        // `/r/` only when the slash itself starts the word.
        let lead_ok =
            !slash || text[..at - 1].chars().last().is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '['));
        if !starts_word || !lead_ok || name.len() < 2 {
            from = at + 1;
            continue;
        }
        let start = if slash { at - 1 } else { at };
        escape_into(&text[written..start], out);
        let page = match kind {
            "r" => format!("https://www.reddit.com/r/{name}/"),
            _ => format!("https://www.reddit.com/user/{name}/"),
        };
        out.push_str("<a href=\"");
        escape_into(&link_to(&page), out);
        out.push_str("\">");
        escape_into(&text[start..at + 2 + name.len()], out);
        out.push_str("</a>");
        written = at + 2 + name.len();
        from = written;
    }
    escape_into(&text[written..], out);
}

/// Where the first `http://` or `https://` starting a word begins. One pass, whatever the post's
/// author put in it.
fn find_link(text: &str) -> Option<usize> {
    let mut from = 0;
    loop {
        let start = from + text[from..].find("http")?;
        let after = &text[start + 4..];
        let starts_word = start == 0 || text[..start].ends_with(|c: char| c.is_whitespace() || c == '(');
        if starts_word && (after.starts_with("://") || after.starts_with("s://")) {
            return Some(start);
        }
        from = start + 4;
    }
}

pub fn escape_into(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}
