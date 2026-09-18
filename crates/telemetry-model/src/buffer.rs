//! Jitter buffer: turns irregular telemetry arrivals into a smooth, continuous
//! signal a renderer can sample at any frame rate.
//!
//! # The problem
//!
//! Telemetry arrives at 1-10 Hz, jittered by the network and by whenever the
//! scheduler happened to run. Rendering wants a value 60-120 times a second. The
//! naive fix — snap to the newest sample each frame — produces visible stepping,
//! and at low rates it looks like the vehicle is teleporting.
//!
//! # The approach
//!
//! Play back on a deliberate delay. Holding roughly two telemetry periods of
//! history means there is almost always a sample on *both* sides of the playback
//! instant, so every frame is an interpolation between two real measurements
//! rather than a guess past the end.
//!
//! The cost is latency, and it is bought deliberately: a viewer cannot perceive
//! 200 ms of delay in a spacecraft attitude display, but they can certainly
//! perceive stepping.
//!
//! # What this deliberately does not do
//!
//! It never extrapolates. Past the newest sample it holds the last known value
//! and reports increasing staleness, because a display that keeps smoothly
//! rotating a vehicle after telemetry stopped is inventing data — and it invents
//! it most convincingly at exactly the moment something has gone wrong. Honest
//! staleness beats plausible fiction. See [`Freshness`].

use alloc::collections::VecDeque;

use crate::{Sample, SpacecraftState};

/// Tuning for a [`JitterBuffer`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BufferConfig {
    /// How far behind the newest telemetry to play back, in seconds.
    ///
    /// Roughly two telemetry periods. Too small and playback runs off the end of
    /// the buffer between samples; too large and the display lags visibly.
    pub delay: f64,
    /// Age past the newest sample beyond which the signal is reported [`Freshness::Stale`].
    pub stale_after: f64,
    /// Maximum samples retained.
    pub capacity: usize,
    /// If telemetry time and playback time diverge by more than this, jump
    /// rather than crawl.
    ///
    /// Not hypothetical: cFE restarts its clock on a processor reset, which we
    /// observed during bring-up — the mission elapsed time went backwards by
    /// days. Without a resync the buffer would wait out the difference in real
    /// time, i.e. forever.
    pub resync_threshold: f64,
}

impl BufferConfig {
    /// Tuning for a known telemetry period, in seconds.
    ///
    /// Playback sits two periods back, which keeps a sample on both sides of the
    /// playback instant even when one is lost or arrives late. `stale_after` is
    /// five periods: long enough not to flicker on a single miss, short enough
    /// that a viewer notices a dead link quickly.
    pub fn for_rate(period_s: f64) -> Self {
        Self {
            delay: period_s * 2.0,
            stale_after: period_s * 5.0,
            ..Self::default()
        }
    }
}

impl Default for BufferConfig {
    fn default() -> Self {
        Self { delay: 0.2, stale_after: 1.0, capacity: 256, resync_threshold: 5.0 }
    }
}

/// How much to trust what [`JitterBuffer::playback`] just returned.
///
/// This is meant to be surfaced in the UI, not just logged.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Freshness {
    /// Nothing received yet.
    NoData,
    /// Samples received, but playback has not yet reached them.
    Warming,
    /// Interpolating between two real samples. The normal state.
    Live,
    /// Past the newest sample, holding its value. `age` seconds since it.
    Holding { age: f64 },
    /// Held for longer than `stale_after`. The link is probably down.
    Stale { age: f64 },
}

impl Freshness {
    /// True when the value came from interpolating two real measurements.
    pub fn is_live(self) -> bool {
        matches!(self, Freshness::Live)
    }

    /// Seconds since the newest sample, where that is meaningful.
    pub fn age(self) -> Option<f64> {
        match self {
            Freshness::Holding { age } | Freshness::Stale { age } => Some(age),
            _ => None,
        }
    }
}

/// Counters worth showing on a link-health panel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BufferStats {
    pub inserted: u64,
    /// Samples that arrived older than one already held — UDP reordering.
    pub reordered: u64,
    /// Samples with a timestamp already present.
    pub duplicates: u64,
    /// Samples discarded because the buffer was full.
    pub evicted: u64,
    /// Playback-clock jumps.
    pub resyncs: u64,
}

