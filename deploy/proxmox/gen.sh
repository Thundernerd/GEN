#!/usr/bin/env bash
# GEN (Game Engine News) installer for Proxmox VE, in the style of the
# community-scripts.org helper scripts: it creates a Debian LXC container and
# installs GEN in it as a systemd service.
#
# On a Proxmox VE node, as root:
#   bash -c "$(curl -fsSL https://raw.githubusercontent.com/Thundernerd/GEN/main/deploy/proxmox/gen.sh)"
#   bash gen.sh                 create a container and install GEN in it
#   bash gen.sh update [CTID]   rebuild GEN in an existing container
# Inside the container:
#   update                      rebuild GEN from its git repository
#
# GEN is built from source in the container. The source is either a git
# repository (GEN_REPO, optionally GEN_REF for a branch or tag) or a checkout
# on the node: the one this script sits in, the current directory, or GEN_SRC.
# Without a checkout, it is DEFAULT_REPO.
#
# Every setting below can be preset in the environment, e.g.
#   CT_ID=120 CT_IP=192.168.1.50/24 CT_GW=192.168.1.1 bash gen.sh

set -Eeuo pipefail

# ---------------------------------------------------------------------------
# Settings
# ---------------------------------------------------------------------------
CT_ID="${CT_ID:-}"
CT_HOSTNAME="${CT_HOSTNAME:-gen}"
CT_CORES="${CT_CORES:-2}"
CT_RAM="${CT_RAM:-2048}"             # MiB; compiling GEN needs about 2 GiB
CT_SWAP="${CT_SWAP:-512}"            # MiB
CT_DISK="${CT_DISK:-8}"              # GiB; the toolchain and build cache take ~4
CT_BRIDGE="${CT_BRIDGE:-vmbr0}"
CT_IP="${CT_IP:-dhcp}"               # "dhcp" or an address like 192.168.1.50/24
CT_GW="${CT_GW:-}"
CT_VLAN="${CT_VLAN:-}"
CT_STORAGE="${CT_STORAGE:-}"         # for the root disk; asked when several fit
CT_TEMPLATE_STORAGE="${CT_TEMPLATE_STORAGE:-}"
CT_PASSWORD="${CT_PASSWORD:-}"       # empty: console logs in as root by itself
GEN_PORT="${GEN_PORT:-8080}"
GEN_BASE_URL="${GEN_BASE_URL:-}"     # empty: http://<container IP>:<port>
GEN_REPO="${GEN_REPO:-}"
GEN_REF="${GEN_REF:-}"
GEN_SRC="${GEN_SRC:-}"
GEN_GITHUB_TOKEN="${GEN_GITHUB_TOKEN:-}"
GEN_DISCORD_TOKEN="${GEN_DISCORD_TOKEN:-}"

DEFAULT_REPO=https://github.com/Thundernerd/GEN.git
BACKTITLE="GEN · Proxmox VE installer"
# Where the setup script and the build source live in the container.
SETUP=/usr/local/sbin/gen-setup
SRC_ARCHIVE=/root/gen-src.tar.gz

# ---------------------------------------------------------------------------
# Output helpers (also copied into the container)
# ---------------------------------------------------------------------------
GN=$'\033[1;92m' RD=$'\033[01;31m' YW=$'\033[33m' BL=$'\033[36m' CL=$'\033[m'
LOG=/dev/null
SPINNER_PID=

