/* The interface of Helm.
 *
 * The interface is unprivileged: everything that needs root rights is done
 * by the helper, which is reached over the local socket.
 */

const state = {
  report: null,
  pending: null,
};

const elements = {
  helperState: document.getElementById("helper-state"),
  helperMessage: document.getElementById("helper-message"),
  rescan: document.getElementById("rescan"),
  retry: document.getElementById("retry"),
  grid: document.getElementById("os-grid"),
  placeholder: document.getElementById("placeholder"),
  placeholderText: document.getElementById("placeholder-text"),
  spinner: document.getElementById("spinner"),
  errorBox: document.getElementById("error-box"),
  errorText: document.getElementById("error-text"),
  summary: document.getElementById("summary"),
  summaryDisks: document.getElementById("summary-disks"),
  summarySystems: document.getElementById("summary-systems"),
  summaryEntries: document.getElementById("summary-entries"),
  footer: document.getElementById("footer-info"),
  modal: document.getElementById("modal"),
  modalTitle: document.getElementById("modal-title"),
  modalBody: document.getElementById("modal-body"),
  modalDetails: document.getElementById("modal-details"),
  modalCancel: document.getElementById("modal-cancel"),
  modalConfirm: document.getElementById("modal-confirm"),
};

const ICONS = {
  linux: `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 2c-2 0-3 2-3 4 0 1.5-.5 2.5-1.5 4C6 11.5 5 13.5 5 16c0 3 3.5 6 7 6s7-3 7-6c0-2.5-1-4.5-2.5-6-1-1.5-1.5-2.5-1.5-4 0-2-1-4-3-4Zm-2 6c.6 0 1 .4 1 1s-.4 1-1 1-1-.4-1-1 .4-1 1-1Zm4 0c.6 0 1 .4 1 1s-.4 1-1 1-1-.4-1-1 .4-1 1-1Zm-2 5.5c1.2 0 2 .6 2 1.2 0 .7-.9 1.3-2 1.3s-2-.6-2-1.3c0-.6.8-1.2 2-1.2Z"/></svg>`,
  windows: `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M3 5.5 10.4 4.4v7.1H3V5.5Zm8.6-1.3L21 3v8.5h-9.4V4.2ZM3 12.7h7.4v7.1L3 18.7v-6Zm8.6 0H21V21l-9.4-1.2v-7.1Z"/></svg>`,
  apple: `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M16.5 12.8c0-2.3 1.9-3.4 2-3.5-1.1-1.6-2.8-1.8-3.4-1.8-1.4-.1-2.7.8-3.4.8-.7 0-1.8-.8-3-.8-1.5 0-3 .9-3.8 2.3-1.6 2.8-.4 7 1.2 9.3.8 1.1 1.7 2.4 2.9 2.3 1.2 0 1.6-.7 3.1-.7 1.4 0 1.9.7 3.1.7 1.3 0 2.1-1.1 2.8-2.2.9-1.3 1.3-2.6 1.3-2.7 0 0-2.5-1-2.5-3.7ZM14.3 5.9c.6-.8 1.1-1.9 1-3-1 0-2.2.7-2.9 1.5-.6.7-1.1 1.9-1 3 1.1.1 2.2-.6 2.9-1.5Z"/></svg>`,
};

function kindOf(os) {
  return os.kind === "windows" ? "windows" : os.kind === "macos" ? "apple" : "linux";
}

function setHelperState(kind, message) {
  elements.helperState.dataset.state = kind;
  elements.helperMessage.textContent = message;
}

function showError(message) {
  elements.errorBox.hidden = false;
  elements.errorText.textContent = message;
  elements.placeholder.hidden = true;
  elements.grid.innerHTML = "";
  elements.summary.hidden = true;
}

function clearError() {
  elements.errorBox.hidden = true;
  elements.errorText.textContent = "";
}

