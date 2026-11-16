//! The dashboard's charts, drawn here as SVG: nothing to download, no chart library, and they
//! work without scripts (scripts add tooltips). Bars stack requests by outcome, lines show how
//! long fetches took, sparklines show trends. Times are UTC, like the rest of the dashboard.

use std::collections::BTreeMap;

use askama::Template;

use crate::stats::Agg;

/// A period the overview shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Range {
    Hour,
    #[default]
    Day,
    Week,
    Month,
    Quarter,
}

impl Range {
    pub const ALL: [Self; 5] = [Self::Hour, Self::Day, Self::Week, Self::Month, Self::Quarter];

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == text)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hour => "1h",
            Self::Day => "24h",
            Self::Week => "7d",
            Self::Month => "30d",
            Self::Quarter => "90d",
        }
    }

    /// "the last …".
    pub const fn label(self) -> &'static str {
        match self {
            Self::Hour => "hour",
            Self::Day => "24 hours",
            Self::Week => "7 days",
            Self::Month => "30 days",
            Self::Quarter => "90 days",
        }
    }

    pub const fn seconds(self) -> i64 {
        match self {
            Self::Hour => 3600,
            Self::Day => 86_400,
            Self::Week => 7 * 86_400,
            Self::Month => 30 * 86_400,
            Self::Quarter => 90 * 86_400,
        }
    }

    /// Seconds per bar: whole minutes, and whole hours past a week, which is all older stats keep.
    pub const fn step(self) -> i64 {
        match self {
            Self::Hour => 60,
            Self::Day => 900,
            Self::Week => 2 * 3600,
            Self::Month => 8 * 3600,
            Self::Quarter => 86_400,
        }
    }

    fn bars(self) -> usize {
        usize::try_from(self.seconds() / self.step()).unwrap_or(1)
    }

    /// The first bar's start; the last bar is the one now is in.
    pub fn since(self, now: i64) -> i64 {
        let bars = i64::try_from(self.bars()).unwrap_or(1);
        now / self.step() * self.step() - (bars - 1) * self.step()
    }

    fn time(self, at: i64) -> String {
        let format = match self {
            Self::Hour | Self::Day => "%H:%M",
            Self::Week | Self::Month => "%-d %b %H:%M",
            Self::Quarter => "%-d %b",
        };
        jiff::Timestamp::from_second(at).map_or_else(|_| "?".into(), |t| t.strftime(format).to_string())
    }
}

/// How the bars stack an outcome, bottom first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Series {
    Hit,
    Extracted,
    Failed,
    Limited,
    Redirect,
}

impl Series {
    const ALL: [Self; 5] = [Self::Hit, Self::Extracted, Self::Failed, Self::Limited, Self::Redirect];

    fn of(outcome: &str) -> Self {
        match outcome {
            "hit" => Self::Hit,
            "extracted" => Self::Extracted,
            "redirect" => Self::Redirect,
            o if o.starts_with("failed") => Self::Failed,
            _ => Self::Limited,
        }
    }

    const fn class(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Extracted => "extracted",
            Self::Failed => "failed",
            Self::Limited => "limited",
            Self::Redirect => "redirect",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Hit => "From the cache",
            Self::Extracted => "Extracted",
            Self::Failed => "Failed",
            Self::Limited => "Busy or limited",
            Self::Redirect => "People, redirected",
        }
    }
}

/// The viewBox's size; the SVG stretches to its box.
const SLOT: f64 = 10.0;
const HEIGHT: f64 = 100.0;
/// How much of its slot a bar fills.
const BAR: f64 = 0.72;

pub struct Part {
    class: &'static str,
    y: f64,
    height: f64,
}

/// One period: where it is, and what the tooltip says about it (JSON: the time, then rows of
/// series class, label and value).
pub struct Slot {
    x: f64,
    tip: String,
    title: String,
}

pub struct Bar {
    slot: Slot,
    /// The bar's left edge inside its slot.
    bx: f64,
    parts: Vec<Part>,
}

pub struct Legend {
    class: &'static str,
    label: &'static str,
    value: String,
}

