#!/usr/bin/env bash
# One-time root setup for the Sandcrab Firecracker backend's `unrestricted` networking
# (docs/design/firecracker-backend.md §6). Idempotent: safe to re-run.
#
# Creates a pool of tap devices owned by the sandcrab user, so the backend itself never
# needs privileges at runtime:
#   <prefix>0 .. <prefix>N-1   host 172.30.<n>.1/30, guest 172.30.<n>.2 (crate: net::TapPlan)
# and a dedicated nftables table `inet sandcrab` (never touches other tables, e.g. Proxmox's
# pve-firewall or Docker's):
#   - masquerade 172.30.0.0/16 out of the uplink;
#   - anti-spoofing: tap <prefix>N only accepts IPv4 from its guest address 172.30.N.2
#     (anything else, incl. all non-IPv4, from a tap is dropped before input/forward rules);
#   - no guest -> host (input from taps: only established/related);
#   - no guest -> guest, no guest -> private/link-local/CGNAT ranges (LAN, cloud metadata,
#     tailnets) or the uplink's own network (on-link subnet + default gateway, even when
#     public) unless --allow-private;
#   - no guest -> reserved/special ranges (this-network, IETF protocol assignments,
#     documentation, benchmarking, multicast, class E, broadcast), always;
#   - no outbound SMTP (tcp 25, 465, 587: spam from the host's IP gets it blocklisted or
#     the server suspended) unless --allow-smtp;
# plus net.ipv4.ip_forward=1. If Docker's DOCKER-USER iptables chain exists, ACCEPT rules
# for tap traffic are added there (Docker sets the FORWARD policy to DROP).
#
# Docker: every block above lives in our own table at priority `filter - 1`, i.e. before
# Docker's (iptables-nft) filter chains, and an nftables drop is final, so Docker can never
# re-open what we block. Only the DOCKER-USER ACCEPTs (guest connectivity) depend on Docker,
# and losing them fails closed. With --persist the unit is bound to docker.service
# (PartOf= + WantedBy=), so a Docker restart/stop+start re-runs this script and restores them.
#
# Taps vanish on reboot: use --persist to install a root-owned copy of this script at
# /usr/local/sbin/sandcrab-fc-net-setup (never the user-writable repo copy: a root unit
# executing a file the sandcrab user can edit would be a root escalation) and a systemd
# oneshot that runs it at boot. --uninstall removes both.
#
# Usage:
#   sudo scripts/fc-net-setup.sh [--user USER] [--count N] [--prefix sctap] [--uplink IF]
#                                [--allow-private] [--allow-smtp] [--persist] [--dry-run]
#   sudo scripts/fc-net-setup.sh --uninstall [--prefix sctap] [--count N]
#
# Verify afterwards:
#   ip -br link show type tun | grep sctap ; cat /sys/class/net/sctap0/owner
#   nft list table inet sandcrab
set -euo pipefail

USER_NAME="${SUDO_USER:-}"
COUNT=64
PREFIX="sctap"
UPLINK=""
ALLOW_PRIVATE=0
ALLOW_SMTP=0
PERSIST=0
UNINSTALL=0
DRY_RUN=0
TABLE="sandcrab"
UNIT="/etc/systemd/system/sandcrab-fc-net.service"
INSTALL_DIR="/usr/local/sbin"
INSTALLED="$INSTALL_DIR/sandcrab-fc-net-setup"
SYSCTL_FILE="/etc/sysctl.d/90-sandcrab-fc.conf"
PRIVATE_V4="10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16, 100.64.0.0/10"
# Never routable to anything a guest should reach (RFC 6890 special-purpose registry).
# 240.0.0.0/4 includes the limited broadcast 255.255.255.255.
RESERVED_V4="0.0.0.0/8, 192.0.0.0/24, 192.0.2.0/24, 198.18.0.0/15, 198.51.100.0/24, \
203.0.113.0/24, 224.0.0.0/4, 240.0.0.0/4"
SMTP_PORTS="25, 465, 587"

