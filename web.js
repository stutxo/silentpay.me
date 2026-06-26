import init, { SilentpayApp, recoveryWifFromSnapshot } from "./pkg/silentpay.js";

(() => {
  const MIN_FEE_RATE = 0.1;
  const CUSTOM_DEFAULT = "0.1";
  const FEE_API = "https://mempool.space/api/v1/fees/precise";
  const TARGETS = [
    ["fastest", "Next block", "fastestFee"],
    ["half_hour", "30 minutes", "halfHourFee"],
    ["hour", "1 hour", "hourFee"],
  ];
  const state = { initialized: false, rates: null };
  const el = (id) => document.getElementById(id);
  const validRate = (value) => {
    const rate = Number(value);
    return Number.isFinite(rate) && rate > 0 ? Math.max(rate, MIN_FEE_RATE) : null;
  };
  const formatRate = (value) => {
    const rate = validRate(value);
    if (rate === null) return "";
    return `${rate.toFixed(2).replace(/\.?0+$/, "")} sat/vB`;
  };
  function sync() {
    const feeTarget = el("fee-target");
    const customFeeRate = el("custom-fee-rate");
    if (!feeTarget || !customFeeRate) return;
    const custom = feeTarget.value === "custom";
    customFeeRate.hidden = !custom;
    if (custom && !customFeeRate.value) customFeeRate.value = CUSTOM_DEFAULT;
  }
  function updateOptionLabels() {
    const feeTarget = el("fee-target");
    if (!feeTarget) return;

    for (const [value, label, key] of TARGETS) {
      const option = feeTarget.querySelector(`option[value="${value}"]`);
      const formatted = formatRate(state.rates && state.rates[key]);
      if (option) option.textContent = formatted ? `${label} (${formatted})` : label;
    }
    const customOption = feeTarget.querySelector('option[value="custom"]');
    if (customOption) customOption.textContent = "Custom sat/vB";
  }
  async function refreshFeeQuotes() {
    updateOptionLabels();
    try {
      const response = await fetch(FEE_API, { cache: "no-store" });
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      state.rates = await response.json();
    } catch (err) {
      state.rates = {};
    }
    updateOptionLabels();
  }
  function initFeeControls() {
    if (state.initialized) return;
    state.initialized = true;
    const feeTarget = el("fee-target");
    if (feeTarget) {
      feeTarget.addEventListener("input", sync);
      feeTarget.addEventListener("change", sync);
    }
    sync();
    refreshFeeQuotes();
    setInterval(refreshFeeQuotes, 60000);
  }
  window.silentpayFeeControls = { init: initFeeControls, refreshFeeQuotes, sync };
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", initFeeControls);
  else initFeeControls();
})();

const STORAGE_KEY = "silentpay-app-snapshot-v1";
const FAILED_SNAPSHOT_KEY = `${STORAGE_KEY}-restore-failed`;
const BACKUP_ACK_PREFIX = `${STORAGE_KEY}-wif-backup-`;
const EXPLORER = "https://mempool.space";
const PHASES = ["waiting_for_deposit", "building_sweep", "broadcasting_sweep", "sweep_broadcast", "confirmed"];
const STEP_LABELS = ["Deposit", "Build", "Broadcast", "Confirm", "Done"];
const LINK_RECIPIENT_RE = /^sp1[023456789acdefghjklmnpqrstuvwxyz]+$/i;
const el = (id) => document.getElementById(id);
const ui = {
  recipient: el("recipient"),
  copyLink: el("copy-link"),
  feeTarget: el("fee-target"),
  customFeeRate: el("custom-fee-rate"),
  start: el("start"),
  reset: el("reset"),
  steps: el("steps"),
  status: el("status"),
  details: el("details"),
  backupReminder: el("backup-reminder"),
  backupWif: el("backup-wif"),
  revealBackupWif: el("reveal-backup-wif"),
  downloadBackupWif: el("download-backup-wif"),
  copyBackupWif: el("copy-backup-wif"),
  ackBackupWif: el("ack-backup-wif"),
  qr: el("qr"),
  commitAddress: el("commit-address"),
  copyAddress: el("copy-address"),
  retry: el("retry"),
  raw: el("raw"),
  rawtx: el("rawtx"),
  copyRaw: el("copy-raw"),
  recovery: el("recovery"),
  revealRecovery: el("reveal-recovery"),
  recoveryWif: el("recovery-wif"),
  copyRecovery: el("copy-recovery"),
  snapshotRecovery: el("snapshot-recovery"),
  snapshotWifBlock: el("snapshot-wif-block"),
  snapshotWif: el("snapshot-wif"),
  copySnapshotWif: el("copy-snapshot-wif"),
  snapshotRaw: el("snapshot-raw"),
  copySnapshot: el("copy-snapshot"),
};
const runtime = {
  app: null,
  pollTimer: null,
  restoreError: null,
  restoreSnapshot: null,
  restoreWif: null,
  backupAddress: "",
  backupWif: "",
};

