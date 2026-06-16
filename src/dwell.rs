//! Adaptive-dwell controller.
//!
//! Decides how long to dwell on each channel based on whether a
//! signal has been observed during the current hop. Shared by every
//! hopping backend (`sdr-usrp-rs`, `sdr-aaronia-rs`) so the dwell-
//! pacing logic lives in one place rather than each backend
//! reinventing it.
//!
//! Behaviour:
//!   * With no detections during the dwell, the deadline is
//!     `start + min`. Quiet channels move on as soon as the minimum
//!     elapses.
//!   * When a detection lands during the dwell, the deadline is
//!     pushed out to `detection_time + extension`, capped at
//!     `start + max`. Hot channels keep listening as long as new
//!     detections keep arriving.
//!   * Setting `max <= min` disables adaptive behaviour — the
//!     deadline is fixed at `min`.

use std::time::{Duration, Instant};

/// Per-hop deadline calculator for hopping SDR backends.
///
/// Construct with `{ min, max, extension }` and call
/// [`deadline`](Self::deadline) once per hop iteration. Pair with
/// [`crate::DwellAdvice`] to get the most-recent signal observation
/// on the current channel.
#[derive(Debug, Clone, Copy)]
pub struct DwellController {
    pub min: Duration,
    pub max: Duration,
    pub extension: Duration,
}

impl DwellController {
    /// Compute the deadline for the current hop. `latest_signal` is
    /// the most recent observation on this channel (or `None` if
    /// nothing has been seen).
    pub fn deadline(&self, start: Instant, latest_signal: Option<Instant>) -> Instant {
        let min_deadline = start + self.min;
        let max_deadline = start + self.max;
        if self.max <= self.min {
            return min_deadline;
        }
        match latest_signal {
            Some(t) if t >= start => {
                let extended = t + self.extension;
                extended.max(min_deadline).min(max_deadline)
            }
            _ => min_deadline,
        }
    }

    /// `true` when the controller will extend dwell beyond `min` on
    /// detections. `false` means every hop lasts exactly `min` ms.
    pub fn is_adaptive(&self) -> bool {
        self.max > self.min
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn returns_min_when_no_signal() {
        let ctrl = DwellController {
            min: ms(50),
            max: ms(500),
            extension: ms(80),
        };
        let start = Instant::now();
        assert_eq!(ctrl.deadline(start, None), start + ms(50));
    }

    #[test]
    fn ignores_stale_signal_from_before_hop_start() {
        let ctrl = DwellController {
            min: ms(50),
            max: ms(500),
            extension: ms(80),
        };
        let start = Instant::now();
        let stale = start - ms(100);
        assert_eq!(ctrl.deadline(start, Some(stale)), start + ms(50));
    }

    #[test]
    fn extends_on_in_hop_signal() {
        let ctrl = DwellController {
            min: ms(50),
            max: ms(500),
            extension: ms(80),
        };
        let start = Instant::now();
        let detection = start + ms(30);
        assert_eq!(ctrl.deadline(start, Some(detection)), start + ms(110));
    }

    #[test]
    fn caps_at_max() {
        let ctrl = DwellController {
            min: ms(50),
            max: ms(500),
            extension: ms(80),
        };
        let start = Instant::now();
        let detection = start + ms(450);
        assert_eq!(ctrl.deadline(start, Some(detection)), start + ms(500));
    }

    #[test]
    fn disabled_when_max_eq_min() {
        let ctrl = DwellController {
            min: ms(150),
            max: ms(150),
            extension: ms(80),
        };
        let start = Instant::now();
        assert!(!ctrl.is_adaptive());
        assert_eq!(ctrl.deadline(start, None), start + ms(150));
        assert_eq!(ctrl.deadline(start, Some(start + ms(50))), start + ms(150));
    }
}
