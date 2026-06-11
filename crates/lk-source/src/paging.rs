//! The one pagination termination rule every listing adapter shares.
//!
//! Every listing API this crate consumes (Gmail/Calendar/Drive `nextPageToken`,
//! Jira `/search/jql` `nextPageToken`, Slack cursors) signals completion ONLY by
//! the absence of a continuation token: a page may legitimately arrive EMPTY —
//! or shorter than requested — while a token is still present, because the server
//! filters items out of a page without ending the listing. Terminating on an
//! empty page therefore silently truncates a fetch, which breaks the
//! complete-refetch contract (the daily page is re-rendered from the fetch, so a
//! dropped item is silently lost knowledge).
//!
//! [`page_step`] is that termination rule as a pure function, single-sourced so
//! no adapter loop can disagree on it: stop at the cap, stop when the token is
//! absent, otherwise keep following tokens under a hard page budget that keeps an
//! unattended ingest finite against a pathological server streaming tokens
//! without progress.

/// Hard per-fetch page budget. A real fetch needs `cap / page_size` pages of
/// data (single digits for every adapter) plus the occasional empty filtered
/// page; 100 is far beyond all of that and only trips on a server looping
/// continuation tokens without progress.
pub(crate) const MAX_PAGES: usize = 100;

/// What the fetch loop does after appending one page.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PageStep {
    /// Follow the continuation token and fetch the next page.
    Continue,
    /// The listing finished or the cap was reached. `dropped` is set iff items
    /// were lost to the cap — an overshoot within the final page, or a
    /// continuation token still pending. The caller truncates to the cap and
    /// warns iff this is set, so an exact-cap fetch with nothing pending stays
    /// silent (never a false alarm).
    Stop { dropped: bool },
    /// The page budget tripped before the listing completed. The caller warns
    /// that results are incomplete and stops — loud, never silent.
    Exhausted,
}

/// Decide the next step from `collected` items so far (pre-truncation), the
/// configured `cap`, whether the response carried a continuation token, and how
/// many pages have been fetched.
pub(crate) fn page_step(
    collected: usize,
    cap: usize,
    has_next: bool,
    pages_fetched: usize,
) -> PageStep {
    if collected >= cap {
        return PageStep::Stop {
            dropped: collected > cap || has_next,
        };
    }
    if !has_next {
        return PageStep::Stop { dropped: false };
    }
    if pages_fetched >= MAX_PAGES {
        return PageStep::Exhausted;
    }
    PageStep::Continue
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_cap_without_token_finishes_clean() {
        assert_eq!(
            page_step(3, 10, false, 1),
            PageStep::Stop { dropped: false }
        );
    }

    #[test]
    fn under_cap_with_token_continues() {
        assert_eq!(page_step(3, 10, true, 1), PageStep::Continue);
    }

    #[test]
    fn empty_page_with_token_continues() {
        // The regression this module exists for: a server may return an empty
        // (filtered) page WITH a continuation token mid-listing. Only the
        // token's absence ends the listing — an empty page must not.
        assert_eq!(page_step(0, 10, true, 1), PageStep::Continue);
        assert_eq!(page_step(0, 10, true, 5), PageStep::Continue);
    }

    #[test]
    fn empty_listing_finishes_clean() {
        assert_eq!(
            page_step(0, 10, false, 1),
            PageStep::Stop { dropped: false }
        );
    }

    #[test]
    fn exact_cap_without_token_stays_silent() {
        // Fetching exactly `cap` with nothing pending drops nothing — a
        // "dropped" warning here would be a false alarm.
        assert_eq!(
            page_step(10, 10, false, 1),
            PageStep::Stop { dropped: false }
        );
    }

    #[test]
    fn exact_cap_with_pending_token_reports_dropped() {
        assert_eq!(page_step(10, 10, true, 1), PageStep::Stop { dropped: true });
    }

    #[test]
    fn overshoot_reports_dropped_even_without_token() {
        assert_eq!(
            page_step(12, 10, false, 1),
            PageStep::Stop { dropped: true }
        );
    }

    #[test]
    fn budget_trips_only_with_more_pages_pending() {
        assert_eq!(page_step(3, 10, true, MAX_PAGES), PageStep::Exhausted);
        // At the budget but complete → clean stop, not Exhausted.
        assert_eq!(
            page_step(3, 10, false, MAX_PAGES),
            PageStep::Stop { dropped: false }
        );
        // At the budget but at cap → the cap verdict wins.
        assert_eq!(
            page_step(10, 10, true, MAX_PAGES),
            PageStep::Stop { dropped: true }
        );
    }
}
