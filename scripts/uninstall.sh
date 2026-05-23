#!/usr/bin/env bash
# wiki-ingest uninstaller.
set -euo pipefail

BINARY_NAME="wi"
SKILL_NAMES=("wiki-ingest" "wi-process")

INSTALL_DIR="${WI_INSTALL_DIR:-$HOME/.local/bin}"
DATA_DIR="${WI_INSTALL_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/wi-ingest}"
WI_UNINSTALL_YES="${WI_UNINSTALL_YES:-0}"

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

# Skills (user-level and project-level for each installed skill name)
for skill in "${SKILL_NAMES[@]}"; do
    skill_user="$HOME/.claude/skills/${skill}"
    skill_project="$(pwd)/.claude/skills/${skill}"

    if [ -d "$skill_user" ]; then
        if prompt_yesno "Remove user-level skill $skill_user?"; then
            render_step "Removing $skill_user"
            rm -rf "$skill_user"
            log_ok "User skill removed: $skill"
            removed=$((removed + 1))
        fi
    fi

    if [ -d "$skill_project" ] && [ "$skill_project" != "$skill_user" ]; then
        if prompt_yesno "Remove project-level skill $skill_project?"; then
            render_step "Removing $skill_project"
            rm -rf "$skill_project"
            log_ok "Project skill removed: $skill"
            removed=$((removed + 1))
        fi
    fi
done

if [ "$removed" -eq 0 ]; then
    printf '\n%sNothing to uninstall.%s\n' "$C_DIM" "$C_RESET"
else
    printf '\n%s✅ Removed %d item(s)%s\n' "$C_GREEN$C_BOLD" "$removed" "$C_RESET"
    printf '%sVault data (.wiki-ingest/) and config.yaml are untouched.%s\n' "$C_DIM" "$C_RESET"
fi
