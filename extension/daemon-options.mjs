import { connectionInfo } from "./daemon-transport.mjs";

const element = id => document.getElementById(id);
const feedback = element("feedback");
const read = async () => (await browser.storage.local.get("daemonSettings")).daemonSettings ?? {};
const savePermissions = daemonSettings => browser.storage.local.set({ daemonSettings });
const permissionIds = ["allowDirectSend", "allowEncrypted", "allowContactWrites"];

async function refreshStatus() {
  try {
    const status = await browser.runtime.sendMessage("tb-agent-status");
    if (element("connection-status").textContent !== status) element("connection-status").textContent = status;
  } catch {
    element("connection-status").textContent = "Extension background is unavailable.";
  }
}

async function initialize() {
  const settings = await read();
  for (const account of await browser.accounts.list(false)) {
    const label = document.createElement("label");
    const checkbox = document.createElement("input");
    checkbox.type = "checkbox";
    checkbox.name = "account";
    checkbox.value = account.id;
    checkbox.checked = Array.isArray(settings.allowedAccounts) && settings.allowedAccounts.includes(account.id);
    label.append(checkbox, " ", account.name);
    element("accounts").append(label);
  }
  for (const id of permissionIds) element(id).checked = settings[id] === true;
  element("experiments").hidden = !browser.mcpServer;
  element("allowDirectSend").closest("label").hidden = !browser.mcpServer;
  element("allowDirectSend").disabled = !browser.mcpServer;
  for (const box of document.querySelectorAll('[name="module"]')) {
    box.checked = Array.isArray(settings.modules) && settings.modules.includes(box.value);
  }
  await refreshStatus();
  setInterval(refreshStatus, 1000);
}

element("connection").addEventListener("change", async event => {
  const [file] = event.target.files;
  if (!file) return;
  try {
    if (file.size > 4096) throw new Error("Invalid file size.");
    await browser.storage.local.set({ daemonConnection: connectionInfo(JSON.parse(await file.text())) });
    feedback.textContent = "Connection file imported.";
  } catch {
    feedback.textContent = "Select a valid tbd connection.extension.json file.";
  } finally {
    event.target.value = "";
  }
});

element("clear").addEventListener("click", async () => {
  try {
    await browser.storage.local.remove("daemonConnection");
    feedback.textContent = "Imported connection cleared.";
  } catch {
    feedback.textContent = "Could not clear the connection.";
  }
});

element("access").addEventListener("submit", async event => {
  event.preventDefault();
  const form = new FormData(event.target);
  try {
    await savePermissions({
      allowedAccounts: form.getAll("account"),
      modules: browser.mcpServer ? form.getAll("module") : [],
      ...Object.fromEntries(permissionIds.map(id => [id, !element(id).disabled && element(id).checked])),
    });
    feedback.textContent = "Permissions saved.";
  } catch {
    feedback.textContent = "Could not save permissions.";
  }
});

void initialize().catch(() => { feedback.textContent = "Could not load tb-agent settings."; });
