#!/usr/bin/env bash
#
# Install the Helm GUI and the privileged helper on a Linux machine.
#
#   sudo ./packaging/linux/install.sh
#
# The script installs:
#   * /usr/bin/helm-gui          the interface
#   * /usr/bin/helm-helper       the privileged helper
#   * /etc/systemd/system/helm-helper.service
#
# and enables the helper as a system service. The members of the "helm"
# group may talk to the helper; the user that runs the installer is added to
# that group.

set -euo pipefail

PREFIX="${PREFIX:-/usr/bin}"
UNIT_DIR="${UNIT_DIR:-/etc/systemd/system}"
GROUP_NAME="${GROUP_NAME:-helm}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if [[ "${EUID}" -ne 0 ]]; then
    echo "this script has to be run as root" >&2
    exit 1
fi

if [[ ! -x "${ROOT}/target/release/helm-gui" || ! -x "${ROOT}/target/release/helm-helper" ]]; then
    echo "the binaries are missing, run 'cargo build --release' first" >&2
    exit 1
fi

echo "==> installing the binaries"
install -Dm0755 "${ROOT}/target/release/helm-gui" "${PREFIX}/helm-gui"
install -Dm0755 "${ROOT}/target/release/helm-helper" "${PREFIX}/helm-helper"

echo "==> installing the system service"
install -Dm0644 "${ROOT}/packaging/linux/helm-helper.service" \
    "${UNIT_DIR}/helm-helper.service"

echo "==> creating the ${GROUP_NAME} group"
if ! getent group "${GROUP_NAME}" >/dev/null; then
    groupadd --system "${GROUP_NAME}"
fi

SUDO_USER_NAME="${SUDO_USER:-${USER:-}}"
if [[ -n "${SUDO_USER_NAME}" && "${SUDO_USER_NAME}" != "root" ]]; then
    echo "==> adding ${SUDO_USER_NAME} to the ${GROUP_NAME} group"
    usermod -aG "${GROUP_NAME}" "${SUDO_USER_NAME}"
    echo "    log out and back in to pick up the new group"
fi

echo "==> enabling the service"
systemctl daemon-reload
systemctl enable --now helm-helper.service
systemctl status helm-helper.service --no-pager || true

cat <<EOF

Helm is installed.

  * Start the interface with:  helm-gui
  * The helper runs as a system service:  systemctl status helm-helper
  * The log of the helper:               journalctl -u helm-helper -f
EOF