spinner() {
  local frames=('-' '\' '|' '/') i=0
  while :; do
    printf '\r %s%s%s %s' "$YW" "${frames[i++ % 4]}" "$CL" "$1" >&2
    sleep 0.15
  done
}

stop_spinner() {
  if [ -n "$SPINNER_PID" ]; then
    kill "$SPINNER_PID" 2>/dev/null || true
    wait "$SPINNER_PID" 2>/dev/null || true
    SPINNER_PID=
    printf '\r\033[K' >&2
  fi
}

msg_info() {
  stop_spinner
  if [ -t 2 ]; then
    spinner "$1" &
    SPINNER_PID=$!
  else
    printf ' - %s...\n' "$1" >&2
  fi
}

msg_ok() {
  stop_spinner
  printf ' %s✔%s %s\n' "$GN" "$CL" "$1" >&2
}

msg_error() {
  stop_spinner
  printf ' %s✖%s %s\n' "$RD" "$CL" "$1" >&2
}

# Runs a command with its output in $LOG, like community-scripts' $STD.
silent() {
  "$@" >>"$LOG" 2>&1
}

on_error() {
  local code=$? line=$1 cmd=$2
  stop_spinner
  msg_error "Failed (exit $code) at line $line: $cmd"
  if [ -s "$LOG" ]; then
    printf '%s--- last lines of %s ---%s\n' "$YW" "$LOG" "$CL" >&2
    tail -n 20 "$LOG" >&2
  fi
  exit "$code"
}

# ---------------------------------------------------------------------------
# Container side: run as gen-setup inside the LXC
# ---------------------------------------------------------------------------
ct_main() {
  LOG=/var/log/gen-setup.log
  trap 'on_error $LINENO "$BASH_COMMAND"' ERR
  trap stop_spinner EXIT
  export DEBIAN_FRONTEND=noninteractive PATH="/root/.cargo/bin:$PATH"
  case "${1:-update}" in
    install) ct_install ;;
    update) ct_update ;;
    *)
      echo "usage: gen-setup install|update" >&2
      exit 2
      ;;
  esac
}

ct_install() {
  : >"$LOG"
  msg_info "Updating the container's packages"
  silent apt-get update
  silent apt-get -y -o Dpkg::Options::=--force-confold dist-upgrade
  msg_ok "Updated the container's packages"

  msg_info "Installing dependencies"
  # aws-lc-sys and the bundled SQLite are compiled from C; no OpenSSL needed.
  silent apt-get install -y --no-install-recommends \
    build-essential cmake perl pkg-config git curl ca-certificates sqlite3
  msg_ok "Installed dependencies"

  install -d -m 0755 /etc/gen
  printf 'GEN_REPO=%q\nGEN_REF=%q\n' "${GEN_REPO:-}" "${GEN_REF:-}" >/etc/gen/source
  chmod 0600 /etc/gen/source

  ct_rust
  ct_fetch_source
  ct_build

  msg_info "Installing GEN"
  id gen >/dev/null 2>&1 ||
    useradd --system --home-dir /opt/gen --shell /usr/sbin/nologin gen
  install -d -o gen -g gen /opt/gen /var/lib/gen
  install -d -m 0750 -o root -g gen /opt/gen/config
  ct_install_files
  ct_write_config
  msg_ok "Installed GEN"

  if [ "${GEN_AUTOLOGIN:-0}" = 1 ]; then
    install -d /etc/systemd/system/container-getty@1.service.d
    cat >/etc/systemd/system/container-getty@1.service.d/override.conf <<'EOF'
[Service]
ExecStart=
ExecStart=-/sbin/agetty --autologin root --noclear --keep-baud tty%I 115200,38400,9600 $TERM
EOF
  fi
  printf '#!/bin/sh\nexec %s update\n' "$SETUP" >/usr/bin/update
  chmod 0755 /usr/bin/update

  msg_info "Starting GEN"
  systemctl daemon-reload
  silent systemctl enable gen
  ct_start
  msg_ok "Started GEN"

  msg_info "Cleaning up"
  silent apt-get -y autoremove
  silent apt-get -y autoclean
  msg_ok "Cleaned up"
}

