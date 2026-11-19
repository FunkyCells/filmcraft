# shellcheck shell=bash
# Shared setup for the packaging scripts. Source it: `. "$(dirname "$0")/../env.sh"`.
#
# Exports:
#   ROOT                    workspace root
#   VERSION                 [workspace.package] version from Cargo.toml (override: FILMCRAFT_VERSION)
#   DIST                    output directory for release artifacts (default: $ROOT/dist/release)
#   FILMCRAFT_BUILD_SHA    git commit baked into the binaries (see crates/engine/src/build_info.rs)
#   FILMCRAFT_BUILD_DATE   UTC build date, YYYY-MM-DD
#   CARGO_TARGET_DIR        cargo's target dir (default: $ROOT/target)

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export ROOT

# The version lives in exactly one place: `[workspace.package] version` in the root Cargo.toml.
# (`cargo xtask version` prints the same thing; awk avoids compiling xtask here.)
workspace_version() {
  awk '
    /^\[/ { in_pkg = ($0 == "[workspace.package]") ; next }
    in_pkg && $1 == "version" { gsub(/[" ]/, "", $3); print $3; exit }
  ' "$ROOT/Cargo.toml"
  return 0
}

VERSION="${FILMCRAFT_VERSION:-$(workspace_version)}"
if [[ -z "$VERSION" ]]; then
  echo "error: could not read [workspace.package] version from $ROOT/Cargo.toml" >&2
  exit 1
fi
export VERSION

DIST="${DIST:-$ROOT/dist/release}"
mkdir -p "$DIST"
export DIST

if [[ -z "${FILMCRAFT_BUILD_SHA:-}" ]]; then
  FILMCRAFT_BUILD_SHA="$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || true)"
fi
export FILMCRAFT_BUILD_SHA
export FILMCRAFT_BUILD_DATE="${FILMCRAFT_BUILD_DATE:-$(date -u +%Y-%m-%d)}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"

# Emit a GitHub Actions warning (plain stderr outside Actions).
warn() {
  if [[ -n "${GITHUB_ACTIONS:-}" ]]; then echo "::warning::$*"; else echo "warning: $*" >&2; fi
  return 0
}

# Copy licence and readme files that exist into a package directory, plus the licences of the
# craft-fonts fonts the build embedded (only when built with CRAFT_FONTS_DIR).
copy_docs() {
  local dest="$1" f
  for f in README.md LICENSE LICENSE-MIT LICENSE-APACHE COPYRIGHT NOTICE; do
    if [[ -f "$ROOT/$f" ]]; then cp "$ROOT/$f" "$dest/"; fi
  done
  copy_font_licences "$dest"
  return 0
}

# Copy each craft-fonts licence ($CRAFT_FONTS_DIR/fonts/<family>/OFL.txt) to <dest>/OFL-<family>.txt.
# Does nothing without CRAFT_FONTS_DIR.
copy_font_licences() {
  local dest="$1" lic family
  if [[ -z "${CRAFT_FONTS_DIR:-}" ]]; then return 0; fi
  for lic in "$CRAFT_FONTS_DIR"/fonts/*/OFL.txt; do
    [[ -f "$lic" ]] || continue
    family="$(basename "$(dirname "$lic")")"
    cp "$lic" "$dest/OFL-$family.txt"
  done
  return 0
}

# Portable SHA-256 of a file (prints just the hash).
sha256() {
  local file="$1"
  if command -v sha256sum >/dev/null; then sha256sum "$file" | cut -d' ' -f1; else shasum -a 256 "$file" | cut -d' ' -f1; fi
  return 0
}

# Download the speech model listed in packaging/speech-model-whisper-base.txt into
# <dest>/whisper-base (cached in $CARGO_TARGET_DIR/bundled-models), refuse any file whose size or
# SHA-256 differs from the manifest, and add the licence notice. The weights are never committed.
fetch_speech_model() {
  local dest="$1" cache="$CARGO_TARGET_DIR/bundled-models/whisper-base" sha size name url got
  mkdir -p "$cache" "$dest/whisper-base"
  while read -r sha size name url; do
    [[ -z "$sha" || "$sha" == \#* ]] && continue
    if [[ ! -f "$cache/$name" ]] || [[ "$(sha256 "$cache/$name")" != "$sha" ]]; then
      echo "==> downloading $name"
      curl -fL --retry 3 -o "$cache/$name.part" "$url"
      mv "$cache/$name.part" "$cache/$name"
    fi
    got="$(sha256 "$cache/$name")"
    if [[ "$got" != "$sha" ]] || [[ "$(wc -c <"$cache/$name" | tr -d ' ')" != "$size" ]]; then
      echo "error: $name does not match packaging/speech-model-whisper-base.txt (sha256 $got)" >&2
      exit 1
    fi
    cp "$cache/$name" "$dest/whisper-base/$name"
  done <"$ROOT/packaging/speech-model-whisper-base.txt"
  cp "$ROOT/packaging/whisper-model-NOTICE.txt" "$dest/whisper-base/NOTICE.txt"
  return 0
}
