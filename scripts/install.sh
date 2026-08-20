#!/bin/sh
# cp2 installer: detect the platform, install the matching binary from this
# archive as ~/.cargo/bin/cp2 (cargo install's default bin dir), and
# ensure ~/.cargo/bin is on $PATH.
#
# The other binaries in the archive are sidecars for auto-deploying to
# machines of other platforms: keep them next to your cp2 binary or in a
# --binaries-dir. Nothing is downloaded — this script only copies local files.
set -e

# ---- detect platform (same mapping as cp2 itself) ----
os=$(uname -s 2>/dev/null || echo unknown)
arch=$(uname -m 2>/dev/null || echo unknown)
case "$os" in
    Linux) os=linux ;;
    Darwin) os=macos ;;
    MINGW*|MSYS*|CYGWIN*) os=windows ;;
    *) echo "install: unsupported OS '$os'" >&2; exit 1 ;;
esac
case "$arch" in
    x86_64|amd64) arch=x86_64 ;;
    aarch64|arm64) arch=aarch64 ;;
    *) echo "install: unsupported architecture '$arch'" >&2; exit 1 ;;
esac
echo "install: detected $os/$arch"

# ---- pick the matching binary, with fallbacks ----
pick() {
    case "$os-$arch" in
        linux-x86_64)
            [ -f "$ARCHIVE/cp2-x86_64-unknown-linux-musl" ] \
                && bin=cp2-x86_64-unknown-linux-musl \
                || bin=cp2-x86_64-unknown-linux-gnu ;;
        linux-aarch64)
            [ -f "$ARCHIVE/cp2-aarch64-unknown-linux-musl" ] \
                && bin=cp2-aarch64-unknown-linux-musl \
                || bin=cp2-aarch64-unknown-linux-gnu ;;
        macos-x86_64) bin=cp2-x86_64-apple-darwin ;;
        macos-aarch64) bin=cp2-aarch64-apple-darwin ;;
        windows-x86_64)
            [ -f "$ARCHIVE/cp2-x86_64-pc-windows-gnu.exe" ] \
                && bin=cp2-x86_64-pc-windows-gnu.exe \
                || bin=cp2-x86_64-pc-windows-msvc.exe ;;
        windows-aarch64)
            [ -f "$ARCHIVE/cp2-aarch64-pc-windows-gnu.exe" ] \
                && bin=cp2-aarch64-pc-windows-gnu.exe \
                || bin=cp2-aarch64-pc-windows-msvc.exe ;;
        *) echo "install: no binary for $os/$arch" >&2; exit 1 ;;
    esac
}
ARCHIVE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
pick
SRC="$ARCHIVE/$bin"
[ -f "$SRC" ] || { echo "install: '$bin' is missing from this archive" >&2; exit 1; }

# ---- install as ~/.cargo/bin/cp2 ----
DEST="$HOME/.cargo/bin/cp2"
mkdir -p "$(dirname "$DEST")"
install -m755 "$SRC" "$DEST"

# ---- ensure ~/.cargo/bin is on $PATH ----
line='export PATH="$HOME/.cargo/bin:$PATH"'
if [ "$os" = "windows" ]; then
    rc="$HOME/.bashrc"   # Git Bash / MSYS2
else
    case "${SHELL:-}" in
        *zsh) rc="$HOME/.zshrc" ;;
        *bash) rc="$HOME/.bashrc" ;;
        *) rc="$HOME/.profile" ;;
    esac
fi
if ! grep -qF 'HOME/.cargo/bin' "$rc" 2>/dev/null; then
    printf '\n# cp2\n%s\n' "$line" >> "$rc"
    echo "install: added ~/.cargo/bin to \$PATH in $rc (run: source $rc)"
else
    echo "install: ~/.cargo/bin already on \$PATH"
fi

echo "install: cp2 installed to $DEST"
"$DEST" --version
