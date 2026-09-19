//! Generates FFI bindings from the actual cFE/OSAL/PSP headers of the pinned
//! v7.0.1 build (see `docker/Dockerfile`), rather than hand-transcribing
//! prototypes. This is itself the Phase 5 question: is bindgen practical
//! against cFE's headers, or does something (macros, bitfields, the mission
//! config indirection) make it not worth it? So far: practical, with one
//! caveat noted in `docs/findings/0006-rust-cfs-app.md` — cFE's perf-log and
//! message-header macros (`CFE_ES_PerfLogEntry`, `CFE_MSG_PTR`, ...) are
//! preprocessor macros, invisible to bindgen, and are reimplemented by hand
//! in `src/lib.rs`.
//!
//! This crate only builds inside the Linux container: cFS headers assume a
//! POSIX target, and the two environment variables below point at a checkout
//! and build tree that only exist there. See `docker/Dockerfile`.

use std::env;
use std::path::PathBuf;

fn main() {
    let cfs_src =
        env::var("CFS_SRC_DIR").expect("CFS_SRC_DIR must point at the cFS bundle checkout (see docker/Dockerfile) — this crate does not build outside the Linux container");
    let cfs_build = env::var("CFS_BUILD_DIR")
        .expect("CFS_BUILD_DIR must point at the cFS mission build tree (e.g. /src/build-native_std)");
    let cpu = env::var("CFS_CPU_NAME").unwrap_or_else(|_| "cpu1".to_string());

    // The exact include set `add_cfe_app` gives every C app, lifted from
    // apps/sample_app's real flags.make rather than guessed (an earlier
    // attempt at this list, taken from a different compile_commands.json
    // entry, was missing the generated per-cpu osal/psp dirs and three
    // fsw/inc dirs — bindgen's "file not found" on osconfig.h is what
    // caught it). `native/default_<cpu>/{inc,osal/inc,psp/inc}` is where
    // cFE's per-mission code generator drops cfe_msgids.h, osconfig.h and
    // friends.
    let include_dirs = [
        format!("{cfs_src}/sample_defs/inc"),
        format!("{cfs_build}/inc"),
        format!("{cfs_build}/native/default_{cpu}/inc"),
        format!("{cfs_build}/native/default_{cpu}/osal/inc"),
        format!("{cfs_build}/native/default_{cpu}/psp/inc"),
        format!("{cfs_src}/cfe/modules/config/fsw/inc"),
        format!("{cfs_src}/cfe/modules/core_api/fsw/inc"),
        format!("{cfs_src}/cfe/modules/es/fsw/inc"),
        format!("{cfs_src}/cfe/modules/evs/fsw/inc"),
        format!("{cfs_src}/cfe/modules/fs/fsw/inc"),
        format!("{cfs_src}/cfe/modules/msg/fsw/inc"),
        format!("{cfs_src}/cfe/modules/resourceid/fsw/inc"),
        format!("{cfs_src}/cfe/modules/sb/fsw/inc"),
        format!("{cfs_src}/cfe/modules/tbl/fsw/inc"),
        format!("{cfs_src}/cfe/modules/time/fsw/inc"),
        format!("{cfs_src}/osal/src/os/inc"),
        format!("{cfs_src}/psp/fsw/inc"),
        format!("{cfs_src}/psp/fsw/modules/iodriver/inc"),
    ];

    let mut builder = bindgen::Builder::default()
        .header_contents(
            "cfe_wrapper.h",
            "#include \"cfe_es.h\"\n#include \"cfe_evs.h\"\n#include \"cfe_sb.h\"\n#include \"cfe_msg.h\"\n#include \"cfe_time.h\"\n",
        )
        .clang_arg("-DSIMULATION=native")
        .clang_arg("-D_XOPEN_SOURCE=600")
        // cFE's RunStatus/EventType/etc. are plain uint32 with #define'd
        // values (see cfe_es_api_typedefs.h), not C enums — nothing to
        // configure bindgen for there. Function-like macros are the actual
        // gap; see the module doc comment above.
        .allowlist_function("CFE_ES_.*")
        .allowlist_function("CFE_EVS_.*")
        .allowlist_function("CFE_SB_.*")
        .allowlist_function("CFE_MSG_.*")
        // CFE_TIME_GetTime is how the control loop measures its own step
        // instead of assuming its timeout fired on schedule — see src/lib.rs.
        .allowlist_function("CFE_TIME_.*")
        .allowlist_type("CFE_.*")
        .allowlist_var("CFE_.*")
        // cFE's C enum constants already carry the enum's own name as a
        // prefix (`enum CFE_ES_RunStatus { CFE_ES_RunStatus_APP_RUN, ... }`).
        // bindgen's default (`prepend_enum_name(true)`) prepends the Rust
        // type name again on top of that, producing
        // `CFE_ES_RunStatus_CFE_ES_RunStatus_APP_RUN`. Off keeps the names
        // exactly as cFE spells them.
        .prepend_enum_name(false)
        .derive_default(true)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));

    for dir in &include_dirs {
        builder = builder.clang_arg(format!("-I{dir}"));
    }

    let bindings = builder.generate().expect("bindgen failed against the pinned v7.0.1 headers");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("failed to write bindings.rs");
}
