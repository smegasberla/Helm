/* The guest strip of Helm.
 *
 * The boot switch and the guest are two different things, so they live in two
 * files: this one only knows about the gaming profile — whether the dedicated
 * card can be handed to a Windows guest, and whether one is running. It never
 * touches a disk or a PCI function itself; everything goes through the helper.
 */

const guest = {
  strip: document.getElementById("guest"),
  state: document.getElementById("guest-state"),
  detail: document.getElementById("guest-detail"),
  start: document.getElementById("guest-start"),
  stop: document.getElementById("guest-stop"),
};

const guestState = {
  plan: null,
  running: false,
  pid: null,
  busy: false,
  preview: false,
};

/* Sample plan, so the strip can be designed and reviewed in a plain browser,
 * where the Tauri bridge does not exist. */
const PREVIEW_PLAN = {
  profile: {
    name: "helm-windows",
    gpu: "0000:01:00.0",
    windows_disk: "/dev/nvme0n1p3",
    windows_esp: "/dev/nvme0n1p1",
    guest_threads: 12,
    guest_affinity: "0-11",
    memory: { reserve: 8 },
    display: "looking_glass",
    hide_hypervisor: true,
  },
  command:
    "qemu-system-x86_64 -name helm-windows -machine q35,accel=kvm -cpu host,kvm=off -smp 12,cores=12,threads=1,sockets=1 -m 16384",
  problems: [
    "/dev/kvmfr0 is missing: install looking-glass-module-dkms and load the kvmfr module",
  ],
  gpu: {
    address: "0000:01:00.0",
    vendor_device: "10de:2520",
    iommu_group: "15",
    isolated: true,
    driver: "nvidia",
  },
};

function guestBridge() {
  return (
    window.__TAURI__?.core?.invoke ??
    window.__TAURI__?.invoke ??
    window.__TAURI_INTERNALS__?.invoke ??
    null
  );
}

async function guestCall(command, args) {
  const invoke = guestBridge();
  if (invoke) return invoke(command, args);
  guestState.preview = true;
  return previewGuestCall(command, args);
}

function previewGuestCall(command) {
  switch (command) {
    case "vm_plan":
      return PREVIEW_PLAN;
    case "vm_status":
      return { running: false, pid: null };
    case "vm_start":
      return { pid: 4242 };
    case "vm_stop":
      return { stopped: false };
    default:
      throw new Error(`unknown command ${command}`);
  }
}

/* ------------------------------------------------------------------ render */

function setGuestStrip(state, label, detail) {
  guest.strip.dataset.state = state;
  guest.state.textContent = label;
  guest.detail.textContent = detail;
}

function setGuestBusy(busy) {
  guestState.busy = busy;
  const ready = Boolean(guestState.plan?.problems?.length === 0);
  guest.start.disabled = busy || guestState.running || !ready;
  guest.stop.disabled = busy || !guestState.running;
}

function renderGuest() {
  const plan = guestState.plan;
  if (guestState.running) {
    setGuestStrip(
      "running",
      "Guest running",
      `Windows owns the dedicated card${guestState.pid ? ` (pid ${guestState.pid})` : ""}. ` +
        "Linux keeps the panel and the efficiency cores.",
    );
    setGuestBusy(guestState.busy);
    return;
  }

  if (!plan) {
    setGuestStrip("unknown", "Not checked", "The helper did not answer.");
    setGuestBusy(guestState.busy);
    return;
  }

  const gpu = plan.gpu;
  const problems = plan.problems ?? [];
  const cores = plan.profile?.guest_threads ?? 0;
  const memory = plan.profile?.memory_mib;

  if (problems.length) {
    setGuestStrip("blocked", `Blocked (${problems.length})`, problems.join(" · "));
    setGuestBusy(guestState.busy);
    return;
  }

  const card = gpu
    ? `${gpu.vendor_device} in IOMMU group ${gpu.iommu_group ?? "?"}`
    : "no card described";
  const who = gpu?.driver ? `taken from ${gpu.driver}` : "unclaimed by a driver";
  const size = memory ? ` · ${Math.round(memory / 1024)} GiB` : "";
  setGuestStrip(
    "ready",
    "Ready",
    `${card}, ${who} · ${cores} threads${size} · ${plan.profile?.display === "looking_glass" ? "Looking Glass window" : "its own monitor"}`,
  );
  setGuestBusy(guestState.busy);
}

/* ----------------------------------------------------------------- actions */

async function refreshGuest() {
  try {
    const [plan, status] = await Promise.all([
      guestCall("vm_plan"),
      guestCall("vm_status"),
    ]);
    guestState.plan = plan;
    guestState.running = Boolean(status?.running);
    guestState.pid = status?.pid ?? null;
  } catch (error) {
    guestState.plan = null;
    setGuestStrip("unknown", "Not checked", String(error));
  }
  renderGuest();
}

async function startGuest() {
  if (guestState.busy || guestState.running) return;
  setGuestBusy(true);
  setGuestStrip("running", "Starting", "The card is being handed to the guest…");
  try {
    const answer = await guestCall("vm_start");
    guestState.running = true;
    guestState.pid = answer?.pid ?? null;
    setGuestStrip(
      "running",
      "Guest running",
      "The interface started Windows on the dedicated card.",
    );
  } catch (error) {
    setGuestStrip("blocked", "Refused", String(error));
    await refreshGuest();
    return;
  }
  setGuestBusy(false);
}

async function stopGuest() {
  if (guestState.busy || !guestState.running) return;
  setGuestBusy(true);
  try {
    await guestCall("vm_stop");
  } catch (error) {
    setGuestStrip("blocked", "Stop failed", String(error));
    setGuestBusy(false);
    return;
  }
  guestState.running = false;
  guestState.pid = null;
  setGuestBusy(false);
  await refreshGuest();
}

guest.start.addEventListener("click", startGuest);
guest.stop.addEventListener("click", stopGuest);

(async function main() {
  setGuestBusy(false);
  await refreshGuest();
})();
