//! Phase 5 (Architecture B): a cFE ES application written in Rust, flying a
//! spacecraft.
//!
//! cFE's ES loader `dlopen()`s this `.so` exactly like a C app module (see
//! `add_cfe_app` in the bundle's CMake — apps link only against `core_api`,
//! an interface target with no library body, so every `CFE_*` symbol here is
//! left undefined at link time and resolved at load time against the
//! `core-cpu1` executable's own exported symbol table). `RUST_APP_Main` is
//! this app's entry point, named in the startup-script line `docker/Dockerfile`
//! `sed`s into `cfe_es_startup.scr` at image-build time.
//!
//! # What it does
//!
//! It runs the vehicle. `crates/vehicle-dyn` — a `no_std` crate from the main
//! workspace, compiled into this `.so` — integrates rigid-body attitude
//! dynamics, allocates torque across four reaction wheels, sequences a
//! pointing survey and runs the array deployment. Every 100 ms this app steps
//! that model and publishes the result on the software bus as
//! [`cfs_msg::rust_app::VEHICLE_TLM_MID`], encoded by
//! `telemetry_model::encode_vehicle_state` — the *same function* `apps/viz`
//! calls to decode it.
//!
//! That sharing is the finding, not the spacecraft. Before this, `apps/viz`
//! showed `-- no source --` for attitude and wheel speeds because stock cFS
//! publishes no vehicle dynamics at all (finding 0005). Now it shows a real
//! cFE field for every one of them, and the field is produced by Rust running
//! inside cFE. There is exactly one definition of the wire format in the
//! repository and both ends of the link are compiled from it.
//!
//! # Structure
//!
//! The original version of this spike was a line-for-line port of
//! `sample_app.c`, kept that way so the C-to-Rust comparison was legible.
//! That comparison is recorded in `docs/findings/0006-rust-cfs-app.md` and the
//! shape survives here: `CFE_EVS_Register`, `CFE_SB_CreatePipe`, a
//! `CFE_ES_RunLoop`/`CFE_SB_ReceiveBuffer` loop, `CFE_MSG_Init` +
//! `CFE_SB_TimeStampMsg` + `CFE_SB_TransmitMsg`. What is new is that the loop
//! now does arithmetic between receiving and transmitting.
//!
//! # `no_std` dependencies in a `std` crate
//!
//! This crate itself is `std` — it needs `catch_unwind`, and the target is
//! hosted Linux. Its four workspace dependencies are taken with
//! `default-features = false`, so they are compiled `no_std` here exactly as
//! they would be for a real flight target. See `Cargo.toml`.

#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case)]

