# 0006 — Rust inside cFS (Architecture B)

**Status:** closed. See [`spikes/rust-cfs-app`](../../spikes/rust-cfs-app).

> **Superseded in one detail by [0007](0007-vehicle-dynamics-in-cfe.md).** This
> finding reports the application's telemetry being observed on the wire at MID
> `0x0890` and treats that as confirmation the packet was correctly formed. The
> packet was correctly formed. It reached the ground for an unrelated reason:
> `0x0890` is `MD_HK_TLM_MID` and is already in `to_lab`'s subscription table,
> so the observation confirmed less than it appeared to. 0007 §2 has the full
> account and the corrected IDs. Everything else below stands, and the
> application described here has since grown a vehicle-dynamics model — the
> structure, the bindgen results and the panic finding are unchanged.

**Verdict: viable with constraints.** A cFE ES application written entirely in
Rust builds, loads, registers with EVS, creates a Software Bus pipe, and
publishes correctly-formed telemetry from inside a live cFS v7.0.1 container —
observed on the wire by the existing `tlm-capture` tool (MID `0x0890`, ~1 Hz,
20-octet packets, zero drops over repeated runs). Nothing about "Rust talking
to cFE" was the hard part. The hard part, and the specific blocker this
verdict is conditioned on: **a Rust app that has ever caught a panic on its
run-loop thread must never call `CFE_ES_ExitApp` again on that thread** — doing
so crashes the whole `core-cpu1` process, taking every other app down with it,
not just the one that panicked. See §4.

## 1. What was built

`spikes/rust-cfs-app` is a `cdylib`, structurally a line-for-line port of
`sample_app.c`: `CFE_EVS_Register`, `CFE_SB_CreatePipe`, a
`CFE_ES_RunLoop`/`CFE_SB_ReceiveBuffer` loop, `CFE_MSG_Init` +
`CFE_SB_TimeStampMsg` + `CFE_SB_TransmitMsg` for a housekeeping packet. It is
loaded by `cfe_es_startup.scr` exactly like a C app:

```
CFE_APP,  rust_app,     RUST_APP_Main,      RUST_APP,     55,   32768, 0x0, 0;
```

No CMake integration exists or was needed. `add_cfe_app` links C apps against
`core_api`, a headers-only CMake `INTERFACE` target — every `CFE_*` symbol an
app calls is left **undefined** in that app's `.so` and resolved at `dlopen()`
time against `core-cpu1`'s own exported symbol table (the executable is built
with `ENABLE_EXPORTS`/`-rdynamic` specifically because the app list is
non-empty; see `cfe/cmake/target/CMakeLists.txt`). A Rust `cdylib` with the
same undefined symbols slots into that mechanism identically to a C `.so` — it
only has to exist at the right path with the right name. `docker/Dockerfile`
builds it in the same container stage as cFS itself (rustup install, then
`cargo build --release`), copies the resulting `.so` to `cf/rust_app.so`, and
patches the startup script with `sed`. See that file for the full recipe.

## 2. bindgen against cFE's real headers

`build.rs` runs bindgen against the exact header set `add_cfe_app` gives every
C app — lifted from `sample_app`'s own `flags.make`, not guessed. This worked,
with three real gaps, all now understood precisely enough to route around:

**Function-like macros are invisible to bindgen**, because they are gone
before bindgen's clang parses a single function signature.
`CFE_ES_PerfLogEntry(id)` / `CFE_ES_PerfLogExit(id)` (`cfe_es.h`) and
`CFE_MSG_PTR(shdr)` (`default_cfe_msg_hdr_pri.h`) are the two this spike
needed. Both are reimplemented by hand in `src/lib.rs`, from the macro
definitions read directly out of the pinned headers — `CFE_MSG_PTR` as a
pointer cast (valid because `Msg` is the first field of
`CFE_MSG_TelemetryHeader_t`, so the two pointers share an address), the perf
macros as direct calls to the real function underneath
(`CFE_ES_PerfLogAdd(id, 0|1)`, which bindgen *does* see).