function formatSize(bytes) {
  if (!bytes) return "";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(value < 10 && unit > 0 ? 1 : 0)} ${units[unit]}`;
}

function osMeta(os) {
  const parts = [];
  if (os.disk_label) parts.push(os.disk_label);
  if (os.partition_label) parts.push(os.partition_label);
  parts.push(`partition ${os.partition_number}`);
  if (os.fs) parts.push(os.fs);
  return parts.join(" · ");
}

function cardFor(os) {
  const kind = kindOf(os);
  const card = document.createElement("article");
  card.className = "os-card";
  card.dataset.kind = kind;

  const head = document.createElement("div");
  head.className = "os-head";

  const icon = document.createElement("div");
  icon.className = "os-icon";
  icon.innerHTML = ICONS[kind] || ICONS.linux;

  const title = document.createElement("div");
  title.className = "os-title";
  const heading = document.createElement("h2");
  heading.textContent = os.name;
  if (!os.bootable) {
    const tag = document.createElement("span");
    tag.className = "tag not-bootable";
    tag.textContent = "no UEFI loader";
    heading.appendChild(tag);
  }
  const meta = document.createElement("p");
  meta.textContent = osMeta(os);
  title.appendChild(heading);
  title.appendChild(meta);
  head.appendChild(icon);
  head.appendChild(title);

  const evidence = document.createElement("ul");
  evidence.className = "evidence";
  for (const line of os.evidence || []) {
    const item = document.createElement("li");
    item.textContent = line;
    evidence.appendChild(item);
  }

  const actions = document.createElement("div");
  actions.className = "os-actions";
  const button = document.createElement("button");
  button.type = "button";
  button.className = "button primary";
  button.textContent = "Boot this system";
  button.disabled = !os.bootable;
  button.addEventListener("click", () => askForSwitch(os));
  actions.appendChild(button);

  card.appendChild(head);
  if ((os.evidence || []).length) card.appendChild(evidence);
  card.appendChild(actions);
  return card;
}

function render(report) {
  state.report = report;
  elements.grid.innerHTML = "";

  const systems = report.oses || [];
  if (!systems.length) {
    elements.placeholder.hidden = false;
    elements.spinner.hidden = true;
    elements.placeholderText.textContent =
      "No operating system was found on the disks of this machine.";
    elements.summary.hidden = true;
  } else {
    elements.placeholder.hidden = true;
    for (const os of systems) {
      elements.grid.appendChild(cardFor(os));
    }
    elements.summary.hidden = false;
    elements.summaryDisks.textContent = String((report.disks || []).length);
    elements.summarySystems.textContent = String(systems.length);
    elements.summaryEntries.textContent = String((report.boot_entries || []).length);
  }

  const warnings = report.warnings || [];
  elements.footer.textContent = warnings.length
    ? warnings.join(" · ")
    : "Helm reads the disks directly: no filesystem is ever mounted.";
}

async function refreshHelper() {
  try {
    const status = await invoke("helper_status");
    if (status.reachable) {
      setHelperState("ok", `Helper ${status.version} · ${status.message}`);
    } else {
      setHelperState("error", status.message);
    }
    return status;
  } catch (error) {
    setHelperState("error", String(error));
    return { reachable: false, message: String(error) };
  }
}

async function detect() {
  clearError();
  elements.placeholder.hidden = false;
  elements.spinner.hidden = false;
  elements.placeholderText.textContent = "Reading the disks of the machine…";
  elements.grid.innerHTML = "";
  setHelperState("busy", "Asking the helper for a detection…");
  elements.rescan.disabled = true;

  try {
    const report = await invoke("detect_oses");
    render(report);
    setHelperState("ok", "Detection finished.");
  } catch (error) {
    showError(String(error));
    setHelperState("error", "The detection failed.");
  } finally {
    elements.rescan.disabled = false;
    elements.spinner.hidden = true;
  }
}

function askForSwitch(os) {
  state.pending = os;
  elements.modalTitle.textContent = `Boot ${os.name}?`;
  elements.modalBody.textContent =
    "The firmware will boot this system on the next start. The machine reboots right away; " +
    "if the boot fails, the firmware falls back to the usual boot order.";
  elements.modalDetails.innerHTML = "";
  for (const line of [osMeta(os), os.boot ? `loader: ${os.boot.label}` : ""]) {
    if (!line) continue;
    const item = document.createElement("li");
    item.textContent = line;
    elements.modalDetails.appendChild(item);
  }
  elements.modal.hidden = false;
  elements.modalConfirm.focus();
}

function closeModal() {
  elements.modal.hidden = true;
  state.pending = null;
}

async function confirmSwitch() {
  const os = state.pending;
  if (!os) return;
  closeModal();
  setHelperState("busy", "Setting the boot target…");
  elements.rescan.disabled = true;
  try {
    await invoke("set_boot_next", { osId: os.id });
    setHelperState("busy", "Rebooting…");
    await invoke("reboot");
    setHelperState("busy", "The machine is rebooting.");
  } catch (error) {
    setHelperState("error", String(error));
    showError(String(error));
  } finally {
    elements.rescan.disabled = false;
  }
}

async function invoke(command, args) {
  const { invoke } = window.__TAURI__ ? window.__TAURI__ : {};
  if (invoke) return invoke(command, args);
  return window.__TAURI_INTERNALS__.invoke(command, args);
}

elements.rescan.addEventListener("click", () => {
  refreshHelper();
  detect();
});
elements.retry.addEventListener("click", () => {
  refreshHelper();
  detect();
});
elements.modalCancel.addEventListener("click", closeModal);
elements.modalConfirm.addEventListener("click", confirmSwitch);
elements.modal.addEventListener("click", (event) => {
  if (event.target === elements.modal) closeModal();
});
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && !elements.modal.hidden) closeModal();
});

(async function main() {
  await refreshHelper();
  detect();
})();
