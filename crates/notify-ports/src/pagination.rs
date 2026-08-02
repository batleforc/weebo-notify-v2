//! Cursor-based pagination, used by every list endpoint (design §8).
//!
//! Offsets are not an option here. An inbox is written to while it is being read,
//! and `LIMIT/OFFSET` under concurrent inserts skips rows and repeats rows — in a
//! notification product that means "I never saw the incident" and "why did this
//! arrive twice", which are the two complaints worth engineering against.
//!
//! The page cursor is the same opaque token as the sync cursor
//! (`notify_domain::Cursor`), so a client has one thing to store and one thing to
//! send back.

use notify_domain::Cursor;
use serde::{Deserialize, Serialize};

/// What a caller asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageRequest {
    /// Where to resume. `None` starts at the beginning of the ordering.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<Cursor>,
    /// How many rows at most. Clamped to [`PageRequest::MAX_LIMIT`] by the adapter,
    /// because a client asking for a million rows is a client that has not thought
    /// about it, not a client we should try to satisfy.
    pub limit: u32,
}

impl PageRequest {
    /// Applied when a caller does not say.
    pub const DEFAULT_LIMIT: u32 = 50;

    /// The most any caller can get in one page.
    pub const MAX_LIMIT: u32 = 200;

    /// A first page of default size.
    #[must_use]
    pub const fn first() -> Self {
        Self {
            after: None,
            limit: Self::DEFAULT_LIMIT,
        }
    }

    /// A first page of a given size, clamped.
    #[must_use]
    pub const fn of_size(limit: u32) -> Self {
        Self {
            after: None,
            limit: Self::clamp(limit),
        }
    }

    /// The next page after `cursor`, keeping this request's size.
    #[must_use]
    pub const fn after(self, cursor: Cursor) -> Self {
        Self {
            after: Some(cursor),
            limit: self.limit,
        }
    }

    /// The limit an adapter should actually apply.
    #[must_use]
    pub const fn effective_limit(self) -> u32 {
        Self::clamp(self.limit)
    }

    const fn clamp(limit: u32) -> u32 {
        if limit == 0 {
            Self::DEFAULT_LIMIT
        } else if limit > Self::MAX_LIMIT {
            Self::MAX_LIMIT
        } else {
            limit
        }
    }
}

impl Default for PageRequest {
    fn default() -> Self {
        Self::first()
    }
}

/// What a caller gets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page<T> {
    /// The rows, in the ordering of the endpoint.
    pub items: Vec<T>,
    /// Pass this back as [`PageRequest::after`] for the next page. `None` means
    /// this was the last one.
    ///
    /// Note that `next` being `Some` does not promise the next page is non-empty:
    /// rows can expire between two calls. Clients follow the cursor until it is
    /// `None` rather than until a page comes back short.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<Cursor>,
}

impl<T> Page<T> {
    /// A page that ends here.
    #[must_use]
    pub const fn last(items: Vec<T>) -> Self {
        Self { items, next: None }
    }

    /// A page with more behind it.
    #[must_use]
    pub const fn more(items: Vec<T>, next: Cursor) -> Self {
        Self {
            items,
            next: Some(next),
        }
    }

    /// An empty final page.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            items: Vec::new(),
            next: None,
        }
    }

    /// How many rows came back.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the page is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Apply a transformation while keeping the cursor.
    #[must_use]
    pub fn map<U, F: FnMut(T) -> U>(self, f: F) -> Page<U> {
        Page {
            items: self.items.into_iter().map(f).collect(),
            next: self.next,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_limit_is_always_sane() {
        assert_eq!(PageRequest::first().effective_limit(), 50);
        assert_eq!(PageRequest::of_size(0).limit, PageRequest::DEFAULT_LIMIT);
        assert_eq!(PageRequest::of_size(10_000).limit, PageRequest::MAX_LIMIT);
        // A hand-built request is clamped when it is used, not only when it is built.
        let unclamped = PageRequest {
            after: None,
            limit: 10_000,
        };
        assert_eq!(unclamped.effective_limit(), PageRequest::MAX_LIMIT);
    }

    #[test]
    fn following_a_page_keeps_the_page_size() {
        let request = PageRequest::of_size(25).after(Cursor::from_sequence(9));
        assert_eq!(request.limit, 25);
        assert_eq!(request.after, Some(Cursor::from_sequence(9)));
    }

    #[test]
    fn a_page_can_be_transformed_without_losing_its_place() {
        let page = Page::more(vec![1, 2, 3], Cursor::from_sequence(4));
        let mapped = page.map(|value| value * 2);
        assert_eq!(mapped.items, [2, 4, 6]);
        assert_eq!(mapped.next, Some(Cursor::from_sequence(4)));
        assert_eq!(mapped.len(), 3);
        assert!(!mapped.is_empty());
        assert!(Page::<u8>::empty().is_empty());
    }
}
