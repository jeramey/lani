//! Live updates (docs/design.md §13.5). A review page keeps an event stream open; the server
//! tells it when the review has changed, when a newer lani has been installed, and when the
//! server stops. The page then fetches itself again and swaps in the parts that changed.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::{self, Stream};
use serde::Deserialize;
use tokio::time::{Interval, MissedTickBehavior};

use super::AppState;
use super::views::{self, PageError};
use crate::error::Error;
use crate::text;

/// How often an open stream checks its review for changes.
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Failed polls in a row after which a stream gives up on an error the next poll might not
/// repeat, such as a database that stayed busy past its timeout.
const FAILURES_BEFORE_GIVING_UP: u32 = 3;

#[derive(Deserialize)]
pub struct EventsQuery {
    /// The latest event the page reflects, which it was rendered with.
    after: Option<i64>,
}

/// `GET /reviews/{id}/events`: the review's live-update stream. It sends:
/// - `changed` when the review has changed (with the latest event id as the message id), or when
///   a newer lani has been installed (data `update`);
/// - `stopping` with the notice to show, as the server stops; the stream then ends;
/// - `failed` with the notice to show, when the review can no longer be watched, or polling keeps
///   failing; the stream then ends.
pub async fn events(
    State(state): State<AppState>,
    Path(review_id): Path<i64>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, PageError> {
    let latest = state
        .call(move |service| service.latest_event_id(review_id))
        .await?;
    // A reconnecting EventSource sends the id of the last change it was told about.
    let reconnected = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok());
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let watch = Watch {
        state,
        review_id,
        seen: reconnected.or(query.after).unwrap_or(latest),
        update_announced: false,
        ticker,
        failures: 0,
        ended: false,
    };
    let stream = stream::unfold(watch, |mut watch| async move {
        if watch.ended {
            return None;
        }
        let event = watch.next_event().await;
        Some((Ok(event), watch))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// One open stream. It polls only while the page is connected: the stream is dropped when the
/// connection closes.
struct Watch {
    state: AppState,
    review_id: i64,
    /// The latest event the page has been told about.
    seen: i64,
    /// Whether this stream has told the page about a newer installed lani.
    update_announced: bool,
    ticker: Interval,
    /// Failed polls in a row.
    failures: u32,
    /// Set after the last message, `stopping` or `failed`.
    ended: bool,
}

impl Watch {
    /// Wait for the next thing to tell the page. The first tick is immediate, so a page that
    /// missed changes between being rendered and connecting hears about them at once.
    async fn next_event(&mut self) -> Event {
        loop {
            tokio::select! {
                stop = self.state.shutdown_requested() => {
                    self.ended = true;
                    return Event::default().event("stopping").data(notice(&stop.message(&self.state.reopen_hint())));
                }
                _ = self.ticker.tick() => {}
            }
            let review_id = self.review_id;
            match self
                .state
                .call(move |service| service.latest_event_id(review_id))
                .await
            {
                // Not `>`: a page rendered from another database (moved aside and recreated)
                // has seen ids this one has not reached.
                Ok(latest) => {
                    self.failures = 0;
                    if latest != self.seen {
                        self.seen = latest;
                        return Event::default()
                            .event("changed")
                            .id(latest.to_string())
                            .data("events");
                    }
                }
                Err(error) => {
                    self.failures += 1;
                    if give_up(&error, self.failures) {
                        self.ended = true;
                        return Event::default()
                            .event("failed")
                            .data(notice(&failure(&error)));
                    }
                    eprintln!(
                        "live updates for review {review_id}: poll failed, retrying: {error}"
                    );
                }
            }
            if !self.update_announced && self.state.outdated() {
                self.update_announced = true;
                return Event::default().event("changed").data("update");
            }
        }
    }
}

/// Whether a stream should stop after `failures` failed polls in a row, the last with `error`.
/// An outdated server and a missing review are final: the next poll would only repeat them.
fn give_up(error: &Error, failures: u32) -> bool {
    matches!(error, Error::Outdated(_) | Error::NotFound(_))
        || failures >= FAILURES_BEFORE_GIVING_UP
}

/// What a page shows when its stream fails.
fn failure(error: &Error) -> String {
    match error {
        Error::Outdated(message) => views::outdated_server_message(message),
        _ => format!("Live updates stopped: {error}. Reload the page to start them again."),
    }
}

/// `text` on one line, as an SSE data field: a lone carriage return would end the line early.
fn notice(text: &str) -> String {
    text::neutralize_line_breaks(text).replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_retry_errors_that_may_pass_and_stop_on_final_ones() {
        let busy = || Error::Io(std::io::Error::other("database is busy"));
        assert!(!give_up(&busy(), 1), "one failure is retried");
        assert!(!give_up(&busy(), FAILURES_BEFORE_GIVING_UP - 1));
        assert!(give_up(&busy(), FAILURES_BEFORE_GIVING_UP));
        assert!(give_up(&Error::Outdated("newer schema".into()), 1));
        assert!(give_up(
            &Error::NotFound("review 9 does not exist".into()),
            1
        ));
    }
}