ct_update() {
  if [ ! -f /etc/gen/source ] || [ ! -x /usr/local/bin/gen ]; then
    msg_error "GEN wasn't installed in this container by gen.sh"
    exit 1
  fi
  # shellcheck disable=SC1091
  . /etc/gen/source
  if [ -z "${GEN_REPO:-}" ] && [ ! -f "$SRC_ARCHIVE" ]; then
    msg_error "This container was installed from a checkout on the Proxmox node."
    echo "   Update it from the node: bash deploy/proxmox/gen.sh update $(hostname)" >&2
    exit 1
  fi
  : >"$LOG"
  ct_rust
  ct_fetch_source
  ct_build

  msg_info "Stopping GEN"
  systemctl stop gen
  msg_ok "Stopped GEN"
  if [ -f /var/lib/gen/gen.db ]; then
    msg_info "Backing up the database"
    sqlite3 /var/lib/gen/gen.db ".backup /var/lib/gen/gen.db.bak"
    chown gen:gen /var/lib/gen/gen.db.bak
    msg_ok "Backed up the database to /var/lib/gen/gen.db.bak"
  fi
  msg_info "Installing GEN"
  ct_install_files
  systemctl daemon-reload
  msg_ok "Installed GEN"
  msg_info "Starting GEN"
  ct_start
  msg_ok "Updated GEN; settings and data were kept"
}

ct_rust() {
  if [ -x /root/.cargo/bin/rustup ]; then
    msg_info "Updating Rust"
    silent rustup update stable
    msg_ok "Updated Rust"
  else
    msg_info "Installing Rust"
    curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs -o /tmp/rustup.sh
    silent sh /tmp/rustup.sh -y --profile minimal --default-toolchain stable
    rm -f /tmp/rustup.sh
    msg_ok "Installed Rust"
  fi
}

ct_fetch_source() {
  local src=/opt/gen-src
  if [ -n "${GEN_REPO:-}" ]; then
    msg_info "Getting the source from git"
    if [ -d "$src/.git" ]; then
      silent git -C "$src" fetch --depth 1 origin "${GEN_REF:-HEAD}"
      silent git -C "$src" reset --hard FETCH_HEAD
    else
      rm -rf "$src"
      silent git clone --depth 1 ${GEN_REF:+--branch "$GEN_REF"} "$GEN_REPO" "$src"
    fi
    msg_ok "Got the source ($(git -C "$src" rev-parse --short HEAD))"
  else
    msg_info "Unpacking the source"
    mkdir -p "$src"
    # Keep target/ so updates build incrementally.
    find "$src" -mindepth 1 -maxdepth 1 ! -name target -exec rm -rf {} +
    tar -xzf "$SRC_ARCHIVE" -C "$src"
    rm -f "$SRC_ARCHIVE"
    msg_ok "Unpacked the source"
  fi
}

ct_build() {
  local start=$SECONDS
  msg_info "Building GEN (the first build takes several minutes)"
  (cd /opt/gen-src && silent cargo build --release --locked)
  msg_ok "Built GEN in $(((SECONDS - start) / 60))m $(((SECONDS - start) % 60))s"
}

ct_install_files() {
  local src=/opt/gen-src
  install -m 0755 "$src/target/release/gen" /usr/local/bin/gen
  rm -rf /opt/gen/templates /opt/gen/static
  cp -r "$src/templates" "$src/static" /opt/gen/
  install -m 0644 "$src/deploy/gen.service" /etc/systemd/system/gen.service
}

ct_write_config() {
  local config=/opt/gen/config/gen.toml discord
  [ -f "$config" ] && return 0
  if grep -q '^GEN_DISCORD_TOKEN=.' /etc/gen/gen.env 2>/dev/null; then
    discord='[discord]
token = "${GEN_DISCORD_TOKEN}"'
  else
    discord='# Set GEN_DISCORD_TOKEN in /etc/gen/gen.env, then uncomment:
# [discord]
# token = "${GEN_DISCORD_TOKEN}"'
  fi
  cat >"$config" <<EOF
# GEN settings, written by the Proxmox installer. Values like \${NAME} are
# read from /etc/gen/gen.env. Restart after changes: systemctl restart gen

[server]
bind = "0.0.0.0:${GEN_PORT:-8080}"
# Public URL, used for links in /feed.xml and in notifications.
base_url = "${GEN_BASE_URL}"
templates_dir = "/opt/gen/templates"
static_dir = "/opt/gen/static"
admin_token = "\${GEN_ADMIN_TOKEN}"

[database]
path = "/var/lib/gen/gen.db"

[polling]
default_interval_minutes = 60
concurrency = 8
# The GITHUB_TOKEN variable in /etc/gen/gen.env is picked up automatically.

# The Discord bot, for managing Discord channels with /gen slash commands.
$discord
EOF
  chown root:gen "$config"
  chmod 0640 "$config"
}