#[allow(unused_imports)] // bindgen emits a handful of unused re-exports (e.g. CFE_TIME_Compare_t) that we don't own.
mod bindings {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

use bindings::{
    CFE_EVS_EventFilter_BINARY, CFE_EVS_EventType_ERROR, CFE_EVS_EventType_INFORMATION,
    CFE_EVS_Register, CFE_EVS_SendEvent, CFE_ES_ExitApp, CFE_ES_PerfLogAdd, CFE_ES_RunLoop,
    CFE_ES_RunStatus_APP_ERROR, CFE_ES_RunStatus_APP_RUN, CFE_ES_WriteToSysLog, CFE_MSG_GetFcnCode,
    CFE_MSG_Init, CFE_MSG_Message_t, CFE_MSG_Size_t, CFE_MSG_TelemetryHeader_t, CFE_SB_Buffer_t,
    CFE_SB_CreatePipe, CFE_SB_MsgId_t, CFE_SB_PipeId_t, CFE_SB_ReceiveBuffer, CFE_SB_Subscribe,
    CFE_SB_TimeStampMsg, CFE_SB_TransmitMsg, CFE_Status_t, CFE_TIME_GetTime, CFE_TIME_SysTime_t,
};
use std::panic::{AssertUnwindSafe, catch_unwind};

use cfs_msg::rust_app::{self, RustAppHk};
use telemetry_model::{VEHICLE_PAYLOAD_LEN, VEHICLE_PAYLOAD_OFFSET, encode_vehicle_state};
use vehicle_dyn::{Command, Vehicle};

// `#define CFE_SB_TIME_OUT ((CFE_Status_t)0xca000001)` (cfe_error.h) is a
// real object-like macro in the translation unit bindgen parsed, but bindgen
// silently drops any macro constant it cannot fold with its own (non-clang)
// expression evaluator — casts to project typedefs like `CFE_Status_t` are
// exactly the case it gives up on, with no error to say so. Confirmed by
// grepping the generated bindings.rs: the doc comments that mention
// `CFE_SB_TIME_OUT` are there, the constant itself is not. See finding 0006.
const CFE_SB_TIME_OUT: CFE_Status_t = 0xca000001_u32 as CFE_Status_t;

// `CFE_ES_PerfLogEntry`/`Exit` are function-like macros in cfe_es.h
// (`#define CFE_ES_PerfLogEntry(id) (CFE_ES_PerfLogAdd(id, 0))`) — invisible
// to bindgen, which only sees the real `CFE_ES_PerfLogAdd` function. Macros
// like this one and `CFE_MSG_PTR` (below) are the actual bindgen gap; see
// finding 0006.
const RUST_APP_PERF_ID: u32 = 500; // Arbitrary — no mission perf-ID table entry exists for this spike.
unsafe fn perf_log_entry(id: u32) {
    unsafe { CFE_ES_PerfLogAdd(id, 0) };
}
unsafe fn perf_log_exit(id: u32) {
    unsafe { CFE_ES_PerfLogAdd(id, 1) };
}

// `CFE_MSG_PTR(shdr)` is `#define CFE_MSG_PTR(shdr) (&((shdr).Msg))`. `Msg` is
// the first field of `CFE_MSG_TelemetryHeader_t`, so a pointer to the whole
// header and a pointer to its `Msg` field share an address — this cast is
// exactly what the macro does, just spelled out.
unsafe fn msg_ptr(hdr: *mut CFE_MSG_TelemetryHeader_t) -> *mut CFE_MSG_Message_t {
    hdr.cast()
}

// ------------------------------------------------------------- event IDs ---

const RUST_APP_STARTUP_EID: u16 = 1;
const RUST_APP_PIPE_ERR_EID: u16 = 2;
const RUST_APP_PANIC_EID: u16 = 3;
const RUST_APP_CMD_EID: u16 = 4;
const RUST_APP_CMD_ERR_EID: u16 = 5;

// ---------------------------------------------------------------- timing ---

/// Control period, milliseconds. 10 Hz: fast enough that the integration is
/// accurate and the downlink drives a 60 Hz render without visible stepping,
/// slow enough to be an unremarkable load on the software bus.
const CONTROL_PERIOD_MS: i32 = 100;

/// Publish housekeeping every this many control cycles — 1 Hz, the rate the
/// rest of the bundle's housekeeping runs at.
const HK_EVERY_CYCLES: u32 = 10;

/// Upper bound on a single integration step, seconds.
///
/// `dt` is *measured*, not assumed, so that a late loop integrates the time it
/// actually lost rather than the time it was scheduled for. But a measured
/// `dt` has a failure mode an assumed one does not: if the task is starved, or
/// the mission clock is stepped by a time command, one step arrives with a
/// huge `dt` and the integrator takes a single enormous leap. Clamping is the
/// standard defence, and it is honest in the direction that matters — the
/// vehicle falls behind real time rather than teleporting.
const MAX_STEP_S: f32 = 0.5;

/// cFE subseconds are 2⁻³² of a second.
const SUBSECOND_SCALE: f64 = 1.0 / 4_294_967_296.0;

fn as_secs(t: CFE_TIME_SysTime_t) -> f64 {
    t.Seconds as f64 + t.Subseconds as f64 * SUBSECOND_SCALE
}

// -------------------------------------------------------------- messages ---

/// Vehicle-state telemetry.
///
/// The payload is a byte array rather than a struct of fields, deliberately.
/// `telemetry_model::encode_vehicle_state` already defines the layout octet by
/// octet and is the definition both ends share; re-declaring those fields as a
/// `#[repr(C)]` struct here would create a second layout that agrees with the
/// first only by inspection, and C struct padding is precisely where that kind
/// of agreement breaks (finding 0005's four-octet offset bug was exactly this
/// class of mistake).
#[repr(C)]
struct VehicleTlm {
    telemetry_header: CFE_MSG_TelemetryHeader_t,
    payload: [u8; VEHICLE_PAYLOAD_LEN],
}

/// Wire size of [`VehicleTlm`], excluding any tail padding `size_of` would add.
///
/// Passed to `CFE_MSG_Init` instead of `size_of::<VehicleTlm>()` so the packet
/// on the wire is exactly the octets that mean something.
const VEHICLE_TLM_LEN: usize = VEHICLE_PAYLOAD_OFFSET + VEHICLE_PAYLOAD_LEN;

/// Housekeeping telemetry: this application's own counters.
#[repr(C)]
struct HkTlm {
    telemetry_header: CFE_MSG_TelemetryHeader_t,
    payload: [u8; RustAppHk::LEN],
}

const HK_TLM_LEN: usize = VEHICLE_PAYLOAD_OFFSET + RustAppHk::LEN;

// The ground decoder finds the payload at octet 16 because
// `CFE_MSG_TelemetryHeader_t` carries a four-octet alignment spare after its
// six-octet timestamp. That is a property of *this build's* header, read by
// bindgen from the same file the C compiler reads. If a future cFE changes it,
// the app fails to compile here rather than publishing packets the ground
// silently misparses — which is what happened the last time this offset was
// wrong (finding 0005).
const _: () = assert!(
    std::mem::offset_of!(VehicleTlm, payload) == VEHICLE_PAYLOAD_OFFSET,
    "cFE telemetry header is not the 16 octets the ground decoder assumes"
);
const _: () = assert!(std::mem::offset_of!(HkTlm, payload) == VEHICLE_PAYLOAD_OFFSET);

// ----------------------------------------------------------------- state ---

struct AppState {
    run_status: u32,
    command_pipe: CFE_SB_PipeId_t,
    vehicle_tlm: VehicleTlm,
    hk_tlm: HkTlm,
    /// The spacecraft.
    vehicle: Vehicle,
    hk: RustAppHk,
    /// Mission time at the previous control cycle, for the measured `dt`.
    last_time_s: f64,
    cycles: u32,
}

unsafe fn init() -> Result<Box<AppState>, ()> {
    let evs_status =
        unsafe { CFE_EVS_Register(std::ptr::null(), 0, CFE_EVS_EventFilter_BINARY as u16) };
    if evs_status < 0 {
        unsafe {
            CFE_ES_WriteToSysLog(c"RUST_APP: Error Registering Events\n".as_ptr());
        }
        return Err(());
    }

    // Boxed rather than returned by value: `AppState` embeds two telemetry
    // buffers and the vehicle model, and cFE gives this task a 32 KiB stack
    // (finding 0006 §3). Moving a multi-hundred-byte struct out of `init` and
    // into `run` twice over is not a problem at this size, but the habit is
    // worth keeping on a stack that small.
    let mut state = Box::new(AppState {
        run_status: CFE_ES_RunStatus_APP_RUN,
        command_pipe: CFE_SB_PipeId_t::default(),
        // SAFETY: CFE_MSG_TelemetryHeader_t is a plain-old-data struct of
        // integer/byte fields (CCSDS header + spare padding); the cFE
        // convention (mirrored by every lab app) is to zero it and let
        // CFE_MSG_Init fill in the fields that matter.
        vehicle_tlm: VehicleTlm {
            telemetry_header: unsafe { std::mem::zeroed() },
            payload: [0; VEHICLE_PAYLOAD_LEN],
        },
        hk_tlm: HkTlm {
            telemetry_header: unsafe { std::mem::zeroed() },
            payload: [0; RustAppHk::LEN],
        },
        vehicle: Vehicle::new(),
        hk: RustAppHk::default(),
        last_time_s: 0.0,
        cycles: 0,
    });

    unsafe {
        CFE_MSG_Init(
            msg_ptr(&mut state.vehicle_tlm.telemetry_header),
            CFE_SB_MsgId_t { Value: rust_app::VEHICLE_TLM_MID.0 as u32 },
            VEHICLE_TLM_LEN as CFE_MSG_Size_t,
        );
        CFE_MSG_Init(
            msg_ptr(&mut state.hk_tlm.telemetry_header),
            CFE_SB_MsgId_t { Value: rust_app::HK_TLM_MID.0 as u32 },
            HK_TLM_LEN as CFE_MSG_Size_t,
        );
    }

    let sb_status =
        unsafe { CFE_SB_CreatePipe(&mut state.command_pipe, 8, c"RUST_APP_CMD_PIPE".as_ptr()) };
    if sb_status < 0 {
        unsafe {
            CFE_ES_WriteToSysLog(c"RUST_APP: Error Creating Pipe\n".as_ptr());
        }
        return Err(());
    }

    // Without this the app would publish telemetry and never hear a command —
    // `ci_lab` puts an uplinked packet on the bus, but only subscribers get it.
    let sub_status = unsafe {
        CFE_SB_Subscribe(
            CFE_SB_MsgId_t { Value: rust_app::CMD_MID.0 as u32 },
            state.command_pipe,
        )
    };
    if sub_status < 0 {
        unsafe {
            CFE_ES_WriteToSysLog(c"RUST_APP: Error Subscribing to Commands\n".as_ptr());
        }
        return Err(());
    }

    state.last_time_s = as_secs(unsafe { CFE_TIME_GetTime() });

    unsafe {
        CFE_EVS_SendEvent(
            RUST_APP_STARTUP_EID,
            CFE_EVS_EventType_INFORMATION as u16,
            c"RUST_APP: flying. vehicle state on 0x%04X at %d Hz, HK on 0x%04X, commands on 0x%04X".as_ptr(),
            rust_app::VEHICLE_TLM_MID.0 as u32,
            1000 / CONTROL_PERIOD_MS,
            rust_app::HK_TLM_MID.0 as u32,
            rust_app::CMD_MID.0 as u32,
        );
    }

    Ok(state)
}

/// Map a function code onto a vehicle command.
///
/// `None` for the codes that are not vehicle commands at all (`NOOP`,
/// `RESET_COUNTERS`) and for unknown ones; the caller distinguishes the two.
fn vehicle_command(function_code: u8) -> Option<Command> {
    Some(match function_code {
        rust_app::SAFE_CC => Command::Safe,
        rust_app::NOMINAL_CC => Command::Nominal,
        rust_app::NEXT_TARGET_CC => Command::NextTarget,
        rust_app::HOLD_CC => Command::Hold,
        rust_app::DEPLOY_CC => Command::Deploy,
        rust_app::STOW_CC => Command::Stow,
        rust_app::DUMP_MOMENTUM_CC => Command::DumpMomentum,
        _ => return None,
    })
}

/// Dispatch one received command packet.
unsafe fn handle_command(state: &mut AppState, buf: *mut CFE_SB_Buffer_t) {
    let mut function_code: bindings::CFE_MSG_FcnCode_t = 0;
    // `CFE_SB_Buffer_t` is a union whose first member is the message, so the
    // buffer pointer and the message pointer are the same address — the same
    // reasoning as `msg_ptr` above, and it avoids an unsafe union field access
    // for no benefit.
    let status = unsafe { CFE_MSG_GetFcnCode(buf.cast::<CFE_MSG_Message_t>(), &mut function_code) };
    if status < 0 {
        state.hk.command_error_counter = state.hk.command_error_counter.wrapping_add(1);
        return;
    }
    let fc = function_code as u8;

    match fc {
        rust_app::NOOP_CC => {
            state.hk.command_counter = state.hk.command_counter.wrapping_add(1);
            unsafe {
                CFE_EVS_SendEvent(
                    RUST_APP_CMD_EID,
                    CFE_EVS_EventType_INFORMATION as u16,
                    c"RUST_APP: NOOP".as_ptr(),
                );
            }
        }
        rust_app::RESET_COUNTERS_CC => {
            state.hk.command_counter = 0;
            state.hk.command_error_counter = 0;
        }
        _ => match vehicle_command(fc) {
            Some(cmd) => {
                state.vehicle.command(cmd);
                state.hk.command_counter = state.hk.command_counter.wrapping_add(1);
                unsafe {
                    CFE_EVS_SendEvent(
                        RUST_APP_CMD_EID,
                        CFE_EVS_EventType_INFORMATION as u16,
                        c"RUST_APP: vehicle command, code %d".as_ptr(),
                        fc as u32,
                    );
                }
            }
            None => {
                state.hk.command_error_counter = state.hk.command_error_counter.wrapping_add(1);
                unsafe {
                    CFE_EVS_SendEvent(
                        RUST_APP_CMD_ERR_EID,
                        CFE_EVS_EventType_ERROR as u16,
                        c"RUST_APP: unknown function code %d".as_ptr(),
                        fc as u32,
                    );
                }
            }
        },
    }
}

/// Step the vehicle and publish it.
unsafe fn publish_vehicle_state(state: &mut AppState) {
    let now = as_secs(unsafe { CFE_TIME_GetTime() });
    // `max(0.0)` as well as the upper clamp: cFE's mission time can be stepped
    // backwards by a time command, and a negative `dt` would integrate the
    // vehicle into its own past.
    let dt = ((now - state.last_time_s).max(0.0) as f32).min(MAX_STEP_S);
    state.last_time_s = now;

    state.vehicle.step(dt);
    state.cycles = state.cycles.wrapping_add(1);

    encode_vehicle_state(&state.vehicle.state(), &mut state.vehicle_tlm.payload);

    unsafe {
        CFE_SB_TimeStampMsg(msg_ptr(&mut state.vehicle_tlm.telemetry_header));
        CFE_SB_TransmitMsg(msg_ptr(&mut state.vehicle_tlm.telemetry_header), true);
    }
}

unsafe fn publish_hk(state: &mut AppState) {
    state.hk.mode = state.vehicle.mode as u8;
    state.hk.wheels_saturated = u8::from(state.vehicle.saturated);
    state.hk.control_cycles = state.cycles;
    state.hk.targets_commanded = state.vehicle.targets_commanded();
    state.hk.encode(&mut state.hk_tlm.payload);

    unsafe {
        CFE_SB_TimeStampMsg(msg_ptr(&mut state.hk_tlm.telemetry_header));
        CFE_SB_TransmitMsg(msg_ptr(&mut state.hk_tlm.telemetry_header), true);
    }
}

unsafe fn run(mut state: Box<AppState>) {
    while unsafe { CFE_ES_RunLoop(&mut state.run_status) } {
        unsafe { perf_log_exit(RUST_APP_PERF_ID) };

        let mut buf_ptr: *mut CFE_SB_Buffer_t = std::ptr::null_mut();
        // The receive timeout is this app's clock. Not `CFE_SB_PEND_FOREVER`:
        // the control loop has to run whether or not a command ever arrives,
        // and this is the housekeeping-app pattern rather than the
        // pure-command-dispatch pattern `sample_app` uses (it relies on a
        // separate HK app to poll it). The loop therefore runs slightly *faster*
        // than 10 Hz while commands are arriving, which is exactly why `dt` is
        // measured rather than assumed to be CONTROL_PERIOD_MS.
        let status =
            unsafe { CFE_SB_ReceiveBuffer(&mut buf_ptr, state.command_pipe, CONTROL_PERIOD_MS) };

        unsafe { perf_log_entry(RUST_APP_PERF_ID) };

        if status >= 0 && !buf_ptr.is_null() {
            unsafe { handle_command(&mut state, buf_ptr) };
        } else if status != CFE_SB_TIME_OUT {
            // A timeout is the normal case — it is how the control loop gets
            // its tick. Anything else is a real pipe error; `sample_app`'s
            // response to that is to log and exit the app (not the process),
            // which this mirrors.
            unsafe {
                CFE_EVS_SendEvent(
                    RUST_APP_PIPE_ERR_EID,
                    CFE_EVS_EventType_ERROR as u16,
                    c"RUST_APP: SB Pipe Read Error, App Will Exit".as_ptr(),
                );
            }
            state.run_status = CFE_ES_RunStatus_APP_ERROR;
            continue;
        }

        unsafe { publish_vehicle_state(&mut state) };
        if state.cycles % HK_EVERY_CYCLES == 0 {
            unsafe { publish_hk(&mut state) };
        }
    }

    unsafe { perf_log_exit(RUST_APP_PERF_ID) };
    unsafe { CFE_ES_ExitApp(state.run_status) };
}

/// cFE ES's entry point for this app, named in the startup script. Called on
/// its own OSAL task (a POSIX thread on this target, created and given a
/// stack by ES before `dlsym`-ing and calling this symbol — never by Rust's
/// own runtime), so there is no `std::thread::spawn` anywhere in this crate.
///
/// # Safety
/// Must only be called by the cFE ES loader as a `CFE_APP` startup-script
/// entry, exactly once, on a thread ES itself manages.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn RUST_APP_Main() {
    unsafe { perf_log_entry(RUST_APP_PERF_ID) };

