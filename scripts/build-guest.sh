#!/usr/bin/env bash
# Build elpis's guest kernel and VM rootfs (guest/Dockerfile) with Docker into guest/out.
# With --install, also put them beside elpis-shim.exe in target/release and target/debug, where the shim
# prefers them over Docker Sandboxes' copies. Other arguments go to `docker buildx build`.
set -euo pipefail
cd "$(dirname "$0")/.."
install=false
args=()
for a in "$@"; do
    if [ "$a" = "--install" ]; then install=true; else args+=("$a"); fi
done
docker buildx build guest --output type=local,dest=guest/out "${args[@]}"
ls -l guest/out
if $install; then
    for dir in target/release target/debug; do
        if [ -d "$dir" ]; then
            cp guest/out/elpis-kernel-x86_64 guest/out/elpis-rootfs-x86_64.erofs "$dir/"
            echo "installed into $dir"
        fi
    done
fi
