#!/usr/bin/env bash
# Lorekeeper installer — see `./install.sh --help` for full usage.
set -euo pipefail

REPO="junyeong-ai/lorekeeper"
BINARY_NAME="lore"
SKILL_NAME="lorekeeper"
LATEST_URL="https://github.com/${REPO}/releases/latest"
RELEASE_BASE="https://github.com/${REPO}/releases/download"

# ── settings (env wins over built-in default; flags win over env) ─────────
EXPLICIT_INSTALL_DIR=0;  [ -n "${LORE_INSTALL_DIR:-}" ]         && EXPLICIT_INSTALL_DIR=1
EXPLICIT_VERSION=0;      [ -n "${LORE_INSTALL_VERSION:-}" ]     && EXPLICIT_VERSION=1
EXPLICIT_SKILL_LEVEL=0;  [ -n "${LORE_INSTALL_SKILL_LEVEL:-}" ] && EXPLICIT_SKILL_LEVEL=1
EXPLICIT_FROM_SOURCE=0;  [ "${LORE_INSTALL_FROM_SOURCE:-0}" = "1" ] && EXPLICIT_FROM_SOURCE=1

INSTALL_DIR="${LORE_INSTALL_DIR:-$HOME/.local/bin}"
DATA_DIR="${LORE_INSTALL_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/lorekeeper}"
LORE_INSTALL_VERSION="${LORE_INSTALL_VERSION:-}"
LORE_INSTALL_SKILL_LEVEL="${LORE_INSTALL_SKILL_LEVEL:-}"
LORE_INSTALL_FROM_SOURCE="${LORE_INSTALL_FROM_SOURCE:-0}"
LORE_INSTALL_FORCE="${LORE_INSTALL_FORCE:-0}"
LORE_INSTALL_YES="${LORE_INSTALL_YES:-0}"
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

detect_tty() {
    if [ "$LORE_INSTALL_YES" = "1" ]; then INPUT_FD=""; return 1; fi
    if [ -t 0 ]; then INPUT_FD="0"; return 0; fi
    if [ -e /dev/tty ] && [ -r /dev/tty ]; then INPUT_FD="/dev/tty"; return 0; fi
    INPUT_FD=""; return 1
}

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

fetch_latest_version() {
    # Resolve through GitHub's stable HTML redirect rather than api.github.com.
    # The HTML path is auth-free and not subject to the 60-req/hr rate limit.
    local final
    final="$(curl -fsSLI --retry 3 --retry-delay 2 -o /dev/null \
        -w '%{url_effective}' "$LATEST_URL")" || return 1
    case "$final" in
        */tag/v*) printf '%s\n' "${final##*/tag/v}" ;;
        *) return 1 ;;
    esac
}

resolve_version() {
    if [ -n "$LORE_INSTALL_VERSION" ]; then echo "$LORE_INSTALL_VERSION"; return; fi
    local v; v="$(fetch_latest_version || true)"
    [ -n "$v" ] || die "Cannot fetch latest version (network issue or no release exists yet)"
    echo "$v"
}

# ═════════════════════════════ DOWNLOAD/INSTALL ════════════════════════════

download_archive() {
    local version="$1" target="$2" archive_name="$3"
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

install_templates() {
    local src_dir="$1"
    local dest_dir="$2/templates"
    [ -d "$src_dir" ] || { log_warn "Templates not found at $src_dir; skipping"; return; }
    render_step "Installing templates to ${dest_dir}"
    mkdir -p "$dest_dir"
    # Copy every *.md.jinja file
    find "$src_dir" -maxdepth 1 -name '*.md.jinja' -exec cp {} "$dest_dir/" \;
    log_ok "Templates installed"
}

# Drop config.example.yaml into the XDG config dir so a binary-only install (no git
# clone) has a template to copy to config.yaml. `wi` auto-discovers
# ~/.config/lorekeeper/config.yaml, so this is where users should put their real config.
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/lorekeeper"
install_config_example() {
    local src="$1"
    [ -f "$src" ] || return 0
    render_step "Installing config example to ${CONFIG_DIR}"
    mkdir -p "$CONFIG_DIR"
    cp "$src" "${CONFIG_DIR}/config.example.yaml"
    log_ok "Config example → ${CONFIG_DIR}/config.example.yaml"
}

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
    local first
    first="$(printf '%s\n%s\n' "$a" "$b" | sort -V | head -n1)"
    [ "$first" = "$a" ] && echo "older" || echo "newer"
}

skill_sha256() {
    local skill_md="$1"
    [ -f "$skill_md" ] || { echo ""; return; }
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$skill_md" | awk '{print $1}'
    else
        shasum -a 256 "$skill_md" | awk '{print $1}'
    fi
}

backup_path() {
    local target="$1"
    [ -e "$target" ] || return 0
    local backup="${target}.backup_$(date +%Y%m%d_%H%M%S)_$$"
    cp -r "$target" "$backup"
    log_info "Backup: $backup"
}

