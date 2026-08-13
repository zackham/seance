#!/usr/bin/env bash
# Build seance and wrap it in a macOS .app so it launches from Finder,
# Spotlight, and the Dock — not just from a terminal.
#
#   ./scripts/bundle-macos.sh              # build + install to /Applications
#   ./scripts/bundle-macos.sh --user       # install to ~/Applications instead
#   ./scripts/bundle-macos.sh --no-build   # bundle whatever is in target/release
#   ./scripts/bundle-macos.sh --dest DIR   # install somewhere else entirely
#
# This is the mac build command: the bundle holds a *copy* of the binary, so a
# bare `cargo build --release` leaves the app stale. Re-run this instead — with
# a warm target dir the whole thing is a few seconds.
#
# A copy, not a symlink into target/, because the executable of a signed bundle
# has to live inside it. Ad-hoc signing is what lets the app keep its identity
# (and its Keychain/TCC grants) across reinstalls.
#
# The icon is generated from assets/icons/seance-macos-1024.png with stock
# sips + iconutil — macOS ships no SVG rasterizer, so the PNG is committed
# rather than rendered here.
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "bundle-macos.sh only runs on macOS (this is $(uname -s))" >&2
  exit 1
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP_NAME="Seance"
BUNDLE_ID="xyz.ham.seance"
ICON_SRC="$ROOT/assets/icons/seance-macos-1024.png"

DO_BUILD=1
DEST="/Applications"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --no-build) DO_BUILD=0; shift ;;
    --user)     DEST="$HOME/Applications"; shift ;;
    --dest)     DEST="${2:?--dest needs a directory}"; shift 2 ;;
    -h|--help)  sed -n '2,17p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 1 ;;
  esac
done

# The workspace version — the first `version = "…"` in Cargo.toml, which is
# [workspace.package]. The [package] below it inherits with version.workspace.
VERSION="$(awk -F'"' '/^version = "/ {print $2; exit}' "$ROOT/Cargo.toml")"
if [[ -z "$VERSION" ]]; then
  echo "could not read version out of Cargo.toml" >&2
  exit 1
fi

if [[ $DO_BUILD -eq 1 ]]; then
  echo "building seance $VERSION (release)…"
  (cd "$ROOT" && cargo build --release)
fi

BIN="$ROOT/target/release/seance"
if [[ ! -x "$BIN" ]]; then
  echo "no release binary at $BIN — drop --no-build, or run cargo build --release" >&2
  exit 1
fi

# Assemble in a staging dir and swap at the end, so a failure halfway through
# can't leave a half-written app where a working one used to be.
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
APP="$STAGE/$APP_NAME.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

echo "rendering icon…"
ICONSET="$STAGE/seance.iconset"
mkdir -p "$ICONSET"
# name:pixels — the sizes iconutil expects, @2x included.
for spec in \
  16x16:16 16x16@2x:32 32x32:32 32x32@2x:64 \
  128x128:128 128x128@2x:256 256x256:256 256x256@2x:512 \
  512x512:512 512x512@2x:1024
do
  sips -z "${spec#*:}" "${spec#*:}" "$ICON_SRC" \
    --out "$ICONSET/icon_${spec%:*}.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/seance.icns"

cp "$BIN" "$APP/Contents/MacOS/seance"
printf 'APPL????' > "$APP/Contents/PkgInfo"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>              <string>$APP_NAME</string>
  <key>CFBundleDisplayName</key>       <string>$APP_NAME</string>
  <key>CFBundleIdentifier</key>        <string>$BUNDLE_ID</string>
  <key>CFBundleExecutable</key>        <string>seance</string>
  <key>CFBundleIconFile</key>          <string>seance</string>
  <key>CFBundlePackageType</key>       <string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key>           <string>$VERSION</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>LSMinimumSystemVersion</key>    <string>11.0</string>
  <key>LSApplicationCategoryType</key> <string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key>   <true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
  <key>NSHumanReadableCopyright</key>  <string>MIT · github.com/zackham/seance</string>
</dict>
</plist>
PLIST

# Ad-hoc signature. Cargo already ad-hoc signs the bare binary on Apple
# Silicon, but that signature covers the executable alone — everything added
# above is outside it, and an inconsistent bundle is what makes macOS refuse
# to launch a freshly-copied app.
echo "signing (ad-hoc)…"
codesign --force --sign - --timestamp=none "$APP" 2>&1 | sed 's/^/  /' || true

mkdir -p "$DEST"
TARGET="$DEST/$APP_NAME.app"
if [[ -e "$TARGET" && ! -w "$DEST" ]]; then
  echo "no write permission on $DEST — try --user (installs to ~/Applications)" >&2
  exit 1
fi
rm -rf "$TARGET"
cp -R "$APP" "$TARGET"

# Tell LaunchServices about it now rather than whenever it next rescans, so
# Spotlight and `open -a Seance` work the moment this script returns.
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
[[ -x "$LSREGISTER" ]] && "$LSREGISTER" -f "$TARGET" || true

echo
echo "installed $TARGET ($VERSION)"
echo "launch: open -a $APP_NAME   ·   or Spotlight → \"$APP_NAME\""
