//! Finding links in a message and swapping their domains.

use std::collections::{BTreeMap, HashMap};

use frankenstein::types::{Message, MessageEntity, MessageEntityType};

/// The `[domains]` table: a link whose host is a key, or a subdomain of one, gets the value as its host.
#[derive(Debug, Clone, Default)]
pub struct Domains {
    map: HashMap<String, String>,
}

impl Domains {
    /// Takes keys and values already normalized by the config check.
    pub fn new(map: &BTreeMap<String, String>) -> Self {
        Self { map: map.iter().map(|(k, v)| (k.clone(), v.clone())).collect() }
    }

    /// Returns the link with its host swapped, or `None` if no key matches.
    /// Path, query string and fragment are kept exactly as sent.
    pub fn swap(&self, link: &str) -> Option<String> {
        let link = link.trim();
        let rest = match split_scheme(link) {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") => rest,
            Some(_) => return None,
            None => link,
        };
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);
        let host_port = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
        if host_port.starts_with('[') {
            return None; // IPv6 literal
        }
        let host = host_port.split_once(':').map_or(host_port, |(host, _)| host);
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let target = self.lookup(&host)?;
        Some(format!("https://{target}{tail}"))
    }

    /// The most specific key wins: `a.b.c` checks `a.b.c`, then `b.c`, then `c`.
    fn lookup(&self, host: &str) -> Option<&str> {
        let mut candidate = host;
        loop {
            if let Some(target) = self.map.get(candidate) {
                return Some(target);
            }
            candidate = candidate.split_once('.')?.1;
        }
    }
}

/// Splits `scheme://rest`. Returns `None` for links without a valid scheme, such as `x.com/a?u=http://b`.
fn split_scheme(link: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = link.split_once("://")?;
    let mut bytes = scheme.bytes();
    let valid = bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'));
    valid.then_some((scheme, rest))
}

/// The swapped links of a message (text or caption), deduplicated and capped at `max`.
pub fn swapped_links(message: &Message, domains: &Domains, max: usize) -> Vec<String> {
    let (text, entities) = match (&message.text, &message.entities, &message.caption, &message.caption_entities) {
        (Some(text), Some(entities), _, _) | (_, _, Some(text), Some(entities)) => (text.as_str(), entities),
        _ => return Vec::new(),
    };
    let mut out: Vec<String> = Vec::new();
    for link in links(text, entities) {
        if out.len() == max {
            break;
        }
        if let Some(swapped) = domains.swap(link)
            && !out.contains(&swapped)
        {
            out.push(swapped);
        }
    }
    out
}

/// The links Telegram marked in the text: `url` entities and `text_link` targets.
fn links<'a>(text: &'a str, entities: &'a [MessageEntity]) -> impl Iterator<Item = &'a str> {
    entities.iter().filter_map(move |entity| match entity.type_field {
        MessageEntityType::Url => utf16_slice(text, entity.offset.into(), entity.length.into()),
        MessageEntityType::TextLink => entity.url.as_deref(),
        _ => None,
    })
}

