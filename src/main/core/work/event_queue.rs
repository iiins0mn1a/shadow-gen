use std::cmp::Reverse;
use std::collections::binary_heap::BinaryHeap;
use std::sync::atomic::{AtomicU64, Ordering};

use shadow_shim_helper_rs::emulated_time::EmulatedTime;

use super::event::Event;

/// A queue of [`Event`]s ordered by their times.
#[derive(Debug)]
pub struct EventQueue {
    queue: BinaryHeap<Reverse<PanickingOrd<Event>>>,
    last_popped_event_time: EmulatedTime,
}

// 简单的原子计数器，用于粗粒度地观测 push/pop 次数和队列长度
static PUSH_COUNT: AtomicU64 = AtomicU64::new(0);
static POP_COUNT: AtomicU64 = AtomicU64::new(0);

// 每隔多少次操作打印一次日志，避免刷屏
const LOG_EVERY: u64 = 1_000;

impl EventQueue {
    pub fn new() -> Self {
        Self {
            queue: BinaryHeap::new(),
            last_popped_event_time: EmulatedTime::SIMULATION_START,
        }
    }

    fn log_stats(&self, op: &'static str, count: u64) {
        if count % LOG_EVERY == 0 {
            let time_ns = self
                .last_popped_event_time
                .duration_since(&EmulatedTime::SIMULATION_START)
                .as_nanos();
            eprintln!(
                "[event-queue] op={} count={} len={} last_pop_time_ns={}",
                op, count, self.queue.len(), time_ns
            );
        }
    }

    /// Push a new [`Event`] on to the queue.
    ///
    /// Will panic if two events are pushed that have no relative order
    /// (`event_a.partial_cmp(&event_b) == None`). Will be non-deterministic if two events are
    /// pushed that are equal (`event_a == event_b`).
    ///
    /// Will panic if the event time is earlier than the last popped event time (time moves
    /// backward).
    pub fn push(&mut self, event: Event) {
        // make sure time never moves backward
        assert!(event.time() >= self.last_popped_event_time);

        self.queue.push(Reverse(event.into()));

        let count = PUSH_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        self.log_stats("push", count);
    }

    /// Pop the earliest [`Event`] from the queue.
    pub fn pop(&mut self) -> Option<Event> {
        let event = self.queue.pop().map(|x| x.0.into_inner());

        // make sure time never moves backward
        if let Some(ref event) = event {
            assert!(event.time() >= self.last_popped_event_time);
            self.last_popped_event_time = event.time();
        }

        let count = POP_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        self.log_stats("pop", count);

        event
    }

    /// The time of the next [`Event`] (the time of the earliest event in the queue).
    pub fn next_event_time(&self) -> Option<EmulatedTime> {
        self.queue.peek().map(|x| x.0.time())
    }
}

impl Default for EventQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// A wrapper type that implements [`Ord`] for types that implement [`PartialOrd`]. If the two
/// objects cannot be compared (`PartialOrd::partial_cmp` returns `None`), the comparison will
/// panic.
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
struct PanickingOrd<T: PartialOrd + Eq>(T);

impl<T: PartialOrd + Eq> PanickingOrd<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T: PartialOrd + Eq> std::convert::From<T> for PanickingOrd<T> {
    fn from(x: T) -> Self {
        PanickingOrd(x)
    }
}

impl<T: PartialOrd + Eq> PartialOrd for PanickingOrd<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T: PartialOrd + Eq> Ord for PanickingOrd<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.partial_cmp(&other.0).unwrap()
    }
}

impl<T: PartialOrd + Eq> std::ops::Deref for PanickingOrd<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T: PartialOrd + Eq> std::ops::DerefMut for PanickingOrd<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
