# Build and run NASA cFS in a Linux container.
#
# cFS targets Linux. OSAL's POSIX port assumes ELF shared objects loaded with
# dlopen, Linux scheduling/permission behavior, and GNU toolchain conventions,
# so building it natively on macOS is a research project of its own and not the
# one we signed up for. Bevy runs natively on the host and talks to this
# container over UDP.
#
# Every message ID and struct layout the Rust side decodes is a property of this
# specific build, which is why CFS_REF is a tag and not a branch.
#
# Build context is the repo root (see compose.yaml) rather than this
# directory, so this stage can also COPY in spikes/rust-cfs-app — the Phase 5
# Rust-inside-cFS spike, built here rather than cross-compiled from the host
# so its cdylib links against the same glibc as core-cpu1 — along with the
# four no_std crates it shares with the ground software.

FROM debian:bookworm AS build

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential cmake git python3 ca-certificates \
        curl clang libclang-dev \
    && rm -rf /var/lib/apt/lists/*

# v7.0.1 (2026-05-13) is the newest release tag and uses the multi-configuration
# build system (target-configs.mk). Older tags such as equuleus-rc1 predate it
# and need the previous `cp cfe/cmake/Makefile.sample` dance instead.
ARG CFS_REF=v7.0.1
WORKDIR /src

# Full clone, not shallow: the submodule pointers belong to the tag, so the tag
# has to be checked out before the submodules are fetched.
RUN git clone https://github.com/nasa/cFS.git . \
    && git checkout --detach "${CFS_REF}" \
    && git submodule init \
    && git submodule update --recursive

# `native_std` is the bundle's own name for the traditional Linux development
# build. It supplies -DMISSIONCONFIG=sample itself, which matters because the
# bundle ships two *_defs directories (sample_defs and a simple_defs symlink) and
# cmake refuses to guess between them.
#
# `native_eds` is the same build with EDS enabled — the configuration to switch
# to when evaluating generated message definitions in Phase 1.
RUN make native_std.prep \
    && make -j"$(nproc)" native_std.install

# --- Phase 5 spike: build the Rust cFE app -----------------------------------
#
# core_api (what add_cfe_app links C apps against) is a headers-only CMake
# interface target — apps carry undefined CFE_* symbols that core-cpu1
# resolves at dlopen() time from its own exported symbol table (it is linked
# with ENABLE_EXPORTS/-rdynamic specifically because CPU1_APPLIST is
# non-empty; see cfe/cmake/target/CMakeLists.txt). A cdylib with the same
# undefined symbols slots into that exactly the same way a C .so does — no
# CMake integration needed, just a shared object next to the others in `cf/`.
WORKDIR /rust-cfs-app-build
COPY rust-toolchain.toml ./rust-toolchain.toml
COPY spikes/rust-cfs-app ./spikes/rust-cfs-app
# The application's path dependencies. These are the *same source files* the
# visualizer compiles against — not a vendored copy — which is the whole claim
# Architecture B rests on: one definition of the wire format and one
# implementation of the vehicle model, compiled into both the ground software
# and the flight software. Copying only these four keeps the build cache from
# being invalidated by every edit to apps/viz.
COPY crates/ccsds ./crates/ccsds
COPY crates/cfs-msg ./crates/cfs-msg
COPY crates/telemetry-model ./crates/telemetry-model
COPY crates/vehicle-dyn ./crates/vehicle-dyn
# The crates use `version.workspace = true` and friends, which need a workspace
# root to inherit from. This stub supplies exactly those keys; the real root
# manifest is not copied because it lists members (apps/viz, the tools) that
# are deliberately absent from this image.
COPY docker/crates-workspace.toml ./Cargo.toml

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"

# build.rs runs bindgen against the exact header set add_cfe_app gives every
# C app (lifted from sample_app's own flags.make), so the FFI surface is
# generated from this build's real headers, not hand-transcribed.
RUN CFS_SRC_DIR=/src CFS_BUILD_DIR=/src/build-native_std CFS_CPU_NAME=cpu1 \
    cargo build --release --manifest-path spikes/rust-cfs-app/Cargo.toml
WORKDIR /src
# ------------------------------------------------------------------------------

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        libc6 iproute2 iputils-ping \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/build-native_std/exe /cfs/exe
COPY --from=build \
    /rust-cfs-app-build/spikes/rust-cfs-app/target/release/librust_cfs_app.so \
    /cfs/exe/cpu1/cf/rust_app.so

# Registers the Rust app alongside the C ones. Priority 55 (sample_app's is
# 50) — no ordering dependency on it, just slotted in next to the other
# lab/demo apps and ahead of the 70-priority ODS apps.
RUN sed -i \
    '/^CFE_APP, sample_app,/a CFE_APP,  rust_app,     RUST_APP_Main,      RUST_APP,     55,   32768, 0x0, 0;' \
    /cfs/exe/cpu1/cf/cfe_es_startup.scr

# core-cpu1 resolves its startup script, tables and app shared objects relative
# to the working directory, so it must run from here.
WORKDIR /cfs/exe/cpu1

EXPOSE 1234/udp
CMD ["./core-cpu1"]
