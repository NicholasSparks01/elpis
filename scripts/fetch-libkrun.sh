#!/usr/bin/env bash
# Fetch the pinned libkrun into third_party/libkrun and apply elpis's patches.
set -euo pipefail
cd "$(dirname "$0")/.."
REV=abe0a0f69a980d693327fbb1ea17d60912d30cc6   # libkrun main, 2026-11-12 (libkrun 2.0, WHP backend)
DEST=third_party/libkrun
if [ ! -d "$DEST/.git" ]; then
    git clone https://github.com/containers/libkrun.git "$DEST"
fi
git -C "$DEST" fetch --quiet origin "$REV" 2>/dev/null || git -C "$DEST" fetch --quiet origin
git -C "$DEST" checkout --quiet --force "$REV"
for p in patches/libkrun/*.patch; do
    git -C "$DEST" apply --whitespace=nowarn "../../$p"
    echo "applied $p"
done
echo "libkrun ready at $DEST ($REV)"
