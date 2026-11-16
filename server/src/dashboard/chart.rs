//! The overview's charts, drawn here as SVG: nothing to download, no chart library, and they
//! work without scripts. Bars stack requests by outcome; sparklines show Telegram's requests
//! per page type. Times are UTC, like the rest of the dashboard.

use std::collections::BTreeMap;

use askama::Template;

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

pub struct Bar {
    /// The slot's left edge, and the bar's inside it.
    x: f64,
    bx: f64,
    title: String,
    parts: Vec<Part>,
}

pub struct Legend {
    class: &'static str,
    label: &'static str,
    count: u64,
}

#[derive(Template)]
#[template(path = "dashboard/chart.html")]
pub struct Chart {
    range: Range,
    total: u64,
    width: f64,
    bar_width: f64,
    bars: Vec<Bar>,
    y_top: u64,
    y_mid: u64,
    x_labels: Vec<String>,
    legend: Vec<Legend>,
}

/// Stacked bars from `(bar start, outcome, count)` rows, as `Stats::series` gives them.
pub fn requests(range: Range, rows: &[(i64, String, u64)], now: i64) -> Chart {
    let (since, step, count) = (range.since(now), range.step(), range.bars());
    let mut slots = vec![BTreeMap::<Series, u64>::new(); count];
    let mut totals = BTreeMap::<Series, u64>::new();
    for (bucket, outcome, n) in rows {
        let Some(slot) = usize::try_from((bucket - since) / step).ok().and_then(|i| slots.get_mut(i)) else { continue };
        let series = Series::of(outcome);
        *slot.entry(series).or_default() += n;
        *totals.entry(series).or_default() += n;
    }
    let peak = slots.iter().map(|s| s.values().sum::<u64>()).max().unwrap_or(0);
    let y_top = nice(peak);
    let scale = HEIGHT / y_top as f64;
    let bars = slots
        .iter()
        .enumerate()
        .map(|(i, slot)| {
            let start = since + i64::try_from(i).unwrap_or(0) * step;
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
            let detail: Vec<String> = slot
                .iter()
                .filter(|(_, n)| **n > 0)
                .map(|(s, n)| format!("{n} {}", s.label().to_lowercase()))
                .collect();
            let what = if detail.is_empty() { "no requests".to_string() } else { detail.join(", ") };
            let x = i as f64 * SLOT;
            Bar {
                x,
                bx: round(x + SLOT * (1.0 - BAR) / 2.0),
                title: format!("{} UTC: {what}", range.time(start)),
                parts,
            }
        })
        .collect();
    let x_labels = [0, count / 4, count / 2, count * 3 / 4]
        .into_iter()
        .map(|i| range.time(since + i64::try_from(i).unwrap_or(0) * step))
        .chain(std::iter::once("now".to_string()))
        .collect();
    Chart {
        range,
        total: totals.values().sum(),
        width: count as f64 * SLOT,
        bar_width: round(SLOT * BAR),
        bars,
        y_top,
        y_mid: y_top / 2,
        x_labels,
        legend: Series::ALL
            .into_iter()
            .map(|s| Legend { class: s.class(), label: s.label(), count: totals.get(&s).copied().unwrap_or(0) })
            .collect(),
    }
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

/// Per-bar counts for each key, from `(bar start, key, count)` rows.
pub fn per_key(range: Range, rows: &[(i64, String, u64)], now: i64) -> BTreeMap<String, Vec<u64>> {
    let (since, step, count) = (range.since(now), range.step(), range.bars());
    let mut out: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for (bucket, key, n) in rows {
        let Some(i) = usize::try_from((bucket - since) / step).ok().filter(|i| *i < count) else { continue };
        out.entry(key.clone()).or_insert_with(|| vec![0; count])[i] += n;
    }
    out
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

    #[test]
    fn bars_stack_by_outcome() {
        let now = 1_700_000_000;
        let range = Range::Hour;
        let last = range.since(now) + 59 * 60;
        let rows = vec![
            (last, "hit".to_string(), 3),
            (last, "failed:timeout".to_string(), 1),
            (last, "redirect".to_string(), 2),
            (last - 60, "busy".to_string(), 1),
            (0, "hit".to_string(), 99),
        ];
        let chart = requests(range, &rows, now);
        assert_eq!(chart.total, 7, "rows before the range are left out");
        assert_eq!((chart.y_top, chart.y_mid), (6, 3));
        let bar = chart.bars.last().unwrap();
        assert_eq!(bar.parts.iter().map(|p| p.class).collect::<Vec<_>>(), ["hit", "failed", "redirect"]);
        assert!(bar.title.ends_with("3 from the cache, 1 failed, 2 people, redirected"), "{}", bar.title);
        assert!((bar.parts.iter().map(|p| p.height).sum::<f64>() - 100.0).abs() < 0.1, "the peak fills the height");
        assert_eq!(chart.x_labels.last().map(String::as_str), Some("now"));
        let html = chart.render().unwrap();
        assert!(html.contains("class=\"s-failed\"") && html.contains("<title>"));
    }

    #[test]
    fn axis_tops_and_sparklines() {
        assert_eq!([0, 1, 2, 3, 7, 11, 45, 101].map(nice), [2, 2, 2, 4, 8, 20, 60, 200]);
        let s = spark(&[0, 5, 10]);
        assert_eq!(s.line, "0,22 50,12 100,2");
        assert_eq!(s.area, "0,24 0,22 50,12 100,2 100,24");
    }
}
