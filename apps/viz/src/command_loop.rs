//! Proof that the loop closes, and how long it takes to close.
//!
//! Phase 4's gate asks for "a button that sends a command and shows the
//! resulting state change". A button that sends a datagram is easy; showing
//! that the datagram *did* something requires watching a value in the downlink
//! change, and being able to say why it changed.
//!
//! The value is `SAMPLE_APP_HkTlm_Payload_t::CommandCounter`. It is incremented
//! by cFE inside `sample_app`'s command handler, on the flight side, and comes
//! back to the ground on the next housekeeping cycle. Nothing on the ground can
//! move it.
//!
//! # Why this measures a round trip and not a send
//!
//! The confirmation is "the counter is no longer what it was when we pressed
//! the key", not "the counter equals what we predicted". Predicting the next
//! value assumes nobody else is commanding `sample_app`, which a ground station
//! may not assume — and the prediction breaks at the `u8` wrap. Comparing
//! against the value at press time is wrap-safe and stays true with other
//! operators on the bus.
//!
//! The number it reports is *not* network latency. `sample_app` housekeeping is
//! published on a scheduler tick — measured at **5.40 s** on the pinned build —
//! so the time is dominated by how long until the next cycle, and a uniformly
//! timed command should average about half of it. Measured over twelve round
//! trips against the container: 0.90 s to 5.52 s, mean 2.94 s, which is the
//! distribution that description predicts.
//!
//! That is the useful number for an operator — "how long until I can see that my
//! command took" — but calling it latency would be wrong by two orders of
//! magnitude, so the panel says "confirmed in", not "latency".

use bevy::prelude::Resource;
use cfs_msg::hk::SampleAppHk;

/// How long to wait for a counter change before calling the command lost.
///
/// Nearly three of the pinned build's 5.40 s housekeeping cycles, so a timeout
/// means something actually failed rather than that a cycle was missed.
pub const TIMEOUT_S: f64 = 15.0;

#[derive(Clone, Copy, Debug)]
struct Pending {
    /// Counter value at the moment the command went out.
    before: u8,
    sent_at: f64,
}

/// Outcome of one round trip, for display.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Outcome {
    Confirmed { seconds: f64 },
    TimedOut,
}

/// Tracks one in-flight command at a time.
///
/// One at a time on purpose: with several outstanding, a single counter change
/// cannot be attributed to a particular command, and a measurement that cannot
/// be attributed is not a measurement. A second press while one is pending
/// still *sends* — refusing to command would be worse — it simply does not
/// start a second measurement.
#[derive(Resource, Debug, Default)]
pub struct CommandLoop {
    pending: Option<Pending>,
    pub sent: u32,
    pub confirmed: u32,
    pub timed_out: u32,
    pub last: Option<Outcome>,
}

impl CommandLoop {
    /// Record that a command has just gone out.
    pub fn on_sent(&mut self, hk: Option<SampleAppHk>, now: f64) {
        self.sent += 1;
        if self.pending.is_some() {
            return;
        }
        // With no housekeeping yet there is no baseline to compare against, so
        // there is nothing to measure — the command still went out.
        if let Some(hk) = hk {
            self.pending = Some(Pending { before: hk.command_counter, sent_at: now });
        }
    }

    /// Feed the latest housekeeping. Call every frame; it is cheap and idempotent.
    pub fn observe(&mut self, hk: Option<SampleAppHk>, now: f64) {
        let Some(pending) = self.pending else { return };

        if let Some(hk) = hk
            && hk.command_counter != pending.before
        {
            self.pending = None;
            self.confirmed += 1;
            self.last = Some(Outcome::Confirmed { seconds: now - pending.sent_at });
            return;
        }

        if now - pending.sent_at > TIMEOUT_S {
            self.pending = None;
            self.timed_out += 1;
            self.last = Some(Outcome::TimedOut);
        }
    }

    pub fn awaiting(&self) -> bool {
        self.pending.is_some()
    }

    /// One line for the panel.
    pub fn summary(&self) -> String {
        let state = match (self.awaiting(), self.last) {
            (true, _) => "awaiting counter change...".to_string(),
            (false, Some(Outcome::Confirmed { seconds })) => {
                format!("confirmed in {seconds:.2}s")
            }
            (false, Some(Outcome::TimedOut)) => {
                format!("TIMED OUT after {TIMEOUT_S:.0}s - command never reached sample_app")
            }
            (false, None) => "no command sent yet".to_string(),
        };
        format!("sent {}  confirmed {}  lost {}  |  {state}", self.sent, self.confirmed, self.timed_out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hk(command_counter: u8) -> Option<SampleAppHk> {
        Some(SampleAppHk { command_counter, command_error_counter: 0 })
    }

    #[test]
    fn a_counter_change_confirms_and_times_the_round_trip() {
        let mut l = CommandLoop::default();
        l.on_sent(hk(3), 10.0);
        // Housekeeping still carrying the old value: not confirmation.
        l.observe(hk(3), 11.0);
        assert!(l.awaiting());
        assert_eq!(l.confirmed, 0);

        l.observe(hk(4), 13.5);
        assert!(!l.awaiting());
        assert_eq!(l.last, Some(Outcome::Confirmed { seconds: 3.5 }));
        assert_eq!(l.confirmed, 1);
    }

    #[test]
    fn silence_eventually_reports_the_command_lost() {
        let mut l = CommandLoop::default();
        l.on_sent(hk(0), 0.0);
        l.observe(hk(0), TIMEOUT_S - 0.1);
        assert!(l.awaiting(), "gave up before the timeout");
        l.observe(hk(0), TIMEOUT_S + 0.1);
        assert_eq!(l.last, Some(Outcome::TimedOut));
        assert_eq!(l.timed_out, 1);
    }

    /// cFE's counter is a `u8`. A scheme that predicted "before + 1" would work
    /// for 255 commands and then quietly stop confirming.
    #[test]
    fn confirmation_survives_the_counter_wrapping() {
        let mut l = CommandLoop::default();
        l.on_sent(hk(255), 1.0);
        l.observe(hk(0), 2.0);
        assert_eq!(l.confirmed, 1);
    }

    /// A reset command drives the counter *down*. That is still a change caused
    /// by our command, and must confirm.
    #[test]
    fn a_counter_that_drops_still_confirms() {
        let mut l = CommandLoop::default();
        l.on_sent(hk(9), 0.0);
        l.observe(hk(0), 1.0);
        assert_eq!(l.confirmed, 1);
    }

    #[test]
    fn a_second_press_sends_but_does_not_start_a_second_measurement() {
        let mut l = CommandLoop::default();
        l.on_sent(hk(1), 0.0);
        l.on_sent(hk(1), 0.5);
        assert_eq!(l.sent, 2);
        l.observe(hk(2), 1.0);
        // Timed from the first press, not the second.
        assert_eq!(l.last, Some(Outcome::Confirmed { seconds: 1.0 }));
        assert_eq!(l.confirmed, 1);
    }

    /// Commanding before any housekeeping has arrived must not fabricate a
    /// baseline of zero, which would confirm on the first real packet.
    #[test]
    fn commanding_before_the_first_housekeeping_measures_nothing() {
        let mut l = CommandLoop::default();
        l.on_sent(None, 0.0);
        assert_eq!(l.sent, 1);
        assert!(!l.awaiting());
        l.observe(hk(7), 1.0);
        assert_eq!(l.confirmed, 0);
        assert_eq!(l.last, None);
    }
}
