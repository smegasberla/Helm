/* The interface of Helm.
 *
 * Nothing here touches a disk or the firmware: every request goes to the
 * privileged helper over its local socket, through the Tauri bridge.
 */

const el = {
  status: document.getElementById("status"),
  statusText: document.getElementById("status-text"),
  banner: document.getElementById("banner"),
  bannerLabel: document.getElementById("banner-label"),
  bannerText: document.getElementById("banner-text"),
  bannerRetry: document.getElementById("banner-retry"),
  railList: document.getElementById("rail-list"),
  railCount: document.getElementById("rail-count"),
  railNote: document.getElementById("rail-note"),
  skeletons: document.getElementById("skeletons"),
  statDisks: document.getElementById("stat-disks"),
  statSystems: document.getElementById("stat-systems"),
  statEntries: document.getElementById("stat-entries"),
  detailIdle: document.getElementById("detail-idle"),
  detailCard: document.getElementById("detail-card"),
  detailKind: document.getElementById("detail-kind"),
  detailName: document.getElementById("detail-name"),
  detailSub: document.getElementById("detail-sub"),
  detailSpecs: document.getElementById("detail-specs"),
  detailEvidence: document.getElementById("detail-evidence"),
  nextboot: document.getElementById("nextboot"),
  nextbootValue: document.getElementById("nextboot-value"),
  actionReboot: document.getElementById("action-reboot"),
  actionSet: document.getElementById("action-set"),
  confirm: document.getElementById("confirm"),
  confirmText: document.getElementById("confirm-text"),
  confirmYes: document.getElementById("confirm-yes"),
  confirmNo: document.getElementById("confirm-no"),
};

const state = {
  report: null,
  selected: null,
  armed: null,
  firmwarePending: false,
  busy: false,
  confirm: null,
  preview: false,
};

const KIND_LABEL = { windows: "Windows", linux: "Linux", macos: "macOS" };

const FS_LABEL = {
  ntfs: "NTFS",
  ext2: "ext2",
  ext3: "ext3",
  ext4: "ext4",
  btrfs: "btrfs",
  xfs: "XFS",
  fat12: "FAT12",
  fat16: "FAT16",
  fat32: "FAT32",
};

/* Sample data, so the interface can be designed and reviewed in a plain
 * browser, where the Tauri bridge does not exist. */
const PREVIEW_REPORT = {
  uefi: true,
  warnings: [],
  disks: [
    {
      index: 0,
      path: "/dev/nvme0n1",
      model: "Samsung SSD 990 PRO 2TB",
      partitions: [
        { number: 1, size_bytes: 268435456, fs: "fat32", is_esp: true },
        { number: 2, size_bytes: 268435456, fs: "ntfs", is_esp: false },
        { number: 3, size_bytes: 1073741824, fs: "ext4", is_esp: false },
      ],
    },
    {
      index: 1,
      path: "/dev/nvme1n1",
      model: "WD Black SN850X 1TB",
      partitions: [{ number: 1, size_bytes: 42949672960, fs: "ext4", is_esp: false }],
    },
  ],
  oses: [
    {
      id: "disk0p3",
      name: "Fedora Linux 41",
      kind: "linux",
      disk_index: 0,
      disk_label: "Samsung SSD 990 PRO 2TB",
      partition_number: 3,
      partition_label: "fedora-root",
      fs: "ext4",
      distro: "Fedora Linux 41 (Workstation Edition)",
      boot: {
        label: "\\EFI\\fedora\\shimx64.efi",
        mode: { mode: "existing_entry", source_var: "Boot0001" },
      },
      evidence: [
        "/etc/os-release names Fedora Linux 41",
        "/boot/vmlinuz-6.11.4 present",
        "GRUB configuration found",
        "\\EFI\\fedora\\shimx64.efi on the EFI System Partition",
      ],
      bootable: true,
    },
    {
      id: "disk0p2",
      name: "Windows 11 Pro",
      kind: "windows",
      disk_index: 0,
      disk_label: "Samsung SSD 990 PRO 2TB",
      partition_number: 2,
      partition_label: "Windows",
      fs: "ntfs",
      distro: "Windows 11 Pro 24H2",
      boot: {
        label: "\\EFI\\Microsoft\\Boot\\bootmgfw.efi",
        mode: { mode: "synthetic", esp_partition_number: 1 },
      },
      evidence: [
        "\\Windows\\System32\\winload.efi present",
        "BCD store found",
        "\\EFI\\Microsoft\\Boot\\bootmgfw.efi on the EFI System Partition",
      ],
      bootable: true,
    },
    {
      id: "disk1p1",
      name: "Arch Linux",
      kind: "linux",
      disk_index: 1,
      disk_label: "WD Black SN850X 1TB",
      partition_number: 1,
      partition_label: "arch",
      fs: "ext4",
      distro: "Arch Linux",
      boot: null,
      evidence: ["/etc/os-release names Arch Linux", "no loader on any EFI System Partition"],
      bootable: false,
    },
  ],
  boot_entries: [
    { var_name: "Boot0000", description: "Windows Boot Manager" },
    { var_name: "Boot0001", description: "Fedora" },
    { var_name: "Boot2001", description: "EFI USB Device" },
  ],
};