# Starts the service and waits for /health. The unit's sandboxing needs mount
# namespaces; if the container doesn't allow them (exit status 226), it is
# switched off with a drop-in and the start is retried.
ct_start() {
  local port dropin=/etc/systemd/system/gen.service.d/lxc.conf
  port=$(sed -n 's/^bind *= *"[^"]*:\([0-9]*\)".*/\1/p' /opt/gen/config/gen.toml)
  systemctl restart gen || true
  if ! ct_wait_healthy "${port:-8080}" &&
    [ "$(systemctl show -p ExecMainStatus --value gen)" = 226 ] && [ ! -f "$dropin" ]; then
    install -d "$(dirname "$dropin")"
    cat >"$dropin" <<'EOF'
# Written by gen-setup: this container doesn't allow the sandboxing in gen.service.
[Service]
ProtectSystem=false
ProtectHome=false
PrivateTmp=false
PrivateDevices=false
ProtectKernelTunables=false
ProtectKernelModules=false
ProtectControlGroups=false
EOF
    systemctl daemon-reload
    systemctl restart gen || true
    ct_wait_healthy "${port:-8080}" || true
  fi
  if ! curl -fsS "http://127.0.0.1:${port:-8080}/health" >/dev/null 2>&1; then
    msg_error "GEN didn't start. Its log:"
    journalctl -u gen -n 30 --no-pager >&2
    exit 1
  fi
}

ct_wait_healthy() {
  local i
  for i in $(seq 1 30); do
    curl -fsS "http://127.0.0.1:$1/health" >/dev/null 2>&1 && return 0
    systemctl is-failed --quiet gen && return 1
    sleep 1
  done
  return 1
}

# ---------------------------------------------------------------------------
# Node side
# ---------------------------------------------------------------------------
header() {
  clear
  cat <<EOF
${BL}   _____ ______ _   __
  / ___// ____// | / /
 / / _ / __/  /  |/ /
/ /_/ // /___ / /|  /
\____//_____//_/ |_/${CL}

 Game Engine News · Proxmox VE LXC installer

EOF
}

check_node() {
  if [ "$(id -u)" -ne 0 ]; then
    msg_error "Run this script as root."
    exit 1
  fi
  if ! command -v pct >/dev/null || ! command -v pveam >/dev/null; then
    msg_error "This isn't a Proxmox VE node (pct and pveam are missing)."
    echo "   Inside a GEN container, run: update" >&2
    exit 1
  fi
}

cancelled() {
  clear
  echo " Cancelled; nothing was changed."
  exit 0
}

ask() { # title text default
  whiptail --backtitle "$BACKTITLE" --title "$1" --inputbox "$2" 10 70 "$3" 3>&1 1>&2 2>&3
}

ask_number() { # variable title text
  local value
  while :; do
    value=$(ask "$2" "$3" "${!1}") || cancelled
    if [[ "$value" =~ ^[0-9]+$ ]] && [ "$value" -gt 0 ]; then
      printf -v "$1" '%s' "$value"
      return
    fi
    whiptail --backtitle "$BACKTITLE" --msgbox "Enter a whole number." 8 40
  done
}

