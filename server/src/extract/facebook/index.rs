//! One pass over the page's JSON: every object in document order, plus nodes by `id`.

use std::borrow::Cow;
use std::collections::HashMap;

use serde_json::{Map, Value};

pub type Obj = Map<String, Value>;

pub struct Index<'a> {
    /// Every JSON object, depth-first in document order (a node before its children).
    pub nodes: Vec<&'a Obj>,
    by_id: HashMap<Cow<'a, str>, Vec<usize>>,
}

impl<'a> Index<'a> {
    pub fn new(documents: impl IntoIterator<Item = &'a Value>) -> Self {
        let mut nodes = Vec::new();
        let mut by_id: HashMap<Cow<'a, str>, Vec<usize>> = HashMap::new();
        for document in documents {
            for node in walk(document) {
                if let Some(id) = id_of(node) {
                    by_id.entry(id).or_default().push(nodes.len());
                }
                nodes.push(node);
            }
        }
        Self { nodes, by_id }
    }

    /// Nodes whose `id` (string or number) equals `id`, in document order.
    pub fn with_id<'s>(&'s self, id: &str) -> impl Iterator<Item = &'a Obj> + 's {
        self.by_id.get(id).into_iter().flatten().map(|&i| self.nodes[i])
    }
}

/// Every object in `value`'s subtree, depth-first in document order, `value` first.
fn walk(value: &Value) -> impl Iterator<Item = &Obj> {
    let mut stack = vec![value];
    std::iter::from_fn(move || {
        loop {
            match stack.pop()? {
                Value::Object(map) => {
                    stack.extend(map.values().rev().filter(|v| v.is_object() || v.is_array()));
                    return Some(map);
                }
                Value::Array(items) => stack.extend(items.iter().rev().filter(|v| v.is_object() || v.is_array())),
                _ => {}
            }
        }
    })
}

/// Objects in the subtree of an object.
pub fn walk_obj(obj: &Obj) -> impl Iterator<Item = &Obj> {
    std::iter::once(obj).chain(obj.values().flat_map(walk))
}

/// The node's `id` as text.
pub fn id_of(node: &Obj) -> Option<Cow<'_, str>> {
    node.get("id").and_then(id_text)
}

/// An ID as text, whether Facebook sent a string or a number.
pub fn id_text(value: &Value) -> Option<Cow<'_, str>> {
    match value {
        Value::String(s) => Some(Cow::Borrowed(s)),
        Value::Number(n) => Some(Cow::Owned(n.to_string())),
        _ => None,
    }
}

/// Follows a dotted path of object keys.
pub fn get<'a>(node: &'a Obj, path: &str) -> Option<&'a Value> {
    let mut parts = path.split('.');
    let mut value = node.get(parts.next()?)?;
    for part in parts {
        value = value.as_object()?.get(part)?;
    }
    Some(value)
}

pub fn get_obj<'a>(node: &'a Obj, path: &str) -> Option<&'a Obj> {
    get(node, path)?.as_object()
}

/// A non-empty string at the path, trimmed.
pub fn text<'a>(node: &'a Obj, path: &str) -> Option<&'a str> {
    get(node, path)?.as_str().map(str::trim).filter(|s| !s.is_empty())
}

/// The first path that holds a non-empty string, like JMESPath's `a || b`.
pub fn first_text<'a>(node: &'a Obj, paths: &[&str]) -> Option<&'a str> {
    paths.iter().find_map(|path| text(node, path))
}

pub fn number(node: &Obj, path: &str) -> Option<i64> {
    get(node, path)?.as_i64()
}

/// The objects at `path.[]`: the array at `path`, then `rest` inside each element,
/// like JMESPath's `path[].rest`. An empty `rest` yields the elements themselves.
pub fn each<'a>(node: &'a Obj, path: &str, rest: &'a str) -> impl Iterator<Item = &'a Obj> + 'a {
    get(node, path)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .filter_map(move |item| if rest.is_empty() { Some(item) } else { get_obj(item, rest) })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn walks_in_document_order() {
        let doc =
            json!({ "id": "a", "z": { "id": 2, "list": [{ "id": "c" }, 5, [{ "id": "d" }]] }, "b": { "id": "e" } });
        let index = Index::new([&doc]);
        let ids: Vec<String> = index.nodes.iter().filter_map(|n| id_of(n).map(Cow::into_owned)).collect();
        assert_eq!(ids, ["a", "2", "c", "d", "e"], "keys keep their order, children after parents");
        assert_eq!(index.with_id("2").count(), 1);
        assert_eq!(index.with_id("x").count(), 0);
    }

    #[test]
    fn paths_and_projections() {
        let doc = json!({ "a": { "b": { "c": " hi " }, "empty": "", "list": [{ "m": { "x": 1 } }, { "n": 1 }, 3] } });
        let node = doc.as_object().unwrap();
        assert_eq!(text(node, "a.b.c"), Some("hi"));
        assert_eq!(text(node, "a.empty"), None);
        assert_eq!(first_text(node, &["a.empty", "a.b.c"]), Some("hi"));
        assert_eq!(each(node, "a.list", "m").count(), 1);
        assert_eq!(each(node, "a.list", "").count(), 2);
        assert_eq!(each(node, "a.missing", "m").count(), 0);
    }
}
