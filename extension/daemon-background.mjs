import { connectDaemon, ToolError } from "./daemon-transport.mjs";
import { accountInventory, dispatchCore } from "./mail-core.mjs";

const CORE = new Set([
  "snapshotMessages", "restoreMessages",
  "listAccounts", "listFolders", "getAccountAccess", "searchMessages", "getRecentMessages",
  "getMessage", "getMessages", "getMessageHeaders", "batchGetMessageHeaders", "displayMessage",
  "updateMessage", "deleteMessages", "saveDraft", "sendMail", "replyToMessage", "forwardMessage",
]);
const MODULES = {
  "experiment.attachments": ["searchAttachments"],
  "experiment.folder-sync": ["refreshFolder"],
  "experiment.contacts": ["searchContacts", "getContact", "createContact", "updateContact", "deleteContact"],
  "experiment.calendar": ["listCalendars", "refreshCalendar", "createEvent", "listEvents", "updateEvent",
    "deleteEvent", "listCategories", "createTask", "listTasks", "updateTask"],
  "experiment.folders": ["createFolder", "renameFolder", "deleteFolder", "moveFolder", "createVirtualFolder",
    "listVirtualFolders", "deleteVirtualFolder", "emptyTrash", "emptyJunk"],
  "experiment.filters": ["listFilters", "createFilter", "updateFilter", "deleteFilter", "reorderFilters", "applyFilters"],
  "experiment.advanced-mail": ["dryRunCompose"],
  "experiment.account-setup": ["openAccountSetup"],
};

function moduleFor(op, args) {
  if (op === "searchMessages" && args.searchBody) return "experiment.gloda";
  if (["getMessage", "getMessages"].includes(op) && (args.saveAttachments || args.includeInlineImages)) {
    return "experiment.advanced-mail";
  }
  if (["sendMail", "saveDraft", "replyToMessage", "forwardMessage"].includes(op) &&
      (args.skipReview === true || args.inReplyTo || args.references || args.includeSignature === false || args.sendAt ||
       args.attachments?.some(item => typeof item === "string"))) {
    return "experiment.advanced-mail";
  }
  if (CORE.has(op)) return "core";
  return Object.keys(MODULES).find(module => MODULES[module].includes(op));
}

let settings;
const messageSession = crypto.randomUUID();
const WRITES = new Set(["restoreMessages", "updateMessage", "deleteMessages", "saveDraft", "sendMail",
  "replyToMessage", "forwardMessage"]);
let pendingWrite = Promise.resolve();
const draftDestination = experiment => typeof experiment?.readDraftFolder === "function" ?
  id => experiment.readDraftFolder(id) : undefined;
let status = "Connecting";
const experiment = () => browser.mcpServer;
const reconnect = connectDaemon({
  async readConnection() {
    const stored = await browser.storage.local.get(["daemonSettings", "daemonConnection"]);
    settings = stored.daemonSettings ?? {};
    if (stored.daemonConnection) return stored.daemonConnection;
    if (!experiment()) return null;
    const file = await experiment().readDaemonConnection();
    if (!file) throw new Error("Waiting for connection file.");
    return file;
  },
  async capabilities() {
    const requested = Array.isArray(settings.modules) ? settings.modules : [];
    let optional = [];
    if (requested.length && experiment()) {
      try {
        const result = await experiment().configureDaemon({
          allowedAccounts: Array.isArray(settings.allowedAccounts) ? settings.allowedAccounts : [],
          allowDirectSend: settings.allowDirectSend === true,
          allowEncrypted: settings.allowEncrypted === true,
          allowContactWrites: settings.allowContactWrites === true,
          modules: requested,
        });
        if (result.success) optional = result.modules;
      } catch {
        // Thunderbird can disable Experiments independently of the add-on.
      }
    }
    return {
      thunderbird: (await browser.runtime.getBrowserInfo()).version,
      extensionVersion: browser.runtime.getManifest().version,
      modules: ["core", ...optional],
      inventory: await accountInventory(browser, Array.isArray(settings.allowedAccounts) ? settings.allowedAccounts : [],
        draftDestination(experiment())),
    };
  },
  async dispatch(op, args, signal, modules) {
    signal.throwIfAborted();
    if (!Array.isArray(args._scope?.accounts) || !Array.isArray(args._scope?.headers)) {
      throw new ToolError("access_denied", "The daemon did not provide an account scope.");
    }
    if (args.skipReview === true && settings.allowDirectSend !== true) {
      throw new ToolError("access_denied", "Review is required. Omit skipReview.");
    }
    if (op === "saveDraft" && args.sendAt && settings.allowDirectSend !== true) {
      throw new ToolError("access_denied", "Scheduled delivery requires direct sending permission.");
    }
    if (["createContact", "updateContact", "deleteContact"].includes(op) && settings.allowContactWrites !== true) {
      throw new ToolError("access_denied", "Contact write access is disabled.");
    }
    const module = moduleFor(op, args);
    if (!module || !modules.includes(module)) throw new ToolError("unsupported_op", "Enable the matching optional module in tb-agent settings.");
    const composeOperation = ["sendMail", "saveDraft", "replyToMessage", "forwardMessage"].includes(op);
    if (module === "core" || composeOperation) {
      const dispatch = () => {
        signal.throwIfAborted();
        return dispatchCore(browser, op, args, signal, { ...settings, messageSession,
          readDraftFolder: draftDestination(experiment()) });
      };
      if (!WRITES.has(op)) return dispatch();
      const write = pendingWrite.then(dispatch);
      pendingWrite = write.catch(() => {});
      return write;
    }
    const independent = module === "experiment.calendar" || module === "experiment.contacts";
    if (!independent && (args._scope.headers.length ||
        !(settings.allowedAccounts ?? []).every(account => args._scope.accounts.includes(account)))) {
      throw new ToolError("access_denied", "This optional mail operation requires full access to all accounts exposed by the extension.");
    }
    const integration = experiment();
    const requestId = crypto.randomUUID();
    const cancel = () => {
      void integration.cancelDaemonTool(requestId).catch(() => {});
    };
    signal.addEventListener("abort", cancel, { once: true });
    try {
      const result = await integration.callDaemonTool(op, args, requestId);
      signal.throwIfAborted();
      if (typeof result?.error === "string") throw new ToolError("operation_failed", result.error);
      return result;
    } finally {
      signal.removeEventListener("abort", cancel);
    }
  },
  setStatus(value) {
    status = value;
  },
});

browser.storage.onChanged.addListener((changes, area) => {
  if (area === "local" && (changes.daemonSettings || changes.daemonConnection)) reconnect();
});
browser.runtime.onMessage.addListener(message => {
  if (message === "tb-agent-status") return Promise.resolve(status);
  return undefined;
});

for (const event of [browser.accounts.onCreated, browser.accounts.onDeleted, browser.accounts.onUpdated,
  browser.identities?.onCreated, browser.identities?.onDeleted, browser.identities?.onUpdated,
  browser.folders.onCreated, browser.folders.onDeleted, browser.folders.onRenamed, browser.folders.onMoved]) {
  event?.addListener(reconnect);
}
