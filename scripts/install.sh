#!/usr/bin/env bash
# Lorekeeper installer — see `./install.sh --help` for full usage.
set -euo pipefail

REPO="junyeong-ai/lorekeeper"
BINARY_NAME="lore"
LATEST_URL="https://github.com/${REPO}/releases/latest"
API_LATEST_URL="https://api.github.com/repos/${REPO}/releases/latest"
RELEASE_BASE="https://github.com/${REPO}/releases/download"

# ── settings (env wins over built-in default; flags win over env) ─────────
EXPLICIT_INSTALL_DIR=0;  [ -n "${LORE_INSTALL_DIR:-}" ]         && EXPLICIT_INSTALL_DIR=1
EXPLICIT_SKILL_LEVEL=0;  [ -n "${LORE_INSTALL_SKILL_LEVEL:-}" ] && EXPLICIT_SKILL_LEVEL=1
EXPLICIT_FROM_SOURCE=0;  [ "${LORE_INSTALL_FROM_SOURCE:-0}" = "1" ] && EXPLICIT_FROM_SOURCE=1

INSTALL_DIR="${LORE_INSTALL_DIR:-$HOME/.local/bin}"
DATA_DIR="${LORE_INSTALL_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/lorekeeper}"
LORE_INSTALL_VERSION="${LORE_INSTALL_VERSION:-}"
LORE_INSTALL_SKILL_LEVEL="${LORE_INSTALL_SKILL_LEVEL:-}"
LORE_INSTALL_FROM_SOURCE="${LORE_INSTALL_FROM_SOURCE:-0}"
LORE_INSTALL_FORCE="${LORE_INSTALL_FORCE:-0}"
# A default run asks NOTHING and every decision resolves to its safe answer, so the documented
# one-liner is one command rather than one command and two questions. Asked whenever a terminal
# was merely reachable, `curl … | bash` prompted — /dev/tty is readable from a pipeline, so the
# ability to ask was taken for the instruction to.
INTERACTIVE="${LORE_INSTALL_INTERACTIVE:-0}"
DRY_RUN="${LORE_INSTALL_DRY_RUN:-0}"

INPUT_FD=""
TMP_DIR=""
USE_UTF8=0
C_RESET=""; C_DIM=""; C_RED=""; C_GREEN=""; C_YELLOW=""; C_BLUE=""; C_BOLD=""

# ═════════════════════════════ UTIL ════════════════════════════════════════

# All human-visible output goes to stderr so stdout is reserved for values
# captured via command substitution.
die()      { printf '%s✗ %s%s\n' "$C_RED" "$*" "$C_RESET" >&2; exit 1; }
log_info() { printf '%s  %s%s\n' "$C_DIM" "$*" "$C_RESET" >&2; }
log_warn() { printf '%s!  %s%s\n' "$C_YELLOW" "$*" "$C_RESET" >&2; }
log_ok()   { printf '%s✓  %s%s\n' "$C_GREEN" "$*" "$C_RESET" >&2; }
render_step() { printf '%s▸  %s%s\n' "$C_BLUE" "$*" "$C_RESET" >&2; }

init_colors() {
    if [ -t 1 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-}" != "dumb" ]; then
        C_RESET=$'\033[0m'
        C_DIM=$'\033[2m'
        C_RED=$'\033[31m'
        C_GREEN=$'\033[32m'
        C_YELLOW=$'\033[33m'
        C_BLUE=$'\033[34m'
        C_BOLD=$'\033[1m'
    fi
    case "${LANG:-}${LC_ALL:-}" in *UTF-8*|*utf8*) USE_UTF8=1 ;; esac
}

# Where a prompt would READ from, which is a different question from whether to prompt at all.
detect_tty() {
    if [ -t 0 ]; then INPUT_FD="0"; return 0; fi
    if [ -e /dev/tty ] && [ -r /dev/tty ]; then INPUT_FD="/dev/tty"; return 0; fi
    INPUT_FD=""; return 1
}