/// Telegram counts entity offsets in UTF-16 code units.
fn utf16_slice(text: &str, offset: usize, length: usize) -> Option<&str> {
    let mut units = 0;
    let mut start = None;
    for (index, ch) in text.char_indices() {
        if units == offset {
            start = Some(index);
        }
        if units == offset + length {
            return Some(&text[start?..index]);
        }
        units += ch.len_utf16();
    }
    if units == offset + length {
        return Some(&text[start?..]);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn domains() -> Domains {
        let map = [
            ("facebook.com", "fb.example.com"),
            ("x.com", "fixupx.com"),
            ("tiktok.com", "tnktok.com"),
            ("youtu.be", "koutube.com"),
        ];
        Domains::new(&map.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect())
    }

    #[test]
    fn swaps_listed_hosts_only() {
        let d = domains();
        for (link, swapped) in [
            ("https://facebook.com/reel/1", Some("https://fb.example.com/reel/1")),
            ("https://m.facebook.com/reel/1", Some("https://fb.example.com/reel/1")),
            ("https://www.web.facebook.com/x", Some("https://fb.example.com/x")),
            ("https://vt.tiktok.com/ZS123/", Some("https://tnktok.com/ZS123/")),
            ("HTTP://WWW.X.COM/a", Some("https://fixupx.com/a")),
            // The path, query and fragment stay exactly as they were.
            ("https://x.com/u/status/1?s=20&t=Ab%2F#frag", Some("https://fixupx.com/u/status/1?s=20&t=Ab%2F#frag")),
            ("https://youtu.be/abc?si=xyz", Some("https://koutube.com/abc?si=xyz")),
            ("https://x.com", Some("https://fixupx.com")),
            ("https://x.com?x=1", Some("https://fixupx.com?x=1")),
            // Bare links, ports, user info, trailing dots.
            ("x.com/a", Some("https://fixupx.com/a")),
            ("x.com/a?u=http://b.com", Some("https://fixupx.com/a?u=http://b.com")),
            ("https://x.com:443/a", Some("https://fixupx.com/a")),
            ("https://user:pw@x.com/a", Some("https://fixupx.com/a")),
            ("https://x.com./a", Some("https://fixupx.com/a")),
            // Other hosts and schemes.
            ("https://notx.com/a", None),
            ("https://x.com.evil.org/a", None),
            ("https://fb.example.com/reel/1", None),
            ("ftp://x.com/a", None),
            ("tg://resolve?domain=x", None),
            ("https://[::1]/a", None),
        ] {
            assert_eq!(d.swap(link).as_deref(), swapped, "{link}");
        }
    }

    #[test]
    fn slices_utf16_offsets() {
        let text = "😀 é https://x.com/a end";
        // 😀 is two UTF-16 units, é one.
        assert_eq!(utf16_slice(text, 5, 15), Some("https://x.com/a"));
        assert_eq!(utf16_slice(text, 5, 19), Some("https://x.com/a end"));
        assert_eq!(utf16_slice(text, 5, 99), None);
        assert_eq!(utf16_slice("", 0, 0), None);
    }

    fn message(text: &str, entities: &[(MessageEntityType, u16, u16, Option<&str>)]) -> Message {
        let entities: Vec<serde_json::Value> = entities
            .iter()
            .map(|(kind, offset, length, url)| {
                serde_json::json!({ "type": kind, "offset": offset, "length": length, "url": url })
            })
            .collect();
        serde_json::from_value(serde_json::json!({
            "message_id": 1, "date": 0, "chat": { "id": 1, "type": "private" },
            "text": text, "entities": entities,
        }))
        .unwrap()
    }

    #[test]
    fn collects_deduplicates_and_caps_text_and_captions() {
        use MessageEntityType::{Bold, TextLink, Url};
        let text = "https://x.com/1 https://x.com/1 https://a.example/ link https://x.com/2";
        let msg = message(
            text,
            &[
                (Url, 0, 15, None),
                (Url, 16, 15, None),
                (Bold, 0, 5, None),
                (Url, 32, 18, None),
                (TextLink, 51, 4, Some("https://m.facebook.com/p")),
                (Url, 56, 15, None),
            ],
        );
        let d = domains();
        assert_eq!(
            swapped_links(&msg, &d, 10),
            ["https://fixupx.com/1", "https://fb.example.com/p", "https://fixupx.com/2"]
        );
        assert_eq!(swapped_links(&msg, &d, 2), ["https://fixupx.com/1", "https://fb.example.com/p"]);

        let caption: Message = serde_json::from_value(serde_json::json!({
            "message_id": 1, "date": 0, "chat": { "id": 1, "type": "private" },
            "caption": "see x.com/a",
            "caption_entities": [{ "type": "url", "offset": 4, "length": 7 }],
        }))
        .unwrap();
        assert_eq!(swapped_links(&caption, &d, 10), ["https://fixupx.com/a"], "captions too");
    }
}
