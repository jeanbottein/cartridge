#!/bin/sh
# Run the •cartridge app built for this machine (Linux, macOS): every app in
# this folder shares its system/ and games. Arguments are passed on
# (./start.sh --serve). On Windows, run cartridge.exe.
dir=$(cd "$(dirname "$0")" && pwd)
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) app=Cartridge-x86_64.AppImage ;;
    Linux-aarch64 | Linux-arm64) app=Cartridge-arm64.AppImage ;;
    # The binary inside the bundle, not `open`: logs stay in the terminal.
    Darwin-arm64) app=Cartridge.app/Contents/MacOS/cartridge ;;
    *)
        echo "No •cartridge for $(uname -sm) in this folder" >&2
        exit 1
        ;;
esac
# Without FUSE (containers), the AppImage unpacks itself to /tmp instead.
case "$app" in
    *.AppImage) [ -e /dev/fuse ] || export APPIMAGE_EXTRACT_AND_RUN=1 ;;
esac
exec "$dir/$app" "$@"
