use serde::Serialize;

/// Rows per page in history and activity.
pub const PAGE_SIZE: usize = 50;

/// The position after the last row of a page: its time and id. Rows sort by time, then id, newest
/// first, so a cursor stays correct while new rows arrive and when many rows share one time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub at: i64,
    pub id: i64,
}

impl Cursor {
    /// Reads `<time>-<id>` as the page API sends it.
    pub fn parse(text: &str) -> Option<Self> {
        let (at, id) = text.split_once('-')?;
        Some(Self {
            at: at.parse().ok()?,
            id: id.parse().ok()?,
        })
    }

    pub fn encode(self) -> String {
        format!("{}-{}", self.at, self.id)
    }

    /// The first page starts after every real row.
    pub fn start() -> Self {
        Self {
            at: i64::MAX,
            id: i64::MAX,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Cursor of the next page; `None` on the last page.
    pub next: Option<String>,
}

impl<T> Page<T> {
    /// Builds a page from up to `PAGE_SIZE + 1` rows: the extra row only tells that more exist.
    pub fn from_rows(mut rows: Vec<T>, cursor_of: impl Fn(&T) -> Cursor) -> Self {
        let more = rows.len() > PAGE_SIZE;
        rows.truncate(PAGE_SIZE);
        let next = more
            .then(|| rows.last().map(|row| cursor_of(row).encode()))
            .flatten();
        Self { items: rows, next }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trips_and_rejects_garbage() {
        let cursor = Cursor { at: 1700, id: 42 };
        assert_eq!(Cursor::parse(&cursor.encode()), Some(cursor));
        for bad in ["", "1700", "a-1", "1-b", "1-2-3"] {
            assert_eq!(Cursor::parse(bad), None, "{bad}");
        }
    }
}
