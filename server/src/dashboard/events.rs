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
use super::pages::Logged;

/// What a `request` event carries.
#[derive(serde::Serialize)]
struct Live {
    html: String,
}

/// How often an open log checks that its sign-in still holds.
const RECHECK: Duration = Duration::from_secs(60);

/// A `request` per counted request: its row in the log, drawn as the page draws its own, with
/// times in the browser's zone. Ends at shutdown, and within a minute of its session ending
/// (signed out, expired or idle), so a stolen session can't keep reading what's previewed.
pub async fn stream(
    State(d): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let events = d.app.stats.subscribe();
    // The stream is read after the request's own scope has ended, so it keeps the zone itself.
    let zone = super::zone();
    let check = tokio::time::interval_at(tokio::time::Instant::now() + RECHECK, RECHECK);
    let state = (d, events, identity, headers, check, zone);
    let stream =
        futures_util::stream::unfold(state, |(d, mut events, identity, headers, mut check, zone)| async move {
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
                    Ok(event) => event,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => return None,
                };
                let row = super::ZONE.sync_scope(zone.clone(), || Logged::of(&event).render());
                match row.map(|html| Event::default().event("request").json_data(Live { html })) {
                    Ok(Ok(event)) => return Some((Ok(event), (d, events, identity, headers, check, zone))),
                    Ok(Err(e)) => tracing::warn!("can't send a request to the live log: {e}"),
                    Err(e) => tracing::warn!("can't draw a request for the live log: {e}"),
                }
            }
        });
    Sse::new(stream).keep_alive(KeepAlive::default())
}
