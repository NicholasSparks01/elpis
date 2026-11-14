# syntax=docker/dockerfile:1
#
# elpis's guest: the VM kernel (elpis-kernel-x86_64, an ELF vmlinux) and the VM root filesystem
# (elpis-rootfs-x86_64.erofs: vminitd + crun), built from containerd/nerdbox at a pinned commit.
#
#   docker buildx build guest --output type=local,dest=guest/out
#
# (scripts/build-guest.sh does that and copies the results beside elpis-shim.exe.)

ARG NERDBOX_REV=1489d09ba60d5b204246b9ed3caf8dfbf33c1e55
ARG GO_IMAGE=golang:1.26.3-bookworm
ARG CRUN_VERSION=1.24

FROM ${GO_IMAGE} AS base
ENV DEBIAN_FRONTEND=noninteractive

# ---- containerd/nerdbox sources (kernel config + patches, vminitd) ----
FROM base AS nerdbox-src
ARG NERDBOX_REV
WORKDIR /src
RUN git init -q . && git remote add origin https://github.com/containerd/nerdbox.git \
    && git fetch -q --depth 1 origin "${NERDBOX_REV}" && git checkout -q FETCH_HEAD

# ---- kernel ----
FROM base AS kernel-build
RUN --mount=type=cache,sharing=locked,id=nb-kernel-apt,target=/var/cache/apt \
    apt-get update && apt-get install -y --no-install-recommends \
        build-essential libncurses-dev flex bison libssl-dev libelf-dev bc cpio wget xz-utils pahole
# The kernel to build, and which of nerdbox's configs to start from
ARG KERNEL_VERSION=6.12.44
ARG KERNEL_BASE_CONFIG=6.12.44
WORKDIR /usr/src
RUN wget -q "https://cdn.kernel.org/pub/linux/kernel/v${KERNEL_VERSION%%.*}.x/linux-${KERNEL_VERSION}.tar.xz" \
    && tar -xf "linux-${KERNEL_VERSION}.tar.xz" && rm "linux-${KERNEL_VERSION}.tar.xz" && mv "linux-${KERNEL_VERSION}" linux
COPY --from=nerdbox-src /src/kernel /nerdbox-kernel
COPY kernel/ /guest-kernel/
WORKDIR /usr/src/linux
RUN <<EOT
    set -e
    cp "/nerdbox-kernel/config-${KERNEL_BASE_CONFIG}-x86_64" .config
    # nerdbox's patches (vsock datagrams, TSI), then ours for this kernel version, if any
    for p in /nerdbox-kernel/patches/*.patch /guest-kernel/patches/*.patch; do
        [ -f "$p" ] && patch -p1 --fuzz=0 < "$p"
    done
    # Our config changes on top of nerdbox's
    ./scripts/kconfig/merge_config.sh -m .config /guest-kernel/elpis.config
    make olddefconfig
    # Every option we asked for must have survived olddefconfig
    grep -E '^CONFIG_' /guest-kernel/elpis.config | while read -r want; do
        grep -qx "$want" .config || { echo "kernel config: $want did not stick"; exit 1; }
    done
EOT
RUN make -j"$(nproc)" vmlinux && mkdir /build && cp vmlinux /build/elpis-kernel-x86_64

# ---- vminitd ----
FROM base AS vminitd-build
COPY --from=nerdbox-src /src /go/src/github.com/containerd/nerdbox
WORKDIR /go/src/github.com/containerd/nerdbox
# elpis's own vminitd plugins (guest/vminitd/*), compiled in beside upstream's
COPY vminitd/ plugins/elpis/
RUN for p in plugins/elpis/*/; do \
        name=$(basename "$p"); \
        sed -i "s#^\t_ \"github.com/containerd/nerdbox/plugins/vminit/task\"#&\n\t_ \"github.com/containerd/nerdbox/plugins/elpis/${name}\"#" cmd/vminitd/main.go; \
        grep -q "plugins/elpis/${name}\"" cmd/vminitd/main.go || { echo "couldn't add plugin ${name}"; exit 1; }; \
    done
RUN --mount=type=cache,target=/root/.cache/go-build,id=nb-vminitd \
    go build -o /build/vminitd -ldflags '-extldflags "-static" -s -w' -tags 'osusergo netgo static_build no_grpc' ./cmd/vminitd

# ---- crun ----
FROM base AS crun
ARG CRUN_VERSION
RUN mkdir /build && wget -q -O /build/crun \
    "https://github.com/containers/crun/releases/download/${CRUN_VERSION}/crun-${CRUN_VERSION}-linux-amd64-disable-systemd"

# ---- mkfs tools for the tools VM (elpis-mkfs): erofs-utils ≥ 1.7 for --tar, so Debian trixie ----
FROM debian:trixie-slim AS mkfs-tools
RUN apt-get update && apt-get install -y --no-install-recommends erofs-utils e2fsprogs && rm -rf /var/lib/apt/lists/*
# The two binaries and every shared library they load, at the same paths
RUN set -e; mkdir /out; \
    for bin in /usr/bin/mkfs.erofs /usr/sbin/mke2fs; do \
        cp --parents "$bin" /out; \
        ldd "$bin" | grep -oE '/[^ ]+' | while read -r lib; do cp --parents -L "$lib" /out; done; \
    done; \
    # glibc dlopens libgcc_s for pthread_exit (mkfs.erofs is multithreaded); ldd doesn't list it
    cp --parents -L /usr/lib/x86_64-linux-gnu/libgcc_s.so.1 /out; \
    mkdir -p /out/etc && cp /etc/mke2fs.conf /out/etc/

FROM base AS elpis-mkfs-build
COPY tools/elpis-mkfs/ /src/
WORKDIR /src
RUN CGO_ENABLED=0 go build -trimpath -ldflags '-s -w' -o /build/elpis-mkfs main.go

# ---- the VM rootfs ----
FROM debian:trixie-slim AS rootfs-build
RUN apt-get update && apt-get install -y --no-install-recommends erofs-utils && rm -rf /var/lib/apt/lists/*
WORKDIR /rootfs
RUN mkdir -p dev etc proc run sbin sys tmp var && ln -s /run var/run
COPY --from=vminitd-build /build/vminitd sbin/vminitd
COPY --from=crun /build/crun sbin/crun
COPY --from=elpis-mkfs-build /build/elpis-mkfs sbin/elpis-mkfs
COPY --from=mkfs-tools /out/ ./
RUN chmod 0755 sbin/vminitd sbin/crun sbin/elpis-mkfs && mkdir /build && mkfs.erofs -zlz4 /build/elpis-rootfs-x86_64.erofs .

FROM scratch
COPY --from=kernel-build /build/elpis-kernel-x86_64 /
COPY --from=rootfs-build /build/elpis-rootfs-x86_64.erofs /