/// A time-ordered window of recent telemetry, sampled on a delay.
#[derive(Debug)]
pub struct JitterBuffer {
    samples: VecDeque<Sample>,
    config: BufferConfig,
    /// Playback position, on the telemetry timescale.
    playback: Option<f64>,
    /// EWMA of the interval between consecutive samples.
    mean_interval: Option<f64>,
    stats: BufferStats,
}

impl JitterBuffer {
    pub fn new(config: BufferConfig) -> Self {
        Self {
            samples: VecDeque::with_capacity(config.capacity.min(1024)),
            config,
            playback: None,
            mean_interval: None,
            stats: BufferStats::default(),
        }
    }

    pub fn config(&self) -> BufferConfig {
        self.config
    }

    pub fn stats(&self) -> BufferStats {
        self.stats
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Estimated telemetry rate in Hz, once two samples have been seen.
    pub fn estimated_rate_hz(&self) -> Option<f64> {
        self.mean_interval.filter(|i| *i > 0.0).map(|i| 1.0 / i)
    }

    /// Insert a sample, keeping the buffer ordered by time.
    ///
    /// Out-of-order and duplicate arrivals are handled rather than assumed away:
    /// this is UDP, and `to_lab` publishes from several apps.
    pub fn insert(&mut self, sample: Sample) {
        // The overwhelmingly common case is in-order arrival, so check the back
        // first and skip the search entirely.
        let position = match self.samples.back() {
            None => 0,
            Some(last) if sample.time > last.time => self.samples.len(),
            Some(_) => {
                self.stats.reordered += 1;
                match self.samples.iter().position(|s| s.time >= sample.time) {
                    Some(i) => {
                        if self.samples[i].time == sample.time {
                            // Same instant: a retransmit, or two packets sharing a
                            // timestamp. Keep the one already held.
                            self.stats.duplicates += 1;
                            return;
                        }
                        i
                    }
                    None => self.samples.len(),
                }
            }
        };

        if let Some(prev) = self.samples.back() {
            let interval = sample.time - prev.time;
            if interval > 0.0 {
                self.mean_interval = Some(match self.mean_interval {
                    Some(m) => m * 0.9 + interval * 0.1,
                    None => interval,
                });
            }
        }

        self.samples.insert(position, sample);
        self.stats.inserted += 1;

        while self.samples.len() > self.config.capacity {
            self.samples.pop_front();
            self.stats.evicted += 1;
        }

        // Start playback `delay` behind the first sample ever seen.
        if self.playback.is_none() {
            self.playback = Some(sample.time - self.config.delay);
        }
        self.resync_if_adrift(sample.time);
    }

    /// If an arriving sample is far from playback, jump to it.
    ///
    /// Keyed on the *arriving* sample rather than the newest held one: a
    /// backwards clock jump lands at the front of the deque, so `back()` would
    /// still be the stale timeline and the resync would never fire.
    fn resync_if_adrift(&mut self, arrival: f64) {
        let Some(playback) = self.playback else { return };
        let target = arrival - self.config.delay;
        if crate::math::abs(target - playback) > self.config.resync_threshold {
            self.playback = Some(target);
            self.stats.resyncs += 1;
            // Samples from before the jump describe a different timeline and
            // would otherwise bracket the playback instant with nonsense.
            self.samples
                .retain(|s| crate::math::abs(s.time - arrival) <= self.config.resync_threshold);
        }
    }

    /// Advance playback by `dt` seconds of wall time.
    ///
    /// Telemetry time and wall time are assumed to run at the same rate; the
    /// resync handles the case where they do not.
    pub fn advance(&mut self, dt: f64) {
        if let Some(p) = self.playback.as_mut() {
            *p += dt;
        }
        // Drop history, but always keep the newest sample at or before the
        // playback instant: it is the left-hand bracket for interpolation. A
        // fixed-age rule would discard it across a telemetry gap, and playback
        // would then snap to the far side of the gap instead of sweeping across
        // it. `capacity` bounds the buffer; this only trims what is unreachable.
        if let Some(playback) = self.playback {
            while self.samples.len() > 2 && self.samples[1].time <= playback {
                self.samples.pop_front();
            }
        }
    }

    /// The state at the current playback instant.
    pub fn playback(&self) -> (SpacecraftState, Freshness) {
        let Some(playback) = self.playback else {
            return (SpacecraftState::default(), Freshness::NoData);
        };
        let (Some(first), Some(last)) = (self.samples.front(), self.samples.back()) else {
            return (SpacecraftState::default(), Freshness::NoData);
        };

        if playback <= first.time {
            // Playback has not caught up to the data yet.
            return (first.state, Freshness::Warming);
        }
        if playback >= last.time {
            let age = playback - last.time;
            let freshness = if age > self.config.stale_after {
                Freshness::Stale { age }
            } else {
                Freshness::Holding { age }
            };
            // Hold, never extrapolate.
            return (last.state, freshness);
        }

        // Find the pair bracketing the playback instant. Linear from the back:
        // playback sits near the newest samples, so this is a short walk.
        let mut before = first;
        let mut after = last;
        for pair in self.samples.as_slices().0.windows(2) {
            if pair[0].time <= playback && playback <= pair[1].time {
                before = &pair[0];
                after = &pair[1];
                break;
            }
        }
        // `as_slices().0` misses pairs spanning the ring's seam, so fall back to
        // a full scan when the windows search did not bracket the instant.
        if !(before.time <= playback && playback <= after.time) || before.time == after.time {
            let mut prev: Option<&Sample> = None;
            for s in self.samples.iter() {
                if s.time >= playback {
                    if let Some(p) = prev {
                        before = p;
                        after = s;
                    }
                    break;
                }
                prev = Some(s);
            }
        }

        let span = after.time - before.time;
        if span <= 0.0 {
            return (after.state, Freshness::Live);
        }
        let t = ((playback - before.time) / span) as f32;
        (before.state.lerp(after.state, t), Freshness::Live)
    }
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use super::*;
    use crate::{Mode, Quat};

    fn sample(time: f64, solar: f32) -> Sample {
        Sample {
            time,
            seq_count: (time * 10.0) as u16,
            state: SpacecraftState {
                attitude: Quat::IDENTITY,
                solar_array_deg: solar,
                deploy_progress: 0.0,
                wheel_rpm: [0.0; 4],
                mode: Mode::Nominal,
            },
        }
    }

    fn cfg() -> BufferConfig {
        BufferConfig { delay: 0.2, stale_after: 1.0, capacity: 16, resync_threshold: 5.0 }
    }

    #[test]
    fn reports_no_data_before_anything_arrives() {
        let b = JitterBuffer::new(cfg());
        assert_eq!(b.playback().1, Freshness::NoData);
    }

    #[test]
    fn interpolates_between_bracketing_samples() {
        let mut b = JitterBuffer::new(cfg());
        for i in 0..5 {
            b.insert(sample(10.0 + i as f64 * 0.1, i as f32 * 10.0));
        }
        // First sample at 10.0 put playback at 9.8; advance to exactly 10.15,
        // halfway between the samples at 10.1 (10 deg) and 10.2 (20 deg).
        b.advance(0.35);
        let (state, freshness) = b.playback();
        assert_eq!(freshness, Freshness::Live);
        assert!((state.solar_array_deg - 15.0).abs() < 0.01, "got {}", state.solar_array_deg);
    }

    #[test]
    fn holds_then_goes_stale_without_extrapolating() {
        let mut b = JitterBuffer::new(cfg());
        b.insert(sample(10.0, 0.0));
        b.insert(sample(10.1, 10.0));

        b.advance(0.35); // playback 10.15 -> past the newest sample at 10.1
        let (held, freshness) = b.playback();
        assert!(matches!(freshness, Freshness::Holding { .. }), "got {freshness:?}");
        assert_eq!(held.solar_array_deg, 10.0, "must hold, not extrapolate past 10 deg");

        b.advance(2.0);
        let (still_held, freshness) = b.playback();
        assert!(matches!(freshness, Freshness::Stale { .. }), "got {freshness:?}");
        assert_eq!(still_held.solar_array_deg, 10.0, "stale value must not drift");
        assert!(freshness.age().unwrap() > 1.0);
    }

    #[test]
    fn accepts_out_of_order_arrivals() {
        let mut b = JitterBuffer::new(cfg());
        b.insert(sample(10.0, 0.0));
        b.insert(sample(10.2, 20.0));
        b.insert(sample(10.1, 10.0)); // late

        assert_eq!(b.stats().reordered, 1);
        let times: Vec<f64> = b.samples.iter().map(|s| s.time).collect();
        assert_eq!(times, vec![10.0, 10.1, 10.2], "buffer must stay time-ordered");

        b.advance(0.25); // playback 10.05, between 10.0 and 10.1
        let (state, freshness) = b.playback();
        assert_eq!(freshness, Freshness::Live);
        assert!((state.solar_array_deg - 5.0).abs() < 0.01, "got {}", state.solar_array_deg);
    }

    #[test]
    fn ignores_duplicate_timestamps() {
        let mut b = JitterBuffer::new(cfg());
        b.insert(sample(10.0, 0.0));
        b.insert(sample(10.2, 20.0));
        b.insert(sample(10.0, 99.0)); // retransmit with different data

        assert_eq!(b.stats().duplicates, 1);
        assert_eq!(b.len(), 2);
        assert_eq!(b.samples[0].state.solar_array_deg, 0.0, "first arrival wins");
    }

    #[test]
    fn evicts_beyond_capacity() {
        let mut b = JitterBuffer::new(BufferConfig { capacity: 4, ..cfg() });
        for i in 0..10 {
            b.insert(sample(10.0 + i as f64 * 0.1, i as f32));
        }
        assert_eq!(b.len(), 4);
        assert_eq!(b.stats().evicted, 6);
    }

    #[test]
    fn resyncs_when_the_clock_jumps_backwards() {
        // cFE restarts its clock on a processor reset; we saw exactly this during
        // bring-up. Without a resync, playback would sit in the future forever.
        let mut b = JitterBuffer::new(cfg());
        for i in 0..5 {
            b.insert(sample(1_000_000.0 + i as f64 * 0.1, i as f32));
        }
        assert_eq!(b.stats().resyncs, 0);

        b.insert(sample(10.0, 99.0)); // clock restarted
        assert_eq!(b.stats().resyncs, 1);

        b.insert(sample(10.1, 98.0));
        b.advance(0.1);
        let (_, freshness) = b.playback();
        assert!(
            matches!(freshness, Freshness::Live | Freshness::Warming | Freshness::Holding { .. }),
            "should track the new timeline, got {freshness:?}"
        );
    }

    #[test]
    fn estimates_the_telemetry_rate() {
        let mut b = JitterBuffer::new(cfg());
        for i in 0..20 {
            b.insert(sample(10.0 + i as f64 * 0.1, i as f32));
        }
        let hz = b.estimated_rate_hz().expect("rate after 20 samples");
        assert!((hz - 10.0).abs() < 0.5, "expected ~10 Hz, got {hz}");
    }

    #[test]
    fn survives_a_gap_and_recovers_to_live() {
        let mut b = JitterBuffer::new(cfg());
        b.insert(sample(10.0, 0.0));
        b.insert(sample(10.1, 10.0));
        b.advance(0.35);
        assert!(matches!(b.playback().1, Freshness::Holding { .. }));

        // Telemetry resumes within the resync threshold.
        b.insert(sample(10.6, 60.0));
        b.insert(sample(10.7, 70.0));
        b.advance(0.4); // playback 10.15 -> 10.55, inside the gap
        let (state, freshness) = b.playback();
        // Sweeps across the gap between the last pre-gap sample (10.1, 10 deg)
        // and the first post-gap one (10.6, 60 deg) rather than snapping.
        assert_eq!(freshness, Freshness::Live, "should recover once data resumes");
        assert!((state.solar_array_deg - 55.0).abs() < 0.5, "got {}", state.solar_array_deg);
    }
}