function cleanLinkedRecipient(value) {
  const recipient = String(value || "").trim();
  return LINK_RECIPIENT_RE.test(recipient) ? recipient : "";
}
function pathRecipient() {
  let path = window.location.pathname.replace(/^\/+|\/+$/g, "");
  if (!path || path.includes("/")) return "";
  try { path = decodeURIComponent(path); }
  catch { return ""; }
  return cleanLinkedRecipient(path);
}
function queryRecipient() {
  const params = new URLSearchParams(window.location.search);
  return cleanLinkedRecipient(params.get("recipient") || params.get("to"));
}
function linkedRecipient() {
  const fromPath = pathRecipient();
  if (fromPath) return { address: fromPath, canonicalize: false };
  const fromQuery = queryRecipient();
  if (fromQuery) return { address: fromQuery, canonicalize: true };
  return null;
}
function canonicalizeRecipientUrl(address) {
  try {
    history.replaceState(null, "", `/${encodeURIComponent(address)}`);
  } catch {}
}
function recipientShareUrl(address) {
  const recipient = cleanLinkedRecipient(address);
  if (!recipient) return "";
  const url = new URL("/", window.location.origin);
  url.pathname = `/${encodeURIComponent(recipient)}`;
  return url.toString();
}
function syncCopyLinkButton() {
  ui.copyLink.disabled = !recipientShareUrl(ui.recipient.value);
}
function applyLinkedRecipient(view) {
  const link = linkedRecipient();
  if (!link) return "";
  if (view.hasActivePayment && view.phase !== "confirmed") {
    return view.destinationAddress === link.address
      ? ""
      : "This browser already has an active staging payment, so the linked recipient was not applied.";
  }
  if (link.canonicalize) canonicalizeRecipientUrl(link.address);
  ui.recipient.value = link.address;
  return "Recipient loaded from link. Create a staging address when ready.";
}