# Whether this run asks. Both halves: the mode is chosen and the terminal is reachable.
asking() { [ "$INTERACTIVE" = "1" ] && [ -n "$INPUT_FD" ]; }

read_line() {
    local answer
    if [ "$INPUT_FD" = "0" ]; then
        IFS= read -r answer || answer=""
    else
        IFS= read -r answer < /dev/tty || answer=""
    fi
    printf '%s' "$answer"
}

# ═════════════════════════════ PROMPTS ═════════════════════════════════════

prompt_choice() {
    local title="$1"; shift
    local default_idx="$1"; shift
    local options=("$@")
    local i answer

    if [ -z "$INPUT_FD" ]; then
        printf '%s\n' "${options[$((default_idx - 1))]}"
        return 0
    fi

    printf '\n%s%s%s\n' "$C_BOLD" "$title" "$C_RESET" >&2
    for i in "${!options[@]}"; do
        printf '  %s%d)%s %s\n' "$C_DIM" "$((i + 1))" "$C_RESET" "${options[$i]}" >&2
    done
    for _ in 1 2 3; do
        printf '%s❯ [%d]%s ' "$C_BLUE" "$default_idx" "$C_RESET" >&2
        answer="$(read_line)"
        answer="${answer:-$default_idx}"
        if [[ "$answer" =~ ^[0-9]+$ ]] && [ "$answer" -ge 1 ] && [ "$answer" -le "${#options[@]}" ]; then
            printf '%s\n' "${options[$((answer - 1))]}"
            return 0
        fi
        log_warn "Invalid choice: $answer"
    done
    die "Too many invalid responses"
}

prompt_yesno() {
    local question="$1" default="$2" answer
    if [ -z "$INPUT_FD" ]; then
        [ "$default" = "Y" ] && return 0 || return 1
    fi
    local hint; [ "$default" = "Y" ] && hint="[Y/n]" || hint="[y/N]"
    printf '%s%s%s %s ' "$C_BOLD" "$question" "$C_RESET" "$hint" >&2
    answer="$(read_line)"
    answer="${answer:-$default}"
    case "$answer" in [Yy]*) return 0 ;; *) return 1 ;; esac
}

prompt_path() {
    local question="$1" default="$2" answer
    if [ -z "$INPUT_FD" ]; then printf '%s\n' "$default"; return; fi
    printf '%s%s%s [%s] ' "$C_BOLD" "$question" "$C_RESET" "$default" >&2
    answer="$(read_line)"
    answer="${answer:-$default}"
    case "$answer" in "~"*) answer="$HOME${answer:1}" ;; esac
    printf '%s\n' "$answer"
}

# ═════════════════════════════ DETECT ══════════════════════════════════════

detect_platform() {
    local os arch
    os="$(uname -s | tr '[:upper:]' '[:lower:]')"
    arch="$(uname -m)"
    case "$os-$arch" in
        linux-x86_64)              echo "x86_64-unknown-linux-musl" ;;
        linux-aarch64|linux-arm64) echo "aarch64-unknown-linux-musl" ;;
        darwin-arm64)              echo "aarch64-apple-darwin" ;;
        *) die "Unsupported platform: $os/$arch (Intel Mac is not supported; use --from-source)" ;;
    esac
}

# The latest published release.
#
# Two sources answer this and they disagree for minutes at a time: the release page is a
# cached view that trails the API right after a release is published, which is exactly when
# someone runs this. Read in that window it names the release before, and the install lands on
# it while reporting success. So the API settles it, and the page answers only where the API
# could not — its rate limit counts against an IP a shared runner can exhaust, and the page has
# no limit to exhaust.
fetch_latest_version() {
    local tag
    tag="$(curl -fsSL --retry 3 --retry-delay 2 \
        -H 'Accept: application/vnd.github+json' "$API_LATEST_URL" 2>/dev/null \
        | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"v\{0,1\}\([^"]*\)".*/\1/p' \
        | head -n1)"
    if [ -n "$tag" ]; then printf '%s\n' "$tag"; return 0; fi

    local final
    final="$(curl -fsSLI --retry 3 --retry-delay 2 -o /dev/null \
        -w '%{url_effective}' "$LATEST_URL")" || return 1
    case "$final" in
        */tag/v*) printf '%s\n' "${final##*/tag/v}" ;;
        *) return 1 ;;
    esac
}

