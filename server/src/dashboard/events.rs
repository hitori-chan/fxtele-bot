//! Live updates for the request logs on the overview and the Requests page: server-sent events.

use std::convert::Infallible;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use tokio::sync::broadcast::error::RecvError;

use super::Shared;

/// A `request` per counted request: when, its site, page type, outcome and time taken, and the
/// post's link. Ends at shutdown so it doesn't hold the server open.
pub async fn stream(State(d): State<Shared>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let events = d.app.stats.subscribe();
    let stream = futures_util::stream::unfold((d, events), |(d, mut events)| async move {
        loop {
            let received = tokio::select! {
                () = d.shutdown.cancelled() => return None,
                received = events.recv() => received,
            };
            let event = match received {
                Ok(event) => Event::default().event("request").json_data(event),
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return None,
            };
            if let Ok(event) = event {
                return Some((Ok(event), (d, events)));
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}
