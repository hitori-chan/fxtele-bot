//! Long lists a page at a time: which rows a page holds, and the links to the others, which keep
//! the list's filters.

use std::ops::Range;

/// A page of a long list.
pub struct Pager {
    /// From 1.
    pub page: usize,
    pub pages: usize,
    pub total: usize,
    size: usize,
    /// The list's address with its filters, without `page`: `/cache?q=x`.
    base: String,
}

/// A link in the pager: a page's number, or a gap between them.
pub struct PageLink {
    pub number: Option<usize>,
    pub href: String,
    pub current: bool,
}

impl Pager {
    /// `size` rows a page, at the page asked for (from 1), kept within the list; `filters` are the
    /// list's query pairs, empty ones left out.
    pub fn new(total: usize, size: usize, page: Option<usize>, path: &str, filters: &[(&str, &str)]) -> Self {
        let pages = total.div_ceil(size.max(1)).max(1);
        Self { page: page.unwrap_or(1).clamp(1, pages), pages, total, size: size.max(1), base: address(path, filters) }
    }

    /// The rows this page shows.
    pub fn rows(&self) -> Range<usize> {
        let start = ((self.page - 1) * self.size).min(self.total);
        start..(start + self.size).min(self.total)
    }

    /// "51–100 of 1,234".
    pub fn showing(&self) -> String {
        let rows = self.rows();
        format!("{}–{} of {}", thousands(rows.start + 1), thousands(rows.end), thousands(self.total))
    }

    pub fn href(&self, page: usize) -> String {
        match (page, self.base.contains('?')) {
            (1, _) => self.base.clone(),
            (_, true) => format!("{}&page={page}", self.base),
            (_, false) => format!("{}?page={page}", self.base),
        }
    }

    pub fn previous(&self) -> Option<String> {
        (self.page > 1).then(|| self.href(self.page - 1))
    }

    pub fn next(&self) -> Option<String> {
        (self.page < self.pages).then(|| self.href(self.page + 1))
    }

    /// The first and last pages, and two either side of this one, with gaps between: 1 … 4 5 6 7
    /// 8 … 20.
    pub fn links(&self) -> Vec<PageLink> {
        let mut links = Vec::new();
        let mut last = 0;
        for n in 1..=self.pages {
            if n != 1 && n != self.pages && n.abs_diff(self.page) > 2 {
                continue;
            }
            if n > last + 1 {
                links.push(PageLink { number: None, href: String::new(), current: false });
            }
            links.push(PageLink { number: Some(n), href: self.href(n), current: n == self.page });
            last = n;
        }
        links
    }
}

/// `path` with the non-empty `filters` as its query.
pub fn address(path: &str, filters: &[(&str, &str)]) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in filters.iter().filter(|(_, value)| !value.is_empty()) {
        query.append_pair(name, value);
    }
    let query = query.finish();
    if query.is_empty() { path.to_string() } else { format!("{path}?{query}") }
}

/// "1,234".
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A page number from a query, where a missing or bad one is the first page.
/// A page number from a query: 1 and up, and no further than any list could go (a page past the
/// end shows the last), so working out where it starts can't overflow.
pub fn page_number(value: &str) -> Option<usize> {
    value.trim().parse().ok().filter(|&n| n > 0).map(|n: usize| n.min(1_000_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_keep_their_filters_and_stay_in_range() {
        let pager = Pager::new(1234, 50, Some(7), "/cache", &[("q", "a b&c"), ("site", "")]);
        assert_eq!((pager.pages, pager.rows()), (25, 300..350));
        assert_eq!(pager.showing(), "301–350 of 1,234");
        assert_eq!(pager.href(1), "/cache?q=a+b%26c", "the first page has no number, and empty filters go");
        assert_eq!(pager.next().as_deref(), Some("/cache?q=a+b%26c&page=8"));
        let numbers: Vec<Option<usize>> = pager.links().iter().map(|l| l.number).collect();
        assert_eq!(numbers, [Some(1), None, Some(5), Some(6), Some(7), Some(8), Some(9), None, Some(25)]);
        assert!(pager.links().iter().any(|l| l.current && l.number == Some(7)));

        let past = Pager::new(120, 50, Some(9), "/activity", &[]);
        assert_eq!((past.page, past.rows(), past.href(2)), (3, 100..120, "/activity?page=2".to_string()));
        let empty = Pager::new(0, 50, None, "/activity", &[]);
        assert_eq!((empty.pages, empty.rows(), empty.next()), (1, 0..0, None));
        assert_eq!((page_number("0"), page_number("x"), page_number(" 3")), (None, None, Some(3)));
    }
}