resolve_version() {
    # Every URL below is built as `v${version}`, so the version here is the bare number. The tag
    # on the releases page is `v0.16.1` and that is what a user copies, which made
    # `--version v0.16.1` request `.../vv0.16.1/lore-vv0.16.1-…` and 404 with a download error
    # naming neither cause.
    if [ -n "$LORE_INSTALL_VERSION" ]; then echo "${LORE_INSTALL_VERSION#v}"; return; fi
    local v; v="$(fetch_latest_version || true)"
    [ -n "$v" ] || die "Cannot fetch latest version (network issue or no release exists yet)"
    echo "$v"
}

# ═════════════════════════════ DOWNLOAD/INSTALL ════════════════════════════

download_archive() {
    local version="$1" archive_name="$2"
    local url="${RELEASE_BASE}/v${version}/${archive_name}"
    render_step "Downloading ${archive_name}"
    curl -fL --retry 3 --retry-delay 2 --progress-bar \
        -o "${TMP_DIR}/${archive_name}" "$url" \
        || die "Download failed: $url"
    curl -fsSL --retry 3 --retry-delay 2 \
        -o "${TMP_DIR}/${archive_name}.sha256" "${url}.sha256" \
        || die "Checksum download failed: ${url}.sha256"
    log_ok "Downloaded"
}

verify_checksum() {
    local archive="$1"
    render_step "Verifying SHA256"
    ( cd "$TMP_DIR" && {
        if command -v sha256sum >/dev/null 2>&1; then
            sha256sum -c "${archive}.sha256" >/dev/null
        elif command -v shasum >/dev/null 2>&1; then
            shasum -a 256 -c "${archive}.sha256" >/dev/null
        else
            die "No sha256 tool found (need sha256sum or shasum)"
        fi
    } ) || die "Checksum mismatch for ${archive}"
    log_ok "Checksum match"
}

extract_archive() {
    local archive="$1"
    render_step "Extracting"
    case "$archive" in
        *.tar.gz) tar -xzf "${TMP_DIR}/${archive}" -C "${TMP_DIR}" ;;
        *.zip)    unzip -q "${TMP_DIR}/${archive}" -d "${TMP_DIR}" ;;
        *)        die "Unknown archive format: $archive" ;;
    esac
    log_ok "Extracted"
}

strip_quarantine() {
    [ "$(uname -s)" = "Darwin" ] || return 0
    command -v xattr >/dev/null 2>&1 || return 0
    xattr -d com.apple.quarantine "$1" 2>/dev/null || true
}

codesign_adhoc() {
    [ "$(uname -s)" = "Darwin" ] || return 0
    command -v codesign >/dev/null 2>&1 || return 0
    codesign --force --sign - "$1" 2>/dev/null || true
}

install_binary() {
    local src="$1"
    local dest_dir="$2"
    local dest="${dest_dir}/${BINARY_NAME}"
    render_step "Installing binary to ${dest}"
    mkdir -p "$dest_dir"
    cp "$src" "$dest.tmp"
    chmod +x "$dest.tmp"
    strip_quarantine "$dest.tmp"
    codesign_adhoc "$dest.tmp"
    mv "$dest.tmp" "$dest"
    log_ok "$dest"
}

