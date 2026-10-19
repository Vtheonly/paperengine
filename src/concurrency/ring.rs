//! Bounded single-producer/single-consumer event ring.
//!
//! A split borrows the ring mutably and creates exactly one producer handle and one
//! consumer handle. The two handles may move to separate threads; neither is Clone.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, Ordering};

#[repr(C, align(64))]
pub struct CacheAlignedSequence {
    value: AtomicU64,
    _padding: [u8; 56],
}

impl CacheAlignedSequence {
    pub const fn new(initial: u64) -> Self {
        Self { value: AtomicU64::new(initial), _padding: [0; 56] }
    }
}

/// A fixed, pointer-free event that is safe to pass across an adapter boundary.
///
/// payload_hash is an opaque key/fingerprint, not a URL or an authorization token.
/// The receiver must resolve real metadata and re-check policy using its own registry.
#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TaskEnvelope {
    pub paper_id: u64,
    pub action_type: u16,
    pub priority: u16,
    pub score: i32,
    pub source_id: u32,
    pub policy_version: u32,
    pub candidate_index: u32,
    pub payload_hash: [u8; 32],
    pub flags: u32,
}

pub const ACTION_ENRICH_METADATA: u16 = 0;
pub const ACTION_RESOLVE_ROUTE: u16 = 1;
pub const ACTION_VALIDATE_CANDIDATE: u16 = 2;
pub const ACTION_ACQUIRE_VERIFIED: u16 = 3;
pub const EVENT_FLAG_FINAL: u32 = 1 << 0;

const _: () = assert!(core::mem::size_of::<TaskEnvelope>() == 64);
const _: () = assert!(core::mem::align_of::<TaskEnvelope>() == 64);
const _: () = assert!(core::mem::size_of::<CacheAlignedSequence>() == 64);
const _: () = assert!(core::mem::align_of::<CacheAlignedSequence>() == 64);

pub struct EngineRingBuffer<const N: usize> {
    slots: Box<[UnsafeCell<TaskEnvelope>]>,
    producer_sequence: CacheAlignedSequence,
    consumer_sequence: CacheAlignedSequence,
}

// SAFETY: the only API that accesses slots is obtained through split(&mut self), which
// creates one non-Clone producer and one non-Clone consumer for the lifetime of the borrow.
// The producer alone writes a slot before publishing its sequence with Release ordering;
// the consumer alone reads it after observing that sequence with Acquire ordering. The
// consumer publishes slot release before the producer reuses that slot. No third slot
// accessor is exposed, so the SPSC ownership contract prevents data races.
unsafe impl<const N: usize> Sync for EngineRingBuffer<N> {}

impl<const N: usize> EngineRingBuffer<N> {
    /// Allocates all cells up front. No heap allocation occurs in enqueue/dequeue.
    pub fn new() -> Self {
        assert!(N >= 2 && N.is_power_of_two(), "capacity must be a power of two >= 2");
        let slots = (0..N)
            .map(|_| UnsafeCell::new(TaskEnvelope::default()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            slots,
            producer_sequence: CacheAlignedSequence::new(0),
            consumer_sequence: CacheAlignedSequence::new(0),
        }
    }

    pub const fn capacity(&self) -> usize { N }

    /// Enforces one producer and one consumer by borrowing the ring mutably.
    pub fn split(&mut self) -> (RingProducer<'_, N>, RingConsumer<'_, N>) {
        let ring: &EngineRingBuffer<N> = self;
        (
            RingProducer { ring, next_sequence: 0 },
            RingConsumer { ring, next_sequence: 0 },
        )
    }
}

impl<const N: usize> Default for EngineRingBuffer<N> {
    fn default() -> Self { Self::new() }
}

pub struct RingProducer<'a, const N: usize> {
    ring: &'a EngineRingBuffer<N>,
    next_sequence: u64,
}

impl<const N: usize> RingProducer<'_, N> {
    /// Returns the unqueued event when the ring is full; no waiting or spinning is hidden
    /// inside the queue. The caller decides how to apply backpressure.
    #[inline]
    pub fn try_enqueue(&mut self, event: TaskEnvelope) -> Result<(), TaskEnvelope> {
        let consumed = self.ring.consumer_sequence.value.load(Ordering::Acquire);
        if self.next_sequence.wrapping_sub(consumed) >= N as u64 {
            return Err(event);
        }

        let index = (self.next_sequence as usize) & (N - 1);
        // SAFETY: SPSC ownership plus the consumer sequence proves this slot is free.
        unsafe { *self.ring.slots[index].get() = event; }
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.ring.producer_sequence.value.store(self.next_sequence, Ordering::Release);
        Ok(())
    }
}

pub struct RingConsumer<'a, const N: usize> {
    ring: &'a EngineRingBuffer<N>,
    next_sequence: u64,
}

impl<const N: usize> RingConsumer<'_, N> {
    #[inline]
    pub fn try_dequeue(&mut self) -> Option<TaskEnvelope> {
        let published = self.ring.producer_sequence.value.load(Ordering::Acquire);
        if self.next_sequence == published {
            return None;
        }

        let index = (self.next_sequence as usize) & (N - 1);
        // SAFETY: Acquire observed the producer's publication; this consumer exclusively
        // owns the read side and the producer cannot reuse the slot until release below.
        let event = unsafe { *self.ring.slots[index].get() };
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.ring.consumer_sequence.value.store(self.next_sequence, Ordering::Release);
        Some(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: u64) -> TaskEnvelope {
        TaskEnvelope { paper_id: id, ..TaskEnvelope::default() }
    }

    #[test]
    fn event_and_sequence_records_are_cache_aligned() {
        assert_eq!(core::mem::size_of::<TaskEnvelope>(), 64);
        assert_eq!(core::mem::align_of::<TaskEnvelope>(), 64);
        assert_eq!(core::mem::size_of::<CacheAlignedSequence>(), 64);
    }

    #[test]
    fn bounded_queue_reports_full_and_empty() {
        let mut ring = EngineRingBuffer::<2>::new();
        let (mut producer, mut consumer) = ring.split();
        assert_eq!(consumer.try_dequeue(), None);
        assert_eq!(producer.try_enqueue(event(1)), Ok(()));
        assert_eq!(producer.try_enqueue(event(2)), Ok(()));
        assert_eq!(producer.try_enqueue(event(3)), Err(event(3)));
        assert_eq!(consumer.try_dequeue(), Some(event(1)));
        assert_eq!(producer.try_enqueue(event(3)), Ok(()));
        assert_eq!(consumer.try_dequeue(), Some(event(2)));
        assert_eq!(consumer.try_dequeue(), Some(event(3)));
        assert_eq!(consumer.try_dequeue(), None);
    }

    #[test]
    fn transfers_events_across_threads_without_loss_or_reordering() {
        const COUNT: u64 = 50_000;
        let mut ring = EngineRingBuffer::<256>::new();
        let (mut producer, mut consumer) = ring.split();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                for id in 1..=COUNT {
                    let mut pending = event(id);
                    loop {
                        match producer.try_enqueue(pending) {
                            Ok(()) => break,
                            Err(returned) => {
                                pending = returned;
                                std::hint::spin_loop();
                            }
                        }
                    }
                }
            });
            scope.spawn(move || {
                for expected in 1..=COUNT {
                    loop {
                        if let Some(item) = consumer.try_dequeue() {
                            assert_eq!(item.paper_id, expected);
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
            });
        });
    }
}
