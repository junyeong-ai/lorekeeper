#!/bin/bash
set -euo pipefail
exec "${LORE_BIN:-lore}" ${LORE_CONFIG:+--config "$LORE_CONFIG"} synthesis annual --previous --wait-for-vault
