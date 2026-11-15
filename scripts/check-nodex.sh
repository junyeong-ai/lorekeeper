#!/usr/bin/env bash
# The same real external-tool contract test runs locally and in CI. Runtime admits
# >=0.69 without an upper bound; the gate pins the floor and its nearest predecessor.
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
            supported_sha=5c19932c6a35a64cfe50982c4d5a1924167f69d9bdd76168624ca90731de7027
            unsupported_sha=cb50a1ac25625e0cf5c97f3728dc693d740ffc172d58f89a3c34fe22b2c1364f
            ;;
        Linux-x86_64)
            target=x86_64-unknown-linux-musl
            extension=tar.gz
            supported_sha=c4a5a034bdb0f3c65a6e8c3f14d8e8e9c8032ae95ce55bed756f7057c2dc6d17
            unsupported_sha=191f5d4cb89e9b02c7cddfa4d2d2dfa5a7db90e8bd7e065b820ff018faf60677
            ;;
        MINGW*-x86_64|MSYS*-x86_64)
            target=x86_64-pc-windows-msvc
            extension=zip
            supported_sha=6cf6b17a8ccbed78debbc2ae20e56e7087f83a8fc160a57051ed13adf9d55de7
            unsupported_sha=44530846fe657307ee7542bd4a5c14ded9019a5814b16088793b31ec328c8f2e
            ;;
        *)
            echo "No pinned Nodex pair for this host; supply both absolute binary paths" >&2
            exit 1
            ;;
    esac
    stage="$(mktemp -d)"
    for version in 0.68.0 0.69.0; do
        directory="$stage/$version"
        mkdir "$directory"
        archive="$directory/nodex.$extension"
        curl --fail --silent --show-error --location --retry 3 --output "$archive" \
            "https://github.com/junyeong-ai/nodex/releases/download/v$version/nodex-v$version-$target.$extension"
        expected="$supported_sha"
        [ "$version" != 0.68.0 ] || expected="$unsupported_sha"
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
        if [ "$version" = 0.69.0 ]; then supported="$binary"; else unsupported="$binary"; fi
    done
fi

LORE_TEST_NODEX_SUPPORTED="$supported" LORE_TEST_NODEX_UNSUPPORTED="$unsupported" \
    cargo nextest run --locked -p lk-source --lib --run-ignored only \
    -E 'test(selected_real_binaries_preserve_compatibility_and_content_refusal)'
