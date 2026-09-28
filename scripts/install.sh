#!/bin/sh
# install.sh — install prebuilt mdrv-oc on macOS & Linux in one command.
# Downloads the right archive from GitHub Releases, verifies it against the
# release's SHA256SUMS.txt, and installs the `mdrv-oc` binary into
# ~/.local/bin.
#
# Usage:
#   curl -fsSL https://github.com/mdrv/mdrv-oc/releases/latest/download/install.sh | sh
#   sh install.sh [--version X.Y.Z] [--prefix DIR] [--no-path]
#
# Windows: use install.ps1 instead (same release page).
# Upgrade any time by running it again.

set -eu

REPO="mdrv/mdrv-oc"
REPO_URL="https://github.com/$REPO"
API_URL="https://api.github.com/repos/$REPO"
RELEASES_URL="$REPO_URL/releases"

TMP=""

cleanup() {
	[ -n "$TMP" ] && rm -rf -- "$TMP"
	return 0
}
trap cleanup EXIT INT TERM

usage() {
	cat <<'EOF'
install.sh — install prebuilt mdrv-oc on macOS & Linux

Usage:
  curl -fsSL https://github.com/mdrv/mdrv-oc/releases/latest/download/install.sh | sh
  sh install.sh [options]

Options:
  --version X.Y.Z   install a specific release (default: latest)
  --prefix DIR      install under DIR/bin (default: ~/.local)
  --no-path         do not offer to add the binary dir to your shell rc
  -h, --help        show this help

Environment:
  MDRV_OC_VERSION   same as --version
  MDRV_OC_PREFIX    same as --prefix

Windows: use install.ps1 from the same release page.
EOF
}

info() { printf '==> %s\n' "$1"; }
err() { printf 'install.sh: error: %s\n' "$1" >&2; exit 1; }

VERSION="${MDRV_OC_VERSION:-}"
PREFIX="${MDRV_OC_PREFIX:-$HOME/.local}"
ADD_PATH=1

while [ $# -gt 0 ]; do
	case "$1" in
		--version)
			[ $# -ge 2 ] || err "--version needs a value"
			VERSION="$2"
			shift 2
			;;
		--version=*)
			VERSION="${1#--version=}"
			shift
			;;
		--prefix)
			[ $# -ge 2 ] || err "--prefix needs a value"
			PREFIX="$2"
			shift 2
			;;
		--prefix=*)
			PREFIX="${1#--prefix=}"
			shift
			;;
		--no-path)
			ADD_PATH=0
			shift
			;;
		-h | --help)
			usage
			exit 0
			;;
		*)
			err "unknown option: $1 (try --help)"
			;;
	esac
done

# Map the machine to a release target. Release archives:
#   {x86_64,aarch64}-unknown-linux-musl, {x86_64,aarch64}-apple-darwin
OS="$(uname -s)"
case "$OS" in
	Darwin) ;;
	Linux) ;;
	*) err "unsupported OS: $OS — on Windows use install.ps1 (same release page)" ;;
esac
case "$(uname -m)" in
	x86_64) ARCH="x86_64" ;;
	arm64 | aarch64) ARCH="aarch64" ;;
	*) err "unsupported architecture: $(uname -m) ($OS)" ;;
esac
if [ "$OS" = "Darwin" ]; then
	TARGET="$ARCH-apple-darwin"
else
	TARGET="$ARCH-unknown-linux-musl"
fi

command -v curl >/dev/null 2>&1 || err "curl is required"
# First available tool wins; the archive is only verified if one exists.
sha_tool=""
if command -v sha256sum >/dev/null 2>&1; then
	sha_tool="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
	sha_tool="shasum"
elif command -v openssl >/dev/null 2>&1; then
	sha_tool="openssl"
fi

# Resolve the release tag. Asset names embed the tag:
#   mdrv-oc-<tag>-<target>.tar.gz
if [ -n "$VERSION" ]; then
	TAG="v${VERSION#v}"
	info "resolving release $TAG"