die() { echo "fc-net-setup: $*" >&2; exit 1; }
log() { echo "fc-net-setup: $*"; }
run() {
  if [[ $DRY_RUN == 1 ]]; then
    printf '+'; printf ' %q' "$@"; printf '\n'
  else
    "$@"
  fi
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --user) USER_NAME="${2:?}"; shift 2 ;;
    --count) COUNT="${2:?}"; shift 2 ;;
    --prefix) PREFIX="${2:?}"; shift 2 ;;
    --uplink) UPLINK="${2:?}"; shift 2 ;;
    --allow-private) ALLOW_PRIVATE=1; shift ;;
    --allow-smtp) ALLOW_SMTP=1; shift ;;
    --persist) PERSIST=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) sed -n '2,/^set -euo/{/^set -euo/!p}' "$0"; exit 0 ;;
    *) die "unknown argument: $1 (see --help)" ;;
  esac
done

# Every value below ends up in nft rules and (with --persist) in a root systemd unit:
# validate strictly so nothing can inject rules or unit directives.
if ! [[ $COUNT =~ ^[1-9][0-9]{0,2}$ ]] || (( COUNT > 256 )); then die "--count must be 1..256"; fi
[[ $PREFIX =~ ^[A-Za-z][A-Za-z0-9_-]{0,11}$ ]] || die "--prefix must match [A-Za-z][A-Za-z0-9_-]{0,11}"
(( ${#PREFIX} + ${#COUNT} <= 15 )) || die "tap names would exceed 15 bytes"
valid_user() { [[ $1 =~ ^[A-Za-z_][A-Za-z0-9._-]{0,31}$ ]]; }
valid_iface() { [[ $1 =~ ^[A-Za-z0-9][A-Za-z0-9_.-]{0,14}$ ]]; }
[[ -z $USER_NAME ]] || valid_user "$USER_NAME" || die "--user must match [A-Za-z_][A-Za-z0-9._-]{0,31}"
[[ -z $UPLINK ]] || valid_iface "$UPLINK" || die "--uplink is not a valid interface name"
if [[ $DRY_RUN == 0 && $EUID -ne 0 ]]; then
  die "must run as root (sudo); use --dry-run to only print the commands"
fi
root_owned_dir() { # <dir>: owned by root and not group/world-writable, all the way up
  local d="$1"
  while :; do
    [[ -d $d && ! -L $d ]] || return 1
    [[ $(stat -c '%u' "$d") == 0 ]] || return 1
    (( ( 0$(stat -c '%a' "$d") & 022 ) == 0 )) || return 1
    [[ $d == / ]] && return 0
    d="$(dirname "$d")"
  done
}
if [[ $PERSIST == 1 ]] && ! root_owned_dir "$INSTALL_DIR"; then
  msg="$INSTALL_DIR (or a parent) is not a root-owned, non-group/world-writable dir; refusing --persist"
  if [[ $DRY_RUN == 1 ]]; then log "warning: $msg"; else die "$msg"; fi
fi
for tool in ip nft; do
  command -v "$tool" >/dev/null || die "\`$tool\` not found (install iproute2 / nftables)"
done

has_docker_user_chain() {
  command -v iptables >/dev/null && iptables -n -L DOCKER-USER >/dev/null 2>&1
}

docker_rules() { # <-I|-D> : the two DOCKER-USER rules
  local op="$1"
  # shellcheck disable=SC2034,SC2054 # used through the `local -n` nameref below
  local a=(DOCKER-USER -i "${PREFIX}+" -o "$UPLINK" -j ACCEPT)
  # shellcheck disable=SC2034,SC2054
  local b=(DOCKER-USER -o "${PREFIX}+" -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT)
  for rule in a b; do
    local -n r="$rule"
    # Explicit if/fi: a false `[[ .. ]] && cmd` as the last command would make the
    # function return 1 and abort the (set -e) script on every idempotent re-run.
    if iptables -C "${r[@]}" 2>/dev/null; then
      if [[ $op == -D ]]; then run iptables -D "${r[@]}"; fi
    else
      if [[ $op == -I ]]; then run iptables -I "${r[@]}"; fi
    fi
  done
}

if [[ $UNINSTALL == 1 ]]; then
  for ((n = 0; n < COUNT; n++)); do
    if [[ -e /sys/class/net/$PREFIX$n ]]; then
      run ip link del "$PREFIX$n"
    fi
  done
  if nft list table inet "$TABLE" >/dev/null 2>&1; then
    run nft delete table inet "$TABLE"
  fi
  if has_docker_user_chain; then
    UPLINK="${UPLINK:-$(ip -o route show default | awk '{for(i=1;i<=NF;i++) if($i=="dev"){print $(i+1); exit}}')}"
    [[ -n $UPLINK ]] && docker_rules -D
  fi
  if [[ -f $UNIT ]]; then
    run systemctl disable --now "$(basename "$UNIT")" || true
    run rm -f "$UNIT"
    run systemctl daemon-reload
  fi
  if [[ -e $INSTALLED ]]; then
    run rm -f "$INSTALLED"
  fi
  [[ -f $SYSCTL_FILE ]] && run rm -f "$SYSCTL_FILE"
  log "uninstalled (net.ipv4.ip_forward left as is: $(cat /proc/sys/net/ipv4/ip_forward))"
  exit 0
fi

[[ -n $USER_NAME ]] || die "--user is required when not run through sudo"
UID_NUM="$(id -u "$USER_NAME")" || die "unknown user $USER_NAME"
GID_NUM="$(id -g "$USER_NAME")"
if [[ -z $UPLINK ]]; then
  UPLINK="$(ip -o route show default | awk '{for(i=1;i<=NF;i++) if($i=="dev"){print $(i+1); exit}}')"
fi
[[ -n $UPLINK ]] || die "no default route; pass --uplink IFACE"
valid_iface "$UPLINK" || die "default-route interface \`$UPLINK\` is not a valid name; pass --uplink"
log "user=$USER_NAME($UID_NUM:$GID_NUM) taps=${PREFIX}0..${PREFIX}$((COUNT - 1)) uplink=$UPLINK"

# 1. Forwarding.
run sysctl -q -w net.ipv4.ip_forward=1
if [[ $PERSIST == 1 ]]; then
  if [[ $DRY_RUN == 1 ]]; then
    run tee "$SYSCTL_FILE" <<<"net.ipv4.ip_forward = 1"
  else
    echo "net.ipv4.ip_forward = 1" >"$SYSCTL_FILE"
  fi
fi

# 2. Tap pool (recreated if owned by someone else).
for ((n = 0; n < COUNT; n++)); do
  tap="$PREFIX$n"
  if [[ -e /sys/class/net/$tap ]]; then
    owner="$(cat "/sys/class/net/$tap/owner" 2>/dev/null || echo -1)"
    if [[ $owner != "$UID_NUM" ]]; then
      log "$tap is owned by uid $owner; recreating for $USER_NAME"
      run ip link del "$tap"
    fi
  fi
  if [[ ! -e /sys/class/net/$tap ]] || [[ $DRY_RUN == 0 && ${owner:-$UID_NUM} != "$UID_NUM" ]]; then
    run ip tuntap add dev "$tap" mode tap user "$UID_NUM" group "$GID_NUM"
  fi
  unset owner
  run ip addr replace "172.30.$n.1/30" dev "$tap"
  run ip link set dev "$tap" up
done

# 3. nftables: replace our table atomically (add-then-delete makes the delete never fail).
private_rule="" uplink_net_rule="" uplink_gw_rule=""
if [[ $ALLOW_PRIVATE == 0 ]]; then
  private_rule="iifname \"${PREFIX}*\" ip daddr { $PRIVATE_V4 } counter drop"
  # The uplink's own network is the host's LAN even when it is public address space (a
  # dedicated server's provider /24 and its router): its on-link subnet(s) and the default
  # gateway, read from the routing table now (re-read on every run, i.e. every boot with
  # --persist). Separate rules: they may overlap PRIVATE_V4, and an nft set may not.
  valid_v4() { [[ $1 =~ ^([0-9]{1,3}\.){3}[0-9]{1,3}(/[0-9]{1,2})?$ ]]; }
  nets="" gws=""
  for a in $(ip -4 route show dev "$UPLINK" scope link proto kernel | awk '{print $1}'); do
    if valid_v4 "$a"; then nets+="${nets:+, }$a"; fi
  done
  for a in $(ip -4 route show default dev "$UPLINK" | awk '{for(i=1;i<NF;i++) if($i=="via") print $(i+1)}'); do
    if valid_v4 "$a"; then gws+="${gws:+, }$a"; fi
  done
  if [[ -n $nets ]]; then uplink_net_rule="iifname \"${PREFIX}*\" ip daddr { $nets } counter drop"; fi
  if [[ -n $gws ]]; then uplink_gw_rule="iifname \"${PREFIX}*\" ip daddr { $gws } counter drop"; fi
fi
smtp_rule=""
if [[ $ALLOW_SMTP == 0 ]]; then
  # Reject (not drop) so a mailer fails fast instead of hanging until its connect timeout.
  smtp_rule="iifname \"${PREFIX}*\" tcp dport { $SMTP_PORTS } counter reject with tcp reset"
fi
# Anti-spoofing: (tap, guest source) pairs; tap n's guest is 172.30.n.2 (crate: net::TapPlan).
guest_srcs=""
for ((n = 0; n < COUNT; n++)); do
  guest_srcs+="${guest_srcs:+, }\"$PREFIX$n\" . 172.30.$n.2"
done
ruleset="$(cat <<EOF
add table inet $TABLE
delete table inet $TABLE
table inet $TABLE {
  set guest_src {
    type ifname . ipv4_addr
    elements = { $guest_srcs }
  }
  chain prerouting {
    type filter hook prerouting priority raw; policy accept;
    iifname "${PREFIX}*" meta nfproto != ipv4 drop
    iifname "${PREFIX}*" iifname . ip saddr != @guest_src drop
  }
  chain input {
    type filter hook input priority filter - 1; policy accept;
    iifname "${PREFIX}*" ct state established,related accept
    iifname "${PREFIX}*" drop
  }
  chain forward {
    type filter hook forward priority filter - 1; policy accept;
    iifname "${PREFIX}*" oifname "${PREFIX}*" drop
    iifname "${PREFIX}*" ip daddr { $RESERVED_V4 } counter drop
    $private_rule
    $uplink_net_rule
    $uplink_gw_rule
    $smtp_rule
    iifname "${PREFIX}*" oifname "$UPLINK" accept
    oifname "${PREFIX}*" ct state established,related accept
    iifname "${PREFIX}*" drop
    oifname "${PREFIX}*" drop
  }
  chain postrouting {
    type nat hook postrouting priority srcnat; policy accept;
    ip saddr 172.30.0.0/16 oifname "$UPLINK" masquerade
  }
}
EOF
)"
if [[ $DRY_RUN == 1 ]]; then
  echo "+ nft -f - <<'EOF'"; echo "$ruleset"; echo "EOF"
else
  nft -f - <<<"$ruleset"
fi

# 4. Docker coexistence (its FORWARD policy is DROP and our accept cannot override it).
if has_docker_user_chain; then
  log "Docker detected: allowing tap traffic in DOCKER-USER"
  docker_rules -I
fi

# 5. Survive reboots: a root-owned copy in a root-owned dir, and a unit running that copy.
if [[ $PERSIST == 1 ]]; then
  self="$(readlink -f "$0")"
  run install -o root -g root -m 0755 "$self" "$INSTALLED"
  extra=""
  if [[ $ALLOW_PRIVATE == 1 ]]; then extra+=" --allow-private"; fi
  if [[ $ALLOW_SMTP == 1 ]]; then extra+=" --allow-smtp"; fi
  # Docker coupling: PartOf= propagates `systemctl restart/stop docker` to this unit (it is
  # ordered After= docker, so it re-runs once Docker is up again), and WantedBy= docker
  # re-starts it whenever docker.service starts (stop+start, crash auto-restart, boot).
  # Re-running is safe while guests are up: taps are kept, the table swap is atomic and
  # conntrack state survives it.
  docker_part="" docker_wanted=""
  if systemctl cat docker.service >/dev/null 2>&1; then
    docker_part=$'\nPartOf=docker.service'
    docker_wanted=" docker.service"
  fi
  unit="[Unit]
Description=Sandcrab Firecracker tap pool + NAT
After=network-online.target docker.service
Wants=network-online.target$docker_part

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=$INSTALLED --user $USER_NAME --count $COUNT --prefix $PREFIX --uplink $UPLINK$extra

[Install]
WantedBy=multi-user.target$docker_wanted"
  if [[ $DRY_RUN == 1 ]]; then
    echo "+ write $UNIT (root:root 0644):"; echo "$unit"
  else
    ( umask 022; echo "$unit" >"$UNIT" )
    chown root:root "$UNIT"
  fi
  run systemctl daemon-reload
  # reenable: also drops [Install] symlinks of an older unit version.
  run systemctl -q reenable "$(basename "$UNIT")"
fi

if ! id -nG "$USER_NAME" | tr ' ' '\n' | grep -qx kvm; then
  log "note: $USER_NAME is not in group kvm (needed for /dev/kvm): usermod -aG kvm $USER_NAME"
fi
log "done"
