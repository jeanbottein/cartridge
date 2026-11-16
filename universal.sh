#!/usr/bin/env bash
# Assemble the universal cartridge from the platform archives (package.sh):
# every system's app side by side in one Cartridge/ folder, all sharing its
# system/ and games, plus start.sh to run the right one from a terminal.
#
#   Cartridge/
#     start.sh                    Linux + macOS: runs the app for this machine
#     Cartridge-x86_64.AppImage   Linux PC, Steam Deck
#     Cartridge-arm64.AppImage    Linux ARM
#     Cartridge.app               macOS (Apple Silicon)
#     cartridge.exe               Windows
#     README.txt LICENSE.txt system/ games/ mods/ saves-copies/
#
# The app names are core::app::UNIVERSAL_APPS (self-update replaces them).
#
# Usage: ./universal.sh <folder holding the four platform archives>
#   -> <folder>/cartridge-universal.zip
# Needs unzip and zip (a zip made on Linux keeps the exec bits for Linux and
# macOS; Windows opens it as it is).
set -euo pipefail

IN="$(cd "$1" && pwd)"
REPO_ROOT="$(cd "$(dirname "$0")" && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

for archive in linux-x86_64 linux-aarch64; do
    mkdir "$WORK/$archive"
    tar -xzf "$IN/cartridge-$archive.tar.gz" -C "$WORK/$archive"
done
for archive in macos-arm64 windows-x86_64; do
    # unzip exits with 1 on mere warnings (e.g. a zip written on Windows).
    unzip -q "$IN/cartridge-$archive.zip" -d "$WORK/$archive" || [ $? -eq 1 ]
done
# ditto stores extended attributes as ._ files: in the .app they would
# break its signature.
find "$WORK/macos-arm64" -name '._*' -delete

# The x86_64 cartridge gives the READMEs, the license and the empty folders.
STAGE="$WORK/out/Cartridge"
mkdir -p "$WORK/out"
cp -R "$WORK/linux-x86_64/Cartridge" "$STAGE"
mv "$STAGE/Cartridge.AppImage" "$STAGE/Cartridge-x86_64.AppImage"
cp "$WORK/linux-aarch64/Cartridge/Cartridge.AppImage" "$STAGE/Cartridge-arm64.AppImage"
cp -R "$WORK/macos-arm64/Cartridge/Cartridge.app" "$STAGE/"
cp "$WORK/windows-x86_64/Cartridge/cartridge.exe" "$STAGE/"
cp "$REPO_ROOT/packaging/start.sh" "$STAGE/start.sh"
chmod +x "$STAGE/start.sh" "$STAGE"/*.AppImage "$STAGE/Cartridge.app/Contents/MacOS/cartridge"

OUT="$IN/cartridge-universal.zip"
rm -f "$OUT"
(cd "$WORK/out" && zip -qr -X "$OUT" Cartridge)
echo "Done: $OUT ($(du -sh "$OUT" | cut -f1))"
