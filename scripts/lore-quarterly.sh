#!/bin/bash
set -euo pipefail
exec "${LORE_BIN:-lore}" ${LORE_CONFIG:+--config "$LORE_CONFIG"} synthesis quarterly --previous --defer --wait-for-vault
