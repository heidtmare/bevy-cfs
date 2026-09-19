# Findings

One file per decision gate. The point of a spike is the written answer, not the
code, so a phase is not finished until its finding is here.

Format: what was asked, what was measured, what was decided, what is still open.
Negative results are first-class — "Rust inside cFS is not worth it because X"
is a successful outcome of Phase 5.

| # | Question | Verdict |
|---|---|---|
| [0001](0001-verification-backlog.md) | What have we assumed but not checked? | Closed; every item confirmed against the real build |
| [0002](0002-cfs-bring-up.md) | Can cFS v7.0.1 be brought up reproducibly? | Yes, in a container, with three non-obvious gotchas |
| [0003](0003-telemetry-to-animation.md) | Can a 1-4 Hz downlink drive a 60 Hz render? | Yes — jitter buffer, interpolate, never extrapolate |
| [0004](0004-animation-mappings.md) | Which Bevy mechanism should drive which signal? | A signal-type → mechanism table, measured three ways |
| [0005](0005-vertical-slice.md) | Does the whole path work against live cFS? | Yes, including a command round trip. Stock cFS publishes **no** vehicle dynamics |
| [0006](0006-rust-cfs-app.md) | Can a cFE application be written in Rust? | Viable with constraints; one sharp `pthread_exit` blocker |
| [0007](0007-vehicle-dynamics-in-cfe.md) | Can the vehicle-dynamics gap be closed from the flight side? | Yes, sharing one `no_std` crate. Message-ID allocation is the real obstacle |

A finding is superseded only in writing: if a later one corrects an earlier
one, the earlier gets a note at the top pointing at it rather than an edit. 0006
carries one from 0007.
