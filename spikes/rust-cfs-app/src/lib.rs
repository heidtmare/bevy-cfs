//! Phase 5 spike (Architecture B): a cFE ES application written in Rust.
//!
//! cFE's ES loader `dlopen()`s this `.so` exactly like a C app module (see
//! `add_cfe_app` in the bundle's CMake — apps link only against `core_api`,
//! an interface target with no library body, so every `CFE_*` symbol here is
//! left undefined at link time and resolved at load time against the
//! `core-cpu1` executable's own exported symbol table). `RUST_APP_Main` is
//! this app's entry point, named in the startup-script line `docker/Dockerfile`
//! `sed`s into `cfe_es_startup.scr` at image-build time.
//!
//! This mirrors `sample_app.c`'s structure deliberately, so the comparison
//! between the C original and this port is legible line for line. See
//! `docs/findings/0006-rust-cfs-app.md` for what did and didn't translate.

#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case)]

#[allow(unused_imports)] // bindgen emits a handful of unused re-exports (e.g. CFE_TIME_Compare_t) that we don't own.
mod bindings {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

use bindings::{
    CFE_EVS_EventFilter_BINARY, CFE_EVS_EventType_ERROR, CFE_EVS_EventType_INFORMATION,
    CFE_EVS_Register, CFE_EVS_SendEvent, CFE_ES_ExitApp, CFE_ES_PerfLogAdd, CFE_ES_RunLoop,
    CFE_ES_RunStatus_APP_ERROR, CFE_ES_RunStatus_APP_EXIT, CFE_ES_RunStatus_APP_RUN,
    CFE_ES_WriteToSysLog, CFE_MSG_Init,
    CFE_MSG_Message_t, CFE_MSG_Size_t, CFE_MSG_TelemetryHeader_t, CFE_SB_Buffer_t,
    CFE_SB_CreatePipe, CFE_SB_MsgId_t, CFE_SB_PipeId_t, CFE_SB_ReceiveBuffer, CFE_SB_TimeStampMsg,
    CFE_SB_TransmitMsg, CFE_Status_t,
};
use std::panic::{catch_unwind, AssertUnwindSafe};

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

const RUST_APP_HK_TLM_MID: u32 = 0x0890; // Ad hoc — not in any mission message-ID table. See finding 0006.
const RUST_APP_STARTUP_EID: u16 = 1;
const RUST_APP_PIPE_ERR_EID: u16 = 2;
const RUST_APP_PANIC_EID: u16 = 3;

#[repr(C)]
#[derive(Default)]
struct HkPayload {
    loop_count: u32,
}

#[repr(C)]
struct HkTlm {
    telemetry_header: CFE_MSG_TelemetryHeader_t,
    payload: HkPayload,
}

struct AppState {
    run_status: u32,
    command_pipe: CFE_SB_PipeId_t,
    hk_tlm: HkTlm,
}

unsafe fn init() -> Result<AppState, ()> {
    let evs_status = unsafe { CFE_EVS_Register(std::ptr::null(), 0, CFE_EVS_EventFilter_BINARY as u16) };
    if evs_status < 0 {
        unsafe {
            CFE_ES_WriteToSysLog(c"RUST_APP: Error Registering Events\n".as_ptr());
        }
        return Err(());
    }

    let mut state = AppState {
        run_status: CFE_ES_RunStatus_APP_RUN,
        command_pipe: CFE_SB_PipeId_t::default(),
        hk_tlm: HkTlm {
            // SAFETY: CFE_MSG_TelemetryHeader_t is a plain-old-data struct of
            // integer/byte fields (CCSDS header + spare padding); the cFE
            // convention (mirrored by every lab app) is to zero it and let
            // CFE_MSG_Init fill in the fields that matter.
            telemetry_header: unsafe { std::mem::zeroed() },
            payload: HkPayload::default(),
        },
    };

    unsafe {
        CFE_MSG_Init(
            msg_ptr(&mut state.hk_tlm.telemetry_header),
            CFE_SB_MsgId_t { Value: RUST_APP_HK_TLM_MID },
            std::mem::size_of::<HkTlm>() as CFE_MSG_Size_t,
        );
    }

    let sb_status = unsafe {
        CFE_SB_CreatePipe(&mut state.command_pipe, 4, c"RUST_APP_CMD_PIPE".as_ptr())
    };
    if sb_status < 0 {
        unsafe {
            CFE_ES_WriteToSysLog(c"RUST_APP: Error Creating Pipe\n".as_ptr());
        }
        return Err(());
    }

    unsafe {
        CFE_EVS_SendEvent(
            RUST_APP_STARTUP_EID,
            CFE_EVS_EventType_INFORMATION as u16,
            c"RUST_APP: initialized, HK on 0x%04X".as_ptr(),
            RUST_APP_HK_TLM_MID,
        );
    }

    Ok(state)
}

unsafe fn run(mut state: AppState) {
    // 1000 ms, not CFE_SB_PEND_FOREVER: the run loop needs to wake on its own
    // to publish HK even when no command ever arrives on the pipe, matching
    // the housekeeping-app pattern rather than the pure-command-dispatch
    // pattern sample_app uses (it relies on a separate HK app to poll it).
    const RECEIVE_TIMEOUT_MS: i32 = 1000;

    while unsafe { CFE_ES_RunLoop(&mut state.run_status) } {
        unsafe { perf_log_exit(RUST_APP_PERF_ID) };

        let mut buf_ptr: *mut CFE_SB_Buffer_t = std::ptr::null_mut();
        let status = unsafe { CFE_SB_ReceiveBuffer(&mut buf_ptr, state.command_pipe, RECEIVE_TIMEOUT_MS) };

        unsafe { perf_log_entry(RUST_APP_PERF_ID) };

        // A timeout with no message is not an error here — it is how this
        // app gets a chance to publish HK on a cadence even though nothing
        // subscribes it to anything. Anything else is a real pipe error;
        // sample_app's response to that is to log and exit the app (not the
        // process), which this mirrors.
        if status < 0 && status != CFE_SB_TIME_OUT {
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

        state.hk_tlm.payload.loop_count = state.hk_tlm.payload.loop_count.wrapping_add(1);
        unsafe {
            CFE_SB_TimeStampMsg(msg_ptr(&mut state.hk_tlm.telemetry_header));
            CFE_SB_TransmitMsg(msg_ptr(&mut state.hk_tlm.telemetry_header), true);
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