# Sets GEN_SRC to a GEN checkout around this script or in the current
# directory, unless it is already set. Fails when there is none.
find_checkout() {
  local dir
  if [ -z "$GEN_SRC" ]; then
    for dir in "$(dirname "${BASH_SOURCE[0]:-.}")/../.." "$PWD"; do
      if grep -qs '^name = "gen"' "$dir/Cargo.toml"; then
        GEN_SRC=$(cd "$dir" && pwd)
        break
      fi
    done
  fi
  [ -n "$GEN_SRC" ] || return 1
  if [ ! -f "$GEN_SRC/Cargo.toml" ] || [ ! -f "$GEN_SRC/deploy/gen.service" ]; then
    msg_error "$GEN_SRC isn't a GEN checkout."
    exit 1
  fi
}

# Finds the source: GEN_REPO, else a checkout (see find_checkout), else
# DEFAULT_REPO.
find_source() {
  [ -n "$GEN_REPO" ] && return 0
  find_checkout && return 0
  GEN_REPO=$DEFAULT_REPO
}

source_label() {
  if [ -n "$GEN_REPO" ]; then
    echo "${GEN_REPO#*@}${GEN_REF:+ ($GEN_REF)}"
  else
    echo "$GEN_SRC (on this node)"
  fi
}

# Picks a storage that holds the given content, asking when several do.
pick_storage() { # content label
  local name type status total used avail pct rest menu=()
  while read -r name type status total used avail pct rest; do
    [ "$status" = active ] || continue
    menu+=("$name" "$type, $((avail / 1024 / 1024)) GiB free")
  done < <(pvesm status -content "$1" 2>/dev/null | tail -n +2)
  if [ "${#menu[@]}" -eq 0 ]; then
    msg_error "No active storage holds $2."
    exit 1
  elif [ "${#menu[@]}" -eq 2 ]; then
    echo "${menu[0]}"
  else
    whiptail --backtitle "$BACKTITLE" --title "Storage" \
      --menu "Storage for $2:" 16 60 6 "${menu[@]}" 3>&1 1>&2 2>&3 || cancelled
  fi
}

summary() {
  cat <<EOF
Container ID:   $CT_ID
Hostname:       $CT_HOSTNAME
OS:             Debian (unprivileged, nesting on)
CPU cores:      $CT_CORES
RAM:            $CT_RAM MiB (swap $CT_SWAP MiB)
Disk:           $CT_DISK GiB
Network:        $CT_BRIDGE, $CT_IP${CT_GW:+ via $CT_GW}${CT_VLAN:+, VLAN $CT_VLAN}
GEN port:       $GEN_PORT
Source:         $(source_label)
EOF
}

advanced_settings() {
  local value
  while :; do
    value=$(ask "Container ID" "Container ID:" "$CT_ID") || cancelled
    if [[ "$value" =~ ^[0-9]+$ ]] && pvesh get /cluster/nextid --vmid "$value" >/dev/null 2>&1; then
      CT_ID=$value
      break
    fi
    whiptail --backtitle "$BACKTITLE" --msgbox "ID $value is invalid or already in use." 8 50
  done
  CT_HOSTNAME=$(ask "Hostname" "Hostname:" "$CT_HOSTNAME") || cancelled
  if [ -n "$GEN_REPO" ]; then
    GEN_REPO=$(ask "GEN source" "Git repository to build GEN from:" "$GEN_REPO") || cancelled
    GEN_REF=$(ask "GEN source" "Branch or tag (empty for the default branch):" "$GEN_REF") || cancelled
    [ -n "$GEN_REPO" ] || GEN_REPO=$DEFAULT_REPO
  fi
  ask_number CT_CORES "CPU" "CPU cores:"
  ask_number CT_RAM "RAM" "RAM in MiB (building GEN needs about 2048):"
  ask_number CT_DISK "Disk" "Disk size in GiB (at least 8 recommended):"
  CT_BRIDGE=$(ask "Network" "Bridge:" "$CT_BRIDGE") || cancelled
  CT_IP=$(ask "Network" "IPv4 address with prefix (e.g. 192.168.1.50/24), or dhcp:" "$CT_IP") || cancelled
  if [ "$CT_IP" != dhcp ]; then
    CT_GW=$(ask "Network" "Gateway:" "$CT_GW") || cancelled
  fi
  CT_VLAN=$(ask "Network" "VLAN tag (empty for none):" "$CT_VLAN") || cancelled
  CT_PASSWORD=$(whiptail --backtitle "$BACKTITLE" --title "Root password" --passwordbox \
    "Root password (empty: the console logs in as root by itself):" 10 70 3>&1 1>&2 2>&3) || cancelled
  ask_number GEN_PORT "GEN" "Port GEN listens on:"
  GEN_BASE_URL=$(ask "GEN" \
    "Public URL of GEN, used in links (empty: http://<container IP>:$GEN_PORT):" "$GEN_BASE_URL") || cancelled
  GEN_GITHUB_TOKEN=$(ask "GEN" \
    "GitHub token, optional (raises the API limit from 60 to 5000 requests an hour):" "$GEN_GITHUB_TOKEN") || cancelled
  GEN_DISCORD_TOKEN=$(ask "GEN" \
    "Discord bot token, optional (turns on the /gen bot):" "$GEN_DISCORD_TOKEN") || cancelled
}