download_skill_tarball() {
    local version="$1" skill_name="$2"
    local archive="${skill_name}-skill-v${version}.tar.gz"
    local url="${RELEASE_BASE}/v${version}/${archive}"
    local extracted_dir="${TMP_DIR}/${skill_name}"

    [ -d "$extracted_dir" ] && { echo "$extracted_dir"; return 0; }

    render_step "Downloading skill ${archive}"
    if ! curl -fsSL --retry 3 --retry-delay 2 -o "${TMP_DIR}/${archive}" "$url" 2>/dev/null; then
        log_warn "Skill archive unavailable at $url; skipping skill install"
        echo ""; return 0
    fi
    if ! curl -fsSL --retry 3 --retry-delay 2 -o "${TMP_DIR}/${archive}.sha256" "${url}.sha256" 2>/dev/null; then
        log_warn "Skill checksum unavailable; skipping skill install"
        echo ""; return 0
    fi
    ( cd "$TMP_DIR" && {
        if command -v sha256sum >/dev/null 2>&1; then sha256sum -c "${archive}.sha256" >/dev/null
        else shasum -a 256 -c "${archive}.sha256" >/dev/null
        fi
    } ) || { log_warn "Skill checksum mismatch; skipping skill install"; echo ""; return 0; }
    tar -xzf "${TMP_DIR}/${archive}" -C "${TMP_DIR}"
    echo "$extracted_dir"
}

install_skill() {
    local level="$1" src="$2"
    [ "$level" = "none" ] && { log_info "Skill install skipped"; return; }
    [ -d "$src" ] || { log_warn "Skill source not found: $src (skipping)"; return; }

    local target
    case "$level" in
        user)    target="$HOME/.claude/skills/${SKILL_NAME}" ;;
        project) target="$(pwd)/.claude/skills/${SKILL_NAME}" ;;
        *)       die "Invalid skill level: $level" ;;
    esac

    render_step "Installing skill → $target"
    if [ -d "$target" ]; then
        # Agent Skills spec has no version field — content hash is the only signal.
        local existing new
        existing="$(skill_sha256 "$target/SKILL.md")"
        new="$(skill_sha256 "$src/SKILL.md")"
        if [ -n "$existing" ] && [ "$existing" = "$new" ]; then
            if [ "$LORE_INSTALL_FORCE" != "1" ] && ! prompt_yesno "Skill is already current. Reinstall?" "N"; then
                log_info "Skill kept"
                return
            fi
        fi
        backup_path "$target"
        rm -rf "$target"
    fi
    mkdir -p "$(dirname "$target")"
    cp -r "$src" "$target"
    log_ok "Skill installed"
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
  --yes, -y                  Accept all defaults non-interactively
  --dry-run                  Print plan, do not execute
  --help, -h                 Show this message

Environment variables (flags win over env, env wins over defaults):
  LORE_INSTALL_DIR, LORE_INSTALL_DATA_DIR, LORE_INSTALL_VERSION,
  LORE_INSTALL_SKILL_LEVEL, LORE_INSTALL_FROM_SOURCE,
  LORE_INSTALL_FORCE, LORE_INSTALL_YES, LORE_INSTALL_DRY_RUN, NO_COLOR
USAGE
}

