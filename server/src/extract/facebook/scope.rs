//! Which JSON belongs to the requested post. Facebook pages also carry feed and suggested
//! posts, so story pages only look at nodes that name the post's story token.

use super::index::{Index, Obj, text};
use crate::extract::Target;

/// Fields that name a story, on the node itself or under `node_v2`.
const TOKEN_FIELDS: [&str; 12] = [
    "post_id",
    "story_fbid",
    "story_token",
    "url",
    "permalink_url",
    "www_url",
    "node_v2.post_id",
    "node_v2.story_fbid",
    "node_v2.story_token",
    "node_v2.url",
    "node_v2.permalink_url",
    "node_v2.www_url",
];

/// Whether a node itself identifies the story (not just something inside it).
pub fn names_story(node: &Obj, token: &str) -> bool {
    TOKEN_FIELDS.iter().any(|field| text(node, field).is_some_and(|value| value.contains(token)))
}

pub fn names_any(node: &Obj, tokens: &[String]) -> bool {
    tokens.iter().any(|token| names_story(node, token))
}

/// The story's own nodes: those naming one of its tokens. Found once, as every story lookup
/// needs them.
pub fn story_nodes<'a>(index: &Index<'a>, tokens: &[String]) -> Vec<&'a Obj> {
    if tokens.is_empty() {
        return Vec::new();
    }
    index.nodes.iter().copied().filter(|n| names_any(n, tokens)).collect()
}

/// Story tokens in route data: any node's `params.story_token` or `params.story_fbid`.
pub fn route_tokens(route: &Index<'_>) -> Vec<String> {
    let mut tokens = Vec::new();
    for node in &route.nodes {
        for key in ["params.story_token", "params.story_fbid"] {
            if let Some(token) = text(node, key)
                && !tokens.iter().any(|t| t == token)
            {
                tokens.push(token.to_string());
            }
        }
    }
    tokens
}

/// The tokens that identify the requested story: the URL's first, then route tokens.
/// Route tokens alone are trusted only for `/share/...` links.
pub fn story_tokens(target: &Target, route_tokens: &[String]) -> Vec<String> {
    let mut tokens = Vec::new();
    match target.story_token() {
        Some(token) => tokens.push(token),
        None if target.allows_route_tokens() => {}
        None => return tokens,
    }
    for token in route_tokens {
        if !tokens.contains(token) {
            tokens.push(token.clone());
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn story_tokens_follow_the_url() {
        let route = vec!["r1".to_string(), "p1".to_string()];
        let posts = crate::extract::facebook::url::request("/page/posts/p1").unwrap();
        assert_eq!(story_tokens(&posts, &route), ["p1", "r1"]);
        let share = crate::extract::facebook::url::request("/share/p/xyz").unwrap();
        assert_eq!(story_tokens(&share, &route), ["r1", "p1"]);
        let reel = crate::extract::facebook::url::request("/reel/1").unwrap();
        assert!(story_tokens(&reel, &route).is_empty());
    }

    #[test]
    fn names_story_checks_own_fields_only() {
        let node =
            json!({ "node_v2": { "url": "https://www.facebook.com/x/posts/tok123" }, "child": { "post_id": "other" } });
        let node = node.as_object().unwrap();
        assert!(names_story(node, "tok123"));
        assert!(!names_story(node, "other"));
    }
}
