import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";
import ts from "typescript";

// Exercise the actual history/launch handlers with KLP and presentation mocked;
// no WebView or native notification service is needed for polling regressions.
const source = ts.createSourceFile(
  "main.ts",
  readFileSync(new URL("../src/main.ts", import.meta.url), "utf8"),
  ts.ScriptTarget.Latest,
  true,
);
const handlers = ["loadNotifications", "ensureLaunchId"].map((name) => {
  const declaration = source.statements.find(
    (node) => ts.isFunctionDeclaration(node) && node.name?.text === name,
  );
  assert.ok(declaration, `${name} must exist`);
  return declaration.getText(source);
}).join("\n");
const code = ts.transpileModule(handlers, {
  compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.None },
}).outputText;

function client({ item, available = true, toasted = false, failFirst = false }) {
  const calls = [];
  const toastedIds = new Set(toasted ? ["emergency"] : []);
  const context = {
    console: { error() {} },
    notifications: new Map(), expandedIds: new Set(), seenIds: new Set(),
    notifSubscribed: false, launchFocusFetched: false, launchIdFetched: false,
    pendingNotificationId: null, toastedIds,
    automaticNotificationsReady: Promise.resolve(),
    automaticNotificationsAvailable: available,
    updateCacheBanner() {}, renderNotifications() {},
    focusNotificationInPanel() {},
    isExpired: (n) => n.expired ?? false,
    surfaceOsToast: async (n) => { toastedIds.add(n.id); calls.push("toast"); },
    invoke: async (command) => {
      calls.push(command);
      if (command === "notifications_list") {
        if (failFirst) { failFirst = false; throw new Error("disconnected"); }
        return { items: item ? [item] : [] };
      }
      if (command === "get_launch_notification") return "emergency";
      return null;
    },
  };
  vm.createContext(context);
  vm.runInContext(code, context);
  return { context, calls, poll: () => context.loadNotifications() };
}

const emergency = { id: "emergency", toast: true, acked_at: null };
for (const [name, options, toasts, reveals] of [
  ["hidden launch stays hidden on later polls", { item: emergency }, 1, 0],
  ["acked launch reveals only once", { item: { ...emergency, acked_at: "now" } }, 0, 1],
  ["expired launch reveals only once", { item: { ...emergency, expired: true } }, 0, 1],
  ["already toasted launch reveals only once", { item: emergency, toasted: true }, 0, 1],
  ["missing launch reveals only once", {}, 0, 1],
  ["unavailable native notifications keep history usable", { item: emergency, available: false }, 0, 0],
  ["failed history fetch preserves launch for reconnect", { item: emergency, failFirst: true }, 1, 0],
]) {
  test(name, async () => {
    const app = client(options);
    for (let poll = 0; poll < 3; poll++) await app.poll();
    assert.equal(app.calls.filter((call) => call === "toast").length, toasts);
    assert.equal(app.calls.filter((call) => call === "show_main_window").length, reveals);
    assert.equal(app.calls.filter((call) => call === "get_launch_notification").length, 1);
    assert.equal(app.context.pendingNotificationId, null);
    if (options.item) assert.ok(app.context.notifications.has("emergency"));
  });
}
