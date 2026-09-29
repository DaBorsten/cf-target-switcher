#!/bin/sh
# Installs cf-ts from GitHub releases on Linux and macOS.
#
#   curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/DaBorsten/cf-target-switcher/main/install.sh | sh
#
# Environment variables:
#   CF_TS_VERSION          release tag to install, e.g. v0.1.0 (default: latest)
#   CF_TS_INSTALL_DIR      target directory (default: ~/.local/bin)
#   CF_TS_NO_MODIFY_PATH   set to 1 to leave shell profiles untouched

set -eu

REPO="DaBorsten/cf-target-switcher"
BIN="cf-ts"

say() { printf '%s\n' "$*"; }
# HTTPS only, also across redirects.
fetch() { curl --proto '=https' --tlsv1.2 -fsSL "$1" -o "$2"; }
err() {
	printf 'error: %s\n' "$*" >&2
	exit 1
}

detect_target() {
	os=$(uname -s)
	arch=$(uname -m)
	case "$os" in
	Linux) os="unknown-linux-musl" ;;
	Darwin)
		os="apple-darwin"
		# A shell running under Rosetta reports x86_64; prefer the native build.
		if [ "$arch" = x86_64 ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null)" = 1 ]; then
			arch=arm64
		fi
		;;
	*) err "unsupported OS: $os (on Windows use install.ps1)" ;;
	esac
	case "$arch" in
	x86_64 | amd64) arch="x86_64" ;;
	aarch64 | arm64) arch="aarch64" ;;
	*) err "unsupported architecture: $arch" ;;
	esac
	target="$arch-$os"
}

verify_checksum() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha="sha256sum"
	elif command -v shasum >/dev/null 2>&1; then
		sha="shasum -a 256"
	else
		say "warning: sha256sum/shasum not found, skipping checksum verification"
		return
	fi
	fetch "$base/SHA256SUMS.txt" "$tmp/SHA256SUMS.txt" || err "could not download SHA256SUMS.txt"
	expected=$(grep " \*\{0,1\}$asset\$" "$tmp/SHA256SUMS.txt" | cut -d' ' -f1)
	[ -n "$expected" ] || err "no checksum found for $asset"
	actual=$($sha "$tmp/$asset" | cut -d' ' -f1)
	[ "$expected" = "$actual" ] || err "checksum mismatch for $asset"
}

ensure_path() {
	case ":$PATH:" in
	*":$dir:"*) return ;;
	esac
	if [ "${CF_TS_NO_MODIFY_PATH:-0}" = 1 ]; then
		say "note: $dir is not on your PATH, add it to run $BIN from anywhere"
		return
	fi

	line="export PATH=\"$dir:\$PATH\""
	case "${SHELL:-}" in
	*/zsh) rc="${ZDOTDIR:-$HOME}/.zshrc" ;;
	*/bash)
		if [ "$(uname -s)" = Darwin ]; then rc="$HOME/.bash_profile"; else rc="$HOME/.bashrc"; fi
		;;
	*/fish)
		rc="$HOME/.config/fish/config.fish"
		line="fish_add_path \"$dir\""
		;;
	*) rc="$HOME/.profile" ;;
	esac

	if [ -f "$rc" ] && grep -Fqx "$line" "$rc"; then
		say "$dir is already added to PATH in $rc."
	else
		mkdir -p "$(dirname "$rc")"
		printf '\n# Added by the cf-ts installer\n%s\n' "$line" >>"$rc"
		say "Added $dir to PATH in $rc."
	fi
	say "Open a new terminal, or run this to use $BIN right away:"
	say "  $line"
}

main() {
	command -v curl >/dev/null 2>&1 || err "curl is required"
	command -v tar >/dev/null 2>&1 || err "tar is required"

	detect_target
	version="${CF_TS_VERSION:-latest}"
	if [ "$version" = latest ]; then
		base="https://github.com/$REPO/releases/latest/download"
	else
		base="https://github.com/$REPO/releases/download/$version"
	fi
	asset="$BIN-$target.tar.gz"
	dir="${CF_TS_INSTALL_DIR:-$HOME/.local/bin}"

	tmp=$(mktemp -d)
	trap 'rm -rf "$tmp"' EXIT

	say "Downloading $asset ($version)..."
	fetch "$base/$asset" "$tmp/$asset" || err "download failed: $base/$asset"
	verify_checksum
	tar -xzf "$tmp/$asset" -C "$tmp"

	# Copy next to the target, then rename, so a running cf-ts can be replaced.
	mkdir -p "$dir"
	cp "$tmp/$BIN-$target/$BIN" "$dir/.$BIN.new"
	chmod 755 "$dir/.$BIN.new"
	mv -f "$dir/.$BIN.new" "$dir/$BIN"

	say "Installed $("$dir/$BIN" --version) to $dir/$BIN"
	ensure_path
}

main "$@"
