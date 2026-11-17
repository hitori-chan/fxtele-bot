//! Live updates for the Requests page's log: server-sent events.

use std::convert::Infallible;
use std::time::Duration;

use axum::Extension;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use tokio::sync::broadcast::error::RecvError;

use super::Shared;
use super::auth::Identity;

/// How often an open log checks that its sign-in still holds.
const RECHECK: Duration = Duration::from_secs(60);

/// A `request` per counted request: when, its site, page type, outcome and time taken, and the
/// post's link. Ends at shutdown, and within a minute of its session ending (signed out, expired
/// or idle), so a stolen session can't keep reading what's previewed.
pub async fn stream(
    State(d): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let events = d.app.stats.subscribe();
    let check = tokio::time::interval_at(tokio::time::Instant::now() + RECHECK, RECHECK);
    let state = (d, events, identity, headers, check);
    let stream = futures_util::stream::unfold(state, |(d, mut events, identity, headers, mut check)| async move {
        loop {
            let received = tokio::select! {
                () = d.shutdown.cancelled() => return None,
                _ = check.tick() => {
                    if d.auth.still_valid(&d.app.db, &identity, &headers).await {
                        continue;
                    }
                    return None;
                }
                received = events.recv() => received,
            };
            let event = match received {
                Ok(event) => Event::default().event("request").json_data(event),
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return None,
            };
            if let Ok(event) = event {
                return Some((Ok(event), (d, events, identity, headers, check)));
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}
