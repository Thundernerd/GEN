#!/usr/bin/env bash
_CS_DEFAULT_URL="${_CS_DEFAULT_URL:-https://raw.githubusercontent.com/Thundernerd/GEN/main/deploy/proxmox}"
_cs_boot="${COMMUNITY_SCRIPTS_CORE_DIR:-$(dirname "${BASH_SOURCE[0]}")/../../core}/core/build.func"
source "$_cs_boot" 2>/dev/null || source <(curl -fsSL "${COMMUNITY_SCRIPTS_CORE_URL:-https://raw.githubusercontent.com/community-scripts/core/main}/core/build.func")
# Copyright (c) 2026 Christiaan Bloemendaal
# License: MIT | https://github.com/Thundernerd/GEN/raw/main/LICENSE
# Source: https://github.com/Thundernerd/GEN
#
# GEN (Game Engine News) for Proxmox VE, built on the community-scripts.org
# engine (https://github.com/community-scripts/core): the same wizard, LXC
# defaults, console auto-login, MOTD and `update` command as every other
# community script. This file and install/gen-install.sh form the scripts root;
# _CS_DEFAULT_URL above points the engine at them.
#
#   bash -c "$(curl -fsSL https://raw.githubusercontent.com/Thundernerd/GEN/main/deploy/proxmox/ct/gen.sh)"

APP="GEN"
var_tags="${var_tags:-news;gamedev}"
var_cpu="${var_cpu:-1}"
var_ram="${var_ram:-512}"
var_disk="${var_disk:-4}"
var_os="${var_os:-debian}"
var_version="${var_version:-13}"
var_arm64="${var_arm64:-yes}"
var_unprivileged="${var_unprivileged:-1}"

# GEN has no generated header in the engine repo, so header_info falls back to
# this banner. core.func sets the fallback when it loads, hence after the source.
_CS_FALLBACK_HEADER='   _____ ______ _   __
  / ___// ____// | / /
 / / _ / __/  /  |/ /
/ /_/ // /___ / /|  /
\____//_____//_/ |_/

 Game Engine News'

header_info "$APP"
variables
color
catch_errors

GEN_REPO="Thundernerd/GEN"

function update_script() {
  header_info
  check_container_storage
  check_container_resources

  if [[ -x /usr/local/sbin/gen-setup ]]; then
    migrate_from_source_build
  elif [[ ! -f /etc/systemd/system/gen.service || ! -d /opt/gen ]]; then
    msg_error "No ${APP} Installation Found!"
    exit
  fi

  if check_for_gh_release "gen" "$GEN_REPO"; then
    msg_info "Stopping Service"
    systemctl stop gen
    msg_ok "Stopped Service"

    if [[ -f /var/lib/gen/gen.db ]]; then
      msg_info "Backing up Database"
      $STD sqlite3 /var/lib/gen/gen.db ".backup '/var/lib/gen/gen.db.bak'"
      chown gen:gen /var/lib/gen/gen.db.bak
      msg_ok "Backed up Database to /var/lib/gen/gen.db.bak"
    fi

    # /opt/gen only holds the release; settings live in /etc/gen, data in /var/lib/gen.
    CLEAN_INSTALL=1 fetch_and_deploy_gh_release "gen" "$GEN_REPO" "prebuild" "latest" "/opt/gen" \
      "gen-*-$(arch_resolve x86_64 aarch64)-unknown-linux-gnu.tar.gz"

    msg_info "Starting Service"
    systemctl start gen
    msg_ok "Started Service"
    msg_ok "Updated successfully!"
  fi
  exit
}

# Containers made by the old standalone installer built GEN from source, kept
# its settings in /opt/gen/config and ran /usr/local/bin/gen. Move the settings
# to /etc/gen and the service to the release layout; data and secrets stay.
function migrate_from_source_build() {
  msg_info "Migrating from the source build to release builds"
  if [[ -f /opt/gen/config/gen.toml && ! -f /etc/gen/gen.toml ]]; then
    install -m 0640 -o root -g gen /opt/gen/config/gen.toml /etc/gen/gen.toml
  fi
  chown root:gen /etc/gen
  chmod 0750 /etc/gen
  [[ -f /etc/gen/gen.env ]] && chown root:gen /etc/gen/gen.env && chmod 0640 /etc/gen/gen.env
  systemctl stop gen 2>/dev/null || true
  write_gen_service
  rm -rf /etc/systemd/system/gen.service.d /usr/local/bin/gen /usr/local/sbin/gen-setup \
    /etc/gen/source /opt/gen-src /root/.cargo /root/.rustup
  systemctl daemon-reload
  # Replace the old `update` (which rebuilt from source) with the engine's.
  write_update_entrypoint "gen" "gen"
  msg_ok "Migrated; GEN now updates from GitHub releases"
}

function write_gen_service() {
  cat <<'EOF' >/etc/systemd/system/gen.service
[Unit]
Description=GEN - Game Engine News
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
User=gen
Group=gen
WorkingDirectory=/opt/gen
Environment=GEN_CONFIG=/etc/gen/gen.toml
EnvironmentFile=-/etc/gen/gen.env
ExecStart=/opt/gen/gen serve
Restart=on-failure
RestartSec=10
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
EOF
}

start
build_container
description

msg_ok "Completed successfully!\n"
echo -e "${CREATING}${GN}${APP} setup has been successfully initialized!${CL}"
echo -e "${INFO}${YW}Access it using the following URL:${CL}"
echo -e "${GATEWAY}${BGN}http://${IP}:8080${CL}"
echo -e "${INFO}${YW}The admin token is in ~/gen.creds; settings in /etc/gen (inside the container).${CL}"
