#!/usr/bin/env bash
# Package the release assets that are not the compiled binary: one tarball per skill, and the
# scheduled pipeline scripts. Each asset gets a `.sha256` beside it, because `scripts/install.sh`
# verifies every asset it downloads and refuses one whose checksum is missing or does not match.
#
# A script rather than inline workflow YAML so the packaging can be RUN — by a maintainer before
# tagging, and by the test that holds it to what the installer expects. A property nobody can
# execute is checked by reading, and reading answers for the spelling rather than the result.
#
# Usage: package-release-assets.sh <version> [output-dir]
set -euo pipefail

VERSION="${1:?usage: package-release-assets.sh <version> [output-dir]}"
OUT="${2:-.}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"

# The pipelines the installer fetches by name. Held against `install.sh`'s own list by
# `every_downloaded_pipeline_is_checksummed_on_both_sides`.
PIPELINES="lore-pipeline.sh lore-daily.sh lore-weekly.sh"

checksum() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" > "$1.sha256"
    else
        shasum -a 256 "$1" > "$1.sha256"
    fi
}

mkdir -p "$OUT"
cd "$OUT"

# Enumerated from the directory, never listed: a skill added to `.claude/skills` must be packaged,
# published and installed without three separate literal lists each having to be remembered.
for dir in "$REPO"/.claude/skills/*/; do
    skill="$(basename "$dir")"
    archive="${skill}-skill-v${VERSION}.tar.gz"
    tar -czf "$archive" -C "$REPO/.claude/skills" "$skill"
    checksum "$archive"
done

for name in $PIPELINES; do
    cp "$REPO/scripts/${name}" .
    checksum "$name"
done