**Some object-like macros are silently dropped, not just skipped.**
`#define CFE_SB_TIME_OUT ((CFE_Status_t)0xca000001)` (`cfe_error.h`) is a real
macro in the translation unit bindgen parsed — confirmed by grepping the
generated `bindings.rs`: the doc comments that reference `CFE_SB_TIME_OUT`
are there, the constant itself is not, and bindgen gives no error or warning
about it. bindgen's macro-to-constant folding uses its own small expression
evaluator (not clang's), and a cast to a project typedef is exactly the shape
it gives up on quietly. Worked around by hand-defining the one constant this
spike needs, with a comment pointing at this finding rather than a bare
magic number.

**`prepend_enum_name` doubles cFE's own naming convention.** cFE's C enums
already carry the enum name as a manual prefix on every variant
(`enum CFE_ES_RunStatus { CFE_ES_RunStatus_APP_RUN = 1, ... }`), a portability
convention older than bindgen. bindgen's default (`prepend_enum_name(true)`)
prepends the Rust type name *again*, producing
`CFE_ES_RunStatus_CFE_ES_RunStatus_APP_RUN`. Not a bindgen bug so much as two
reasonable conventions compounding; `.prepend_enum_name(false)` in `build.rs`
turns it off and the names come out exactly as cFE spells them.

Nothing else needed correcting. Struct layouts (`CFE_MSG_TelemetryHeader_t`,
`CFE_SB_MsgId_t`, `CFE_SB_Buffer_t`, ...) came out right on the first try,
verified by dumping the actual header bytes cFE wrote after `CFE_MSG_Init`
(`08 90 C0 00 00 0D 00 00` — big-endian StreamId `0x0890`, standalone-packet
sequence flags, length field `13` = `sizeof(HkTlm) - 7`, all correct) and
independently by capturing the packet off the wire with `tlm-capture`.
`CFE_MSG_TelemetryHeader_t`'s definition in particular is reached through an
absolute-path `#include` baked into a generated header
(`build-native_std/inc/cfe_msg_hdr.h` → literal path into
`cfe/modules/msg/option_inc/default_cfe_msg_hdr_pri.h`) rather than resolved
through a search path — bindgen and the real GCC build read the *same file*,
which is presumably exactly why cFS generates it that way, and it closes off
an entire class of "bindgen picked a different header variant" bug before it
can happen.

One red herring worth naming so it doesn't cost the next person an hour: cFE
logs `"No subscribers for MsgId 0x808,sender RUST_APP"` for our packet, and
`0x0890 != 0x0808`. This looks like a wire-format bug and isn't one —
`cfe_sb_priv.c` builds that string from `TxnPtr->RoutingMsgId`, an internal
reduced routing-table key, and mislabels it "MsgId" in the event text. The
real MsgId on the wire, independently confirmed twice above, is correct.

## 3. Allocation, threading, table services

**Allocation.** Rust's default global allocator (the system `malloc`/`free`)
works with no special setup — this is a native Linux POSIX target with a
normal heap, the same one the C apps already use. The "no heap after init"
rule some flight software follows is a project convention enforced by review,
not something this target enforces mechanically; this spike's own code only
allocates during `init()` (the `AppState`/`HkTlm` construction), matching that
convention by habit rather than by any constraint bindgen or cFE imposed. A
real RTOS/VxWorks target would need to answer this question again from
scratch — it does not carry over from this native-Linux result.

**Threading/TLS.** `RUST_APP_Main` runs on a POSIX thread that cFE ES's
loader creates and gives a stack to *before* calling this symbol — there is no
`std::thread::spawn` anywhere in this crate, and none is needed: Rust's TLS
and panic machinery work correctly on a thread Rust's own runtime didn't
create, as `catch_unwind` demonstrably working here confirms. The one
concrete number worth carrying forward: cFE gave this app 32768 bytes of
stack (matching `sample_app`'s own allocation in the startup script) and this
spike never came close to exhausting it — but Rust codegen is not C codegen,
and a larger real app should not assume the same stack size is enough without
checking.

**Table services and event filters** were not exercised — `CFE_EVS_Register`
was called with no filters (`NULL, 0`, identical to `sample_app`'s own
trivial usage), and nothing here touches `CFE_TBL_*`. This is a real gap in
this spike, not a finding that they work: table services in particular
involve a distinct memory-sharing/CRC-validation model that a Rust struct
would need to match byte-for-byte, and that question is open.

## 4. Panics across FFI — and the crash that matters

The plan's framing — `panic = "abort"` vs. a `catch_unwind` boundary — turned
out to have a third option once actually tested, and the real risk was not
where that framing pointed.

`panic = "abort"` was not used, deliberately: it is a Cargo *profile*
setting, applied to the entire dependency graph a `cargo build` invocation
compiles, not something one crate can opt into alone. Forcing it here would
have forced an abort-on-panic strategy onto every workspace member built in
the same invocation — `apps/viz` included. `spikes/rust-cfs-app` is its own
standalone workspace precisely so this and the CFS_SRC_DIR/CFS_BUILD_DIR
build-time env vars don't leak into the rest of the repo (see its
`Cargo.toml`), so a separate profile was available if needed, and turned out
not to be.

It also turned out to be less necessary than the plan assumed. Since Rust
1.71, an unwind that reaches an `extern "C"` function boundary without an
`-unwind` ABI is *already* caught by the Rust runtime and turned into a clean
`abort()` — not the undefined behavior it used to be. `RUST_APP_Main` panicking
with no `catch_unwind` at all would not corrupt anything; it would simply take
the whole process down cleanly. `catch_unwind` is not covering undefined
behavior, then — it is trading an unconditional process-wide abort for the
chance to fail *one app* instead, which matters a great deal in this
architecture specifically: every cFS app here is `dlopen`'d into the same
address space as every other app and as `core-cpu1` itself, so one panicking
Rust app has the blast radius of the whole spacecraft's flight software
process by default. `catch_unwind` around all of `RUST_APP_Main`'s logic
converts that into an isolated, per-app failure — the same shape as
`sample_app.c`'s own "log the error, set `RunStatus = APP_ERROR`, exit this
app" pattern, just reached by a different mechanism.

That much was expected. What was not: **calling `CFE_ES_ExitApp` on a thread
that has already had a panic caught on it by `catch_unwind` crashes
`core-cpu1` outright**, with exit code 133 (`SIGTRAP`). Verified directly,
three ways, by deliberately injecting a panic into a test build and watching
the container:

1. `catch_unwind` around the panic → log the recovery via `CFE_EVS_SendEvent`
   / `CFE_ES_WriteToSysLog` (both succeed) → call `CFE_ES_ExitApp`: **the
   container exits**, code 133.
2. Same app, no panic, ordinary clean shutdown (`RunStatus = APP_EXIT` after
   N loop iterations) → call `CFE_ES_ExitApp`: **runs fine**, process stays
   up. This is the same `CFE_ES_ExitApp` call, on the same thread, with the
   same arguments — so the crash is not "calling `CFE_ES_ExitApp` from Rust,"
   full stop.
3. Same panic as (1), `catch_unwind` recovers and logs identically, but
   `CFE_ES_ExitApp` is **not** called afterward — the function just returns:
   **runs fine**, process stays up, other apps keep ticking.

The mechanism, as far as this spike could pin down without instrumenting
glibc itself: `CFE_ES_ExitApp` → OSAL's `OS_TaskExit` → glibc's
`pthread_exit`, which is not a simple thread-termination call — it is itself
implemented as a forced stack unwind (the same class of mechanism as a C++
exception or a Rust panic, at the `libgcc`/`libunwind` personality-routine
level), so that any pending `pthread_cleanup_push` handlers and C++/Rust
destructors on that thread's stack run before the thread actually dies. A
thread whose Rust unwinder has already run once via `catch_unwind` and is
then driven through a *second*, foreign (non-Rust) unwind by `pthread_exit`
hits something in that interaction the Rust or glibc unwinder does not handle
— possibly Rust's own guard against nested panics/unwinds now seeing a
foreign exception it doesn't recognize as a case it already resolved. This
spike did not go further than isolating the trigger precisely (case 1 vs. 2
vs. 3 above); doing so would mean instrumenting glibc's unwinder or Rust's
panic runtime directly, which is its own investigation.

**The workaround is case 3**, and it is what `RUST_APP_Main` ships with: after
`catch_unwind` reports a caught panic, log it and simply return from the entry
point rather than calling `CFE_ES_ExitApp`. The cost is real and stated
plainly rather than papered over: cFE ES never learns this app's exit status
or that its task ended, because the normal notification path *is*
`CFE_ES_ExitApp`. Whatever ES's bookkeeping does with an app whose OS thread
has silently died — not exercised here — is the next open question this
finding hands off, and it is the one thing standing between "viable" and
"viable with constraints."

## 5. What this changes about the plan's framing

Section 8's enumeration (bindgen, panics/`unwind`, allocation, threading, table
services/perf) named the right categories but guessed wrong about which one
would bite: allocation and threading were non-events on this target, bindgen
had real but minor and fully worked-around gaps, and the panic question had a
real, sharp answer — but the shape of that answer (a `pthread_exit`/Rust-unwind
interaction discovered only by deliberately crashing the container three
different ways) was not one the plan anticipated, because it required a
process-level app-exit path, not just a bare panic, to surface.
