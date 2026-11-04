#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' 'Use the installed Lorekeeper 0.120.1 or newer: lore self uninstall' 'For a custom installation, run that executable with --config PATH self uninstall --data-dir PATH.' 'This script does not remove files. Independent deletion cannot validate vault or installation claims.' >&2
exit 1