choose_settings() {
  CT_ID=${CT_ID:-$(pvesh get /cluster/nextid)}
  if whiptail --backtitle "$BACKTITLE" --title "GEN LXC" --yes-button "Default" --no-button "Advanced" \
    --yesno "Create the container with these settings?\n\n$(summary)" 20 74; then
    :
  else
    advanced_settings
    whiptail --backtitle "$BACKTITLE" --title "GEN LXC" --yes-button "Create" --no-button "Cancel" \
      --yesno "Create the container with these settings?\n\n$(summary)" 20 74 || cancelled
  fi
  [ -n "$CT_STORAGE" ] || CT_STORAGE=$(pick_storage rootdir "the container's disk")
  [ -n "$CT_TEMPLATE_STORAGE" ] || CT_TEMPLATE_STORAGE=$(pick_storage vztmpl "container templates")
}

download_template() {
  local arch available
  arch=$(dpkg --print-architecture)
  msg_info "Finding the Debian template"
  silent pveam update || true
  available=$(pveam available --section system | awk '{print $2}')
  TEMPLATE=$(grep -E "^debian-13-standard_.*_${arch}\.tar\.(zst|gz|xz)$" <<<"$available" | sort -V | tail -n 1 || true)
  [ -n "$TEMPLATE" ] ||
    TEMPLATE=$(grep -E "^debian-12-standard_.*_${arch}\.tar\.(zst|gz|xz)$" <<<"$available" | sort -V | tail -n 1 || true)
  if [ -z "$TEMPLATE" ]; then
    msg_error "No Debian 12 or 13 template is available for $arch."
    exit 1
  fi
  msg_ok "Using $TEMPLATE"
  if ! pveam list "$CT_TEMPLATE_STORAGE" | grep -F "/$TEMPLATE" >/dev/null; then
    msg_info "Downloading $TEMPLATE"
    silent pveam download "$CT_TEMPLATE_STORAGE" "$TEMPLATE"
    msg_ok "Downloaded $TEMPLATE"
  fi
}

