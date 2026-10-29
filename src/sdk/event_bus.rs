//! A native, typed event bus (the Rust counterpart of upstream
//! `createEventBus` / `EventBus`, `core/event-bus.ts`).
//!
//! Upstream's bus is channel-string based with `unknown` payloads because
//! TypeScript cannot type an emitter; this Rust facade is generic over the
//! event type instead. One bus carries one event type — the common embedding
//! shape (a session's events, an application's own notifications) — and a
//! consumer that wants dynamic payloads instantiates
//! `EventBus<serde_json::Value>`.
//!
//! Semantics kept from upstream:
//!
//! - handlers run in registration order;
//! - subscribing returns an unsubscribe handle (upstream's `on` returns a
//!   function); dropping the handle unsubscribes, and [`EventBus::off`] does
//!   it explicitly;
//! - a handler may subscribe/unsubscribe during dispatch: the handler list is
//!   snapshotted before the first call;
//! - [`EventBus::clear`] drops every handler (upstream `EventBusController`).
//!
//! Deviation: upstream isolates a throwing (async) handler; a Rust closure
//! has no error channel, and panics are the caller's contract, so a panicking
//! handler propagates to [`EventBus::emit`]'s caller (hosts that need
//! isolation wrap their own `catch_unwind`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

/// One registered handler.
type Handler<T> = Arc<dyn Fn(&T) + Send + Sync>;

struct Entry<T> {
    id: u64,
    handler: Handler<T>,
}

struct EventBusInner<T> {
    handlers: Mutex<Vec<Entry<T>>>,
    next_id: AtomicU64,
}

/// A cheap-cloneable, typed publish/subscribe bus.
///
/// Clones share the same handler list. Send + Sync when `T: Send + Sync`.
pub struct EventBus<T> {
    inner: Arc<EventBusInner<T>>,
}

impl<T> Clone for EventBus<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> Default for EventBus<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> std::fmt::Debug for EventBus<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EventBus")
            .field("listeners", &self.listener_count())
            .finish()
    }
}

impl<T> EventBus<T> {
    /// Create an empty bus.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(EventBusInner {
                handlers: Mutex::new(Vec::new()),
                next_id: AtomicU64::new(1),
            }),
        }
    }

    /// Subscribe one handler. The returned [`EventBusSubscription`]
    /// unsubscribes when dropped (or explicitly through [`Self::off`]).
    pub fn on<F>(&self, handler: F) -> EventBusSubscription<T>
    where
        F: Fn(&T) + Send + Sync + 'static,
    {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner
            .handlers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Entry {
                id,
                handler: Arc::new(handler),
            });
        EventBusSubscription {
            bus: Arc::downgrade(&self.inner),
            id,
            active: true,
        }
    }

    /// Publish one event to every handler registered at call time, in
    /// registration order.
    pub fn emit(&self, event: &T) {
        let handlers: Vec<Handler<T>> = self
            .inner
            .handlers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|entry| Arc::clone(&entry.handler))
            .collect();
        for handler in handlers {
            handler(event);
        }
    }

    /// Remove one subscription explicitly. Returns whether it was registered
    /// on THIS bus.
    ///
    /// A handle from a DIFFERENT bus never removes anything (returns `false`):
    /// registration ids are per-bus counters, so an unrelated bus' id can
    /// collide with a local one, and removing by id alone would silently
    /// unsubscribe the wrong handler (upstream's `on` returns a closure bound
    /// to the bus it was registered on, which cannot be applied to another
    /// bus at all).
    pub fn off(&self, subscription: &EventBusSubscription<T>) -> bool {
        if !std::ptr::eq(subscription.bus.as_ptr(), Arc::as_ptr(&self.inner)) {
            return false;
        }
        let mut handlers = self
            .inner
            .handlers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = handlers.len();
        handlers.retain(|entry| entry.id != subscription.id);
        handlers.len() != before
    }

    /// Remove every handler (upstream `EventBusController.clear`).
    pub fn clear(&self) {
        self.inner
            .handlers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Number of registered handlers.
    pub fn listener_count(&self) -> usize {
        self.inner
            .handlers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Whether anybody is subscribed.
    pub fn has_listeners(&self) -> bool {
        self.listener_count() > 0
    }
}

/// The handle returned by [`EventBus::on`]. Dropping it unsubscribes; use
/// [`EventBus::off`] when the removal must be observed eagerly.
pub struct EventBusSubscription<T> {
    bus: Weak<EventBusInner<T>>,
    id: u64,
    active: bool,
}

impl<T> EventBusSubscription<T> {
    /// The registration id (unique per bus; useful for diagnostics).
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Whether this handle is still registered on its bus: `false` after
    /// [`EventBus::off`] removed it, after [`EventBus::clear`], and once the
    /// bus itself is gone. Dropping a handle that is no longer registered is
    /// the no-op it looks like.
    pub fn is_active(&self) -> bool {
        if !self.active {
            return false;
        }
        let Some(bus) = self.bus.upgrade() else {
            return false;
        };
        let handlers = bus
            .handlers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        handlers.iter().any(|entry| entry.id == self.id)
    }

    /// Explicitly unsubscribe (same as dropping it).
    pub fn unsubscribe(mut self) -> bool {
        self.active = false;
        let Some(bus) = self.bus.upgrade() else {
            return false;
        };
        let mut handlers = bus
            .handlers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = handlers.len();
        handlers.retain(|entry| entry.id != self.id);
        handlers.len() != before
    }
}

impl<T> Drop for EventBusSubscription<T> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Some(bus) = self.bus.upgrade() {
            let mut handlers = bus
                .handlers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            handlers.retain(|entry| entry.id != self.id);
        }
    }
}

impl<T> std::fmt::Debug for EventBusSubscription<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EventBusSubscription")
            .field("id", &self.id)
            .field("active", &self.active)
            .finish()
    }
}

/// Create an empty [`EventBus`] (the Rust counterpart of upstream
/// `createEventBus`).
pub fn create_event_bus<T>() -> EventBus<T> {
    EventBus::new()
}