parse_args() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --version)       LORE_INSTALL_VERSION="$2"; EXPLICIT_VERSION=1; shift 2 ;;
            --install-dir)   INSTALL_DIR="$2"; EXPLICIT_INSTALL_DIR=1; shift 2 ;;
            --data-dir)      DATA_DIR="$2"; shift 2 ;;
            --skill)         LORE_INSTALL_SKILL_LEVEL="$2"; EXPLICIT_SKILL_LEVEL=1; shift 2 ;;
            --from-source)   LORE_INSTALL_FROM_SOURCE=1; EXPLICIT_FROM_SOURCE=1; shift ;;
            --force)         LORE_INSTALL_FORCE=1; shift ;;
            --yes|-y)        LORE_INSTALL_YES=1; shift ;;
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
        user)    printf '  %sskill%s     ~/.claude/skills/%s\n' "$C_DIM" "$C_RESET" "$SKILL_NAME" ;;
        project) printf '  %sskill%s     ./.claude/skills/%s\n' "$C_DIM" "$C_RESET" "$SKILL_NAME" ;;
        none)    printf '  %sskill%s     (skipped)\n' "$C_DIM" "$C_RESET" ;;
    esac
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
    local platform="" version method bin_dest skill_level binary_src templates_src config_example_src

    local repo_dir=""
    if [ -f "$(dirname "$0")/../Cargo.toml" ]; then
        repo_dir="$(cd "$(dirname "$0")/.." && pwd)"
    fi

    if [ "$EXPLICIT_FROM_SOURCE" = "1" ] || [ -z "$INPUT_FD" ]; then
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
        version="$(grep -m1 '^version' "${repo_dir:-.}/crates/lk-cli/Cargo.toml" 2>/dev/null | cut -d'"' -f2 || echo "dev")"
    fi

    render_banner "$platform" "$version"

    if [ -n "$INPUT_FD" ] && [ "$EXPLICIT_INSTALL_DIR" != "1" ]; then
        local loc
        loc="$(prompt_choice "Install location" 1 \
            "~/.local/bin          (recommended)" \
            "/usr/local/bin        (may need sudo)" \
            "Custom path…")"
        case "$loc" in
            "~/.local/bin"*)   INSTALL_DIR="$HOME/.local/bin" ;;
            "/usr/local/bin"*) INSTALL_DIR="/usr/local/bin" ;;
            "Custom"*)         INSTALL_DIR="$(prompt_path "Install path" "$HOME/.local/bin")" ;;
        esac
    fi
    bin_dest="${INSTALL_DIR}/${BINARY_NAME}"

    if [ "$EXPLICIT_SKILL_LEVEL" = "1" ]; then
        skill_level="$LORE_INSTALL_SKILL_LEVEL"
    elif [ -z "$INPUT_FD" ]; then
        skill_level="user"
    else
        local pick
        pick="$(prompt_choice "Claude Code skill" 1 \
            "User-level            ~/.claude/skills/${SKILL_NAME}" \
            "Project-level         ./.claude/skills/${SKILL_NAME}" \
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

    if [ -n "$INPUT_FD" ] && ! prompt_yesno "Proceed?" "Y"; then
        log_info "Aborted by user"; exit 0
    fi

    local skip_binary=0
    if [ -f "$bin_dest" ] && [ "$LORE_INSTALL_FORCE" != "1" ]; then
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
                local ext archive
                case "$platform" in *windows*) ext="zip" ;; *) ext="tar.gz" ;; esac
                archive="${BINARY_NAME}-v${version}-${platform}.${ext}"
                download_archive "$version" "$platform" "$archive"
                verify_checksum "$archive"
                extract_archive "$archive"
                # The release archive contains a top-level `lore-v{ver}-{target}/` dir
                # (see .github/workflows/release.yml), so the binary and templates live
                # one level down from TMP_DIR.
                local stage="${TMP_DIR}/${BINARY_NAME}-v${version}-${platform}"
                binary_src="${stage}/${BINARY_NAME}"
                templates_src="${stage}/templates"
                config_example_src="${stage}/config.example.yaml"
                ;;
            source)
                [ -n "$repo_dir" ] || die "--from-source requires running from a cloned repo"
                binary_src="$(build_from_source "$repo_dir")"
                templates_src="${repo_dir}/templates"
                config_example_src="${repo_dir}/config.example.yaml"
                ;;
        esac
        install_binary "$binary_src" "$INSTALL_DIR"
        install_templates "$templates_src" "$DATA_DIR"
        install_config_example "$config_example_src"
    fi

    if [ "$skill_level" != "none" ]; then
        # Install all bundled skills. lorekeeper = lore command invocation surface;
        # lore-process = queue drainer used in queue-mode workflows.
        for skill in "lorekeeper" "lore-process" "lore-setup" "lore-wiki"; do
            local skill_src=""
            if [ -n "$repo_dir" ] && [ -d "$repo_dir/.claude/skills/$skill" ]; then
                skill_src="$repo_dir/.claude/skills/$skill"
            else
                skill_src="$(download_skill_tarball "$version" "$skill")"
            fi
            if [ -n "$skill_src" ]; then
                SKILL_NAME="$skill" install_skill "$skill_level" "$skill_src"
            fi
        done
    fi

    printf '\n'
    check_path "$INSTALL_DIR"
    printf '\n%s✅ Installation complete%s\n' "$C_GREEN$C_BOLD" "$C_RESET"
    printf '\nNext steps:\n'
    printf '  %s1.%s Create your config (auto-discovered, no repo needed):\n' "$C_BOLD" "$C_RESET"
    printf '       cp %s/config.example.yaml %s/config.yaml\n' "$CONFIG_DIR" "$CONFIG_DIR"
    printf '       $EDITOR %s/config.yaml\n' "$CONFIG_DIR"
    printf '  %s2.%s %s init credentials%s     Enter API tokens interactively\n' "$C_BOLD" "$C_RESET" "$C_BOLD" "$C_RESET"
    printf '  %s3.%s %s validate%s             Verify config + credentials\n' "$C_BOLD" "$C_RESET" "$C_BOLD" "$C_RESET"
    printf '  %s4.%s %s ingest --dry-run%s     Preview ingest without writing\n' "$C_BOLD" "$C_RESET" "$C_BOLD" "$C_RESET"
    printf '  %s5.%s %s schedule%s | crontab -  Register the daily cron\n' "$C_BOLD" "$C_RESET" "$C_BOLD" "$C_RESET"
    printf '  %s/lorekeeper%s · %s/lore-process%s   via Claude Code (queue-mode)\n' "$C_BOLD" "$C_RESET" "$C_BOLD" "$C_RESET"
}

main "$@"