/* --------------------------------------------------------------- plumbing */

function bridge() {
  return (
    window.__TAURI__?.core?.invoke ??
    window.__TAURI__?.invoke ??
    window.__TAURI_INTERNALS__?.invoke ??
    null
  );
}

async function call(command, args) {
  const invoke = bridge();
  if (invoke) return invoke(command, args);
  state.preview = true;
  return previewCall(command, args);
}

function previewCall(command, args) {
  switch (command) {
    case "helper_status":
      return {
        reachable: true,
        version: "0.1.0",
        uefi: true,
        can_write_boot_variables: true,
        message: "The firmware variables are available.",
      };
    case "detect_oses":
      return PREVIEW_REPORT;
    case "boot_next":
      return { pending: false };
    case "set_boot_next":
      return { os_id: args.osId, label: "" };
    case "reboot":
      return null;
    default:
      throw new Error(`unknown command ${command}`);
  }
}

function setStatus(kind, text) {
  el.status.dataset.state = kind;
  el.statusText.textContent = text;
}

function showBanner(label, text, retryable) {
  el.bannerLabel.textContent = label;
  el.bannerText.textContent = text;
  el.bannerRetry.hidden = !retryable;
  el.banner.hidden = false;
}

function hideBanner() {
  el.banner.hidden = true;
  el.bannerText.textContent = "";
}

function setBusy(busy) {
  state.busy = busy;
  const os = osById(state.selected);
  const enabled = Boolean(os?.bootable) && !busy;
  el.actionReboot.disabled = !enabled;
  el.actionSet.disabled = !enabled;
}

