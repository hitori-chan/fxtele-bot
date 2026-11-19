//! What differs between the sites' pages, all in one place. Everything else about a preview is the
//! same for every site.

use crate::extract::Source;

pub struct Style {
    /// The colour down the side of Discord's embed.
    pub colour: &'static str,
    /// What the likes are counted with.
    pub liked: &'static str,
    /// Posts have a headline of their own (Reddit's), so the title is that, and who posted where
    /// goes under it.
    pub headlines: bool,
    pub text: TextStyle,
    /// Why a post can't be read, when the site says there's no such post.
    pub not_public: &'static str,
}

/// How a post's text is written, and what in it links where.
pub enum TextStyle {
    /// Markdown, with `r/name` and `u/name` linked.
    Markdown,
    /// Plain text, with `#tag` linked, and `@name` where the site has them. Each makes the page's
    /// address from the name, already escaped.
    Plain { hashtag: fn(&str) -> String, mention: Option<fn(&str) -> String> },
}

static FACEBOOK: Style = Style {
    colour: "#0866ff",
    liked: "👍",
    headlines: false,
    text: TextStyle::Plain { hashtag: |tag| format!("https://www.facebook.com/hashtag/{tag}"), mention: None },
    not_public: "Facebook doesn't show this post publicly: it may be private, deleted or for friends only.",
};

static INSTAGRAM: Style = Style {
    colour: "#e1306c",
    liked: "❤️",
    headlines: false,
    text: TextStyle::Plain {
        hashtag: |tag| format!("https://www.instagram.com/explore/tags/{tag}/"),
        mention: Some(|name| format!("https://www.instagram.com/{name}/")),
    },
    not_public: "Instagram doesn't show this publicly: it may be private, age-restricted or deleted.",
};

static REDDIT: Style = Style {
    colour: "#ff4500",
    liked: "⬆️",
    headlines: true,
    text: TextStyle::Markdown,
    not_public: "Reddit doesn't show this publicly: it may be removed, deleted or in a private community.",
};

static THREADS: Style = Style {
    colour: "#000000",
    liked: "❤️",
    headlines: false,
    text: TextStyle::Plain {
        hashtag: |tag| format!("https://www.threads.com/search?q={tag}&serp_type=tags"),
        mention: Some(|name| format!("https://www.threads.com/@{name}")),
    },
    not_public: "Threads doesn't show this publicly: it may be private or deleted.",
};

impl Style {
    pub fn of(source: Source) -> &'static Self {
        match source {
            Source::Facebook => &FACEBOOK,
            Source::Instagram => &INSTAGRAM,
            Source::Reddit => &REDDIT,
            Source::Threads => &THREADS,
        }
    }
}
