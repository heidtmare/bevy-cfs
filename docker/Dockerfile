# Build and run NASA cFS in a Linux container.
#
# cFS targets Linux. OSAL's POSIX port assumes ELF shared objects loaded with
# dlopen, Linux scheduling/permission behavior, and GNU toolchain conventions,
# so building it natively on macOS is a research project of its own and not the
# one we signed up for. Bevy runs natively on the host and talks to this
# container over UDP.
#
# Pin CFS_REF to a tag before Phase 1 and record it in docs/findings/ — every
# message ID and struct layout the Rust side decodes is a property of this
# specific build.

FROM debian:bookworm AS build

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential cmake git python3 ca-certificates \
    && rm -rf /var/lib/apt/lists/*

ARG CFS_REF=main
WORKDIR /src
RUN git clone --recurse-submodules --shallow-submodules \
        https://github.com/nasa/cFS.git . \
    && git checkout "${CFS_REF}" \
    && git submodule update --init --recursive

# The bundle ships a sample Makefile and definitions tree that have to be copied
# into place before the first build; this is the documented lab setup.
RUN cp cfe/cmake/Makefile.sample Makefile \
    && cp -r cfe/cmake/sample_defs sample_defs

RUN make SIMULATION=native prep && make && make install

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        libc6 iproute2 iputils-ping \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/build/exe /cfs/exe

# core-cpu1 resolves its startup script, tables and app shared objects relative
# to the working directory, so it must run from here.
WORKDIR /cfs/exe/cpu1

EXPOSE 1234/udp
CMD ["./core-cpu1"]
