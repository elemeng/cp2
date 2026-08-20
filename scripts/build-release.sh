#!/bin/bash
# Build cp2 for every supported target and assemble a distribution tarball:
#   cp2-<version>/install.sh
#   cp2-<version>/cp2-<triple>[.exe]   (one per target)
#
# Targets whose toolchain is not installed are skipped with a note — drop a
# prebuilt binary of the right name into the staging dir and re-run to include
# it. The native target is always built (as the gnu fallback) so the tarball
# is usable even with no cross toolchains installed.
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
STAGE="cp2-$VERSION"
echo "== assembling $STAGE =="
rm -rf "$STAGE"
mkdir -p "$STAGE"

build() { # target-triple  output-name
    local target="$1" out="$2"
    if ! rustup target list --installed | grep -qx "$target"; then
        echo "skip $target (toolchain not installed): place a prebuilt binary at $STAGE/$out"
        return
    fi
    echo "building $target..."
    # aws-lc-rs (the russh crypto backend, bundled on Windows clients) needs a
    # C compiler for the target; for x86-64 Windows it also wants nasm unless
    # the crate's prebuilt NASM objects are used (this env var opts in).
    if AWS_LC_SYS_PREBUILT_NASM=1 cargo build --release --target "$target" >/dev/null 2>&1; then
        if [ -f "target/$target/release/cp2.exe" ]; then
            cp "target/$target/release/cp2.exe" "$STAGE/$out"
        else
            cp "target/$target/release/cp2" "$STAGE/$out"
        fi
        chmod +x "$STAGE/$out"
    else
        echo "skip $target (build failed): place a prebuilt binary at $STAGE/$out"
        echo "  last build error:"
        AWS_LC_SYS_PREBUILT_NASM=1 cargo build --release --target "$target" 2>&1 | tail -4 | sed 's/^/    /'
    fi
}

NATIVE=$(rustc -vV | sed -n 's/^host: //p')

# Linux (musl = libc-agnostic, the auto-deploy default)
build x86_64-unknown-linux-musl cp2-x86_64-unknown-linux-musl
build aarch64-unknown-linux-musl cp2-aarch64-unknown-linux-musl
# macOS
build x86_64-apple-darwin cp2-x86_64-apple-darwin
build aarch64-apple-darwin cp2-aarch64-apple-darwin
# Windows (GNU builds cross-compile from Linux; since the Windows client
# bundles the C-based aws-lc-rs crypto backend, this needs a mingw-w64 C
# compiler for the target — apt: gcc-mingw-w64-x86-64 (x86_64) or
# gcc-mingw-w64 (aarch64, newer distros). nasm is optional: the build sets
# AWS_LC_SYS_PREBUILT_NASM=1 to use the crate's prebuilt x86-64 NASM objects.)
build x86_64-pc-windows-gnu cp2-x86_64-pc-windows-gnu.exe
build aarch64-pc-windows-gnu cp2-aarch64-pc-windows-gnu.exe
# Native build as the gnu/msvc fallback for the local platform
echo "building native ($NATIVE)..."
cargo build --release >/dev/null
cp target/release/cp2 "$STAGE/cp2-$NATIVE"
chmod +x "$STAGE/cp2-$NATIVE"

# Smoke-test the binaries that can run on this machine (the native build).
echo "== smoke test =="
"$STAGE/cp2-$NATIVE" --version
"$STAGE/cp2-$NATIVE" --help | head -3

cp scripts/install.sh "$STAGE/install.sh"
chmod +x "$STAGE/install.sh"

tar -czf "$STAGE.tar.gz" "$STAGE"
echo "== created $STAGE.tar.gz =="
du -h "$STAGE.tar.gz"
