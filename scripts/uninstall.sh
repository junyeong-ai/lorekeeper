#!/usr/bin/env bash
# Lorekeeper uninstaller.
set -euo pipefail

BINARY_NAME="lore"
SKILL_NAMES=("lore-ingest" "lore-process" "lore-setup" "lore-wiki" "lore-capture" "lore-extract" "lore-day")

INSTALL_DIR="${LORE_INSTALL_DIR:-$HOME/.local/bin}"
DATA_DIR="${LORE_INSTALL_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/lorekeeper}"
LORE_UNINSTALL_YES="${LORE_UNINSTALL_YES:-0}"

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
    [ "$LORE_UNINSTALL_YES" = "1" ] && return 0
    local answer
    printf '%s%s%s [y/N] ' "$C_BOLD" "$1" "$C_RESET"
    IFS= read -r answer < /dev/tty || answer="N"
    case "$answer" in [Yy]*) return 0 ;; *) return 1 ;; esac
}

print_usage() {
    cat <<'USAGE'
Lorekeeper uninstaller

Usage:
  ./scripts/uninstall.sh [--yes] [--keep-data] [--install-dir PATH] [--data-dir PATH]

Removes:
  - <install-dir>/lore                             (binary)
  - <data-dir>/templates                           (installed templates)
  - ~/.config/lorekeeper/config.example.yaml       (installed config template + deploy records)
  - ~/.claude/skills/lore-*                        (user-level skills)
  - ./.claude/skills/lore-*                        (project-level skills, if present)
  - <data-dir>/pipelines                           (scheduled pipeline scripts)
  - ~/.claude/scheduled-tasks/lore-*-ingest        (superseded, pre-0.11)

Flags:
  --install-dir PATH  Where the binary was installed
                      (default: $LORE_INSTALL_DIR, else $HOME/.local/bin)
  --data-dir PATH     Where templates and pipelines were installed
                      (default: $LORE_INSTALL_DATA_DIR, else
                       $XDG_DATA_HOME/lorekeeper or $HOME/.local/share/lorekeeper)
  --yes, -y           Skip all confirmations
  --keep-data         Keep everything this installed except the binary and the skills
  --help, -h          Show this message

The two path flags mirror install.sh's. Without them, an install made with
`install.sh --install-dir /opt/bin` was invisible here and the binary stayed.
USAGE
}

KEEP_DATA=0
while [ $# -gt 0 ]; do
    case "$1" in
        --yes|-y)      LORE_UNINSTALL_YES=1; shift ;;
        --keep-data)   KEEP_DATA=1; shift ;;
        --install-dir) INSTALL_DIR="$2"; shift 2 ;;
        --data-dir)    DATA_DIR="$2"; shift 2 ;;
        --help|-h)     print_usage; exit 0 ;;
        *)           die "Unknown flag: $1 (use --help)" ;;
    esac
done

init_colors

printf '\n%sLorekeeper uninstaller%s\n\n' "$C_BOLD" "$C_RESET"

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
        log_ok "Templates removed"
        removed=$((removed + 1))
    fi
fi

# What `lore self deploy` wrote into the config directory: the example, and the two records it
# keeps beside it. `config.yaml` itself is user data and is never touched. Each is asked about
# on its own presence — while the records rode on the example's, a user who tidied the example
# away after copying it kept the `data-dir` record, and a later bare `lore self deploy`
# resurrected the directory this had just removed, in preference to the default, with nothing
# said.
lore_config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/lorekeeper"
if [ "$KEEP_DATA" != "1" ]; then
    for artifact in config.example.yaml data-dir deployed-skills; do
        path="${lore_config_dir}/${artifact}"
        [ -f "$path" ] || continue
        if prompt_yesno "Remove $path?"; then
            render_step "Removing $path"
            rm -f "$path"
            log_ok "Removed: $artifact"
            removed=$((removed + 1))
        fi
    done
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

    # Only remove project-level skills if they are NOT inside a git repo's source tree.
    # Deleting .claude/skills/ from within a cloned repo destroys source files.
    if [ -d "$skill_project" ] && [ "$skill_project" != "$skill_user" ]; then
        if git -C "$(dirname "$skill_project")" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
            log_info "Skipping $skill_project (inside git repo — source file, not installed copy)"
        elif prompt_yesno "Remove project-level skill $skill_project?"; then
            render_step "Removing $skill_project"
            rm -rf "$skill_project"
            log_ok "Project skill removed: $skill"
            removed=$((removed + 1))
        fi
    fi
done

# The scheduled pipelines. Installed data like the templates beside them, so `--keep-data`
# keeps them: it removed the scripts a scheduler was still firing while promising to "keep
# templates and vault data", and the jobs then failed nightly with no file to point at.
pipelines_dir="$DATA_DIR/pipelines"
if [ "$KEEP_DATA" != "1" ] && [ -d "$pipelines_dir" ]; then
    if prompt_yesno "Remove pipelines $pipelines_dir?"; then
        render_step "Removing $pipelines_dir"
        rm -rf "$pipelines_dir"
        log_ok "Pipelines removed"
        removed=$((removed + 1))
    fi
fi
# Last, because this is the point where everything this installed under it has been considered.
# Asked from inside the templates block it ran while the pipelines were still there, so it
# always failed and left an empty directory behind — which reads, to anyone looking afterwards,
# exactly like an install that was not removed.
[ "$KEEP_DATA" = "1" ] || rmdir "$DATA_DIR" 2>/dev/null || true
printf '%sIf you registered them with launchd or cron, unload those jobs too.%s\n' "$C_DIM" "$C_RESET"

# Scheduled tasks installed by versions up to 0.10, before the pipelines replaced them.
# Still offered here so an upgrade path that never re-ran the installer can clean up.
for sched_name in lore-daily-ingest lore-weekly-ingest; do
    sched_task="$HOME/.claude/scheduled-tasks/$sched_name"
    if [ -d "$sched_task" ]; then
        if prompt_yesno "Remove scheduled task $sched_task?"; then
            render_step "Removing $sched_task"
            rm -rf "$sched_task"
            log_ok "Superseded scheduled task removed: $sched_name"
            removed=$((removed + 1))
        fi
    fi
done

if [ "$removed" -eq 0 ]; then
    printf '\n%sNothing to uninstall.%s\n' "$C_DIM" "$C_RESET"
else
    printf '\n%s✅ Removed %d item(s)%s\n' "$C_GREEN$C_BOLD" "$removed" "$C_RESET"
    printf '%sVault data (.lorekeeper/) and config.yaml are untouched.%s\n' "$C_DIM" "$C_RESET"
fi
