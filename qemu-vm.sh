#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: qemu-vm.sh <command> [options] [-- command...]

Commands:
  up                 Create/start the VM and wait for SSH.
  down               Stop the VM.
  status             Show VM status.
  ssh [-- cmd...]    Run SSH command in VM (or open shell).

Options for `up`:
  --workspace <dir>  Host path mounted to /app (default: current directory).
  --target-dir <dir> Host path mounted to /target (default: ./target).
  --ssh-port <port>  Localhost SSH port (default: 2222).
  --recreate         Stop a running VM first, then start with current mount paths.

Environment overrides:
  QEMU_VM_NAME              VM name (default: netsim-vm)
  QEMU_VM_STATE_DIR         State root dir (default: ./.qemu-vm)
  QEMU_VM_IMAGE_URL         Cloud image URL
  QEMU_VM_MEM_MB            Memory in MiB (default: 4096)
  QEMU_VM_CPUS              vCPU count (default: 4)
  QEMU_VM_DISK_GB           Disk size in GB for overlay (default: 40)
  QEMU_VM_SSH_USER          VM SSH user (default: dev)
  QEMU_VM_QEMU_BIN          QEMU binary path (default: qemu-system-x86_64)
  QEMU_VM_SEED_PORT         cloud-init HTTP seed port (default: 8555)
  QEMU_VM_VIRTIOFSD_BIN     virtiofsd path (auto-detected when unset)
EOF
}

err() {
  echo "qemu-vm: $*" >&2
  exit 1
}

log() {
  echo "qemu-vm: $*"
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || err "missing required command: $1"
}

