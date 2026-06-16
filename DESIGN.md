# Design: Common SDR Abstraction (sdr-source-rs)

This document outlines the architectural and architectural design of the `sdr-source-rs` crate, which provides the common hardware abstraction layer for the SDR detection applications drone detection system.

## 1. Introduction

`sdr-source-rs` defines a single unifying trait (`SdrSource`) and a shared set of primitives (`IqPacket`, `SourceConfig`, `DwellAdvice`, `DwellController`) that all SDR backends implement. This guarantees that the main orchestrator and the detector worker pool can process data identically, regardless of whether it originated from a USRP, HackRF, Aaronia Spectran, or an offline file.

## 2. System Architecture

### The `SdrSource` Trait

```rust
pub trait SdrSource: Send {
    fn start(
        self: Box<Self>,
        config: SourceConfig,
        advice: Arc<dyn DwellAdvice>,
    ) -> Result<SdrHandle, SdrError>;
}
```

- **Builder Pattern**: Backends are configured via their own concrete struct (e.g., `UsrpSource`, `HackRfSource`), which captures hardware-specific parameters like antenna ports or reference levels. Calling `start` consumes this builder.
- **`SdrHandle`**: The `start` function spawns the backend's capture thread(s) and returns a handle containing a bounded `crossbeam::channel::Receiver<IqPacket>` and a shutdown closure.
- **Asynchronous Execution**: All hardware I/O and capture loops run on a dedicated thread spawned by the backend, completely decoupled from the detector worker pool.

### The Standardized Packet Model

```rust
pub struct IqPacket {
    pub samples: PooledIqBuffer,  // Deref<Target = [Complex32]>
    pub center_frequency_hz: f64,
    pub sample_rate_hz: f32,
    pub overrun: bool,
}
```

- **`PooledIqBuffer`**: A smart wrapper around `Vec<Complex32>` that implements `Deref<Target = [Complex32]>`. When the packet is dropped, the inner vector is automatically returned to a `crossbeam::channel`-based recycling pool in the backend. This eliminates heap allocations in the hot capture loop, preventing hardware overflows at high sample rates (50+ MSPS). Consumers use `packet.samples` exactly like a `&[Complex32]` slice — the pooling is transparent.
- **`Complex32`**: All IQ data is normalized to single-precision complex floats (`[-1.0, 1.0)`) natively by the backend, hiding hardware-specific integer widths (e.g., HackRF 8-bit, Pluto 12-bit, USRP 12-bit).
- **Embedded Metadata**: Because SDR backends hop frequencies, every packet carries its *actual* center frequency and sample rate, avoiding race conditions between the orchestrator's state and the capture thread's state.

## 3. Hopping and Dwell Management

Channel hopping is a hardware-specific operation (e.g., USRP must recreate streamers, Pluto modifies an IIO string attribute, HackRF drops out of RX mode). However, the *timing* of those hops is standardized using the `DwellController`.

### DwellController Logic

The `DwellController` evaluates whether to remain on a channel or tune to the next one based on real-time feedback from the detector:

1. **Initial Deadline**: Set to `now + dwell_min`.
2. **Signal Feedback**: The capture thread periodically polls `advice.latest_signal_at(current_freq)`.
3. **Adaptive Extension**: If a signal was detected, the deadline is extended to `latest_signal + dwell_extension`.
4. **Hard Cap**: The deadline is never allowed to exceed `now + dwell_max`.

This adaptive approach minimizes time wasted on empty channels while ensuring active channels are monitored long enough to capture complete telemetry bursts.

## 4. Error Handling

Hardware failures, USB disconnects, and configuration errors are unified under the `SdrError` enum:
- `BadConfig`: Invalid sample rates, empty channel lists.
- `NotFound`: Hardware disconnected or URI unreachable.
- `Io` / `Backend`: Underlying library failures (e.g., `uhd` or `libusb` errors).


## 🧩 **Shared Types**

| Type | Role |
|---|---|
| `SourceConfig` | Common configuration surface: `sample_rate_hz`, `channels_hz`, `dwell_min`, `dwell_max`, `dwell_extension`. Backend-specific knobs (USRP gain, Aaronia ref level) live on the backend's own builder. |
| `DwellAdvice` | Trait the orchestrator implements so capture threads can poll the per-channel signal log. Hopping backends use this to extend dwell on hot channels; non-hopping backends accept it at the boundary and ignore it. |
| `DwellController` | Per-hop deadline calculator with adaptive extension. Shared by every hopping backend (`sdr-usrp-rs`, `sdr-aaronia-rs` HTTP and SDK). See [`src/dwell.rs`](src/dwell.rs); pair with `DwellAdvice` to get the most-recent signal observation for the current channel. |
| `SdrError` | `NotFound` / `BadConfig` / `Io` / `Backend(anyhow::Error)`. Plain `thiserror` enum — every backend funnels its own error types through it. |
| `freq_key_khz(hz) -> u64` | kHz-quantised key used by `DwellAdvice` for the per-channel log. Channel spacing for DJI / FPV is several MHz, so kHz quantisation is safe and avoids floating-point comparison hazards. |

## 🛠️ **Implementing a new backend**

1. Define a builder struct that owns the configuration (`MySource { ... }`).
2. `impl SdrSource for MySource`. In `start`:
   - Validate `config` (sample rate range, channel list non-empty if you hop, etc).
   - Open hardware / file / connection. Wrap any errors in `SdrError::NotFound`, `SdrError::Io`, or `SdrError::Backend(anyhow!)`.
   - `let (tx, receiver) = crossbeam::channel::bounded::<IqPacket>(1024);`
   - Spawn a capture thread that loops: read samples → assemble `IqPacket` → `tx.send(...)`. Honour an `AtomicBool` stop flag so `stop` can wind the thread down.
   - Return `SdrHandle { receiver, stop: Box::new(move || stop_flag.store(true, ...)) }`.

See [`sdr-usrp-rs`](https://github.com/isaacbentley/sdr-usrp-rs) for a hopping backend (the dwell
controller polls `DwellAdvice` between samples and adjusts the per-hop
deadline) and [`sdr-file-rs`](https://github.com/isaacbentley/sdr-file-rs) for a non-hopping file
replay backend.
