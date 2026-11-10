#!/usr/bin/env bash
# Moved: the GEN Proxmox installer is now ct/gen.sh next to this file, built on
# the community-scripts.org engine. This shim keeps the old one-liner working:
#   bash -c "$(curl -fsSL https://raw.githubusercontent.com/Thundernerd/GEN/main/deploy/proxmox/gen.sh)"
# Use the new one instead:
#   bash -c "$(curl -fsSL https://raw.githubusercontent.com/Thundernerd/GEN/main/deploy/proxmox/ct/gen.sh)"
set -euo pipefail
here="$(dirname "${BASH_SOURCE[0]:-.}")/ct/gen.sh"
if [[ -f "$here" ]]; then
  exec bash "$here"
fi
exec bash -c "$(curl -fsSL https://raw.githubusercontent.com/Thundernerd/GEN/main/deploy/proxmox/ct/gen.sh)"