/// The tooltip's JSON and the plain title (for screen readers and no scripts).
fn tip(time: &str, rows: &[(&str, &str, String)]) -> (String, String) {
    let json = serde_json::json!({ "t": time, "rows": rows });
    let what: Vec<String> = rows.iter().map(|(_, label, value)| format!("{label}: {value}")).collect();
    let title = if what.is_empty() { format!("{time}: nothing") } else { format!("{time}: {}", what.join(", ")) };
    (json.to_string(), title)
}

#[derive(Template)]
#[template(path = "dashboard/chart.html")]
pub struct Chart {
    range: Range,
    total: u64,
    width: f64,
    bar_width: f64,
    bars: Vec<Bar>,
    y_labels: [String; 3],
    x_labels: Vec<String>,
    legend: Vec<Legend>,
}

fn x_labels(range: Range, since: i64) -> Vec<String> {
    let (step, count) = (range.step(), range.bars());
    [0, count / 4, count / 2, count * 3 / 4]
        .into_iter()
        .map(|i| range.time(since + i64::try_from(i).unwrap_or(0) * step))
        .chain(std::iter::once("now".to_string()))
        .collect()
}

/// Rows into per-slot values, dropping those outside the range.
fn slotted<T: Clone>(range: Range, now: i64, rows: impl IntoIterator<Item = (i64, T)>) -> Vec<Vec<T>> {
    let (since, step) = (range.since(now), range.step());
    let mut slots = vec![Vec::new(); range.bars()];
    for (bucket, value) in rows {
        if let Some(slot) = usize::try_from((bucket - since) / step).ok().and_then(|i| slots.get_mut(i)) {
            slot.push(value);
        }
    }
    slots
}

/// Stacked bars from `(bar start, outcome, requests)` rows, as `Stats::series` gives them.
pub fn requests(range: Range, rows: &[(i64, String, Agg)], now: i64) -> Chart {
    let since = range.since(now);
    let slots: Vec<BTreeMap<Series, u64>> =
        slotted(range, now, rows.iter().map(|(bucket, outcome, agg)| (*bucket, (Series::of(outcome), agg.count))))
            .into_iter()
            .map(|parts| {
                let mut slot = BTreeMap::new();
                for (series, n) in parts {
                    *slot.entry(series).or_default() += n;
                }
                slot
            })
            .collect();
    let mut totals = BTreeMap::<Series, u64>::new();
    for (series, n) in slots.iter().flatten() {
        *totals.entry(*series).or_default() += n;
    }
    let y_top = nice(slots.iter().map(|s| s.values().sum::<u64>()).max().unwrap_or(0));
    let scale = HEIGHT / y_top as f64;
    let bars = slots
        .iter()
        .enumerate()
        .map(|(i, slot)| {
            let mut top = HEIGHT;
            let parts = slot
                .iter()
                .filter(|(_, n)| **n > 0)
                .map(|(series, n)| {
                    // At least a sliver, so a single request still shows.
                    let height = (*n as f64 * scale).max(1.2);
                    top -= height;
                    Part { class: series.class(), y: round(top), height: round(height) }
                })
                .collect();
            let rows: Vec<_> = slot.iter().rev().map(|(s, n)| (s.class(), s.label(), n.to_string())).collect();
            let (tip, title) = tip(&range.time(since + i64::try_from(i).unwrap_or(0) * range.step()), &rows);
            let x = i as f64 * SLOT;
            Bar { slot: Slot { x, tip, title }, bx: round(x + SLOT * (1.0 - BAR) / 2.0), parts }
        })
        .collect();
    Chart {
        range,
        total: totals.values().sum(),
        width: range.bars() as f64 * SLOT,
        bar_width: round(SLOT * BAR),
        bars,
        y_labels: [y_top.to_string(), (y_top / 2).to_string(), "0".into()],
        x_labels: x_labels(range, since),
        legend: Series::ALL
            .into_iter()
            .map(|s| Legend {
                class: s.class(),
                label: s.label(),
                value: totals.get(&s).copied().unwrap_or(0).to_string(),
            })
            .collect(),
    }
}

