#!/usr/bin/env bash
# wiki-ingest uninstaller.
set -euo pipefail

BINARY_NAME="wi"
SKILL_NAME="wiki-ingest"

INSTALL_DIR="${WI_INSTALL_DIR:-$HOME/.local/bin}"
DATA_DIR="${WI_INSTALL_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/wi-ingest}"
WI_UNINSTALL_YES="${WI_UNINSTALL_YES:-0}"
SKILL_USER="$HOME/.claude/skills/${SKILL_NAME}"
SKILL_PROJECT="$(pwd)/.claude/skills/${SKILL_NAME}"

C_RESET=""; C_DIM=""; C_RED=""; C_GREEN=""; C_YELLOW=""; C_BOLD=""

init_colors() {
    if [ -t 1 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-}" != "dumb" ]; then
        C_RESET=$'\033[0m'; C_DIM=$'\033[2m'; C_RED=$'\033[31m'
        C_GREEN=$'\033[32m'; C_YELLOW=$'\033[33m'; C_BOLD=$'\033[1m'
    fi
}

die()      { printf '%s✗ %s%s\n' "$C_RED" "$*" "$C_RESET" >&2; exit 1; }
log_info() { printf '%s  %s%s\n' "$C_DIM" "$*" "$C_RESET" >&2; }
log_ok()   { printf '%s✓  %s%s\n' "$C_GREEN" "$*" "$C_RESET" >&2; }
render_step() { printf '%s▸  %s%s\n' "$C_YELLOW" "$*" "$C_RESET" >&2; }

prompt_yesno() {
    [ "$WI_UNINSTALL_YES" = "1" ] && return 0
    local answer
    printf '%s%s%s [y/N] ' "$C_BOLD" "$1" "$C_RESET"
    IFS= read -r answer < /dev/tty || answer="N"
    case "$answer" in [Yy]*) return 0 ;; *) return 1 ;; esac
}

print_usage() {
    cat <<'USAGE'
wiki-ingest uninstaller

Usage:
  ./scripts/uninstall.sh [--yes] [--keep-data]

Removes:
  - $WI_INSTALL_DIR/wi                            (binary)
  - $WI_INSTALL_DATA_DIR/templates                (installed templates)
  - ~/.claude/skills/wiki-ingest                  (user-level skill)
  - ./.claude/skills/wiki-ingest                  (project-level skill, if present)

Flags:
  --yes, -y       Skip all confirmations
  --keep-data     Keep templates and vault data (only remove binary + skill)
  --help, -h      Show this message
USAGE
}

KEEP_DATA=0
while [ $# -gt 0 ]; do
    case "$1" in
        --yes|-y)    WI_UNINSTALL_YES=1; shift ;;
        --keep-data) KEEP_DATA=1; shift ;;
        --help|-h)   print_usage; exit 0 ;;
        *)           die "Unknown flag: $1 (use --help)" ;;
    esac
done

init_colors

printf '\n%swiki-ingest uninstaller%s\n\n' "$C_BOLD" "$C_RESET"

removed=0

# Binary
bin_path="${INSTALL_DIR}/${BINARY_NAME}"
if [ -f "$bin_path" ]; then
    if prompt_yesno "Remove binary $bin_path?"; then
        render_step "Removing $bin_path"
        rm -f "$bin_path"
        log_ok "Binary removed"
        removed=$((removed + 1))
    fi
fi

# Templates
if [ "$KEEP_DATA" != "1" ] && [ -d "${DATA_DIR}/templates" ]; then
    if prompt_yesno "Remove templates ${DATA_DIR}/templates?"; then
        render_step "Removing ${DATA_DIR}/templates"
        rm -rf "${DATA_DIR}/templates"
        # Remove parent if now empty
        rmdir "$DATA_DIR" 2>/dev/null || true
        log_ok "Templates removed"
        removed=$((removed + 1))
    fi
fi

# User skill
if [ -d "$SKILL_USER" ]; then
    if prompt_yesno "Remove user-level skill $SKILL_USER?"; then
        render_step "Removing $SKILL_USER"
        rm -rf "$SKILL_USER"
        log_ok "User skill removed"
        removed=$((removed + 1))
    fi
fi

# Project skill
if [ -d "$SKILL_PROJECT" ] && [ "$SKILL_PROJECT" != "$SKILL_USER" ]; then
    if prompt_yesno "Remove project-level skill $SKILL_PROJECT?"; then
        render_step "Removing $SKILL_PROJECT"
        rm -rf "$SKILL_PROJECT"
        log_ok "Project skill removed"
        removed=$((removed + 1))
    fi
fi

if [ "$removed" -eq 0 ]; then
    printf '\n%sNothing to uninstall.%s\n' "$C_DIM" "$C_RESET"
else
    printf '\n%s✅ Removed %d item(s)%s\n' "$C_GREEN$C_BOLD" "$removed" "$C_RESET"
    printf '%sVault data (.wiki-ingest/) and config.yaml are untouched.%s\n' "$C_DIM" "$C_RESET"
fi
