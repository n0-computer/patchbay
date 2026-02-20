#!/usr/bin/env bash
set -euo pipefail

echo "setcap.sh: starting capability setup..."

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "setcap.sh: this only works on Linux."
    exit 1
fi

if ! command -v setcap >/dev/null 2>&1; then
    echo "setcap.sh: setcap not found. Install libcap2-bin (Debian/Ubuntu) or libcap (Fedora)."
    exit 1
fi

caps="cap_net_admin,cap_sys_admin,cap_net_raw+ep"
system_tools=(ip tc nft ping ping6)

lookup_tool() {
    local tool="$1"
    local lookup_path="/usr/sbin:/sbin:/usr/bin:/bin:${PATH}"
    if [[ "${need_sudo:-0}" -eq 1 ]]; then
        sudo env PATH="$lookup_path" which "$tool" 2>/dev/null || true
    else
        PATH="$lookup_path" which "$tool" 2>/dev/null || true
    fi
}

echo "setcap.sh: this will apply '${caps}' to:"
echo "  - system tools: ${system_tools[*]}"
echo "  - project binaries/tests under target dir:"
echo "    * <target>/debug/<crate>"
echo "    * <target>/release/<crate>"
echo "    * <target>/debug/deps/<crate>-* (test binaries)"

need_sudo=0
if [[ "$EUID" -ne 0 ]]; then
    need_sudo=1
    if ! command -v sudo >/dev/null 2>&1; then
        echo "setcap.sh: sudo not found; run as root."
        exit 1
    fi
fi

maybe_sudo=()
if [[ "$need_sudo" -eq 1 ]]; then
    maybe_sudo=(sudo)
    echo "setcap.sh: requesting sudo privileges..."
    sudo -v
fi

crate_name="$(cargo metadata --format-version 1 --no-deps | jq -r '.packages[0].name')"
if [[ -z "${crate_name}" ]]; then
    echo "setcap.sh: failed to determine crate name via cargo metadata."
    exit 1
fi

export CARGO_TARGET_DIR="$(cargo metadata --format-version 1 --no-deps | jq -r '.target_directory')"
echo "Using CARGO_TARGET_DIR=${CARGO_TARGET_DIR}"

echo "Required capabilities: $caps"
echo "Granting capabilities to system tools if present..."
for tool in "${system_tools[@]}"; do
    bin_path="$(lookup_tool "$tool")"
    if [[ -n "$bin_path" && -x "$bin_path" ]]; then
        echo "  setcap -> $bin_path"
        "${maybe_sudo[@]}" setcap "$caps" "$bin_path"
    else
        echo "  skip (not found): $tool"
    fi
done

echo "Building binaries (no run) to locate outputs..."
cargo build
cargo test --no-run

host_target="$(rustc -vV | awk '/^host:/{print $2}')"
base_target_dir="${CARGO_TARGET_DIR}"
if [[ -z "${base_target_dir}" || "${base_target_dir}" == "null" ]]; then
    echo "setcap.sh: failed to determine target dir via cargo metadata."
    exit 1
fi

target_dirs=("${base_target_dir}" "${base_target_dir}/${host_target}")

echo "Granting capabilities to ${crate_name} binaries in discovered target dirs..."
shopt -s nullglob
for target_dir in "${target_dirs[@]}"; do
    for bin in "${target_dir}/debug/${crate_name}" "${target_dir}/release/${crate_name}"; do
        if [[ -x "$bin" ]]; then
            echo "  setcap -> $bin"
            "${maybe_sudo[@]}" setcap "$caps" "$bin"
        fi
    done

    for bin in "${target_dir}/debug/deps/${crate_name}-"*; do
        if [[ -x "$bin" && ! "$bin" =~ \.d$ ]]; then
            echo "  setcap -> $bin"
            "${maybe_sudo[@]}" setcap "$caps" "$bin"
        fi
    done
done

# echo "Probing netns creation capability..."
# probe_ns="netsim-cap-probe-$$"
# if ip netns add "${probe_ns}" >/dev/null 2>&1; then
#   ip netns del "${probe_ns}" >/dev/null 2>&1 || true
#   echo "  probe ok: ip netns add works without sudo."
# else
#   echo "  probe failed: ip netns add is blocked on this host."
#   echo "  this usually means mount propagation changes are not permitted for non-root sessions."
#   echo "  use 'sudo cargo run/test' locally or run via the Lima VM tasks."
#   exit 1
# fi

echo
echo "Setup complete."
echo "You can now run locally without sudo as long as you don't rebuild."
echo "If you rebuild, rerun ./setcap.sh to reapply capabilities."
