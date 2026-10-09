//! The overview's Activity section (docs/design.md §13.3): a review's events in pages, newest
//! first, with a choice of page size. Paging back anchors the pages at the latest event, so they
//! stay put while new events arrive; a notice then says how many arrived.

use std::fmt::Write as _;

use axum::http::StatusCode;

use super::views::{self, EventView, PageError, review_url};
use crate::service::{ActivityPage, ActivityRequest};

/// The page sizes offered, in order; `None` is all events on one page.
const PAGE_SIZES: [Option<u32>; 5] = [Some(10), Some(25), Some(50), Some(100), None];
/// The page size when the URL names none.
const DEFAULT_PER_PAGE: u32 = 10;

/// The activity page an overview URL asks for: its `events` (page size), `page` and `latest`
/// (anchor) query values. Absent or empty values mean the defaults: 10 per page, page 1, and no
/// anchor.
pub fn request(
    events: Option<&str>,
    page: Option<&str>,
    latest: Option<&str>,
) -> Result<ActivityRequest, PageError> {
    let per_page = match present(events) {
        None => Some(DEFAULT_PER_PAGE),
        Some("all") => None,
        Some(value) => PAGE_SIZES
            .into_iter()
            .flatten()
            .find(|size| size.to_string() == value)
            .map(Some)
            .ok_or_else(|| {
                bad(&format!(
                    "{value:?} is not a page size; use 10, 25, 50, 100 or all."
                ))
            })?,
    };
    let page = match present(page) {
        None => 1,
        Some(value) => positive(value)
            .and_then(|page| u32::try_from(page).ok())
            .ok_or_else(|| bad(&format!("{value:?} is not a page number.")))?,
    };
    let anchor = match present(latest) {
        None => None,
        Some(value) => Some(
            positive(value)
                .and_then(|id| i64::try_from(id).ok())
                .ok_or_else(|| bad(&format!("{value:?} is not an event number.")))?,
        ),
    };
    if per_page.is_none() && page > 1 {
        return Err(bad("All events are on page 1."));
    }
    Ok(ActivityRequest {
        per_page,
        page,
        anchor,
    })
}

/// A query value, unless it is empty.
fn present(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

/// A whole number above 0, written with digits only.
fn positive(text: &str) -> Option<u64> {
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().filter(|number| *number > 0)
}

fn bad(message: &str) -> PageError {
    PageError::new(StatusCode::BAD_REQUEST, "Unknown activity page", message)
}

/// The overview's URL with an issue filter and an activity page, at `fragment`. Defaults are
/// left out of the query, except the issue filter, which is always named.
pub fn overview_url(
    review_id: i64,
    status: &str,
    request: ActivityRequest,
    fragment: &str,
) -> String {
    let mut url = format!("{}?status={status}", review_url(review_id));
    match request.per_page {
        Some(DEFAULT_PER_PAGE) => {}
        Some(per_page) => {
            let _ = write!(url, "&events={per_page}");
        }
        None => url.push_str("&events=all"),
    }
    if request.page > 1 {
        let _ = write!(url, "&page={}", request.page);
    }
    if let Some(anchor) = request.anchor {
        let _ = write!(url, "&latest={anchor}");
    }
    let _ = write!(url, "#{fragment}");
    url
}

/// The Activity section of the overview.
pub struct ActivityView {
    pub events: Vec<EventView>,
    /// E.g. "Events 11–20 of 57 · page 2 of 6"; `None` when there is nothing to page through.
    pub position: Option<String>,
    /// E.g. "3 new events since you paged back."
    pub new_events: Option<String>,
    /// The latest events, page 1 without an anchor, at the current page size.
    pub latest_url: String,
    /// *Latest*, *Previous N* and *Next N*, as they apply.
    pub links: Vec<PageLink>,
    /// The page sizes; empty when there is nothing to page through.
    pub sizes: Vec<SizeLink>,
}

/// A link that changes the page. `key` identifies it in `data-in-place`, which also lets
/// `lani.js` keep the keyboard focus on it.
pub struct PageLink {
    pub key: &'static str,
    pub label: String,
    pub url: String,
}

pub struct SizeLink {
    pub label: String,
    /// `None` for the current size.
    pub url: Option<String>,
}

#[must_use]
pub fn view(
    review_id: i64,
    status: &str,
    request: ActivityRequest,
    page: &ActivityPage,
) -> ActivityView {
    let url = |request: ActivityRequest| overview_url(review_id, status, request, "activity");
    let latest = ActivityRequest {
        page: 1,
        anchor: None,
        ..request
    };
    let shown = u64::try_from(page.events.len()).unwrap_or(u64::MAX);
    let (first, last) = (page.offset + 1, page.offset + shown);
    let anchored = request.anchor.is_some();

    let mut links = Vec::new();
    // On page 2, *Previous* already leads to the latest.
    if anchored && request.page != 2 {
        links.push(PageLink {
            key: "latest",
            label: "Latest".to_string(),
            url: url(latest),
        });
    }
    if request.page > 1 {
        // Back to page 1 means back to the latest events, without an anchor.
        let previous = if request.page == 2 {
            latest
        } else {
            ActivityRequest {
                page: request.page - 1,
                ..request
            }
        };
        let size = request.per_page.map_or(0, u64::from);
        links.push(PageLink {
            key: "previous",
            label: format!("Previous {size}"),
            url: url(previous),
        });
    }
    if last < page.total {
        let left = page.total - last;
        let count = request
            .per_page
            .map_or(left, |per_page| left.min(u64::from(per_page)));
        links.push(PageLink {
            key: "next",
            label: format!("Next {count}"),
            url: url(ActivityRequest {
                page: request.page + 1,
                // Paging back anchors the pages at the latest event, so they stay put.
                anchor: request.anchor.or(Some(page.latest_id)),
                ..request
            }),
        });
    }

    let pageable = anchored || page.total > u64::from(DEFAULT_PER_PAGE);
    let sizes = if pageable {
        PAGE_SIZES
            .into_iter()
            .map(|per_page| SizeLink {
                label: per_page.map_or_else(|| "All".to_string(), |size| size.to_string()),
                url: (per_page != request.per_page).then(|| {
                    // The page that holds the first event shown now.
                    let page_number = per_page.map_or(1, |size| {
                        u32::try_from((first - 1) / u64::from(size) + 1).unwrap_or(u32::MAX)
                    });
                    let anchor = request
                        .anchor
                        .or((page_number > 1).then_some(page.latest_id));
                    url(ActivityRequest {
                        per_page,
                        page: page_number,
                        anchor,
                    })
                }),
            })
            .collect()
    } else {
        Vec::new()
    };
    let position = pageable.then(|| {
        let mut position = format!("Events {first}–{last} of {}", page.total);
        if let Some(per_page) = request.per_page {
            let pages = page.total.div_ceil(u64::from(per_page)).max(1);
            if pages > 1 {
                let _ = write!(position, " · page {} of {pages}", request.page);
            }
        }
        position
    });
    let new_events = (page.newer > 0).then(|| {
        let noun = if page.newer == 1 { "event" } else { "events" };
        format!("{} new {noun} since you paged back.", page.newer)
    });
    ActivityView {
        events: page.events.iter().map(views::event).collect(),
        position,
        new_events,
        latest_url: url(latest),
        links,
        sizes,
    }
}