# Drop config.example.yaml into the XDG config dir so a binary-only install (no git
# clone) has a template to copy to config.yaml. `lore` auto-discovers
# ~/.config/lorekeeper/config.yaml, so this is where users should put their real config.
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/lorekeeper"
build_from_source() {
    local repo_dir="$1"
    render_step "Building from source (cargo build --release --locked)"
    command -v cargo >/dev/null 2>&1 || die "cargo not found — install Rust from https://rustup.rs"
    ( cd "$repo_dir" && cargo build --release --locked --quiet -p lore ) || die "cargo build failed"
    echo "${repo_dir}/target/release/${BINARY_NAME}"
}

# ═════════════════════════════ SKILL ═══════════════════════════════════════

compare_versions() {
    # echoes: equal | older | newer | unknown
    local a="${1#v}" b="${2#v}"
    [ -z "$a" ] || [ -z "$b" ] && { echo "unknown"; return; }
    [ "$a" = "$b" ] && { echo "equal"; return; }
    # `sort -V` orders `1.0.0` BEFORE `1.0.0-beta.1`; semver is the other way round, a
    # prerelease precedes its release. Installing 1.0.0 over an installed 1.0.0-beta.1 therefore
    # asked "Installed v1.0.0-beta.1 is newer — Downgrade?", whose default answer keeps the beta.
    # Compared as (release, prerelease) so the release part decides first and, on a tie, the side
    # WITH a prerelease is the older one.
    local a_rel="${a%%-*}" b_rel="${b%%-*}"
    local a_pre="" b_pre=""
    [ "$a" != "$a_rel" ] && a_pre="${a#*-}"
    [ "$b" != "$b_rel" ] && b_pre="${b#*-}"

    if [ "$a_rel" != "$b_rel" ]; then
        local first
        first="$(printf '%s\n%s\n' "$a_rel" "$b_rel" | sort -V | head -n1)"
        [ "$first" = "$a_rel" ] && echo "older" || echo "newer"
        return
    fi
    if [ -z "$a_pre" ]; then echo "newer"; return; fi   # b has one, a does not
    if [ -z "$b_pre" ]; then echo "older"; return; fi   # a has one, b does not
    local first
    first="$(printf '%s\n%s\n' "$a_pre" "$b_pre" | sort -V | head -n1)"
    [ "$first" = "$a_pre" ] && echo "older" || echo "newer"
}