function saveSnapshot(view = runtime.app.currentPayment()) {
  if (!view.hasActivePayment || view.phase === "confirmed") {
    clearSnapshot();
    return;
  }
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(runtime.app.exportSnapshot()));
    runtime.restoreError = null;
    runtime.restoreSnapshot = null;
    runtime.restoreWif = null;
  } catch {}
}
function clearSnapshot() {
  try { localStorage.removeItem(STORAGE_KEY); } catch {}
  try { localStorage.removeItem(FAILED_SNAPSHOT_KEY); } catch {}
  runtime.restoreSnapshot = null;
  runtime.restoreWif = null;
}
function preserveFailedSnapshot(raw, recoveryWif = "") {
  runtime.restoreSnapshot = raw;
  runtime.restoreWif = recoveryWif;
  try { localStorage.setItem(FAILED_SNAPSHOT_KEY, raw); } catch {}
}
function recoverSnapshotWif(snapshot) {
  try { return recoveryWifFromSnapshot(snapshot) || ""; }
  catch { return ""; }
}
function loadApp() {
  const raw = localStorage.getItem(STORAGE_KEY);
  if (!raw) return new SilentpayApp();
  let snapshot;
  try { snapshot = JSON.parse(raw); }
  catch (err) {
    preserveFailedSnapshot(raw);
    runtime.restoreError = `Saved payment JSON could not be read, so the raw recovery snapshot was preserved below: ${err.message || err}`;
    return new SilentpayApp();
  }
  const recoveryWif = recoverSnapshotWif(snapshot);
  try { return SilentpayApp.fromSnapshot(snapshot); }
  catch (err) {
    preserveFailedSnapshot(raw, recoveryWif);
    runtime.restoreError = recoveryWif
      ? `Saved payment could not be restored, but the staging WIF was recovered below: ${err.message || err}`
      : `Saved payment could not be restored, so the raw recovery snapshot was preserved below: ${err.message || err}`;
    return new SilentpayApp();
  }
}
function explorerTx(txid) {
  const link = document.createElement("a");
  link.href = `${EXPLORER}/tx/${encodeURIComponent(txid)}`;
  link.target = "_blank";
  link.rel = "noreferrer";
  link.textContent = txid;
  return link;
}
function sats(value) {
  return value === null || value === undefined ? "" : `${Number(value).toLocaleString()} sats`;
}
function syncFeeControls() {
  window.silentpayFeeControls?.sync();
}
function backupAckKey(address) {
  return `${BACKUP_ACK_PREFIX}${address}`;
}
function backupAcknowledged(view) {
  if (!view.commitAddress) return false;
  try { return localStorage.getItem(backupAckKey(view.commitAddress)) === "1"; }
  catch { return false; }
}
function shouldShowBackupReminder(view) {
  return !!view.hasActivePayment
    && view.phase !== "confirmed"
    && !!view.commitAddress
    && !backupAcknowledged(view);
}
function resetBackupWif() {
  runtime.backupWif = "";
  ui.backupWif.value = "";
  ui.backupWif.hidden = true;
  ui.revealBackupWif.hidden = false;
  ui.downloadBackupWif.hidden = true;
  ui.copyBackupWif.hidden = true;
  ui.ackBackupWif.disabled = true;
}
function ensureBackupWif() {
  if (!runtime.backupWif) runtime.backupWif = runtime.app.recoveryWif();
  ui.backupWif.value = runtime.backupWif;
  ui.backupWif.hidden = false;
  ui.revealBackupWif.hidden = true;
  ui.downloadBackupWif.hidden = false;
  ui.copyBackupWif.hidden = false;
  ui.ackBackupWif.disabled = false;
  return runtime.backupWif;
}
function buildBackupText(view, wif) {
  return [
    "silentpay.me staging WIF backup",
    "",
    `Created: ${new Date().toISOString()}`,
    `Staging address: ${view.commitAddress || ""}`,
    `Silent payment recipient: ${view.destinationAddress || ""}`,
    "",
    `Recovery WIF: ${wif}`,
    "",
    "Sparrow sweep recovery:",
    "1. Open the wallet you want to recover into.",
    "2. Choose Tools > Sweep Private Key.",
    "3. Paste the WIF and set the script type to Native Segwit/P2WPKH.",
    "4. Sweep to your wallet or the intended sp1... silent payment address, then review, sign, and broadcast.",
    "",
    "Anyone with this WIF can spend funds that remain at the staging address.",
  ].join("\n");
}
function backupFilename(view) {
  const suffix = String(view.commitAddress || "staging").replace(/[^a-zA-Z0-9]/g, "").slice(0, 16);
  return `silentpay-wif-backup-${suffix || "staging"}.txt`;
}
function renderBackupReminder(view) {
  const show = shouldShowBackupReminder(view);
  ui.backupReminder.hidden = !show;
  if (!show) {
    runtime.backupAddress = "";
    resetBackupWif();
    return;
  }
  if (runtime.backupAddress !== view.commitAddress) {
    runtime.backupAddress = view.commitAddress || "";
    resetBackupWif();
  }
}
function renderFeeControls(view) {
  ui.feeTarget.value = view.sweepFeeTarget || "fastest";
  if (!ui.feeTarget.value) ui.feeTarget.value = "fastest";
  ui.customFeeRate.value = view.customFeeRateSatVb === null || view.customFeeRateSatVb === undefined
    ? ""
    : String(view.customFeeRateSatVb);
  syncFeeControls();
}
function selectedFeePreference() {
  const target = ui.feeTarget.value || "fastest";
  if (target !== "custom") return { target, customFeeRateSatVb: null };

  if (!ui.customFeeRate.value) ui.customFeeRate.value = "0.1";
  const customFeeRateSatVb = Number(ui.customFeeRate.value);
  if (!Number.isFinite(customFeeRateSatVb) || customFeeRateSatVb <= 0) {
    throw new Error("Enter a positive custom sweep fee rate.");
  }
  return { target, customFeeRateSatVb };
}
function setBusy(active) {
  const view = runtime.app?.currentPayment();
  const activeLocked = view?.hasActivePayment && view.phase !== "confirmed";
  ui.start.disabled = active;
  ui.feeTarget.disabled = active || activeLocked;
  ui.customFeeRate.disabled = active || activeLocked;
  ui.retry.disabled = active || !view?.canRetrySweep;
}
function renderSteps(view) {
  const phase = view.phase;
  const failed = phase === "failed";
  const current = Math.max(0, PHASES.indexOf(phase));
  const nodes = STEP_LABELS.map((label, index) => {
    let cls = "";
    if (failed && index === Math.max(0, current)) cls = "failed";
    else if (phase === "confirmed" || index < current) cls = "done";
    else if (index === current && view.hasActivePayment) cls = "current";
    const step = document.createElement("div");
    step.className = `step ${cls}`.trim();
    step.textContent = label;
    return step;
  });
  ui.steps.replaceChildren(...nodes);
}
function clearRecoveryWif() {
  ui.recoveryWif.value = "";
  ui.recoveryWif.hidden = true;
  ui.copyRecovery.hidden = true;
  ui.revealRecovery.hidden = false;
}
function renderDetails(view) {
  if (!view.hasActivePayment) {
    ui.details.textContent = "No active payment.";
    ui.raw.hidden = true;
    ui.rawtx.value = "";
    ui.recovery.hidden = true;
    clearRecoveryWif();
    return;
  }
  const rows = [
    ["Recipient", view.destinationAddress],
    ["Deposited", sats(view.depositedSat)],
    ["Fee target", view.sweepFeeLabel],
    ["Sweep fee", sats(view.sweepFeeSat)],
    ["Swept amount", sats(view.sweepAmountSat)],
    ["Fee rate", view.feeRateSatVb ? `${view.feeRateSatVb.toFixed(2)} sat/vB` : ""],
    ["Sweep txid", view.sweepTxid ? explorerTx(view.sweepTxid) : null],
  ];
  const dl = document.createElement("dl");
  for (const [label, value] of rows) {
    const dt = document.createElement("dt");
    dt.textContent = label;
    const dd = document.createElement("dd");
    if (value instanceof Node) dd.append(value);
    else dd.textContent = value || "...";
    dl.append(dt, dd);
  }
  ui.details.replaceChildren(dl);
  ui.raw.hidden = !view.sweepTxHex;
  ui.rawtx.value = view.sweepTxHex || "";
  ui.recovery.hidden = view.phase === "confirmed";
  if (ui.recovery.hidden) clearRecoveryWif();
}
function renderSnapshotRecovery() {
  const hasSnapshot = !!runtime.restoreSnapshot;
  const hasWif = !!runtime.restoreWif;
  ui.snapshotRecovery.hidden = !hasSnapshot && !hasWif;
  ui.snapshotWifBlock.hidden = !hasWif;
  ui.snapshotWif.value = runtime.restoreWif || "";
  ui.snapshotRaw.value = runtime.restoreSnapshot || "";
}
function renderQr(view) {
  ui.commitAddress.textContent = view.commitAddress || "";
  ui.copyAddress.disabled = !view.commitAddress;
  if (!view.hasActivePayment || !view.commitAddress) {
    ui.qr.textContent = "Create a payment to show a QR code.";
    return;
  }
  try {
    const img = document.createElement("img");
    img.alt = "Bitcoin staging address QR code";
    img.src = `data:image/svg+xml;charset=utf-8,${encodeURIComponent(runtime.app.paymentQrSvg())}`;
    ui.qr.replaceChildren(img);
  } catch {
    ui.qr.textContent = view.bip21Uri || view.commitAddress;
  }
}
function render(view = runtime.app.currentPayment()) {
  ui.status.textContent = runtime.restoreError || view.message || "";
  const activeLocked = view.hasActivePayment && view.phase !== "confirmed";
  ui.recipient.disabled = activeLocked;
  syncCopyLinkButton();
  ui.feeTarget.disabled = activeLocked;
  ui.customFeeRate.disabled = activeLocked;
  ui.start.hidden = activeLocked;
  ui.reset.hidden = !view.hasActivePayment;
  ui.retry.disabled = !view.canRetrySweep;
  renderFeeControls(view);
  renderSteps(view);
  renderBackupReminder(view);
  renderDetails(view);
  renderSnapshotRecovery();
  renderQr(view);
}
async function startPayment() {
  const silentPaymentAddress = ui.recipient.value.trim();
  if (!silentPaymentAddress) {
    ui.status.textContent = "Paste a silent payment address first.";
    return;
  }
  setBusy(true);
  try {
    const view = runtime.app.setRecipient({ silentPaymentAddress, sweepFee: selectedFeePreference() });
    saveSnapshot(view);
    render(view);
    await pollOnce();
  } catch (err) {
    ui.status.textContent = `Error: ${err.message || err}`;
  } finally {
    setBusy(false);
  }
}
async function pollOnce() {
  const before = runtime.app.currentPayment();
  if (!before.hasActivePayment || before.isTerminal) return;
  try {
    const view = await runtime.app.pollOnce();
    saveSnapshot(view);
    render(view);
  } catch (err) {
    ui.status.textContent = `Polling error: ${err.message || err}`;
  }
}
async function retry() {
  setBusy(true);
  try {
    const view = await runtime.app.retryBroadcast();
    saveSnapshot(view);
    render(view);
  } catch (err) {
    ui.status.textContent = `Retry error: ${err.message || err}`;
  } finally {
    setBusy(false);
  }
}
function resetPayment() {
  const view = runtime.app.currentPayment();
  if (view.hasActivePayment && view.phase !== "confirmed" && !confirm("Resetting can strand funds sent to the current staging address. Continue?")) return;
  runtime.app.reset();
  clearSnapshot();
  ui.recipient.value = "";
  clearRecoveryWif();
  render();
}
function revealRecoveryWif() {
  try {
    ui.recoveryWif.value = runtime.app.recoveryWif();
    ui.recoveryWif.hidden = false;
    ui.copyRecovery.hidden = false;
    ui.revealRecovery.hidden = true;
  } catch (err) {
    ui.status.textContent = `Recovery error: ${err.message || err}`;
  }
}
function revealBackupWif() {
  try { ensureBackupWif(); }
  catch (err) { ui.status.textContent = `Backup error: ${err.message || err}`; }
}
function downloadBackupWif() {
  let wif;
  try { wif = ensureBackupWif(); }
  catch (err) {
    ui.status.textContent = `Backup error: ${err.message || err}`;
    return;
  }
  const view = runtime.app.currentPayment();
  const blob = new Blob([buildBackupText(view, wif)], { type: "text/plain" });
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = backupFilename(view);
  document.body.append(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 0);
  ui.status.textContent = "WIF backup downloaded. Keep it private and stored locally.";
}
function acknowledgeBackupWif() {
  const view = runtime.app.currentPayment();
  if (!view.commitAddress) return;
  try { localStorage.setItem(backupAckKey(view.commitAddress), "1"); } catch {}
  runtime.backupAddress = "";
  resetBackupWif();
  render(view);
  ui.status.textContent = "Staging WIF backup marked as saved locally.";
}
async function copy(text, button, label) {
  if (!text) return;
  try {
    await navigator.clipboard.writeText(text);
    button.textContent = "Copied";
    setTimeout(() => { button.textContent = label; }, 1200);
  } catch {}
}
async function copyShareUrl() {
  const url = recipientShareUrl(ui.recipient.value);
  if (!url) {
    ui.status.textContent = "Enter a valid sp1 silent payment address first.";
    return;
  }
  await copy(url, ui.copyLink, "Copy share URL");
}
async function copyBackupWif() {
  try { await copy(ensureBackupWif(), ui.copyBackupWif, "Copy WIF"); }
  catch (err) { ui.status.textContent = `Backup error: ${err.message || err}`; }
}
async function initApp() {
  await init();
  runtime.app = loadApp();
  const view = runtime.app.currentPayment();
  if (view.destinationAddress) ui.recipient.value = view.destinationAddress;
  const linkedRecipientMessage = applyLinkedRecipient(view);
  render(view);
  if (linkedRecipientMessage && !runtime.restoreError) ui.status.textContent = linkedRecipientMessage;
  ui.recipient.addEventListener("input", syncCopyLinkButton);
  ui.copyLink.addEventListener("click", copyShareUrl);
  window.silentpayFeeControls?.init();
  ui.start.addEventListener("click", startPayment);
  ui.reset.addEventListener("click", resetPayment);
  ui.retry.addEventListener("click", retry);
  ui.revealRecovery.addEventListener("click", revealRecoveryWif);
  ui.revealBackupWif.addEventListener("click", revealBackupWif);
  ui.downloadBackupWif.addEventListener("click", downloadBackupWif);
  ui.copyBackupWif.addEventListener("click", copyBackupWif);
  ui.ackBackupWif.addEventListener("click", acknowledgeBackupWif);
  ui.copyAddress.addEventListener("click", () => copy(runtime.app.currentPayment().commitAddress, ui.copyAddress, "Copy address"));
  ui.copyRaw.addEventListener("click", () => copy(ui.rawtx.value, ui.copyRaw, "Copy raw tx"));
  ui.copyRecovery.addEventListener("click", () => copy(ui.recoveryWif.value, ui.copyRecovery, "Copy recovery WIF"));
  ui.copySnapshotWif.addEventListener("click", () => copy(ui.snapshotWif.value, ui.copySnapshotWif, "Copy recovered WIF"));
  ui.copySnapshot.addEventListener("click", () => copy(ui.snapshotRaw.value, ui.copySnapshot, "Copy snapshot"));
  runtime.pollTimer = setInterval(pollOnce, Math.max(view.pollIntervalMs || 10000, 5000));
  await pollOnce();
}
initApp();
