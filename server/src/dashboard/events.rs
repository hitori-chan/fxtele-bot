//! Live updates for the overview (server-sent events) and the chart's data.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::Stream;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use super::{Shared, pages, server_error};

const SNAPSHOT_EVERY: Duration = Duration::from_secs(5);
/// Points on the chart, whatever the period.
const POINTS: i64 = 288;

/// A `snapshot` of the overview numbers every few seconds, and a `request` per counted request
/// (outcome, page type and time only). Ends at shutdown so it doesn't hold the server open.
pub async fn stream(State(d): State<Shared>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut ticks = tokio::time::interval(SNAPSHOT_EVERY);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let events = d.app.stats.subscribe();
    let stream = futures_util::stream::unfold((d, events, ticks), |(d, mut events, mut ticks)| async move {
        loop {
            let event = tokio::select! {
                () = d.shutdown.cancelled() => return None,
                _ = ticks.tick() => match pages::snapshot(&d.app).await {
                    Ok(data) => Event::default().event("snapshot").json_data(data),
                    Err(e) => {
                        tracing::warn!("can't build a dashboard snapshot: {e:#}");
                        continue;
                    }
                },
                received = events.recv() => match received {
                    Ok(event) => Event::default().event("request").json_data(event),
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => return None,
                },
            };
            if let Ok(event) = event {
                return Some((Ok(event), (d, events, ticks)));
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Debug, Deserialize)]
pub struct SeriesQuery {
    hours: Option<i64>,
}

/// `{timestamps: [...], series: {outcome: [count per point]}}`; every `failed:*` is one series.
pub async fn series(State(d): State<Shared>, Query(q): Query<SeriesQuery>) -> Response {
    let hours = q.hours.unwrap_or(24).clamp(1, 24 * 90);
    let step = ((hours * 3600 / POINTS).max(60) / 60) * 60;
    let now = jiff::Timestamp::now().as_second();
    let since = (now - hours * 3600) / step * step;
    let rows = match d.app.stats.series(since, step).await {
        Ok(rows) => rows,
        Err(e) => return server_error(&e),
    };
    let timestamps: Vec<i64> =
        std::iter::successors(Some(since), |t| Some(t + step)).take_while(|t| *t <= now).collect();
    let mut series: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for (bucket, outcome, count) in rows {
        let Some(index) = usize::try_from((bucket - since) / step).ok().filter(|i| *i < timestamps.len()) else {
            continue;
        };
        let name = if outcome.starts_with("failed:") { "failed".to_string() } else { outcome };
        series.entry(name).or_insert_with(|| vec![0; timestamps.len()])[index] += count;
    }
    let timestamps = if series.is_empty() { Vec::new() } else { timestamps };
    Json(json!({ "timestamps": timestamps, "series": series })).into_response()
}