abspath() {
  local p="$1"
  if [[ "$p" = /* ]]; then
    echo "$p"
  else
    echo "$PWD/$p"
  fi
}

vm_name="${QEMU_VM_NAME:-netsim-vm}"
state_root="${QEMU_VM_STATE_DIR:-$PWD/.qemu-vm}"
image_url="${QEMU_VM_IMAGE_URL:-https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2}"
mem_mb="${QEMU_VM_MEM_MB:-4096}"
cpus="${QEMU_VM_CPUS:-4}"
disk_gb="${QEMU_VM_DISK_GB:-40}"
ssh_user="${QEMU_VM_SSH_USER:-dev}"
qemu_bin="${QEMU_VM_QEMU_BIN:-qemu-system-x86_64}"
ssh_port="${QEMU_VM_SSH_PORT:-2222}"
seed_port="${QEMU_VM_SEED_PORT:-8555}"
workspace="${QEMU_VM_WORKSPACE:-$PWD}"
target_dir="${QEMU_VM_TARGET_DIR:-$PWD/target}"

vm_dir="$state_root/$vm_name"
base_img="$vm_dir/base.qcow2"
disk_img="$vm_dir/disk.qcow2"
seed_img="$vm_dir/seed.iso"
seed_dir="$vm_dir/seed-http"
user_data="$vm_dir/user-data"
meta_data="$vm_dir/meta-data"
network_cfg="$vm_dir/network-config"
seed_mode_file="$vm_dir/seed-mode"
seed_pid_file="$vm_dir/seed-http.pid"
virtiofsd_bin="${QEMU_VM_VIRTIOFSD_BIN:-}"
fs_mode="9p"
workspace_sock="$vm_dir/workspace.vfs.sock"
target_sock="$vm_dir/target.vfs.sock"
workspace_vfs_pid="$vm_dir/workspace.virtiofsd.pid"
target_vfs_pid="$vm_dir/target.virtiofsd.pid"
pid_file="$vm_dir/qemu.pid"
serial_log="$vm_dir/serial.log"
ssh_key="$vm_dir/id_ed25519"
known_hosts="$vm_dir/known_hosts"
runtime_file="$vm_dir/runtime.env"
recreate=0

ensure_dirs() {
  mkdir -p "$vm_dir"
}

persist_runtime() {
  cat >"$runtime_file" <<EOF
workspace=${workspace}
target_dir=${target_dir}
fs_mode=${fs_mode}
ssh_port=${ssh_port}
EOF
}

check_running_mount_paths() {
  [[ -f "$runtime_file" ]] || return 0
  local running_workspace running_target
  running_workspace="$(awk -F= '$1=="workspace"{print substr($0, index($0,$2)); exit}' "$runtime_file")"
  running_target="$(awk -F= '$1=="target_dir"{print substr($0, index($0,$2)); exit}' "$runtime_file")"
  if [[ -n "$running_workspace" && "$running_workspace" != "$1" ]]; then
    err "VM already running with workspace '${running_workspace}', requested '$1'. Use: $0 up --recreate --workspace '$1' --target-dir '$2'"
  fi
  if [[ -n "$running_target" && "$running_target" != "$2" ]]; then
    err "VM already running with target dir '${running_target}', requested '$2'. Use: $0 up --recreate --workspace '$1' --target-dir '$2'"
  fi
}

cleanup_seed_server() {
  if [[ -f "$seed_pid_file" ]]; then
    local pid
    pid="$(cat "$seed_pid_file" || true)"
    if [[ -n "$pid" ]]; then
      kill "$pid" >/dev/null 2>&1 || true
    fi
    rm -f "$seed_pid_file"
  fi
}

cleanup_virtiofsd() {
  local pid
  for f in "$workspace_vfs_pid" "$target_vfs_pid"; do
    if [[ -f "$f" ]]; then
      pid="$(cat "$f" || true)"
      if [[ -n "$pid" ]]; then
        kill "$pid" >/dev/null 2>&1 || true
      fi
      rm -f "$f"
    fi
  done
  rm -f "$workspace_sock" "$target_sock"
}

detect_virtiofsd_bin() {
  if [[ -n "$virtiofsd_bin" && -x "$virtiofsd_bin" ]]; then
    echo "$virtiofsd_bin"
    return 0
  fi

  local cand
  for cand in \
    /usr/lib/virtiofsd \
    /usr/libexec/virtiofsd \
    /usr/lib/qemu/virtiofsd \
    /usr/bin/virtiofsd \
    /opt/homebrew/libexec/virtiofsd
  do
    if [[ -x "$cand" ]]; then
      echo "$cand"
      return 0
    fi
  done
  return 1
}

select_fs_mode() {
  if virtiofsd_bin="$(detect_virtiofsd_bin)"; then
    fs_mode="virtiofs"
  else
    fs_mode="9p"
  fi
}

is_running() {
  [[ -f "$pid_file" ]] || return 1
  local pid
  pid="$(cat "$pid_file")"
  [[ -n "$pid" ]] || return 1
  kill -0 "$pid" >/dev/null 2>&1
}

detect_accel() {
  local os accel cpu
  os="$(uname -s)"
  accel="tcg"
  cpu="max"
  if [[ "$os" == "Linux" && -r /dev/kvm && -w /dev/kvm ]]; then
    accel="kvm"
    cpu="host"
  elif [[ "$os" == "Darwin" ]]; then
    if "$qemu_bin" -accel help 2>/dev/null | grep -q '^hvf$'; then
      accel="hvf"
      cpu="host"
    fi
  fi
  echo "$accel;$cpu"
}

ensure_image() {
  if [[ -f "$base_img" ]]; then
    return
  fi
  echo "qemu-vm: downloading base image..."
  need_cmd curl
  curl -fsSL "$image_url" -o "$base_img.tmp"
  mv "$base_img.tmp" "$base_img"
}

ensure_key() {
  if [[ -f "$ssh_key" && -f "${ssh_key}.pub" ]]; then
    return
  fi
  need_cmd ssh-keygen
  ssh-keygen -q -t ed25519 -N '' -f "$ssh_key"
}

render_cloud_init() {
  local pub
  pub="$(cat "${ssh_key}.pub")"

  cat >"$user_data" <<EOF
#cloud-config
users:
  - default
  - name: ${ssh_user}
    shell: /bin/bash
    sudo: ALL=(ALL) NOPASSWD:ALL
    groups: [sudo]
    ssh_authorized_keys:
      - ${pub}
ssh_pwauth: false
write_files:
  - path: /etc/modules-load.d/netsim.conf
    permissions: "0644"
    content: |
      sch_netem
      virtiofs
runcmd:
  - modprobe sch_netem || true
  - modprobe virtiofs || true
  - modprobe 9p || true
  - modprobe 9pnet_virtio || true
  - mkdir -p /app /target
EOF

  cat >"$meta_data" <<EOF
instance-id: ${vm_name}
local-hostname: ${vm_name}
EOF

  cat >"$network_cfg" <<'EOF'
version: 2
ethernets:
  eth0:
    dhcp4: true
EOF
}

create_seed_iso() {
  if command -v cloud-localds >/dev/null 2>&1; then
    cloud-localds -N "$network_cfg" "$seed_img" "$user_data" "$meta_data"
    echo "iso" >"$seed_mode_file"
    return 0
  fi

  local mkiso
  if command -v genisoimage >/dev/null 2>&1; then
    mkiso="genisoimage"
  elif command -v mkisofs >/dev/null 2>&1; then
    mkiso="mkisofs"
  elif command -v xorriso >/dev/null 2>&1; then
    mkiso="xorriso -as mkisofs"
  else
    return 1
  fi

  local tmp
  tmp="$(mktemp -d "$vm_dir/seed.XXXXXX")"
  cp "$user_data" "$tmp/user-data"
  cp "$meta_data" "$tmp/meta-data"
  cp "$network_cfg" "$tmp/network-config"
  # shellcheck disable=SC2086
  $mkiso -output "$seed_img" -volid cidata -joliet -rock "$tmp" >/dev/null 2>&1
  rm -rf "$tmp"
  echo "iso" >"$seed_mode_file"
  return 0
}

create_seed_http() {
  need_cmd python3
  mkdir -p "$seed_dir"
  cp "$user_data" "$seed_dir/user-data"
  cp "$meta_data" "$seed_dir/meta-data"
  cp "$network_cfg" "$seed_dir/network-config"
  echo "http" >"$seed_mode_file"
}

create_seed() {
  if create_seed_iso; then
    return
  fi
  create_seed_http
}

start_seed_server() {
  [[ -f "$seed_mode_file" ]] || err "seed mode not initialized"
  if [[ "$(cat "$seed_mode_file")" != "http" ]]; then
    return
  fi

  cleanup_seed_server
  (
    cd "$seed_dir"
    exec python3 -m http.server "$seed_port" --bind 0.0.0.0
  ) >"$vm_dir/seed-http.log" 2>&1 &
  echo "$!" >"$seed_pid_file"
  sleep 1
  if ! kill -0 "$(cat "$seed_pid_file")" >/dev/null 2>&1; then
    err "cloud-init HTTP seed server failed to start on port ${seed_port}"
  fi
}

start_virtiofsd() {
  [[ "$fs_mode" == "virtiofs" ]] || return
  cleanup_virtiofsd

  nohup "$virtiofsd_bin" --shared-dir "$workspace" --socket-path "$workspace_sock" --cache auto --sandbox none --inode-file-handles=never >"$vm_dir/workspace.virtiofsd.log" 2>&1 < /dev/null &
  echo "$!" >"$workspace_vfs_pid"
  nohup "$virtiofsd_bin" --shared-dir "$target_dir" --socket-path "$target_sock" --cache auto --sandbox none --inode-file-handles=never >"$vm_dir/target.virtiofsd.log" 2>&1 < /dev/null &
  echo "$!" >"$target_vfs_pid"

  # Wait briefly for sockets to appear.
  local i
  for i in $(seq 1 30); do
    if [[ -S "$workspace_sock" && -S "$target_sock" ]]; then
      local wp tp
      wp="$(cat "$workspace_vfs_pid" 2>/dev/null || true)"
      tp="$(cat "$target_vfs_pid" 2>/dev/null || true)"
      if [[ -n "$wp" ]] && kill -0 "$wp" >/dev/null 2>&1 && [[ -n "$tp" ]] && kill -0 "$tp" >/dev/null 2>&1; then
        return
      fi
      break
    fi
    sleep 0.1
  done
  err "virtiofsd failed to become healthy; check ${vm_dir}/workspace.virtiofsd.log and ${vm_dir}/target.virtiofsd.log"
}

ensure_disk() {
  need_cmd qemu-img
  if [[ -f "$disk_img" ]]; then
    return
  fi
  qemu-img create -f qcow2 -F qcow2 -b "$base_img" "$disk_img" "${disk_gb}G" >/dev/null
}

wait_for_ssh() {
  local i
  log "waiting for SSH on 127.0.0.1:${ssh_port} ..."
  for i in $(seq 1 180); do
    if ssh_cmd true >/dev/null 2>&1; then
      cleanup_seed_server
      log "SSH is reachable"
      return
    fi
    if (( i % 5 == 0 )) && [[ -f "$serial_log" ]]; then
      log "booting... $(tail -n 1 "$serial_log" | tr -d '\r')"
    fi
    sleep 1
  done
  cleanup_seed_server
  err "VM did not become reachable via SSH on port ${ssh_port}"
}

ensure_guest_mounts() {
  local mnt_opts
  mnt_opts="trans=virtio,version=9p2000.L,msize=262144"

  ssh_cmd "sudo mkdir -p /app /target"
  ssh_cmd "sudo sed -i '/[[:space:]]\\/app[[:space:]].*9p/d; /[[:space:]]\\/target[[:space:]].*9p/d' /etc/fstab || true"
  if [[ "$fs_mode" == "virtiofs" ]]; then
    ssh_cmd "sudo sh -lc 'mountpoint -q /app || mount -t virtiofs workspace /app || mount -t 9p -o ${mnt_opts} workspace /app'"
    ssh_cmd "sudo sh -lc 'mountpoint -q /target || mount -t virtiofs target /target || mount -t 9p -o ${mnt_opts} target /target'"
  else
    ssh_cmd "sudo sh -lc 'mountpoint -q /app || mount -t 9p -o ${mnt_opts} workspace /app || mount -t virtiofs workspace /app'"
    ssh_cmd "sudo sh -lc 'mountpoint -q /target || mount -t 9p -o ${mnt_opts} target /target || mount -t virtiofs target /target'"
  fi

  # Fast sanity checks for this repo's expected paths.
  ssh_cmd "test -f /app/Cargo.toml" || err "/app is mounted but missing /app/Cargo.toml; check host workspace path"
  ssh_cmd "test -d /target" || err "/target mount is not available in guest"
}

ssh_cmd() {
  ssh \
    -i "$ssh_key" \
    -o StrictHostKeyChecking=accept-new \
    -o UserKnownHostsFile="$known_hosts" \
    -o IdentitiesOnly=yes \
    -o ConnectTimeout=5 \
    -p "$ssh_port" \
    "${ssh_user}@127.0.0.1" \
    "$@"
}

start_vm() {
  is_running && return

  need_cmd "$qemu_bin"
  need_cmd ssh
  workspace="$(abspath "$workspace")"
  target_dir="$(abspath "$target_dir")"
  mkdir -p "$target_dir"
  select_fs_mode
  if [[ "$fs_mode" == "virtiofs" ]]; then
    start_virtiofsd
  fi
  start_seed_server

  local accel cpu
  IFS=';' read -r accel cpu <<<"$(detect_accel)"
  local seed_mode
  seed_mode="$(cat "$seed_mode_file")"
  local qemu_seed_args=()
  if [[ "$seed_mode" == "iso" ]]; then
    qemu_seed_args=(
      -drive "if=virtio,media=cdrom,format=raw,readonly=on,file=${seed_img}"
    )
  else
    qemu_seed_args=(
      -smbios "type=1,serial=ds=nocloud-net;s=http://10.0.2.2:${seed_port}/"
    )
  fi

  local qemu_fs_args=()
  if [[ "$fs_mode" == "virtiofs" ]]; then
    qemu_fs_args=(
      -object "memory-backend-memfd,id=mem,size=${mem_mb}M,share=on"
      -numa "node,memdev=mem"
      -chardev "socket,id=workspacefs,path=${workspace_sock}"
      -device "vhost-user-fs-pci,chardev=workspacefs,tag=workspace"
      -chardev "socket,id=targetfs,path=${target_sock}"
      -device "vhost-user-fs-pci,chardev=targetfs,tag=target"
    )
  else
    qemu_fs_args=(
      -virtfs "local,path=${workspace},mount_tag=workspace,security_model=none,multidevs=remap,id=workspace"
      -virtfs "local,path=${target_dir},mount_tag=target,security_model=none,multidevs=remap,id=target"
    )
  fi

  "$qemu_bin" \
    -name "$vm_name" \
    -daemonize \
    -pidfile "$pid_file" \
    -display none \
    -serial "file:${serial_log}" \
    -m "$mem_mb" \
    -smp "$cpus" \
    -accel "$accel" \
    -cpu "$cpu" \
    -drive "if=virtio,format=qcow2,file=${disk_img}" \
    "${qemu_seed_args[@]}" \
    -netdev "user,id=net0,hostfwd=tcp:127.0.0.1:${ssh_port}-:22" \
    -device virtio-net-pci,netdev=net0 \
    "${qemu_fs_args[@]}"
  persist_runtime
}

up() {
  ensure_dirs
  workspace="$(abspath "$workspace")"
  target_dir="$(abspath "$target_dir")"
  log "workspace=${workspace}"
  log "target=${target_dir}"
  if (( recreate )) && is_running; then
    log "recreate requested; stopping existing VM"
    down
  fi
  if is_running; then
    check_running_mount_paths "$workspace" "$target_dir"
    log "vm already running; skipping boot path"
    wait_for_ssh
    log "ensuring /app and /target mounts"
    ensure_guest_mounts
    log "${vm_name} ready (ssh: ${ssh_user}@127.0.0.1:${ssh_port})"
    return
  fi
  ensure_image
  ensure_key
  log "rendering cloud-init"
  render_cloud_init
  create_seed
  ensure_disk
  log "starting qemu"
  start_vm
  wait_for_ssh
  log "ensuring /app and /target mounts"
  ensure_guest_mounts
  log "${vm_name} ready (ssh: ${ssh_user}@127.0.0.1:${ssh_port})"
}

down() {
  cleanup_seed_server
  if ! is_running; then
    cleanup_virtiofsd
    echo "qemu-vm: ${vm_name} is not running"
    return
  fi

  local pid i
  pid="$(cat "$pid_file")"
  kill "$pid" >/dev/null 2>&1 || true
  for i in $(seq 1 20); do
    if ! kill -0 "$pid" >/dev/null 2>&1; then
      rm -f "$pid_file"
      rm -f "$runtime_file"
      cleanup_virtiofsd
      echo "qemu-vm: ${vm_name} stopped"
      return
    fi
    sleep 1
  done

  kill -9 "$pid" >/dev/null 2>&1 || true
  rm -f "$pid_file"
  rm -f "$runtime_file"
  cleanup_virtiofsd
  echo "qemu-vm: ${vm_name} stopped (forced)"
}

status() {
  if is_running; then
    echo "qemu-vm: ${vm_name} running (pid $(cat "$pid_file"), ssh port ${ssh_port})"
  else
    echo "qemu-vm: ${vm_name} stopped"
  fi
}

command="${1:-help}"
if [[ $# -gt 0 ]]; then
  shift
fi

while [[ $# -gt 0 ]]; do
  case "$1" in
    --workspace)
      [[ $# -ge 2 ]] || err "--workspace needs a value"
      workspace="$2"
      shift 2
      ;;
    --target-dir)
      [[ $# -ge 2 ]] || err "--target-dir needs a value"
      target_dir="$2"
      shift 2
      ;;
    --ssh-port)
      [[ $# -ge 2 ]] || err "--ssh-port needs a value"
      ssh_port="$2"
      shift 2
      ;;
    --recreate)
      recreate=1
      shift
      ;;
    --)
      shift
      break
      ;;
    *)
      break
      ;;
  esac
done

case "$command" in
  up)
    up
    ;;
  down)
    down
    ;;
  status)
    status
    ;;
  ssh)
    ensure_dirs
    is_running || err "VM is not running; run: $0 up"
    if [[ $# -gt 0 ]]; then
      ssh_cmd "$@"
    else
      ssh_cmd
    fi
    ;;
  help|-h|--help)
    usage
    ;;
  *)
    usage
    err "unknown command: $command"
    ;;
esac
