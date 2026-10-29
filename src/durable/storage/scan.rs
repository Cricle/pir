//! Scan order and cursor resolution shared by the storage backends.
//!
//! Mirrors upstream `src/storage/scan.ts`: a cursor continues in the order it
//! was created with, whether or not the query repeats that order; a query that
//! asks for a different order is rejected as an invalid request. A cursor
//! without an order (written before scans carried one) continues in the scan's
//! default order.

use crate::durable::errors::StorageError;
use crate::durable::types::{Cursor, ScanOrder};

/// Where a built-in storage scan starts: its order and the last ID a previous
/// page returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanStart {
    /// The order the scan moves in.
    pub order: ScanOrder,
    /// The last ID a previous page returned, if any.
    pub after: Option<i64>,
}

/// Resolve a scan's order and position.
pub fn scan_start(
    requested: Option<ScanOrder>,
    cursor: Option<&Cursor>,
    fallback: ScanOrder,
) -> Result<ScanStart, StorageError> {
    let Some(cursor) = cursor else {
        return Ok(ScanStart {
            order: requested.unwrap_or(fallback),
            after: None,
        });
    };
    if cursor.after < 0 {
        return Err(StorageError::Request(
            crate::durable::errors::StorageRequestError::new("Invalid storage cursor"),
        ));
    }
    let order = cursor.order.unwrap_or(fallback);
    if let Some(requested) = requested {
        if requested != order {
            return Err(StorageError::Request(
                crate::durable::errors::StorageRequestError::new(format!(
                    "The cursor continues a {} scan; the query asks for {}",
                    order_name(order),
                    order_name(requested)
                )),
            ));
        }
    }
    Ok(ScanStart {
        order,
        after: Some(cursor.after),
    })
}

/// The continuation of a scan whose last returned item has `id`.
pub fn next_cursor(id: i64, order: ScanOrder) -> Cursor {
    Cursor {
        after: id,
        order: Some(order),
    }
}

fn order_name(order: ScanOrder) -> &'static str {
    match order {
        ScanOrder::Ascending => "ascending",
        ScanOrder::Descending => "descending",
    }
}

/// Build a [`Page`](crate::durable::types::Page) from an over-fetched slice.
///
/// `values` must hold at most `limit + 1` items: when it holds more than
/// `limit`, the extra item proves there is another page and only the first
/// `limit` are returned.
pub fn page_of<T: Clone>(
    values: &[T],
    limit: usize,
    order: ScanOrder,
    id: impl Fn(&T) -> i64,
) -> (Vec<T>, Option<Cursor>) {
    let items: Vec<T> = values.iter().take(limit).cloned().collect();
    if values.len() <= limit {
        (items, None)
    } else {
        let next = items.last().map(|last| next_cursor(id(last), order));
        (items, next)
    }
}
