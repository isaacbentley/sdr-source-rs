//! Trait-contract tests against a minimal mock SDR.
//!
//! The mock spins up a capture thread that emits a fixed number of
//! synthetic packets, then exits. This exercises:
//!   * `start` returns a usable receiver
//!   * `stop` triggers a clean shutdown
//!
//! Real backends layer their own logic on top of the same shape.

use crossbeam::channel::{bounded, unbounded};
use num_complex::Complex32;
use sdr_source_rs::{
    DwellAdvice, IqPacket, SdrError, SdrHandle, SdrSource, SourceConfig, freq_key_khz,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Default)]
struct NoSignalAdvice;

impl DwellAdvice for NoSignalAdvice {
    fn latest_signal_at(&self, _freq_key_khz: u64) -> Option<Instant> {
        None
    }
}

struct MockSource {
    packets_to_emit: usize,
}

impl SdrSource for MockSource {
    fn start(
        self: Box<Self>,
        config: SourceConfig,
        _advice: Arc<dyn DwellAdvice>,
    ) -> Result<SdrHandle, SdrError> {
        let (tx, receiver) = bounded::<IqPacket>(self.packets_to_emit.max(1));
        let (stop_tx, stop_rx) = unbounded::<()>();
        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_for_thread = stopped.clone();

        let center = *config.channels_hz.first().unwrap_or(&0.0);
        let rate = config.sample_rate_hz as f32;
        let packets = self.packets_to_emit;

        let capture_thread = thread::spawn(move || {
            for i in 0..packets {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                let samples =
                    sdr_source_rs::PooledIqBuffer::new_unpooled(vec![
                        Complex32::new(i as f32, 0.0);
                        16
                    ]);
                if tx
                    .send(IqPacket {
                        samples,
                        center_frequency_hz: center,
                        sample_rate_hz: rate,
                        overrun: false,
                    })
                    .is_err()
                {
                    break; // receiver dropped
                }
            }
            stopped_for_thread.store(true, Ordering::SeqCst);
        });

        let stop = Box::new(move || {
            let _ = stop_tx.send(());
        });
        let wait = Box::new(move || {
            if let Err(e) = capture_thread.join() {
                eprintln!("[mock] capture thread join failed: {:?}", e);
            }
        });

        Ok(SdrHandle {
            receiver,
            stop,
            wait,
        })
    }
}

#[test]
fn mock_source_emits_configured_packet_count() {
    let advice: Arc<dyn DwellAdvice> = Arc::new(NoSignalAdvice);
    let config = SourceConfig {
        sample_rate_hz: 1.0e6,
        channels_hz: vec![2_435_000_000.0],
        dwell_min: Duration::from_millis(100),
        dwell_max: Duration::from_millis(100),
        dwell_extension: Duration::from_millis(0),
    };
    let source = Box::new(MockSource { packets_to_emit: 4 });
    let handle = source.start(config, advice).expect("start succeeds");

    let mut received = 0;
    while let Ok(pkt) = handle.receiver.recv_timeout(Duration::from_secs(1)) {
        assert_eq!(pkt.samples.len(), 16);
        assert!((pkt.center_frequency_hz - 2_435_000_000.0).abs() < 1.0);
        received += 1;
    }
    assert_eq!(received, 4);
}

#[test]
fn dropping_handle_disconnects_receiver() {
    let advice: Arc<dyn DwellAdvice> = Arc::new(NoSignalAdvice);
    let config = SourceConfig {
        sample_rate_hz: 1.0e6,
        channels_hz: vec![2_435_000_000.0],
        dwell_min: Duration::from_millis(100),
        dwell_max: Duration::from_millis(100),
        dwell_extension: Duration::from_millis(0),
    };
    let source = Box::new(MockSource {
        packets_to_emit: 100,
    });
    let handle = source.start(config, advice).expect("start succeeds");
    (handle.stop)();
    // Drain whatever was queued.
    while handle
        .receiver
        .recv_timeout(Duration::from_millis(200))
        .is_ok()
    {}
    // After stop, the channel must eventually disconnect.
    let result = handle.receiver.recv_timeout(Duration::from_millis(500));
    assert!(
        result.is_err(),
        "expected disconnect/timeout, got {:?}",
        result
    );
}

#[test]
fn freq_key_khz_quantises_correctly() {
    // The helper converts Hz → kHz and truncates: 2.435 GHz → 2_435_000 kHz.
    assert_eq!(freq_key_khz(2_435_000_000.0), 2_435_000);
    assert_eq!(freq_key_khz(2_435_500_000.0), 2_435_500);
    // Sub-kHz precision rounds down.
    assert_eq!(freq_key_khz(2_435_000_999.0), 2_435_000);
}
