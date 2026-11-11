//! The kernel's safety governor: panic airbag, render preemption, memory cap.
//!
//! None of these types know anything about pixels. They exist so a misbehaving
//! cartridge or an out-of-memory request can never take down the kernel.

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::types::{EvalContext, NodeError, NodeTrait, Packet};

/// Monotonic counter identifying the render the viewer currently wants.
///
/// Every in-flight render carries the epoch it started with. When the viewer
/// requests a newer frame the epoch is bumped, and renders whose epoch is no
/// longer current can be abandoned before they waste GPU time.
#[derive(Debug, Clone, Default)]
pub struct RenderEpoch {
    epoch: Arc<AtomicU64>,
}

impl RenderEpoch {
    /// Creates an epoch at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Advances the epoch and returns the new value.
    pub fn bump(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Returns the epoch renders should currently be working on.
    pub fn current(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Returns `true` if a render started at `epoch` is still wanted.
    pub fn is_current(&self, epoch: u64) -> bool {
        self.current() == epoch
    }
}

/// A hard cap on how many bytes the kernel may have allocated at once.
///
/// The governor is shared by every cartridge; reservations are atomic so
/// concurrent evaluations cannot jointly overshoot the limit.
#[derive(Debug, Clone)]
pub struct MemoryGovernor {
    allocated_bytes: Arc<AtomicUsize>,
    max_bytes: usize,
}

impl MemoryGovernor {
    /// Creates a governor that permits at most `max_bytes` outstanding bytes.
    pub fn new(max_bytes: usize) -> Self {
        Self {
            allocated_bytes: Arc::new(AtomicUsize::new(0)),
            max_bytes,
        }
    }

    /// Bytes currently reserved by live [`MemoryPermit`]s.
    pub fn allocated_bytes(&self) -> usize {
        self.allocated_bytes.load(Ordering::Acquire)
    }

    /// The configured cap.
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Bytes still available for reservation.
    pub fn available_bytes(&self) -> usize {
        self.max_bytes.saturating_sub(self.allocated_bytes())
    }

    /// Atomically reserves `bytes`, or fails if that would exceed the cap.
    ///
    /// The reservation is released automatically when the returned permit is
    /// dropped.
    pub fn request_permit(&self, bytes: usize) -> Result<MemoryPermit, NodeError> {
        let mut current = self.allocated_bytes.load(Ordering::Acquire);

        loop {
            let available = self.max_bytes.saturating_sub(current);
            let Some(next) = current.checked_add(bytes) else {
                return Err(NodeError::MemoryExceeded {
                    requested: bytes,
                    available,
                });
            };

            if next > self.max_bytes {
                return Err(NodeError::MemoryExceeded {
                    requested: bytes,
                    available,
                });
            }

            match self.allocated_bytes.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(MemoryPermit {
                        allocated_bytes: Arc::clone(&self.allocated_bytes),
                        bytes,
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }
}

/// An RAII reservation of `bytes` in a [`MemoryGovernor`].
///
/// Dropping the permit returns the bytes to the pool.
#[derive(Debug)]
#[must_use = "the reservation is released as soon as the permit is dropped"]
pub struct MemoryPermit {
    allocated_bytes: Arc<AtomicUsize>,
    bytes: usize,
}

impl MemoryPermit {
    /// How many bytes this permit reserves.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Drop for MemoryPermit {
    fn drop(&mut self) {
        self.allocated_bytes.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// Evaluates a cartridge with the kernel's panic airbag engaged.
///
/// A panicking node yields [`NodeError::Panic`] instead of unwinding into the
/// kernel thread. This is the only way the kernel should call `evaluate`.
pub fn evaluate_with_airbag(
    node: &dyn NodeTrait,
    ctx: &EvalContext,
    inputs: &[Packet],
) -> Result<Packet, NodeError> {
    match panic::catch_unwind(AssertUnwindSafe(|| node.evaluate(ctx, inputs))) {
        Ok(result) => result,
        Err(payload) => Err(NodeError::Panic(panic_payload_message(payload))),
    }
}

/// Turns a `catch_unwind` payload into readable text.
fn panic_payload_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}
