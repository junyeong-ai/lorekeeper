#!/usr/bin/env bash
# The same real external-tool contract test runs locally and in CI. Runtime admits
# >=0.66 without an upper bound; the gate pins the floor and its nearest predecessor.
set -euo pipefail

cd "$(dirname "$0")/.."

stage=""
trap 'if [ -n "$stage" ]; then rm -rf "$stage"; fi' EXIT

supported="${LORE_TEST_NODEX_SUPPORTED:-}"
unsupported="${LORE_TEST_NODEX_UNSUPPORTED:-}"
if [ -n "$supported" ] || [ -n "$unsupported" ]; then
    [ -n "$supported" ] && [ -n "$unsupported" ] || {
        echo "Set both LORE_TEST_NODEX_SUPPORTED and LORE_TEST_NODEX_UNSUPPORTED" >&2
        exit 1
    }
else
    case "$(uname -s)-$(uname -m)" in
        Darwin-*)
            target=universal-apple-darwin
            extension=tar.gz
            supported_sha=1c012442c94d084c53a038c30db53357a2a12deb86d9d11f9854bc6d241a6adf
            unsupported_sha=2b64abc346fa98c6cae3744bdc7d980bf4719266723df9f3cf5bf4740cf1c894
            ;;
        Linux-x86_64)
            target=x86_64-unknown-linux-musl
            extension=tar.gz
            supported_sha=c4a716eab8c043746fc46edec86afe940ee566a177db0f31d0d9a12ebb29a92b
            unsupported_sha=5174348d65123219e5b097387093b945fdf4fa8b47818b92772e4b6e7560b993
            ;;
        MINGW*-x86_64|MSYS*-x86_64)
            target=x86_64-pc-windows-msvc
            extension=zip
            supported_sha=a566a841d48050bcdca5b8de4c7d1187a47b5a385fccc81b4f0bd04bb13d0263
            unsupported_sha=897814596613063d3b6cb5d03040fb37f11e6203907672dad12fcb905300fc2c
            ;;
        *)
            echo "No pinned Nodex pair for this host; supply both absolute binary paths" >&2
            exit 1
            ;;
    esac
    stage="$(mktemp -d)"
    for version in 0.65.0 0.66.0; do
        directory="$stage/$version"
        mkdir "$directory"
        archive="$directory/nodex.$extension"
        curl --fail --silent --show-error --location --retry 3 --output "$archive" \
            "https://github.com/junyeong-ai/nodex/releases/download/v$version/nodex-v$version-$target.$extension"
        expected="$supported_sha"
        [ "$version" != 0.65.0 ] || expected="$unsupported_sha"
        if command -v sha256sum >/dev/null 2>&1; then
            actual="$(sha256sum "$archive" | awk '{print $1}')"
        else
            actual="$(shasum -a 256 "$archive" | awk '{print $1}')"
        fi
        [ "$actual" = "$expected" ] || { echo "Nodex $version checksum mismatch" >&2; exit 1; }
        if [ "$extension" = zip ]; then
            binary="$directory/nodex.exe"
            python -c 'import sys, zipfile; sys.stdout.buffer.write(zipfile.ZipFile(sys.argv[1]).read("nodex.exe"))' "$archive" > "$binary"
            binary="$(cygpath -aw "$binary")"
        else
            binary="$directory/nodex"
            tar -xzOf "$archive" nodex > "$binary"
            chmod 700 "$binary"
        fi
        if [ "$version" = 0.66.0 ]; then supported="$binary"; else unsupported="$binary"; fi
    done
fi

LORE_TEST_NODEX_SUPPORTED="$supported" LORE_TEST_NODEX_UNSUPPORTED="$unsupported" \
    cargo nextest run --locked -p lk-source --lib --run-ignored only \
    -E 'test(selected_real_binaries_preserve_compatibility_and_content_refusal)'