/// How long fetches took: the median and the 95th percentile per period, as lines.
#[derive(Template)]
#[template(path = "dashboard/latency.html")]
pub struct Latency {
    range: Range,
    fetches: u64,
    width: f64,
    /// Each line's class and its runs of points (a period without fetches breaks the line).
    lines: Vec<(&'static str, Vec<String>)>,
    slots: Vec<Slot>,
    y_labels: [String; 3],
    x_labels: Vec<String>,
    legend: Vec<Legend>,
}

/// Fetches are extractions, successful or not: the cache and refusals take no time worth showing.
pub fn is_fetch(outcome: &str) -> bool {
    outcome == "extracted" || outcome.starts_with("failed:")
}

pub fn latency(range: Range, rows: &[(i64, String, Agg)], now: i64, format: fn(u64) -> String) -> Latency {
    let since = range.since(now);
    let slots: Vec<Agg> = slotted(
        range,
        now,
        rows.iter().filter(|(_, outcome, _)| is_fetch(outcome)).map(|(bucket, _, agg)| (*bucket, *agg)),
    )
    .into_iter()
    .map(|aggs| {
        aggs.into_iter().fold(Agg::default(), |mut all, agg| {
            all.merge(agg);
            all
        })
    })
    .collect();
    let mut all = Agg::default();
    for agg in &slots {
        all.merge(*agg);
    }
    let percentiles = [("p50", "Median", 50), ("p95", "95th percentile", 95)];
    let peak = slots.iter().filter_map(|a| a.percentile(95)).max().unwrap_or(0);
    let y_top = nice_millis(peak);
    let y = |ms: u64| round(HEIGHT - ms.min(y_top) as f64 * HEIGHT / y_top as f64);
    let lines = percentiles
        .iter()
        .map(|&(class, _, p)| {
            let mut runs = vec![String::new()];
            for (i, agg) in slots.iter().enumerate() {
                match agg.percentile(p) {
                    Some(ms) => {
                        let run = runs.last_mut().expect("never empty");
                        run.push_str(&format!("{},{} ", round(i as f64 * SLOT + SLOT / 2.0), y(ms)));
                    }
                    None if runs.last().is_some_and(|r| !r.is_empty()) => runs.push(String::new()),
                    None => {}
                }
            }
            runs.retain(|r| !r.is_empty());
            (class, runs)
        })
        .collect();
    let slot_tips = slots
        .iter()
        .enumerate()
        .map(|(i, agg)| {
            let mut rows: Vec<(&str, &str, String)> = percentiles
                .iter()
                .filter_map(|&(class, label, p)| agg.percentile(p).map(|ms| (class, label, format(ms))))
                .collect();
            if agg.count > 0 {
                rows.push(("", "Fetches", agg.count.to_string()));
            }
            let (tip, title) = tip(&range.time(since + i64::try_from(i).unwrap_or(0) * range.step()), &rows);
            Slot { x: i as f64 * SLOT, tip, title }
        })
        .collect();
    Latency {
        range,
        fetches: all.count,
        width: range.bars() as f64 * SLOT,
        lines,
        slots: slot_tips,
        y_labels: [format(y_top), format(y_top / 2), "0".into()],
        x_labels: x_labels(range, since),
        legend: percentiles
            .iter()
            .map(|&(class, label, p)| Legend {
                class,
                label,
                value: all.percentile(p).map_or_else(|| "–".into(), format),
            })
            .collect(),
    }
}

/// The axis top for times: a round number of milliseconds or seconds.
fn nice_millis(peak: u64) -> u64 {
    [200, 500, 1000, 2000, 4000, 6000, 8000, 10_000, 15_000, 20_000, 30_000, 60_000]
        .into_iter()
        .find(|&top| top >= peak)
        .unwrap_or_else(|| nice(peak))
}

/// A sparkline's points in a 100×24 box, from per-bar counts.
pub struct Spark {
    pub line: String,
    pub area: String,
}

pub fn spark(counts: &[u64]) -> Spark {
    let peak = counts.iter().copied().max().unwrap_or(0).max(1) as f64;
    let last = counts.len().saturating_sub(1).max(1) as f64;
    let points: Vec<String> = counts
        .iter()
        .enumerate()
        .map(|(i, n)| format!("{},{}", round(i as f64 * 100.0 / last), round(22.0 - *n as f64 * 20.0 / peak)))
        .collect();
    let line = points.join(" ");
    Spark { area: format!("0,24 {line} 100,24"), line }
}

/// The axis top: 2, 4, 6, 8 or 10 times a power of ten, so the middle line is a whole number.
fn nice(peak: u64) -> u64 {
    let mut base = 1;
    loop {
        for m in [2, 4, 6, 8, 10] {
            if m * base >= peak {
                return m * base;
            }
        }
        base *= 10;
    }
}

fn round(n: f64) -> f64 {
    (n * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_end_with_the_current_bar() {
        let now = 1_700_000_123;
        for range in Range::ALL {
            let since = range.since(now);
            assert_eq!(since % range.step(), 0);
            let last = since + (i64::try_from(range.bars()).unwrap() - 1) * range.step();
            assert!(last <= now && now < last + range.step(), "{range:?}");
            assert_eq!(Range::parse(range.as_str()), Some(range));
        }
        assert_eq!(Range::parse("2h"), None);
    }

    fn row(bucket: i64, outcome: &str, ms: &[u64]) -> (i64, String, Agg) {
        let mut agg = Agg::default();
        for &ms in ms {
            agg.merge(Agg::one(ms));
        }
        (bucket, outcome.to_string(), agg)
    }

    #[test]
    fn bars_stack_by_outcome() {
        let now = 1_700_000_000;
        let range = Range::Hour;
        let last = range.since(now) + 59 * 60;
        let rows = vec![
            row(last, "hit", &[1, 1, 1]),
            row(last, "failed:timeout", &[9000]),
            row(last, "redirect", &[1, 1]),
            row(last - 60, "busy", &[0]),
            row(0, "hit", &[1; 9]),
        ];
        let chart = requests(range, &rows, now);
        assert_eq!(chart.total, 7, "rows before the range are left out");
        assert_eq!(chart.y_labels, ["6", "3", "0"]);
        let bar = chart.bars.last().unwrap();
        assert_eq!(bar.parts.iter().map(|p| p.class).collect::<Vec<_>>(), ["hit", "failed", "redirect"]);
        assert!(bar.slot.title.ends_with("People, redirected: 2, Failed: 1, From the cache: 3"), "{}", bar.slot.title);
        assert!(bar.slot.tip.contains(r#"["failed","Failed","1"]"#), "{}", bar.slot.tip);
        assert!((bar.parts.iter().map(|p| p.height).sum::<f64>() - 100.0).abs() < 0.1, "the peak fills the height");
        assert_eq!(chart.x_labels.last().map(String::as_str), Some("now"));
        let html = chart.render().unwrap();
        assert!(html.contains("class=\"s-failed\"") && html.contains("<title>"));
    }

    #[test]
    fn latency_lines_break_where_nothing_was_fetched() {
        let now = 1_700_000_000;
        let range = Range::Hour;
        let first = range.since(now);
        let rows = vec![
            row(first, "extracted", &[400, 600]),
            row(first + 60, "failed:timeout", &[3000]),
            row(first + 60, "hit", &[1, 1, 1, 1]),
            row(first + 180, "extracted", &[800]),
        ];
        let chart = latency(range, &rows, now, |ms| format!("{ms} ms"));
        assert_eq!(chart.fetches, 4, "cache hits aren't fetches");
        assert_eq!(chart.y_labels[0], "4000 ms");
        let (class, runs) = &chart.lines[0];
        assert_eq!((*class, runs.len()), ("p50", 2), "{runs:?}");
        assert!(chart.slots[1].tip.contains("Fetches") && chart.slots[2].title.ends_with("nothing"));
        assert!(chart.render().unwrap().contains("polyline"));
    }

    #[test]
    fn axis_tops_and_sparklines() {
        assert_eq!([0, 1, 2, 3, 7, 11, 45, 101].map(nice), [2, 2, 2, 4, 8, 20, 60, 200]);
        assert_eq!([0, 300, 1200, 70_000].map(nice_millis), [200, 500, 2000, 80_000]);
        let s = spark(&[0, 5, 10]);
        assert_eq!(s.line, "0,22 50,12 100,2");
        assert_eq!(s.area, "0,24 0,22 50,12 100,2 100,24");
    }
}