    // A single catch_unwind around all of app logic, not a per-call one and
    // not `panic = "abort"`. Two reasons: (1) since Rust 1.71, an unwind that
    // reaches an `extern "C"` boundary already aborts cleanly on its own —
    // this catch is not covering undefined behavior, it is trading that
    // clean-abort for a graceful one; (2) `panic = "abort"` is a profile-wide
    // setting Cargo applies to the whole dependency graph of a build
    // invocation, and this spike is built with its own profile
    // (`release-abort` is deliberately NOT used — see finding 0006) so it
    // does not force an unwind-vs-abort choice onto `apps/viz` or anything
    // else in the workspace. The real payoff of catching here: one app
    // panicking sets `CFE_ES_RunStatus_APP_ERROR` and exits *that app*,
    // rather than aborting `core-cpu1` — which, because every app is
    // `dlopen`'d into the same address space, would otherwise take every
    // other app down with it. That single-address-space blast radius is
    // itself a Phase 5 finding, not just an implementation detail.
    let outcome = catch_unwind(AssertUnwindSafe(|| unsafe {
        match init() {
            Ok(state) => run(state),
            Err(()) => CFE_ES_ExitApp(CFE_ES_RunStatus_APP_ERROR),
        }
    }));

    if outcome.is_err() {
        unsafe {
            CFE_EVS_SendEvent(
                RUST_APP_PANIC_EID,
                CFE_EVS_EventType_ERROR as u16,
                c"RUST_APP: panicked, exiting app (not process)".as_ptr(),
            );
            CFE_ES_WriteToSysLog(c"RUST_APP: recovered from a panic via catch_unwind\n".as_ptr());
        }
        // Deliberately NOT calling CFE_ES_ExitApp here — confirmed by testing
        // (see finding 0006), calling it on this thread, after catch_unwind
        // has already caught a panic here, crashes the whole core-cpu1
        // process (SIGTRAP). CFE_ES_ExitApp -> OSAL's OS_TaskExit ->
        // glibc's pthread_exit, which is itself a forced-unwind mechanism;
        // driving a second, foreign unwind through a thread whose Rust
        // unwinder has already run once is the trigger, not panicking itself
        // (a plain CFE_ES_ExitApp call with no prior panic on the thread is
        // fine — the normal end of `run()`, above, does exactly that).
        // Returning from this function without calling it leaves ES's own
        // bookkeeping for this app stale (it does not learn the exit
        // status), which is the real, still-open cost of this workaround.
    }
}