create_container() {
  local net="name=eth0,bridge=$CT_BRIDGE,ip=$CT_IP" args=()
  [ -n "$CT_GW" ] && net+=",gw=$CT_GW"
  [ -n "$CT_VLAN" ] && net+=",tag=$CT_VLAN"
  [ -n "$CT_PASSWORD" ] && args+=(--password "$CT_PASSWORD")
  msg_info "Creating LXC container $CT_ID"
  silent pct create "$CT_ID" "$CT_TEMPLATE_STORAGE:vztmpl/$TEMPLATE" \
    --hostname "$CT_HOSTNAME" --ostype debian \
    --cores "$CT_CORES" --memory "$CT_RAM" --swap "$CT_SWAP" \
    --rootfs "$CT_STORAGE:$CT_DISK" --net0 "$net" \
    --unprivileged 1 --features nesting=1,keyctl=1 \
    --onboot 1 --timezone host --tags gen "${args[@]}"
  CREATED=1
  msg_ok "Created LXC container $CT_ID"

  msg_info "Starting the container"
  pct start "$CT_ID"
  local i
  for i in $(seq 1 60); do
    CT_ADDR=$(pct exec "$CT_ID" -- hostname -I 2>/dev/null | awk '{print $1}') || true
    [ -n "$CT_ADDR" ] && pct exec "$CT_ID" -- getent hosts deb.debian.org >/dev/null 2>&1 && break
    sleep 1
  done
  if [ -z "$CT_ADDR" ]; then
    msg_error "The container got no network address."
    exit 1
  fi
  msg_ok "Started the container at $CT_ADDR"
}

# Writes the container-side script, made of the functions above.
write_setup() {
  {
    echo '#!/usr/bin/env bash'
    echo '# GEN setup and updates, written by deploy/proxmox/gen.sh.'
    echo 'set -Eeuo pipefail'
    declare -p GN RD YW BL CL SETUP SRC_ARCHIVE
    echo 'LOG=/dev/null SPINNER_PID='
    declare -f spinner stop_spinner msg_info msg_ok msg_error silent on_error
    declare -f ct_main ct_install ct_update ct_rust ct_fetch_source ct_build \
      ct_install_files ct_write_config ct_start ct_wait_healthy
    echo 'ct_main "$@"'
  } >"$1"
}

push_source() {
  msg_info "Copying the source from $GEN_SRC"
  tar -C "$GEN_SRC" -czf "$WORK/src.tar.gz" Cargo.toml Cargo.lock src templates static deploy
  pct push "$CT_ID" "$WORK/src.tar.gz" "$SRC_ARCHIVE" --perms 0600
  msg_ok "Copied the source"
}

