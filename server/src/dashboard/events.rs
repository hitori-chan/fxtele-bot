//! Live updates for the overview: server-sent events.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use tokio::sync::broadcast::error::RecvError;

use super::{Shared, pages};

const SNAPSHOT_EVERY: Duration = Duration::from_secs(5);

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