# The version a checkout declares, read from `[workspace.package]` — single-sourced there, so
# every crate inherits it and `crates/*/Cargo.toml` no longer carries a literal.
#
# The quoted value is taken whole rather than filtered down to digits and dots: a prerelease or
# build metadata (`1.2.3-beta.4+build.5`) is a valid version that filtering silently rewrote into
# a different one. Leading whitespace is allowed because TOML allows it, and a key that only ever
# matched at column zero returned nothing at all for an indented manifest — which reads as "no
# version" and sends a source install downloading `dev` assets.
repo_version() {
    local repo="$1"
    awk '/^[[:space:]]*\[workspace\.package\][[:space:]]*$/ { in_section = 1; next }
         /^[[:space:]]*\[/                                 { in_section = 0 }
         in_section && match($0, /^[[:space:]]*version[[:space:]]*=[[:space:]]*["'"'"']/) {
             value = substr($0, RLENGTH + 1)
             sub(/["'"'"'].*$/, "", value)
             print value
             exit
         }' "$repo/Cargo.toml" 2>/dev/null
}

# Everything besides the binary is written by the binary.
#
# The skills, the pipeline scripts, the rendering templates and the config example are
# compiled into `lore`, so the version that deploys them is the version that carries them —
# there is no second artifact to fetch, verify, or find a stale copy of. This is also what a
# later `lore self update` runs, so an install and an update leave the same directories.
deploy_artifacts() {
    local bin="$1" skill_level="$2"
    # A release older than the one that introduced `lore self` carries none of this and cannot
    # deploy it. `--version` is a documented flag, so pinning such a release must say what
    # happened rather than fail on an unknown subcommand and then advise running it.
    if ! "$bin" self --help >/dev/null 2>&1; then
        log_warn "This version has no 'lore self' — its skills and pipelines were published as"
        log_warn "separate release assets. Install a newer version to deploy them from the binary."
        return 0
    fi
    render_step "Deploying skills, pipelines and templates"
    "$bin" self deploy --skills "$skill_level" --data-dir "$DATA_DIR" \
        || die "Deploy failed; the binary is installed — run: $bin self deploy"
}

# Print how to schedule what was just installed, with this machine's paths resolved.
#
# `--pipeline-dir` is what makes the daily and weekly entries run the installed scripts
# rather than bare `lore ingest` / `lore synthesis weekly`. Those subcommands are only the
# FIRST stage of their pipeline: the queue drain and `lore queue apply` live in the scripts,
# so scheduling the bare commands ingests every morning and never fills a summary or
# materializes a concept. Run interactively, `lore schedule` also picks up the PATH and the
# `claude` binary the scripts need, which a scheduler does not provide on its own.
print_pipeline_schedule() {
    # The binary THIS run installed, not whatever `lore` PATH resolves to. A first install into
    # a directory not yet on PATH — the case this script warns about a few lines later — would
    # otherwise print an older `lore` found elsewhere, and the scheduled job would keep running
    # that one. The plist needs an absolute path either way, and only one of them is knowable.
    local lore_bin="${INSTALL_DIR}/${BINARY_NAME}"

    if [ "$(uname -s)" = "Darwin" ]; then
        printf '       %slore schedule --format launchd --bin %s \\\n' "$C_BOLD" "$lore_bin"
        printf '            --pipeline-dir %s/pipelines%s\n' "$DATA_DIR" "$C_RESET"
        printf '       Review the plists it prints, write them to ~/Library/LaunchAgents/,\n'
        printf '       then load each with launchctl bootstrap (the output shows how).\n'
        printf '       launchd runs a job missed during sleep; cron silently skips it.\n'
    else
        printf '       %slore schedule --pipeline-dir %s/pipelines%s | crontab -\n' \
            "$C_BOLD" "$DATA_DIR" "$C_RESET"
    fi
}

# Legacy Claude Code scheduled-task definitions, installed by versions up to 0.10. They
# scheduled `lore ingest` through Claude Desktop, which meant a day was silently skipped
# whenever the app was not running; the pipelines above replace them with a system
# scheduler. Their definitions also describe a drain contract the code no longer has, so
# leaving them in place would keep a stale agent spec on disk.
LEGACY_SCHEDULED_TASKS="lore-daily-ingest lore-weekly-ingest"

remove_legacy_scheduled_tasks() {
    local name dir removed=0
    for name in $LEGACY_SCHEDULED_TASKS; do
        dir="$HOME/.claude/scheduled-tasks/${name}"
        if [ -d "$dir" ]; then
            rm -rf "$dir"
            removed=$((removed + 1))
        fi
    done
    if [ "$removed" -gt 0 ]; then
        log_ok "Removed ${removed} superseded scheduled task(s); schedule the pipelines instead"
        log_info "Also drop their entries from ~/.claude/scheduled-tasks/registry.json if present"
    fi
}

# ═════════════════════════════ ORCHESTRATION ═══════════════════════════════

print_usage() {
    cat <<'USAGE'
Lorekeeper installer

Usage:
  curl -fsSL https://raw.githubusercontent.com/junyeong-ai/lorekeeper/main/scripts/install.sh | bash
  ./scripts/install.sh [flags]

Flags:
  --version VERSION          Install specific version (default: latest)
  --install-dir PATH         Install binary here (default: $HOME/.local/bin)
  --data-dir PATH            Install templates here
                             (default: $XDG_DATA_HOME/lorekeeper or $HOME/.local/share/lorekeeper)
  --skill user|project|none  Skill install level (default: user)
  --from-source              Build from source instead of downloading prebuilt
  --force                    Overwrite existing install without prompting
  --interactive, -i          Ask before each decision instead of taking the defaults
  --yes, -y                  Take the defaults without asking (this is the default)
  --dry-run                  Print plan, do not execute
  --help, -h                 Show this message

A default run asks nothing: the prebuilt binary for this platform into
$HOME/.local/bin, with the skills at user level. Each of those has a flag, and
--interactive restores the guided prompts — they read /dev/tty, so they work
even under `curl | bash`.

Environment variables (flags win over env, env wins over defaults):
  LORE_INSTALL_DIR, LORE_INSTALL_DATA_DIR, LORE_INSTALL_VERSION,
  LORE_INSTALL_SKILL_LEVEL, LORE_INSTALL_FROM_SOURCE, LORE_INSTALL_INTERACTIVE,
  LORE_INSTALL_FORCE, LORE_INSTALL_DRY_RUN, NO_COLOR
USAGE
}

parse_args() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --version)       LORE_INSTALL_VERSION="$2"; shift 2 ;;
            --install-dir)   INSTALL_DIR="$2"; EXPLICIT_INSTALL_DIR=1; shift 2 ;;
            --data-dir)      DATA_DIR="$2"; shift 2 ;;
            --skill)         LORE_INSTALL_SKILL_LEVEL="$2"; EXPLICIT_SKILL_LEVEL=1; shift 2 ;;
            --from-source)   LORE_INSTALL_FROM_SOURCE=1; EXPLICIT_FROM_SOURCE=1; shift ;;
            --force)         LORE_INSTALL_FORCE=1; shift ;;
            --interactive|-i) INTERACTIVE=1; shift ;;
            --yes|-y)        INTERACTIVE=0; shift ;;
            --dry-run)       DRY_RUN=1; shift ;;
            --help|-h)       print_usage; exit 0 ;;
            *)               die "Unknown flag: $1 (use --help)" ;;
        esac
    done
}