install_gen() {
  header
  check_node
  find_source
  choose_settings
  header
  echo " Creating GEN in container $CT_ID. Details are logged to $WORK/install.log."
  echo
  LOG=$WORK/install.log
  : >"$LOG"
  download_template
  create_container

  local base_url=${GEN_BASE_URL:-http://$CT_ADDR:$GEN_PORT} admin_token
  admin_token=$(openssl rand -hex 32)
  {
    echo "# Secrets for GEN, referenced as \${NAME} in /opt/gen/config/gen.toml."
    echo "GEN_ADMIN_TOKEN=$admin_token"
    echo "# Optional GitHub token: raises the API limit to 5000 requests an hour."
    echo "GITHUB_TOKEN=$GEN_GITHUB_TOKEN"
    echo "# Discord bot token; also uncomment [discord] in gen.toml."
    echo "GEN_DISCORD_TOKEN=$GEN_DISCORD_TOKEN"
    echo "RUST_LOG=info"
  } >"$WORK/gen.env"
  pct exec "$CT_ID" -- install -d -m 0755 /etc/gen
  pct push "$CT_ID" "$WORK/gen.env" /etc/gen/gen.env --perms 0600

  write_setup "$WORK/gen-setup"
  pct push "$CT_ID" "$WORK/gen-setup" "$SETUP" --perms 0755
  [ -n "$GEN_SRC" ] && push_source
  pct exec "$CT_ID" -- env GEN_REPO="$GEN_REPO" GEN_REF="$GEN_REF" GEN_PORT="$GEN_PORT" \
    GEN_BASE_URL="$base_url" GEN_AUTOLOGIN=$([ -z "$CT_PASSWORD" ] && echo 1 || echo 0) \
    "$SETUP" install

  pct set "$CT_ID" --description "<h3>GEN · Game Engine News</h3><p><a href=\"$base_url\">$base_url</a></p><p>Update: run <code>update</code> in the container.</p>"
  CREATED=
  echo
  msg_ok "Completed successfully!"
  cat <<EOF

 ${BL}GEN is running at${CL}  $base_url
 ${BL}Admin token${CL}        $admin_token
                    (log in at $base_url/login; also in /etc/gen/gen.env)
 ${BL}Settings${CL}           /opt/gen/config/gen.toml and /etc/gen/gen.env
 ${BL}Logs${CL}               pct exec $CT_ID -- journalctl -u gen -f
 ${BL}Update${CL}             $(update_hint)

EOF
}

update_hint() {
  if [ -n "$GEN_REPO" ]; then
    echo "run 'update' in the container"
  else
    echo "bash $GEN_SRC/deploy/proxmox/gen.sh update $CT_ID"
  fi
}

update_gen() {
  check_node
  CT_ID=${1:-$CT_ID}
  if [ -z "$CT_ID" ]; then
    local menu=() id status lock name
    while read -r id status lock name; do
      [ -n "$name" ] || { name=$lock; lock=; }
      menu+=("$id" "$name ($status)")
    done < <(pct list | tail -n +2)
    [ "${#menu[@]}" -gt 0 ] || { msg_error "There are no containers."; exit 1; }
    CT_ID=$(whiptail --backtitle "$BACKTITLE" --title "Update GEN" \
      --menu "Container to update:" 16 60 8 "${menu[@]}" 3>&1 1>&2 2>&3) || cancelled
  elif ! [[ "$CT_ID" =~ ^[0-9]+$ ]]; then
    # A hostname, as printed by gen-setup.
    CT_ID=$(pct list | awk -v h="$CT_ID" 'NR > 1 && $NF == h { print $1; exit }')
    [ -n "$CT_ID" ] || { msg_error "No container is named $1."; exit 1; }
  fi
  if [ "$(pct status "$CT_ID" | awk '{print $2}')" != running ]; then
    msg_error "Container $CT_ID isn't running."
    exit 1
  fi
  if ! pct exec "$CT_ID" -- test -f /etc/gen/source; then
    msg_error "GEN wasn't installed in container $CT_ID by this script."
    exit 1
  fi
  header
  if ! pct exec "$CT_ID" -- sh -c '. /etc/gen/source; [ -n "$GEN_REPO" ]'; then
    if ! find_checkout; then
      msg_error "Container $CT_ID builds from a checkout on the node; run this script from one."
      exit 1
    fi
    LOG=$WORK/update.log
    push_source
  fi
  write_setup "$WORK/gen-setup"
  pct push "$CT_ID" "$WORK/gen-setup" "$SETUP" --perms 0755
  pct exec "$CT_ID" -- "$SETUP" update
}

node_main() {
  WORK=$(mktemp -d /tmp/gen-install.XXXXXX)
  CREATED=
  trap 'on_node_error $LINENO "$BASH_COMMAND"' ERR
  trap 'stop_spinner; rm -rf "$WORK"' EXIT
  case "${1:-install}" in
    install) install_gen ;;
    update) update_gen "${2:-}" ;;
    *)
      echo "usage: bash gen.sh [install | update [CTID]]" >&2
      exit 2
      ;;
  esac
}

on_node_error() {
  local code=$? log=$LOG
  # Keep the log, which the EXIT trap would remove with $WORK.
  if [ -s "$log" ]; then
    cp "$log" /tmp/gen-install.log
    LOG=/tmp/gen-install.log
  fi
  if [ -n "${CREATED:-}" ]; then
    echo " Container $CT_ID was left in place for inspection (pct enter $CT_ID; log in /var/log/gen-setup.log)." >&2
    echo " Remove it with: pct stop $CT_ID; pct destroy $CT_ID --purge" >&2
  fi
  (exit "$code") || on_error "$@"
}

if [ -x /usr/local/sbin/gen-setup ] && ! command -v pct >/dev/null; then
  # Run inside a GEN container: update it.
  exec /usr/local/sbin/gen-setup update
fi
node_main "$@"
