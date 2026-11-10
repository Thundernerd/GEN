#!/usr/bin/env bash

# Copyright (c) 2026 Christiaan Bloemendaal
# License: MIT | https://github.com/Thundernerd/GEN/raw/main/LICENSE
# Source: https://github.com/Thundernerd/GEN

source /dev/stdin <<<"$FUNCTIONS_FILE_PATH"
color
verb_ip6
catch_errors
setting_up_container
network_check
update_os

msg_info "Installing Dependencies"
$STD apt install -y sqlite3 openssl
msg_ok "Installed Dependencies"

fetch_and_deploy_gh_release "gen" "Thundernerd/GEN" "prebuild" "latest" "/opt/gen" \
  "gen-*-$(arch_resolve x86_64 aarch64)-unknown-linux-gnu.tar.gz"

msg_info "Configuring GEN"
useradd --system --home-dir /var/lib/gen --shell /usr/sbin/nologin gen
install -d -m 0750 -o gen -g gen /var/lib/gen
install -d -m 0750 -o root -g gen /etc/gen
ADMIN_TOKEN=$(openssl rand -hex 32)
cat <<EOF >/etc/gen/gen.env
# Secrets for GEN, referenced as \${NAME} in /etc/gen/gen.toml.
# Restart after changes: systemctl restart gen
GEN_ADMIN_TOKEN=${ADMIN_TOKEN}
# Optional GitHub token (a fine-grained token with no permissions is enough):
# GitHub releases then come from the API, with prerelease flags.
GITHUB_TOKEN=
# Discord bot token; also uncomment [discord] in /etc/gen/gen.toml.
GEN_DISCORD_TOKEN=
RUST_LOG=info
EOF
cat <<EOF >/etc/gen/gen.toml
# GEN settings. Values like \${NAME} are read from /etc/gen/gen.env.
# Restart after changes: systemctl restart gen

[server]
bind = "0.0.0.0:8080"
# Public URL, used for links in /feed.xml and in notifications.
base_url = "http://${LOCAL_IP}:8080"
templates_dir = "/opt/gen/templates"
static_dir = "/opt/gen/static"
admin_token = "\${GEN_ADMIN_TOKEN}"

[database]
path = "/var/lib/gen/gen.db"
# retention_days = 365

[polling]
default_interval_minutes = 60
concurrency = 8

# The Discord bot, for managing Discord channels with /gen slash commands.
# Set GEN_DISCORD_TOKEN in /etc/gen/gen.env, then uncomment:
# [discord]
# token = "\${GEN_DISCORD_TOKEN}"
EOF
chown root:gen /etc/gen/gen.env /etc/gen/gen.toml
chmod 0640 /etc/gen/gen.env /etc/gen/gen.toml
cat <<EOF >~/gen.creds
GEN Admin Token
URL:   http://${LOCAL_IP}:8080/login
Token: ${ADMIN_TOKEN}
EOF
msg_ok "Configured GEN"

msg_info "Creating Service"
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
systemctl enable -q --now gen
msg_ok "Created Service"

motd_ssh
customize
cleanup_lxc
