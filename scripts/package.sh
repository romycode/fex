#!/bin/sh
set -eu

project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$project_root"
case "$(uname -s)" in
    Linux)
        cargo build --release --locked
        bundle=$(mktemp -d)
        trap 'rm -rf "$bundle"' EXIT
        mkdir -p "$bundle/fex"
        cp target/release/fex packaging/linux/fex.desktop packaging/linux/install.sh "$bundle/fex/"
        cp assets/icons/folder.svg "$bundle/fex/fex.svg"
        cp LICENSE README.md "$bundle/fex/"
        mkdir -p dist
        tar -C "$bundle" -czf "dist/fex-linux-$(uname -m).tar.gz" fex
        ;;
    Darwin)
        cargo build --release --locked --target aarch64-apple-darwin
        bundle=$(mktemp -d)
        trap 'rm -rf "$bundle"' EXIT
        mkdir -p "$bundle/Fex.app/Contents/MacOS" "$bundle/Fex.app/Contents/Resources"
        cp LICENSE "$bundle/Fex.app/Contents/Resources/"
        cp target/aarch64-apple-darwin/release/fex "$bundle/Fex.app/Contents/MacOS/fex"
        cp packaging/macos/Info.plist "$bundle/Fex.app/Contents/"
        mkdir -p dist
        ditto -c -k --keepParent "$bundle/Fex.app" dist/Fex-macos-arm64.zip
        ;;
    *)
        printf '%s\n' "Unsupported platform" >&2
        exit 1
        ;;
esac
