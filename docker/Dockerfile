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

FROM debian:bookworm AS build

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential cmake git python3 ca-certificates \
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

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        libc6 iproute2 iputils-ping \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/build-native_std/exe /cfs/exe

# core-cpu1 resolves its startup script, tables and app shared objects relative
# to the working directory, so it must run from here.
WORKDIR /cfs/exe/cpu1

EXPOSE 1234/udp
CMD ["./core-cpu1"]
