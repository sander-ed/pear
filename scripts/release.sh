#!/usr/bin/env bash
# Publish a pear release from a Mac.
#
#   1. Bump [package].version in Cargo.toml, run `cargo update -p pear`, commit.
#   2. Run scripts/release.sh
#
# This builds the macOS binaries (arm64 + x86_64, libghostty-vt linked
# statically), tests them, pushes main, and creates the vVERSION tag and GitHub
# Release with the macOS archives. Publishing the release starts the Release
# workflow, which adds the Linux binaries and updates the Homebrew tap.
#
# --dry-run builds, tests, and packages into dist/ without pushing anything.
set -euo pipefail

GHOSTTY_VT_VERSION=1.0.1
GHOSTTY_VT_SHA256=333dfce97641ef9b536bb9e08907ad86abaaada50e8f7417b70d9284adef37aa
ZIG_VERSION=0.17.1
MACOS_MIN=11.0
# rust target : zig target
TARGETS=("aarch64-apple-darwin:aarch64-macos.$MACOS_MIN" "x86_64-apple-darwin:x86_64-macos.$MACOS_MIN")

dry_run=false
case "${1:-}" in
  "") ;;
  --dry-run) dry_run=true ;;
  *) echo "usage: $0 [--dry-run]" >&2; exit 2 ;;
esac

die() { echo "release: $*" >&2; exit 1; }
step() { printf '\n==> %s\n' "$*"; }

cd "$(git rev-parse --show-toplevel)"
[[ "$(uname -s)" == Darwin ]] || die "run this on macOS"

# cargo +stable needs rustup's proxies; Homebrew's rustup is keg-only.
for dir in /opt/homebrew/opt/rustup/bin /usr/local/opt/rustup/bin "$HOME/.cargo/bin"; do
  [[ -x "$dir/rustup" ]] && export PATH="$dir:$PATH" && break
done
command -v rustup > /dev/null || die "rustup not found (brew install rustup)"
command -v gh > /dev/null || die "gh not found (brew install gh)"
command -v zig > /dev/null || die "zig $ZIG_VERSION not found (brew install zig)"
[[ "$(zig version)" == "$ZIG_VERSION" ]] || die "need zig $ZIG_VERSION, found $(zig version)"

version=$(sed -nE 's/^version *= *"([^"]+)".*/\1/p' Cargo.toml | head -n1)
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.+-]+)?$ ]] || die "bad version '$version' in Cargo.toml"
tag="v$version"

cargo metadata --locked --format-version 1 > /dev/null || die "Cargo.lock is stale; run cargo update -p pear"
if ! $dry_run; then
  step "Checking the repository for $tag"
  [[ -z "$(git status --porcelain)" ]] || die "working tree is not clean"
  [[ "$(git branch --show-current)" == main ]] || die "not on main"
  git fetch --quiet --tags origin main
  git merge-base --is-ancestor origin/main HEAD || die "main is behind origin/main; pull first"
  git rev-parse -q --verify "refs/tags/$tag" > /dev/null && die "tag $tag already exists; bump the version"
fi

rustup toolchain install stable --profile minimal --no-self-update \
  --target aarch64-apple-darwin --target x86_64-apple-darwin > /dev/null

cache="${XDG_CACHE_HOME:-$HOME/.cache}/pear/libghostty-vt-$GHOSTTY_VT_VERSION"
build_ghostty() {
  local zig_target=$1 prefix="$cache/$1"
  [[ -f "$prefix/lib/libghostty-vt.a" ]] && return
  local src="$cache/src/libghostty-vt-$GHOSTTY_VT_VERSION"
  if [[ ! -d "$src" ]]; then
    mkdir -p "$cache/src"
    local archive="$cache/src/libghostty-vt.tar.gz"
    curl -fsSL -o "$archive" \
      "https://release.files.ghostty.org/libghostty-vt/$GHOSTTY_VT_VERSION/libghostty-vt-$GHOSTTY_VT_VERSION.tar.gz"
    echo "$GHOSTTY_VT_SHA256  $archive" | shasum -a 256 -c - > /dev/null || die "libghostty-vt checksum mismatch"
    tar -xzf "$archive" -C "$cache/src"
  fi
  (cd "$src" && zig build -Demit-lib-vt=true -Doptimize=ReleaseFast -Dtarget="$zig_target" --prefix "$prefix")
}

rm -rf dist && mkdir dist
export MACOSX_DEPLOYMENT_TARGET=$MACOS_MIN
for pair in "${TARGETS[@]}"; do
  target=${pair%%:*} zig_target=${pair##*:}
  step "Building $target"
  build_ghostty "$zig_target"
  GHOSTTY_VT_DIR="$cache/$zig_target" cargo +stable build --release --locked --target "$target"

  binary="target/$target/release/pear"
  run=("$binary")
  [[ "$target" == x86_64-* && "$(uname -m)" == arm64 ]] && run=(arch -x86_64 "$binary")
  [[ "$("${run[@]}" --version)" == "pear $version (libghostty-vt)" ]] || die "$target: unexpected --version output"
  # Only system libraries: the binary must run without Homebrew dependencies.
  if otool -L "$binary" | tail -n +2 | grep -v '^[[:space:]]*/usr/lib/' | grep -q .; then
    otool -L "$binary"; die "$target links non-system libraries"
  fi

  if [[ "$target" == "$(uname -m | sed 's/arm64/aarch64/')-apple-darwin" ]]; then
    step "Testing $target"
    GHOSTTY_VT_DIR="$cache/$zig_target" cargo +stable test --release --locked --target "$target"
    rustup component add --toolchain stable clippy rustfmt > /dev/null
    cargo +stable fmt --check || die "run cargo fmt"
    GHOSTTY_VT_DIR="$cache/$zig_target" cargo +stable clippy --locked --all-targets --target "$target" -- -D warnings
    expect tests/e2e.exp "$binary" > /dev/null || die "end-to-end test failed (rerun: expect tests/e2e.exp $binary)"
  fi

  staging=$(mktemp -d)
  cp "$binary" LICENSE README.md "$staging/"
  tar -czf "dist/pear-$version-$target.tar.gz" -C "$staging" .
  rm -rf "$staging"
done
(cd dist && shasum -a 256 pear-*)

if $dry_run; then
  step "Dry run: archives are in dist/; nothing was pushed"
  exit 0
fi

step "Publishing $tag"
git push origin HEAD:main
gh release create "$tag" dist/*.tar.gz --target "$(git rev-parse HEAD)" \
  --title "pear $tag" --generate-notes
git fetch --quiet --tags origin

cat <<EOF

Published $tag with the macOS binaries. The Release workflow now builds the
Linux binaries and updates the tap. Follow it with:

  gh run watch -R sander-ed/pear \$(gh run list -R sander-ed/pear -w Release -L 1 --json databaseId -q '.[0].databaseId')
EOF