else
	info "resolving latest mdrv-oc release"
	RELEASE_JSON=$(curl -fsSL "$API_URL/releases/latest") || err "could not reach the GitHub API (rate limited? pin a release with --version X.Y.Z)"
	TAG=$(printf '%s\n' "$RELEASE_JSON" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
	[ -n "$TAG" ] || err "could not determine the latest release (pin one with --version X.Y.Z)"
fi

ASSET="mdrv-oc-$TAG-$TARGET.tar.gz"
BASE_URL="$RELEASES_URL/download/$TAG"
BINDIR="$PREFIX/bin"

OLD_VERSION=""
if [ -x "$BINDIR/mdrv-oc" ]; then
	OLD_VERSION=$("$BINDIR/mdrv-oc" --version 2>/dev/null || true)
fi

TMP=$(mktemp -d)

info "downloading $ASSET"
curl -fsSL "$BASE_URL/$ASSET" -o "$TMP/$ASSET" || err "download failed: $BASE_URL/$ASSET"

# Verify against the release's own SHA256SUMS.txt.
info "downloading SHA256SUMS.txt"
if curl -fsSL "$BASE_URL/SHA256SUMS.txt" -o "$TMP/SHA256SUMS.txt"; then
	EXPECTED=$(grep -E "^[0-9a-f]{64}[[:space:]]+\*?$(printf '%s' "$ASSET" | sed 's/[][\.*^$/]/\\&/g')$" "$TMP/SHA256SUMS.txt" | head -n 1 | cut -d ' ' -f1)
	if [ -n "$EXPECTED" ]; then
		if [ -z "$sha_tool" ]; then
			info "no sha256 tool found (sha256sum/shasum/openssl) — skipping checksum verification"
		else
			info "verifying sha256 ($EXPECTED)"
			actual=""
			case "$sha_tool" in
				sha256sum) actual=$(sha256sum "$TMP/$ASSET" | cut -d ' ' -f1) ;;
				shasum) actual=$(shasum -a 256 "$TMP/$ASSET" | cut -d ' ' -f1) ;;
				openssl) actual=$(openssl dgst -sha256 -r "$TMP/$ASSET" | cut -d ' ' -f1) ;;
			esac
			[ "$actual" = "$EXPECTED" ] || err "checksum mismatch — the download is corrupted or was tampered with"
		fi
	else
		info "no checksum for $ASSET in SHA256SUMS.txt — skipping verification"
	fi
else
	info "no SHA256SUMS.txt published for this release — skipping verification"
fi

info "extracting"
tar -xzf "$TMP/$ASSET" -C "$TMP" || err "extraction failed"
SRC="$TMP/mdrv-oc"
[ -x "$SRC/mdrv-oc" ] || err "unexpected archive layout (mdrv-oc/ not found)"

mkdir -p "$BINDIR" 2>/dev/null || err "cannot create $BINDIR (use --prefix or run under sudo)"
info "installing into $BINDIR"
install -m 0755 "$SRC/mdrv-oc" "$BINDIR/mdrv-oc" || err "could not write to $BINDIR (use --prefix or run under sudo)"

NEW_VERSION=$("$BINDIR/mdrv-oc" --version)

# PATH: pick the rc file matching the login shell (zsh on stock macOS,
# bash on most Linux distros). Offers to append an export line when running
# interactively; otherwise prints the instructions. Never touches rc files
# without an answer.
rc="$HOME/.bashrc"
case "${SHELL:-}" in
	*zsh*) rc="$HOME/.zshrc" ;;
esac

on_path=0
case ":$PATH:" in
	*":$BINDIR:"*) on_path=1 ;;
esac
path_action="already-on-path"
if [ "$on_path" = 0 ]; then
	path_action="manual"
	if [ "$ADD_PATH" = 1 ] && [ -t 0 ] && [ -t 1 ]; then
		if [ -f "$rc" ] && grep -qF "$BINDIR" "$rc"; then
			path_action="already-in-rc"
		else
			printf '\n%s is not on your PATH.\nAdd it to %s? [y/N] ' "$BINDIR" "$rc"
			read -r answer || answer=""
			case "$answer" in
				y | Y | yes | Yes | YES)
					printf '\n# Added by mdrv-oc installer\nexport PATH="%s:$PATH"\n' "$BINDIR" >>"$rc"
					path_action="added"
					;;
			esac
		fi
	fi
fi

printf '\n'
info "mdrv-oc $NEW_VERSION installed ($BINDIR/mdrv-oc)"
if [ -n "$OLD_VERSION" ] && [ "$OLD_VERSION" != "$NEW_VERSION" ]; then
	info "upgraded from $OLD_VERSION"
fi
case "$path_action" in
	already-on-path | already-in-rc | added)
		if [ "$path_action" = "added" ]; then
			printf '    PATH updated in %s — open a new terminal, then run:  mdrv-oc --help\n' "$rc"
		else
			printf '    Start with:  mdrv-oc --help\n'
		fi
		;;
	manual)
		printf '    Add mdrv-oc to your PATH by putting this in %s:\n        export PATH="%s:$PATH"\n    Then run:  mdrv-oc --help\n' "$rc" "$BINDIR"
		;;
esac
printf '    Shell completions (carapace):  mdrv-oc completion\n'
printf '    Upgrade:   run this script again\n'