render_banner() {
    local platform="$1" version="$2"
    local top bot
    if [ "$USE_UTF8" = "1" ]; then
        top="╭──────────────────────────────────────────╮"
        bot="╰──────────────────────────────────────────╯"
    else
        top="+------------------------------------------+"
        bot="+------------------------------------------+"
    fi
    printf '\n%s%s%s\n' "$C_BOLD" "$top" "$C_RESET"
    printf '%s  Lorekeeper installer%s\n' "$C_BOLD" "$C_RESET"
    printf '%s  v%s • %s%s\n' "$C_DIM" "$version" "$platform" "$C_RESET"
    printf '%s%s%s\n' "$C_BOLD" "$bot" "$C_RESET"
}

render_review() {
    local method="$1" bin_dest="$2" data_dest="$3" skill_level="$4" version="$5"
    printf '\n%sReview%s\n' "$C_BOLD" "$C_RESET"
    printf '  %sbinary%s    %s (v%s, %s)\n' "$C_DIM" "$C_RESET" "$bin_dest" "$version" "$method"
    printf '  %stemplates%s %s/templates\n' "$C_DIM" "$C_RESET" "$data_dest"
    case "$skill_level" in
        user)    printf '  %sskills%s    ~/.claude/skills/lore-*\n' "$C_DIM" "$C_RESET" ;;
        project) printf '  %sskills%s    ./.claude/skills/lore-*\n' "$C_DIM" "$C_RESET" ;;
        none)    printf '  %sskill%s     (skipped)\n' "$C_DIM" "$C_RESET" ;;
    esac
    printf '  %spipeline%s  %s/pipelines/lore-{daily,weekly}.sh\n' "$C_DIM" "$C_RESET" "$DATA_DIR"
}

check_path() {
    local dir="$1"
    case ":$PATH:" in
        *":$dir:"*) log_ok "$dir is in PATH" ;;
        *)
            log_warn "$dir is not in PATH"
            echo "   Add to your shell profile:"
            echo "     export PATH=\"$dir:\$PATH\""
            ;;
    esac
}