function formatBytes(bytes) {
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

function osById(id) {
  return (state.report?.oses ?? []).find((os) => os.id === id) ?? null;
}

function partitionOf(os) {
  const disk = (state.report?.disks ?? []).find((entry) => entry.index === os.disk_index);
  return (disk?.partitions ?? []).find((part) => part.number === os.partition_number) ?? null;
}

function diskOf(os) {
  return (state.report?.disks ?? []).find((entry) => entry.index === os.disk_index) ?? null;
}

function metaLine(os) {
  const parts = [os.disk_label || "unknown disk"];
  parts.push(`partition ${os.partition_number}`);
  parts.push(FS_LABEL[os.fs] ?? os.fs ?? "unknown filesystem");
  if (os.partition_label) parts.push(os.partition_label);
  return parts.join(" · ");
}

function loaderText(os) {
  if (!os.boot) return "no UEFI loader found";
  const mode = os.boot.mode ?? {};
  if (mode.mode === "existing_entry") {
    return `${os.boot.label} · firmware entry ${mode.source_var}`;
  }
  return `${os.boot.label} · built from the EFI System Partition`;
}

function rowState(os) {
  if (state.armed === os.id) return { text: "next boot", kind: "armed" };
  if (!os.bootable) return { text: "no loader", kind: "blocked" };
  return { text: "ready", kind: "ready" };
}

/* ------------------------------------------------------------------ views */

function renderRail() {
  const oses = state.report?.oses ?? [];
  el.railList.textContent = "";
  el.railCount.textContent = String(oses.length);

  oses.forEach((os, index) => {
    const item = document.createElement("li");

    const row = document.createElement("button");
    row.type = "button";
    row.className = "row";
    row.setAttribute("role", "option");
    row.setAttribute("aria-selected", String(os.id === state.selected));
    row.dataset.id = os.id;
    row.dataset.bootable = String(Boolean(os.bootable));

    const order = document.createElement("span");
    order.className = "row-index";
    order.textContent = String(index + 1).padStart(2, "0");

    const main = document.createElement("span");
    main.className = "row-main";
    const name = document.createElement("span");
    name.className = "row-name";
    name.textContent = os.name;
    const meta = document.createElement("span");
    meta.className = "row-meta";
    meta.textContent = metaLine(os);
    main.append(name, meta);

    const badge = rowState(os);
    const status = document.createElement("span");
    status.className = "row-state";
    status.dataset.state = badge.kind;
    status.textContent = badge.text;

    row.append(order, main, status);
    row.addEventListener("click", () => select(os.id));
    item.append(row);
    el.railList.append(item);
  });

  if (!oses.length && !state.busy) {
    el.railNote.hidden = false;
    el.railNote.textContent =
      "No operating system was found on the disks of this machine. Helm looks for the " +
      "files that prove a system can boot; nothing is ever mounted.";
  } else {
    el.railNote.hidden = true;
  }
}

function appendSpec(term, suffix, dataState) {
  const dt = document.createElement("dt");
  dt.textContent = term;
  const dd = document.createElement("dd");
  dd.textContent = suffix;
  if (dataState) dd.dataset.state = dataState;
  el.detailSpecs.append(dt, dd);
}

function renderNextBoot() {
  const armed = state.armed ? osById(state.armed) : null;
  if (armed) {
    el.nextboot.dataset.state = "armed";
    el.nextbootValue.textContent = `${armed.name} · ${armed.boot?.label ?? "loader"}`;
    return;
  }
  if (state.firmwarePending) {
    el.nextboot.dataset.state = "armed";
    el.nextbootValue.textContent = "a one-shot target is already armed";
    return;
  }
  el.nextboot.dataset.state = "idle";
  el.nextbootValue.textContent = "Firmware boot order";
}

function renderDetail() {
  const os = osById(state.selected);

  if (!os) {
    el.detailIdle.hidden = false;
    el.detailCard.hidden = true;
    return;
  }

  el.detailIdle.hidden = true;
  el.detailCard.hidden = false;

  el.detailKind.textContent = KIND_LABEL[os.kind] ?? os.kind ?? "system";
  el.detailName.textContent = os.name;
  el.detailSub.textContent = os.distro || os.disk_label || "";

  el.detailSpecs.textContent = "";
  const disk = diskOf(os);
  appendSpec("Disk", disk ? `${os.disk_label} · ${disk.path}` : os.disk_label || "unknown disk");
  appendSpec(
    "Partition",
    `${os.partition_number}${os.partition_label ? ` · ${os.partition_label}` : ""}`,
  );
  const part = partitionOf(os);
  appendSpec(
    "Filesystem",
    `${FS_LABEL[os.fs] ?? os.fs ?? "unknown"}${part ? ` · ${formatBytes(part.size_bytes)}` : ""}`,
  );
  appendSpec("Loader", loaderText(os), os.boot ? undefined : "blocked");
  appendSpec("Firmware", state.report?.uefi ? "UEFI" : "legacy BIOS", state.report?.uefi ? undefined : "blocked");

  el.detailEvidence.textContent = "";
  const evidence = os.evidence ?? [];
  const lines = evidence.length ? evidence : ["no signal was recorded for this system"];
  for (const line of lines) {
    const item = document.createElement("li");
    item.textContent = line;
    el.detailEvidence.append(item);
  }

  renderNextBoot();
  hideConfirm();
  setBusy(state.busy);
}

function renderSummary() {
  const report = state.report;
  el.statDisks.textContent = report ? String((report.disks ?? []).length) : "—";
  el.statSystems.textContent = report ? String((report.oses ?? []).length) : "—";
  el.statEntries.textContent = report ? String((report.boot_entries ?? []).length) : "—";
}

/* ---------------------------------------------------------------- actions */

function select(id) {
  if (state.selected === id) return;
  state.selected = id;
  for (const row of el.railList.querySelectorAll(".row")) {
    row.setAttribute("aria-selected", String(row.dataset.id === id));
  }
  renderDetail();
}

function armConfirm(mode) {
  const os = osById(state.selected);
  if (!os || !os.bootable || state.busy) return;
  state.confirm = mode;
  el.confirmText.textContent =
    mode === "reboot"
      ? `BootNext is written for ${os.name} and the machine reboots right away.`
      : `BootNext is written for ${os.name}. The next start uses it once, then the usual ` +
        "boot order returns.";
  el.confirm.hidden = false;
  el.confirmYes.focus();
}

function hideConfirm() {
  state.confirm = null;
  el.confirm.hidden = true;
}

async function runSwitch(withReboot) {
  const os = osById(state.selected);
  if (!os) return;
  hideConfirm();
  setBusy(true);
  setStatus("busy", `Writing BootNext for ${os.name}…`);

  try {
    await call("set_boot_next", { osId: os.id });
    state.armed = os.id;
    state.firmwarePending = false;
    renderRail();
    renderDetail();
    if (withReboot) {
      setStatus("busy", "The machine is rebooting.");
      await call("reboot");
    } else {
      setStatus("ok", `BootNext is set to ${os.name}.`);
    }
  } catch (error) {
    setStatus("error", "The boot target could not be written.");
    showBanner("Firmware", String(error), false);
  } finally {
    setBusy(false);
  }
}

/* ------------------------------------------------------------------- load */

async function refreshHelper() {
  try {
    const status = await call("helper_status");
    if (!status.reachable) {
      setStatus("error", status.message || "the helper is not reachable");
      showBanner("Helper", status.message || "the helper is not reachable", true);
      return status;
    }
    const version = status.version ? `helper ${status.version} · ` : "";
    setStatus(status.can_write_boot_variables ? "ok" : "error", `${version}${status.message}`);
    if (!status.can_write_boot_variables) {
      showBanner("No firmware write", status.message, false);
    }
    return status;
  } catch (error) {
    setStatus("error", "the helper is not reachable");
    showBanner("Helper", String(error), true);
    return { reachable: false, can_write_boot_variables: false };
  }
}

async function loadPending() {
  try {
    const data = await call("boot_next");
    state.firmwarePending = Boolean(data?.pending) && !state.armed;
  } catch {
    // A machine without firmware variables simply has nothing pending.
    state.firmwarePending = false;
  }
  renderNextBoot();
}

async function detect() {
  hideBanner();
  state.busy = true;
  state.selected = null;
  state.report = null;
  el.railList.textContent = "";
  el.railNote.hidden = true;
  el.skeletons.hidden = false;
  el.railCount.textContent = "—";
  renderSummary();
  renderDetail();
  setStatus("busy", "Reading the disks of the machine…");

  try {
    const report = await call("detect_oses");
    state.report = report;
    state.busy = false;
    el.skeletons.hidden = true;
    renderSummary();

    const first = (report.oses ?? [])[0] ?? null;
    state.selected = first ? first.id : null;
    renderRail();
    renderDetail();
    setStatus("ok", "Detection finished.");

    if (state.preview) {
      showBanner("Sample data", "No Tauri runtime here: the systems shown are examples.", false);
    }
    await loadPending();
  } catch (error) {
    state.busy = false;
    el.skeletons.hidden = true;
    setStatus("error", "The detection failed.");
    showBanner("Detection", String(error), true);
    renderRail();
    renderDetail();
  }
}

el.actionReboot.addEventListener("click", () => armConfirm("reboot"));
el.actionSet.addEventListener("click", () => armConfirm("set"));
el.confirmYes.addEventListener("click", () => runSwitch(state.confirm === "reboot"));
el.confirmNo.addEventListener("click", hideConfirm);
el.bannerRetry.addEventListener("click", async () => {
  await refreshHelper();
  await detect();
});

document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && !el.confirm.hidden) {
    hideConfirm();
    return;
  }
  if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
  if (!event.target?.closest?.(".row") && event.target !== el.railList) return;

  const oses = state.report?.oses ?? [];
  if (!oses.length) return;
  const current = oses.findIndex((os) => os.id === state.selected);
  const step = event.key === "ArrowDown" ? 1 : -1;
  const next = Math.min(oses.length - 1, Math.max(0, (current < 0 ? 0 : current) + step));
  event.preventDefault();
  select(oses[next].id);
  el.railList.querySelector(`.row[data-id="${oses[next].id}"]`)?.focus();
});

(async function main() {
  renderSummary();
  renderDetail();
  await refreshHelper();
  await detect();
})();
