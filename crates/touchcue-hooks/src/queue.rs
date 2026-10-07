//! Bounded single-consumer queue that never blocks its producers and drops
//! only droppable items.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};

use tokio::sync::Notify;

/// An item that may be dropped when its queue is full.
pub(crate) trait Droppable {
    fn droppable(&self) -> bool;
}

/// Outcome of [`Queue::push`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pushed {
    Queued,
    /// The queue was full: the oldest droppable item, or this item when the
    /// queue holds none, was dropped.
    Dropped,
    /// As [`Pushed::Dropped`], and [`STALL_FACTOR`] times the capacity of
    /// items were dropped since an item was last taken.
    Stalled,
    /// The queue was closed and the item was dropped.
    Closed,
}

/// Drops without a take, in multiples of the capacity, that make
/// [`Queue::push`] report [`Pushed::Stalled`].
pub(crate) const STALL_FACTOR: usize = 4;

/// Queue of at most `capacity` items, plus the items that are not droppable.
///
/// The queue exceeds `capacity` only by items that are not droppable.
#[derive(Debug)]
pub(crate) struct Queue<T> {
    state: Mutex<State<T>>,
    notify: Notify,
    capacity: usize,
}

#[derive(Debug)]
struct State<T> {
    items: VecDeque<T>,
    closed: bool,
    /// Items dropped since an item was last taken.
    dropped: usize,
}

impl<T: Droppable> Queue<T> {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(State {
                items: VecDeque::new(),
                closed: false,
                dropped: 0,
            }),
            notify: Notify::new(),
            capacity,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State<T>> {
        match self.state.lock() {
            Ok(state) => state,
            // Every critical section leaves the state consistent.
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Appends `item` without waiting.
    pub(crate) fn push(&self, item: T) -> Pushed {
        let pushed = {
            let mut state = self.lock();
            if state.closed {
                return Pushed::Closed;
            }
            if state.items.len() < self.capacity {
                state.items.push_back(item);
                Pushed::Queued
            } else if let Some(oldest) = state.items.iter().position(Droppable::droppable) {
                state.items.remove(oldest);
                state.items.push_back(item);
                self.dropped(&mut state)
            } else if item.droppable() {
                return self.dropped(&mut state);
            } else {
                state.items.push_back(item);
                Pushed::Queued
            }
        };
        self.notify.notify_one();
        pushed
    }

    /// Counts a dropped item.
    fn dropped(&self, state: &mut State<T>) -> Pushed {
        state.dropped = state.dropped.saturating_add(1);
        if state.dropped >= self.capacity.saturating_mul(STALL_FACTOR) {
            Pushed::Stalled
        } else {
            Pushed::Dropped
        }
    }

    /// Lets the queued items be taken, then makes [`Queue::pop`] return
    /// `None`; later pushes are dropped.
    pub(crate) fn close(&self) {
        self.lock().closed = true;
        self.notify.notify_one();
    }

    #[cfg(test)]
    pub(crate) fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// Takes the oldest item, waiting for one; `None` once the queue is
    /// closed and empty. Meant for one consumer.
    pub(crate) async fn pop(&self) -> Option<T> {
        loop {
            let notified = self.notify.notified();
            {
                let mut state = self.lock();
                if let Some(item) = state.items.pop_front() {
                    state.dropped = 0;
                    return Some(item);
                }
                if state.closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().items.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    enum Item {
        Keep(u32),
        Drop(u32),
    }

    impl Droppable for Item {
        fn droppable(&self) -> bool {
            matches!(self, Self::Drop(_))
        }
    }

    #[tokio::test]
    async fn full_queue_drops_the_oldest_droppable_item() {
        let queue = Queue::new(3);
        assert_eq!(queue.push(Item::Keep(1)), Pushed::Queued);
        assert_eq!(queue.push(Item::Drop(2)), Pushed::Queued);
        assert_eq!(queue.push(Item::Drop(3)), Pushed::Queued);
        assert_eq!(queue.push(Item::Drop(4)), Pushed::Dropped);
        assert_eq!(queue.push(Item::Keep(5)), Pushed::Dropped);
        assert_eq!(queue.push(Item::Keep(6)), Pushed::Dropped);
        // Only items that are not droppable are left, so a droppable one is
        // refused and one that is not exceeds the capacity.
        assert_eq!(queue.push(Item::Drop(7)), Pushed::Dropped);
        assert_eq!(queue.push(Item::Keep(8)), Pushed::Queued);
        assert_eq!(queue.len(), 4);
        queue.close();
        assert!(queue.is_closed());
        assert_eq!(queue.push(Item::Keep(9)), Pushed::Closed);
        let mut taken = Vec::new();
        while let Some(item) = queue.pop().await {
            taken.push(item);
        }
        assert_eq!(
            taken,
            [Item::Keep(1), Item::Keep(5), Item::Keep(6), Item::Keep(8)]
        );
    }

    #[tokio::test]
    async fn drops_without_a_take_report_a_stall() {
        let queue = Queue::new(2);
        queue.push(Item::Drop(0));
        queue.push(Item::Drop(1));
        let pushed: Vec<Pushed> = (2..10).map(|n| queue.push(Item::Drop(n))).collect();
        assert_eq!(pushed.last(), Some(&Pushed::Stalled));
        assert_eq!(
            pushed.iter().filter(|p| **p == Pushed::Dropped).count(),
            2 * STALL_FACTOR - 1
        );
        assert_eq!(queue.pop().await, Some(Item::Drop(8)));
        assert_eq!(queue.push(Item::Drop(10)), Pushed::Queued);
        assert_eq!(queue.push(Item::Drop(11)), Pushed::Dropped);
    }

    #[tokio::test]
    async fn pop_waits_for_a_push() -> Result<(), tokio::task::JoinError> {
        let queue = std::sync::Arc::new(Queue::new(1));
        let consumer = tokio::spawn({
            let queue = std::sync::Arc::clone(&queue);
            async move { queue.pop().await }
        });
        tokio::task::yield_now().await;
        queue.push(Item::Keep(1));
        assert_eq!(consumer.await?, Some(Item::Keep(1)));
        Ok(())
    }
}
