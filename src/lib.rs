#![doc = include_str!("../README.md")]
//! Common abstraction for SDR sources.
//!
//! Every backend (USRP B210, Aaronia HTTP/SDK/file, RTSA file replay)
//! implements [`SdrSource`] and emits [`IqPacket`]s through a
//! [`SdrHandle::receiver`]. The orchestrator selects a backend at
//! runtime and consumes the same shape regardless of where the
//! samples came from.

mod dwell;
pub use dwell::DwellController;

use crossbeam::channel::{Receiver, Sender};
use num_complex::Complex32;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A buffer of IQ samples that automatically returns itself to a pool when dropped.
///
/// This eliminates heap-allocation overhead in the high-frequency capture loop. The capture
/// thread pulls an empty vector from a `crossbeam::channel`, passes it to the hardware or
/// C FFI, and wraps it in a `PooledIqBuffer`. When the orchestrator finishes processing
/// the packet, the `Drop` implementation clears the vector and sends it back to the channel.
#[derive(Debug)]
pub struct PooledIqBuffer {
    vec: Option<Vec<Complex32>>,
    recycler: Option<Sender<Vec<Complex32>>>,
}

impl PooledIqBuffer {
    /// Create a new buffer without a recycler. It will drop normally.
    pub fn new_unpooled(vec: Vec<Complex32>) -> Self {
        Self {
            vec: Some(vec),
            recycler: None,
        }
    }

    /// Create a new pooled buffer. When dropped, the vector will be returned to `recycler`.
    pub fn new_pooled(vec: Vec<Complex32>, recycler: Sender<Vec<Complex32>>) -> Self {
        Self {
            vec: Some(vec),
            recycler: Some(recycler),
        }
    }

    /// Take ownership of the inner vector, bypassing the pool.
    pub fn take_inner(mut self) -> Vec<Complex32> {
        self.vec.take().unwrap()
    }
}

impl Drop for PooledIqBuffer {
    fn drop(&mut self) {
        if let Some(mut vec) = self.vec.take() {
            if let Some(recycler) = &self.recycler {
                vec.clear();
                let _ = recycler.send(vec);
            }
        }
    }
}

impl std::ops::Deref for PooledIqBuffer {
    type Target = [Complex32];
    fn deref(&self) -> &Self::Target {
        // Invariant: `vec` is `None` only after `take_inner()` which consumes
        // `self`, so this is unreachable in safe code.
        self.vec
            .as_deref()
            .expect("PooledIqBuffer used after take_inner()")
    }
}

impl std::ops::DerefMut for PooledIqBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.vec
            .as_deref_mut()
            .expect("PooledIqBuffer used after take_inner()")
    }
}

/// One IQ batch tagged with the centre frequency the SDR was tuned to
/// at capture time.
///
/// All backends produce the same shape so the orchestrator can
/// dispatch detection uniformly. `sample_rate_hz` is `f32` rather
/// than `f64` because the downstream DSP (FFT, resampler) operates
/// in single precision — keeping the rate at the same width avoids
/// silent narrowings at the boundary.
#[derive(Debug)]
pub struct IqPacket {
    pub samples: PooledIqBuffer,
    pub center_frequency_hz: f64,
    pub sample_rate_hz: f32,
    pub overrun: bool,
}

/// Configuration shared across SDR backends.
///
/// Backend-specific options (USRP master clock, Aaronia ref level,
/// etc.) live on the backend's own builder; this struct is the
/// *common* surface every backend understands.
#[derive(Debug, Clone)]
pub struct SourceConfig {
    pub sample_rate_hz: f64,
    pub channels_hz: Vec<f64>,
    pub dwell_min: Duration,
    pub dwell_max: Duration,
    pub dwell_extension: Duration,
}

/// Read-only view of the orchestrator's per-frequency signal log,
/// polled by capture threads to drive adaptive dwell.
///
/// Implementations look up the most recent signal observation for a
/// given frequency key (kHz-bucketed, see `freq_key_khz`); capture
/// extends the dwell when a recent observation falls inside the
/// current hop window. A `None` return means no signal has ever been
/// observed on that frequency.
pub trait DwellAdvice: Send + Sync {
    fn latest_signal_at(&self, freq_key_khz: u64) -> Option<Instant>;
}

/// An SDR backend.
///
/// Implementations:
/// - own all hardware setup, channel hopping, and IQ buffer pooling
/// - emit [`IqPacket`]s through the [`SdrHandle::receiver`]
/// - shut down cleanly when [`SdrHandle::stop`] is invoked or the
///   handle is dropped
pub trait SdrSource: Send {
    fn start(
        self: Box<Self>,
        config: SourceConfig,
        advice: Arc<dyn DwellAdvice>,
    ) -> Result<SdrHandle, SdrError>;
}

/// Handle to a running SDR capture.
///
/// Calling `stop()` (or dropping every receiver clone) signals the
/// capture thread to wind down and release the hardware on its next
/// loop iteration. The capture thread is then *joined* when `wait` is
/// invoked: the orchestrator calls it after draining the channel, so
/// hardware release and final logging happen deterministically before
/// the handle goes away. Downstream consumers read [`IqPacket`]s from
/// `receiver` until it disconnects.
pub struct SdrHandle {
    pub receiver: Receiver<IqPacket>,
    /// Shutdown hook — call this to ask the capture thread to wind
    /// down. If the consumer drops the handle without calling
    /// `stop()`, the receiver disconnect on the next iteration
    /// triggers the same effect.
    pub stop: Box<dyn FnOnce() + Send>,
    /// Join hook — call this to block until the capture thread has
    /// fully exited (releasing the hardware and flushing its final
    /// logs). Must be called *after* every receiver clone has been
    /// dropped: while a receiver is alive the capture thread may be
    /// parked on `tx.send`, and joining it then would deadlock. A
    /// panic in the capture thread is logged here, not propagated.
    pub wait: Box<dyn FnOnce() + Send>,
}

/// Errors any SDR backend may surface.
#[derive(Debug, thiserror::Error)]
pub enum SdrError {
    #[error("hardware not found: {0}")]
    NotFound(String),
    #[error("configuration rejected: {0}")]
    BadConfig(String),
    #[error("I/O error during capture: {0}")]
    Io(String),
    #[error("backend error: {0}")]
    Backend(#[from] anyhow::Error),
}

/// A frequency bucket key for [`DwellAdvice`]: kHz-quantised Hz.
///
/// Channel spacing for DJI / FPV is several MHz, so a kHz quantum is
/// safe (no two real channels collide). Bucketing avoids floating-
/// point comparison hazards in the map lookup.
#[inline]
pub fn freq_key_khz(center_frequency_hz: f64) -> u64 {
    (center_frequency_hz * 1e-3) as u64
}