cleanup() { [ -n "$TMP_DIR" ] && [ -d "$TMP_DIR" ] && rm -rf "$TMP_DIR"; }

check_writable() {
    local dir="$1"
    # Walk up the path until we find an existing directory, then check it
    # is a writable directory. mkdir -p will create the missing segments.
    while [ -n "$dir" ] && [ ! -e "$dir" ]; do
        local parent; parent="$(dirname "$dir")"
        [ "$parent" = "$dir" ] && return 1
        dir="$parent"
    done
    [ -d "$dir" ] && [ -w "$dir" ]
}

main() {
    init_colors
    parse_args "$@"
    trap cleanup EXIT INT TERM
    TMP_DIR="$(mktemp -d 2>/dev/null || mktemp -d -t lore-install)"

    detect_tty || true
    if [ "$INTERACTIVE" = "1" ] && [ -z "$INPUT_FD" ]; then
        die "--interactive was asked for and there is no terminal to ask at"
    fi
    local platform="" version method bin_dest skill_level binary_src

    local repo_dir=""
    if [ -f "$(dirname "$0")/../Cargo.toml" ]; then
        repo_dir="$(cd "$(dirname "$0")/.." && pwd)"
    fi

    if [ "$EXPLICIT_FROM_SOURCE" = "1" ] || ! asking; then
        method=$([ "$LORE_INSTALL_FROM_SOURCE" = "1" ] && echo "source" || echo "prebuilt")
    else
        local pick
        pick="$(prompt_choice "Install method" 1 \
            "Prebuilt binary        (recommended)" \
            "Build from source      (requires Rust)")"
        case "$pick" in Prebuilt*) method="prebuilt" ;; Build*) method="source" ;; esac
    fi

    if [ "$method" = "prebuilt" ]; then
        platform="$(detect_platform)"
        version="$(resolve_version)"
    else
        platform="$(uname -s | tr '[:upper:]' '[:lower:]')-$(uname -m)"
        version="$(repo_version "${repo_dir:-.}")"
        [ -n "$version" ] || version="dev"
    fi

    render_banner "$platform" "$version"

    if asking && [ "$EXPLICIT_INSTALL_DIR" != "1" ]; then
        local loc
        # The `~` here is display text and, below, the label to match it against — neither is a
        # path being resolved, so quoting it is correct and expanding it would break the match.
        # shellcheck disable=SC2088
        loc="$(prompt_choice "Install location" 1 \
            "~/.local/bin          (recommended)" \
            "/usr/local/bin        (may need sudo)" \
            "Custom path…")"
        # shellcheck disable=SC2088
        case "$loc" in
            "~/.local/bin"*)   INSTALL_DIR="$HOME/.local/bin" ;;
            "/usr/local/bin"*) INSTALL_DIR="/usr/local/bin" ;;
            "Custom"*)         INSTALL_DIR="$(prompt_path "Install path" "$HOME/.local/bin")" ;;
        esac
    fi
    bin_dest="${INSTALL_DIR}/${BINARY_NAME}"

    if [ "$EXPLICIT_SKILL_LEVEL" = "1" ]; then
        skill_level="$LORE_INSTALL_SKILL_LEVEL"
    elif ! asking; then
        skill_level="user"
    else
        local pick
        pick="$(prompt_choice "Claude Code skill" 1 \
            "User-level            ~/.claude/skills/lore-*" \
            "Project-level         ./.claude/skills/lore-*" \
            "Skip")"
        case "$pick" in
            User-level*)    skill_level="user" ;;
            Project-level*) skill_level="project" ;;
            Skip)           skill_level="none" ;;
        esac
    fi

    case "$skill_level" in user|project|none) ;;
        *) die "Invalid skill level: $skill_level (expected user|project|none)" ;;
    esac

    render_review "$method" "$bin_dest" "$DATA_DIR" "$skill_level" "$version"

    if [ "$DRY_RUN" = "1" ]; then
        printf '\n%s(dry-run) Not executing%s\n' "$C_YELLOW" "$C_RESET"
        exit 0
    fi

    if asking && ! prompt_yesno "Proceed?" "Y"; then
        log_info "Aborted by user"; exit 0
    fi

    # A source install always publishes what it just built. Two commits share a version, so
    # "v1.2.3 already installed" said nothing about whether the binary matches this checkout —
    # and the skills and pipelines beside it are taken from the checkout unconditionally, which
    # left an old binary running against new skills.
    local skip_binary=0
    if [ -f "$bin_dest" ] && [ "$LORE_INSTALL_FORCE" != "1" ] && [ "$method" != "source" ]; then
        local existing; existing="$("$bin_dest" --version 2>/dev/null | awk '{print $2}' || echo "")"
        local cmp; cmp="$(compare_versions "$existing" "$version")"
        case "$cmp" in
            equal)
                prompt_yesno "lore v$existing already installed. Reinstall?" "N" || { log_info "Kept existing install"; skip_binary=1; } ;;
            newer)
                prompt_yesno "Installed v$existing is newer than v$version. Downgrade?" "N" || { log_info "Kept existing install"; skip_binary=1; } ;;
            older|unknown) : ;;
        esac
    fi

    printf '\n'

    if [ "$skip_binary" != "1" ]; then
        if ! check_writable "$INSTALL_DIR"; then
            die "Install dir not writable: $INSTALL_DIR
  Try:   ./scripts/install.sh --install-dir \"\$HOME/.local/bin\"
  Or:    sudo ./scripts/install.sh --install-dir \"$INSTALL_DIR\""
        fi
        case "$method" in
            prebuilt)
                local archive="${BINARY_NAME}-v${version}-${platform}.tar.gz"
                download_archive "$version" "$archive"
                verify_checksum "$archive"
                extract_archive "$archive"
                # The release archive contains a top-level `lore-v{ver}-{target}/` dir
                # (see .github/workflows/release.yml), so the binary lives one level down
                # from TMP_DIR.
                local stage="${TMP_DIR}/${BINARY_NAME}-v${version}-${platform}"
                binary_src="${stage}/${BINARY_NAME}"
                ;;
            source)
                [ -n "$repo_dir" ] || die "--from-source requires running from a cloned repo"
                binary_src="$(build_from_source "$repo_dir")"
                ;;
        esac
        install_binary "$binary_src" "$INSTALL_DIR"
    fi

    deploy_artifacts "${INSTALL_DIR}/${BINARY_NAME}" "$skill_level"
    remove_legacy_scheduled_tasks

    printf '\n'
    check_path "$INSTALL_DIR"
    printf '\n%s✅ Installation complete%s\n' "$C_GREEN$C_BOLD" "$C_RESET"
    printf '\nNext steps:\n'
    printf '  %s1.%s Create your config (auto-discovered, no repo needed):\n' "$C_BOLD" "$C_RESET"
    printf '       cp %s/config.example.yaml %s/config.yaml\n' "$CONFIG_DIR" "$CONFIG_DIR"
    printf '       $EDITOR %s/config.yaml\n' "$CONFIG_DIR"
    printf '  %s2.%s %slore init credentials%s   Enter API tokens interactively\n' "$C_BOLD" "$C_RESET" "$C_BOLD" "$C_RESET"
    printf '  %s3.%s %slore validate%s           Verify config + credentials\n' "$C_BOLD" "$C_RESET" "$C_BOLD" "$C_RESET"
    printf '  %s4.%s %slore ingest --dry-run%s   Preview ingest without writing\n' "$C_BOLD" "$C_RESET" "$C_BOLD" "$C_RESET"
    printf '  %s5.%s Schedule the two pipelines, then the janitors:\n' "$C_BOLD" "$C_RESET"
    print_pipeline_schedule
}

main "$@"
