import { createHash } from "node:crypto";
import fs from "node:fs/promises";
import { createRequire } from "node:module";
import os from "node:os";
import path from "node:path";

const TOOL_OBSERVE = "browser_observe";
const TOOL_STEP = "browser_step";
const CAPTURE_VIEWPORT = "viewport";
const CAPTURE_FULL_PAGE = "full_page";
const DEFAULT_PROFILE_LOCK_TIMEOUT_MS = 120_000;
const PROFILE_LOCK_STALE_MS = 10 * 60_000;
const PROFILE_LOCK_POLL_MS = 250;

main().catch((error) => {
  writeResponse({
    contentItems: [
      {
        type: "inputText",
        text: `Browser Playwright provider failed: ${error?.stack || error}`,
      },
    ],
    success: false,
    error: String(error?.message || error),
  });
  process.exitCode = 0;
});

async function main() {
  if (process.platform === "win32") {
    throw new Error("Native Browser profile state is unsupported on Windows; no state was changed.");
  }
  const request = JSON.parse(await readStdin());
  validateVisualArguments(request.arguments || {});
  captureMode();
  const { chromium } = loadPlaywright();
  const { stateDir } = await browserProfile(request);
  await withProfileLock(stateDir, async () => {
    const headless = playwrightHeadless();
    const viewport = viewportFromRequest(request);
    const context = await chromium.launchPersistentContext(stateDir, {
      ...launchOptions({ headless, viewport }),
      headless,
      viewport,
    });

    let page;
    let inspection;
    try {
      page = await activePage(context);
      inspection = installInspection(page, request.arguments?.inspection);
      await restoreOrNavigate(page, request, stateDir);

      const summaries = [];
      if (request.tool === TOOL_STEP) {
        const actions = canonicalActions(request.arguments);
        if (actions.length === 0) {
          throw new Error("browser_step requires an action or non-empty actions array.");
        }
        for (const action of actions) {
          summaries.push(await runAction(page, action));
        }
      } else if (request.tool !== TOOL_OBSERVE) {
        throw new Error(`Unsupported browser tool ${request.tool}`);
      }

      await page.waitForLoadState("domcontentloaded", { timeout: 10_000 }).catch(() => {});
      const captureBundle = await captureScreenshots(page, request.arguments || {});
      await saveState(stateDir, page);
      let artifactResult = null;
      let artifactError = null;
      if (request.arguments?.save_artifact === true) {
        try {
          artifactResult = await saveCaptureArtifacts(stateDir, captureBundle, request);
          if (!artifactResult.success) {
            artifactError = `artifact_save: ${artifactResult.error}`;
          }
        } catch (error) {
          artifactError = `artifact_save: ${errorMessage(error)}`;
        }
      }
      const inspectionResult = await snapshotInspection(page, inspection);
      writeResponse(await responseForPage(page, captureBundle, summaries, {
        artifactResult,
        error: captureBundle.error || artifactError,
        pageHints: request.arguments?.scope === "viewport_and_page" || request.arguments?.interaction_map?.scope === "page",
        pageHintOffset: request.arguments?.interaction_map?.offset || 0,
        inspection: inspectionResult,
      }));
    } finally {
      inspection?.cleanup();
      await context.close().catch(() => {});
    }
  });
}

function loadPlaywright() {
  const require = createRequire(import.meta.url);
  return require("playwright");
}

async function readStdin() {
  const chunks = [];
  for await (const chunk of process.stdin) {
    chunks.push(chunk);
  }
  return Buffer.concat(chunks).toString("utf8");
}

async function browserProfile(request) {
  const baseDir = await browserStateRoot(
    process.env.CODEX_BROWSER_PLAYWRIGHT_STATE_DIR,
  );
  const threadId = request.threadId;
  if (typeof threadId !== "string" || !threadId.trim()) {
    throw new Error("Browser thread isolation requires a non-empty threadId.");
  }

  const profilesDir = path.join(baseDir, "profiles");
  const profileDir = path.join(profilesDir, safePathComponent(threadId));
  await ensurePrivateStateDirectory(baseDir);
  await ensurePrivateStateDirectory(profilesDir);
  await ensurePrivateStateDirectory(profileDir);
  return { stateDir: profileDir };
}

async function browserStateRoot(configured) {
  if (typeof configured === "string" && configured.trim()) {
    const root = path.isAbsolute(configured)
      ? path.resolve(configured)
      : path.resolve(process.cwd(), configured);
    await validateStatePath(root, { allowMissing: true });
    return root;
  }

  const codexHome = await activeCodexHome();
  const root = path.join(codexHome, "browser-computer-use-playwright");
  await validateStatePath(root, { allowMissing: true });
  return root;
}

async function activeCodexHome() {
  const configured = process.env.CODEX_HOME;
  if (typeof configured === "string" && configured.length > 0) {
    const candidate = path.resolve(process.cwd(), configured);
    const stat = await fs.stat(candidate).catch((error) => {
      throw new Error(`Cannot resolve active CODEX_HOME: ${error.message}`);
    });
    if (!stat.isDirectory()) {
      throw new Error("Active CODEX_HOME must be a directory.");
    }
    // Codex canonicalizes an explicitly configured CODEX_HOME before using it.
    return fs.realpath(candidate);
  }

  return path.join(await fs.realpath(os.homedir()), ".codex");
}

async function ensurePrivateStateDirectory(directory) {
  await validateStatePath(directory, { allowMissing: true });
  await fs.mkdir(directory, { recursive: true, mode: 0o700 });
  await validateStatePath(directory);
}

async function validateStatePath(directory, { allowMissing = false } = {}) {
  if (!path.isAbsolute(directory) || path.resolve(directory) !== directory) {
    throw new Error(
      `Browser state path must be absolute and canonical: ${directory}`,
    );
  }

  const { root } = path.parse(directory);
  const components = directory.slice(root.length).split(path.sep).filter(Boolean);
  let current = root;
  let missing = false;
  for (const component of components) {
    current = path.join(current, component);
    let stat;
    try {
      stat = await fs.lstat(current);
    } catch (error) {
      if (error?.code === "ENOENT" && allowMissing) {
        missing = true;
        continue;
      }
      throw error;
    }
    if (missing) {
      throw new Error(
        `Browser state path reappeared below a missing ancestor: ${current}`,
      );
    }
    if (stat.isSymbolicLink()) {
      throw new Error(`Browser state path component must not be a symlink: ${current}`);
    }
    if (!stat.isDirectory()) {
      throw new Error(`Browser state path component must be a directory: ${current}`);
    }
    validatePathOwnershipAndMode(current, stat, current === directory);
    if ((await fs.realpath(current)) !== current) {
      throw new Error(`Browser state path component must be canonical: ${current}`);
    }
  }
}

function validatePathOwnershipAndMode(component, stat, isStateDirectory) {
  if (typeof process.getuid !== "function") {
    return;
  }
  const uid = process.getuid();
  const mode = stat.mode & 0o7777;
  if (stat.uid !== uid && stat.uid !== 0) {
    throw new Error(
      `Browser state path ancestor has an untrusted owner: ${component}`,
    );
  }
  const writableByGroupOrOther = mode & 0o022;
  const trustedStickyRootDirectory = stat.uid === 0 && Boolean(mode & 0o1000);
  if (
    writableByGroupOrOther &&
    (!trustedStickyRootDirectory || isStateDirectory)
  ) {
    throw new Error(
      `Browser state path ancestor is writable by untrusted users: ${component}`,
    );
  }
  if (isStateDirectory && stat.uid !== uid) {
    throw new Error(`Browser state directory must be owned by the current user: ${component}`);
  }
}

function safePathComponent(value) {
  const text = String(value || "default");
  const slug =
    text
      .replace(/[^a-zA-Z0-9._-]+/g, "_")
      .replace(/^_+|_+$/g, "")
      .slice(0, 64) || "default";
  const hash = createHash("sha256").update(text).digest("hex").slice(0, 12);
  return `${slug}-${hash}`;
}

async function withProfileLock(stateDir, body) {
  const lockDir = path.join(stateDir, ".codex-provider.lock");
  const deadline = Date.now() + lockTimeoutMs();
  while (true) {
    try {
      await fs.mkdir(lockDir);
      await fs.writeFile(
        path.join(lockDir, "owner.json"),
        JSON.stringify({
          pid: process.pid,
          startedAt: new Date().toISOString(),
        }),
      );
      try {
        return await body();
      } finally {
        await fs.rm(lockDir, { recursive: true, force: true }).catch(() => {});
      }
    } catch (error) {
      if (error?.code !== "EEXIST") {
        throw error;
      }
      if (await removeStaleProfileLock(lockDir)) {
        continue;
      }
      if (Date.now() >= deadline) {
        throw new Error(
          `Timed out waiting for browser profile lock at ${lockDir}. Another native browser call may still be using the headed Chrome profile.`,
        );
      }
      await sleep(PROFILE_LOCK_POLL_MS);
    }
  }
}

async function removeStaleProfileLock(lockDir) {
  try {
    const stat = await fs.stat(lockDir);
    if (Date.now() - stat.mtimeMs < PROFILE_LOCK_STALE_MS) {
      return false;
    }
    await fs.rm(lockDir, { recursive: true, force: true });
    return true;
  } catch (error) {
    return error?.code === "ENOENT";
  }
}

function lockTimeoutMs() {
  return envNumber(
    "CODEX_BROWSER_PLAYWRIGHT_LOCK_TIMEOUT_MS",
    DEFAULT_PROFILE_LOCK_TIMEOUT_MS,
  );
}

function playwrightHeadless() {
  const raw = (process.env.CODEX_BROWSER_PLAYWRIGHT_HEADLESS || "1").toLowerCase();
  return !["0", "false", "no", "off"].includes(raw);
}

function launchOptions({ headless, viewport }) {
  const options = {};
  const executablePath = trimmedEnv("CODEX_BROWSER_PLAYWRIGHT_EXECUTABLE_PATH");
  if (executablePath) {
    options.executablePath = executablePath;
  }
  const channel = trimmedEnv("CODEX_BROWSER_PLAYWRIGHT_CHANNEL");
  if (channel && !executablePath) {
    options.channel = channel;
  }
  if (!headless && viewport) {
    options.args = [
      "--window-position=0,0",
      `--window-size=${viewport.width},${viewport.height}`,
    ];
  }
  return options;
}

function viewportFromRequest(request) {
  const view = request.arguments?.view || {};
  return {
    width: numberOrDefault(
      view.viewportWidth,
      envNumber("CODEX_BROWSER_PLAYWRIGHT_VIEWPORT_WIDTH", 1280),
    ),
    height: numberOrDefault(
      view.viewportHeight,
      envNumber("CODEX_BROWSER_PLAYWRIGHT_VIEWPORT_HEIGHT", 720),
    ),
  };
}

function validateVisualArguments(args) {
  if (args.scope !== undefined && !["viewport", "viewport_and_page"].includes(args.scope)) {
    throw new Error("unsupported visual scope; expected viewport or viewport_and_page");
  }
  if (args.interaction_map !== undefined && (!args.interaction_map || typeof args.interaction_map !== "object" || Array.isArray(args.interaction_map) || (args.interaction_map.scope !== undefined && args.interaction_map.scope !== "page") || (args.interaction_map.offset !== undefined && (!Number.isInteger(args.interaction_map.offset) || args.interaction_map.offset < 0)) || Object.keys(args.interaction_map).some((key) => !["scope", "offset"].includes(key)))) {
    throw new Error("interaction_map supports only scope=page and a non-negative integer offset");
  }
  inspectionConfig(args.inspection);
  if (args.captures !== undefined) {
    if (!Array.isArray(args.captures) || args.captures.length === 0 || args.captures.length > 4) {
      throw new Error("captures must contain between one and four labeled captures");
    }
    const labels = new Set();
    for (const capture of args.captures) {
      if (!capture || typeof capture !== "object" || Array.isArray(capture) || typeof capture.label !== "string" || !capture.label.trim() || capture.label.length > 80 || labels.has(capture.label)) {
        throw new Error("each capture requires a unique non-empty label of at most 80 characters");
      }
      labels.add(capture.label);
      for (const key of ["viewportWidth", "viewportHeight"]) {
        if (capture[key] !== undefined && (!Number.isInteger(capture[key]) || capture[key] < 1 || capture[key] > 4096)) {
          throw new Error(`${key} must be an integer from 1 through 4096`);
        }
      }
      if (capture.scroll !== undefined && !["current", "top", "bottom"].includes(capture.scroll)) {
        throw new Error("capture scroll must be current, top, or bottom");
      }
      if (capture.scrollY !== undefined && (!Number.isFinite(capture.scrollY) || capture.scrollY < 0)) {
        throw new Error("capture scrollY must be a non-negative finite number");
      }
      if (capture.scroll !== undefined && capture.scrollY !== undefined) {
        throw new Error("capture may set scroll or scrollY, not both");
      }
      if (capture.settle_ms !== undefined && (!Number.isInteger(capture.settle_ms) || capture.settle_ms < 0 || capture.settle_ms > 2000)) {
        throw new Error("capture settle_ms must be an integer from 0 through 2000");
      }
      const allowed = new Set(["label", "viewportWidth", "viewportHeight", "scroll", "scrollY", "settle_ms"]);
      if (Object.keys(capture).some((key) => !allowed.has(key))) {
        throw new Error("capture contains an unsupported visual option");
      }
    }
  }
  if (args.save_artifact !== undefined && typeof args.save_artifact !== "boolean") {
    throw new Error("save_artifact must be a boolean");
  }
}

const INSPECTION_SECTIONS = new Set(["runtime", "frames", "console", "network"]);
const INSPECTION_ROW_LIMIT = 20;

function inspectionConfig(value) {
  if (value === undefined || value === null) return null;
  if (!value || typeof value !== "object" || Array.isArray(value) || Object.keys(value).some((key) => key !== "sections")) {
    throw new Error("Browser inspection request is invalid.");
  }
  if (value.sections === undefined) return { sections: [...INSPECTION_SECTIONS] };
  if (!Array.isArray(value.sections) || value.sections.some((section) => typeof section !== "string" || !INSPECTION_SECTIONS.has(section))) {
    throw new Error("Browser inspection request is invalid.");
  }
  return { sections: [...new Set(value.sections)] };
}

function installInspection(page, value) {
  const config = inspectionConfig(value);
  const rows = { console: [], network: [] };
  const omitted = { console: 0, network: 0 };
  const listeners = [];
  const push = (kind, row) => {
    if (rows[kind].length < INSPECTION_ROW_LIMIT) rows[kind].push(row);
    else omitted[kind] += 1;
  };
  if (config?.sections.includes("console")) {
    const onConsole = (message) => {
      const type = safeConsoleType(message?.type?.());
      push("console", { type, severity: consoleSeverity(type) });
    };
    page.on("console", onConsole);
    listeners.push(["console", onConsole]);
  }
  if (config?.sections.includes("network")) {
    const onResponse = (response) => {
      const request = response?.request?.();
      push("network", {
        method: safeMethod(request?.method?.()),
        status: boundedStatus(response?.status?.()),
        resourceType: safeResourceType(request?.resourceType?.()),
        destination: safeOrigin(response?.url?.()),
      });
    };
    page.on("response", onResponse);
    listeners.push(["response", onResponse]);
  }
  return {
    config,
    rows,
    omitted,
    cleanup() {
      for (const [event, listener] of listeners) page.off?.(event, listener);
    },
  };
}

async function snapshotInspection(page, collector) {
  const config = collector.config;
  if (!config) return null;
  const result = { requested: config.sections, scope: "call", observed: {}, omitted: {} };
  if (config.sections.includes("runtime")) {
    result.runtime = await page.evaluate(({ markerLimit }) => {
      const markerSelectors = [
        ["react", "[data-reactroot], [data-react]"],
        ["angular", "[ng-version]"],
        ["vue", "[data-vue-meta]"],
        ["svelte", "[data-svelte]"],
        ["next", "#__next"],
        ["root", "#root"],
        ["app", "#app"],
      ];
      const markers = markerSelectors
        .flatMap(([marker, selector]) => (document.querySelector(selector) ? [{ marker }] : []))
        .slice(0, markerLimit);
      return { userAgent: navigator.userAgent ? "present" : "absent", markers };
    }, { markerLimit: INSPECTION_ROW_LIMIT }).catch(() => ({ status: "unavailable", markers: [] }));
    result.observed.runtime = result.runtime.markers.length;
  }
  if (config.sections.includes("frames")) {
    const frames = page.frames?.() || [];
    result.frames = [];
    for (const frame of frames.slice(0, INSPECTION_ROW_LIMIT)) {
      let owner;
      try {
        const main = frame === page.mainFrame?.();
        owner = main ? null : await frame.frameElement?.();
        const box = await owner?.boundingBox?.();
        const frameUrl = frame.url?.();
        result.frames.push({ main, url: safeOrigin(frameUrl), crossOrigin: isCrossOrigin(page.url?.(), frameUrl), geometry: boundedGeometry(box) });
      } catch {
        result.frames.push({ main: false, url: "opaque", crossOrigin: true, geometry: null, status: "unavailable" });
      } finally {
        await owner?.dispose?.().catch?.(() => {});
      }
    }
    result.observed.frames = frames.length;
    result.omitted.frames = Math.max(0, frames.length - INSPECTION_ROW_LIMIT);
  }
  for (const section of ["console", "network"]) {
    if (config.sections.includes(section)) {
      result[section] = collector.rows[section];
      result.observed[section] = collector.rows[section].length + collector.omitted[section];
      result.omitted[section] = collector.omitted[section];
    }
  }
  return result;
}

const SAFE_METHODS = new Set(["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "CONNECT", "TRACE"]);
const SAFE_RESOURCE_TYPES = new Set(["document", "stylesheet", "image", "media", "font", "script", "texttrack", "xhr", "fetch", "eventsource", "websocket", "manifest", "other"]);
const SAFE_CONSOLE_TYPES = new Set(["debug", "dir", "dirxml", "error", "info", "log", "trace", "warning", "startGroup", "startGroupCollapsed", "endGroup"]);
function safeMethod(value) { return typeof value === "string" && SAFE_METHODS.has(value.toUpperCase()) ? value.toUpperCase() : "OTHER"; }
function safeResourceType(value) { return typeof value === "string" && SAFE_RESOURCE_TYPES.has(value) ? value : "other"; }
function safeConsoleType(value) { return typeof value === "string" && SAFE_CONSOLE_TYPES.has(value) ? value : "other"; }
function consoleSeverity(type) { return ["error", "warning", "info", "debug"].includes(type) ? type : "log"; }
function boundedStatus(value) { return Number.isInteger(value) && value >= 100 && value <= 599 ? value : null; }
function safeOrigin(value) {
  try {
    const url = new URL(String(value));
    if (!["http:", "https:"].includes(url.protocol) || url.host.length > 256) return "opaque";
    return `${url.protocol}//${url.host}`;
  } catch { return "opaque"; }
}
function isCrossOrigin(pageUrl, frameUrl) {
  try { return new URL(pageUrl).origin !== new URL(frameUrl).origin; } catch { return true; }
}
function boundedGeometry(box) {
  if (!box || !["x", "y", "width", "height"].every((key) => Number.isFinite(box[key]))) return null;
  return { x: box.x, y: box.y, width: box.width, height: box.height };
}

function captureMode() {
  const mode = (process.env.CODEX_BROWSER_PLAYWRIGHT_CAPTURE_MODE || CAPTURE_VIEWPORT).toLowerCase();
  if (mode === CAPTURE_VIEWPORT || mode === CAPTURE_FULL_PAGE) return mode;
  throw new Error(
    `unsupported capture mode ${JSON.stringify(mode)}; expected ${CAPTURE_VIEWPORT} or ${CAPTURE_FULL_PAGE}`,
  );
}

async function activePage(context) {
  const existing = context.pages().find((page) => !page.isClosed());
  return existing || context.newPage();
}

async function restoreOrNavigate(page, request, stateDir) {
  const explicitUrl = request.arguments?.url;
  if (explicitUrl) {
    await page.goto(explicitUrl, { waitUntil: "domcontentloaded", timeout: timeoutMs(request) });
    return;
  }

  const statePath = path.join(stateDir, "state.json");
  const state = await readJsonOrNull(statePath);
  if (state?.url && page.url() === "about:blank") {
    await page.goto(state.url, { waitUntil: "domcontentloaded", timeout: timeoutMs(request) });
  }
}

function canonicalActions(argumentsValue) {
  if (Array.isArray(argumentsValue?.actions) && argumentsValue.actions.length > 0) {
    return argumentsValue.actions;
  }
  if (argumentsValue?.action || argumentsValue?.type) {
    return [argumentsValue];
  }
  return [];
}

async function runAction(page, action) {
  const type = action.type || action.action;
  switch (type) {
    case "navigate":
      await requireUrl(page, action);
      return `navigated to ${action.url}`;
    case "click":
      await click(page, action);
      return action.selector ? "clicked browser selector" : `clicked at ${action.x},${action.y}`;
    case "type":
      await typeText(page, action);
      return action.selector ? "typed into browser selector" : "typed into focused browser element";
    case "keypress":
      await keypress(page, action);
      return "sent browser keypress";
    case "key_down":
      await keyDown(page, action);
      return "sent browser key down";
    case "key_up":
      await keyUp(page, action);
      return "sent browser key up";
    case "scroll":
    case "mouse_wheel":
      await mouseWheel(page, action);
      return "scrolled browser viewport";
    case "wait":
      await page.waitForTimeout(numberOrDefault(action.ms, 1000));
      return `waited ${numberOrDefault(action.ms, 1000)} ms`;
    case "select":
      await selectOption(page, action);
      return "selected browser option";
    case "drag":
      await drag(page, action);
      return "dragged in browser viewport";
    case "hover":
      await hover(page, action);
      return action.selector ? "hovered browser selector" : `hovered at ${action.x},${action.y}`;
    case "mouse_move":
      await mouseMove(page, action);
      return `moved browser mouse to ${action.x},${action.y}`;
    case "mouse_down":
      await mouseDown(page, action);
      return "sent browser mouse down";
    case "mouse_up":
      await mouseUp(page, action);
      return "sent browser mouse up";
    default:
      throw new Error(`Unsupported browser action ${type}`);
  }
}

async function requireUrl(page, action) {
  if (!action.url) {
    throw new Error("navigate requires url");
  }
  await page.goto(action.url, { waitUntil: "domcontentloaded", timeout: timeoutMs({ arguments: action }) });
}

async function click(page, action) {
  const locator = locatorFromAction(page, action);
  if (locator) {
    await withKeyboardModifiers(page, action, async () => {
      await locator.click({
        ...clickOptions(action),
        timeout: timeoutMs({ arguments: action }),
      });
    });
  } else {
    await withKeyboardModifiers(page, action, async () => {
      await moveMouseToActionPoint(page, action);
      await page.mouse.click(
        requiredNumber(action, "x"),
        requiredNumber(action, "y"),
        clickOptions(action),
      );
    });
  }
}

async function typeText(page, action) {
  const text = action.text || "";
  const locator = locatorFromAction(page, action);
  if (locator && textEntryMethod(action) === "fill") {
    await locator.fill(text, { timeout: timeoutMs({ arguments: action }) });
    return;
  }
  if (locator) {
    await locator.click({ timeout: timeoutMs({ arguments: action }) });
    if (action.replace !== false) {
      await selectAllAndClear(page);
    }
  }
  await page.keyboard.type(text, keyboardDelayOptions(action));
}

async function keypress(page, action) {
  const key = Array.isArray(action.keys) && action.keys.length > 0 ? action.keys.join("+") : action.key;
  if (!key) {
    throw new Error("keypress requires key or keys");
  }
  await page.keyboard.press(key, keyboardDelayOptions(action));
}

async function keyDown(page, action) {
  const key = requiredKey(action, "key_down");
  await page.keyboard.down(key);
}

async function keyUp(page, action) {
  const key = requiredKey(action, "key_up");
  await page.keyboard.up(key);
}

async function mouseWheel(page, action) {
  await withKeyboardModifiers(page, action, async () => {
    await page.mouse.wheel(
      numberOrDefault(action.scroll_x, 0),
      numberOrDefault(action.scroll_y, 720),
    );
  });
}

async function selectOption(page, action) {
  const locator = locatorFromAction(page, action);
  if (!locator) {
    throw new Error("select requires selector");
  }
  await locator.selectOption(action.value || action.text || action.label || "");
}

async function drag(page, action) {
  await withKeyboardModifiers(page, action, async () => {
    await page.mouse.move(
      requiredNumber(action, "x1"),
      requiredNumber(action, "y1"),
      mouseMoveOptions(action),
    );
    await page.mouse.down(mouseButtonOptions(action));
    await page.mouse.move(
      requiredNumber(action, "x2"),
      requiredNumber(action, "y2"),
      mouseMoveOptions(action),
    );
    await page.mouse.up(mouseButtonOptions(action));
  });
}

async function hover(page, action) {
  const locator = locatorFromAction(page, action);
  if (locator) {
    await locator.hover({ timeout: timeoutMs({ arguments: action }) });
  } else {
    await mouseMove(page, action);
  }
}

async function mouseMove(page, action) {
  await page.mouse.move(
    requiredNumber(action, "x"),
    requiredNumber(action, "y"),
    mouseMoveOptions(action),
  );
}

async function mouseDown(page, action) {
  await withKeyboardModifiers(page, action, async () => {
    await moveMouseIfActionPoint(page, action);
    await page.mouse.down(mouseButtonOptions(action));
    await delayAfterMouseEvent(page, action);
  });
}

async function mouseUp(page, action) {
  await withKeyboardModifiers(page, action, async () => {
    await moveMouseIfActionPoint(page, action);
    await page.mouse.up(mouseButtonOptions(action));
    await delayAfterMouseEvent(page, action);
  });
}

function locatorFromAction(page, action) {
  const selector = action.selector;
  if (!selector) {
    return null;
  }
  if (typeof selector === "string") {
    return page.locator(selector).first();
  }
  if (selector.css) {
    return page.locator(selector.css).first();
  }
  if (selector.text) {
    return page.getByText(selector.text, selectorOptions(selector)).first();
  }
  if (selector.label) {
    return page.getByLabel(selector.label, selectorOptions(selector)).first();
  }
  if (selector.placeholder) {
    return page
      .getByPlaceholder(selector.placeholder, selectorOptions(selector))
      .first();
  }
  if (selector.test_id || selector.testId) {
    return page.getByTestId(selector.test_id || selector.testId).first();
  }
  if (selector.title) {
    return page.getByTitle(selector.title, selectorOptions(selector)).first();
  }
  if (selector.alt_text || selector.altText) {
    return page
      .getByAltText(selector.alt_text || selector.altText, selectorOptions(selector))
      .first();
  }
  if (selector.role) {
    return page.getByRole(selector.role, roleSelectorOptions(selector)).first();
  }
  return null;
}

function textEntryMethod(action) {
  return action.method === "fill" || action.input_method === "fill"
    ? "fill"
    : "keyboard";
}

async function selectAllAndClear(page) {
  const modifier = process.platform === "darwin" ? "Meta" : "Control";
  await page.keyboard.press(`${modifier}+A`);
  await page.keyboard.press("Backspace");
}

function requiredKey(action, actionName) {
  if (!action.key) {
    throw new Error(`${actionName} requires key`);
  }
  return action.key;
}

function clickOptions(action) {
  return compactOptions({
    ...mouseButtonOptions(action),
    ...keyboardDelayOptions(action),
    clickCount: positiveIntegerOrUndefined(action.click_count),
  });
}

function mouseButtonOptions(action) {
  return { button: mouseButton(action) };
}

function mouseButton(action) {
  const button = action.button || "left";
  if (!["left", "right", "middle"].includes(button)) {
    throw new Error("button must be left, right, or middle");
  }
  return button;
}

function mouseMoveOptions(action) {
  return compactOptions({ steps: positiveIntegerOrUndefined(action.steps) });
}

function keyboardDelayOptions(action) {
  return compactOptions({
    delay: nonNegativeIntegerOrUndefined(action.delay_ms),
  });
}

async function moveMouseToActionPoint(page, action) {
  await page.mouse.move(
    requiredNumber(action, "x"),
    requiredNumber(action, "y"),
    mouseMoveOptions(action),
  );
}

async function moveMouseIfActionPoint(page, action) {
  const hasX = typeof action.x === "number";
  const hasY = typeof action.y === "number";
  if (hasX !== hasY) {
    throw new Error(
      "mouse action requires both x and y when either coordinate is provided",
    );
  }
  if (hasX && hasY) {
    await page.mouse.move(action.x, action.y, mouseMoveOptions(action));
  }
}

async function delayAfterMouseEvent(page, action) {
  const delay = nonNegativeIntegerOrUndefined(action.delay_ms);
  if (delay !== undefined) {
    await page.waitForTimeout(delay);
  }
}

async function withKeyboardModifiers(page, action, body) {
  const modifiers = Array.isArray(action.modifiers) ? action.modifiers : [];
  for (const modifier of modifiers) {
    if (!["Alt", "Control", "Meta", "Shift"].includes(modifier)) {
      throw new Error("modifiers must contain only Alt, Control, Meta, or Shift");
    }
  }
  for (const modifier of modifiers) {
    await page.keyboard.down(modifier);
  }
  try {
    return await body();
  } finally {
    for (const modifier of modifiers.slice().reverse()) {
      await page.keyboard.up(modifier).catch(() => {});
    }
  }
}

function selectorOptions(selector) {
  return selector.exact === undefined ? undefined : { exact: Boolean(selector.exact) };
}

function roleSelectorOptions(selector) {
  const options = selector.name ? { name: selector.name } : {};
  if (selector.exact !== undefined) {
    options.exact = Boolean(selector.exact);
  }
  return Object.keys(options).length > 0 ? options : undefined;
}

function compactOptions(options) {
  return Object.fromEntries(
    Object.entries(options).filter(([, value]) => value !== undefined),
  );
}

async function captureScreenshot(page) {
  const errors = [];
  const fullPage = captureMode() === CAPTURE_FULL_PAGE;
  try {
    return {
      buffer: await page.screenshot({ type: "png", fullPage }),
      method: "page.screenshot",
    };
  } catch (error) {
    errors.push(`page.screenshot: ${errorMessage(error)}`);
  }

  for (const fromSurface of [true, false]) {
    let cdp = null;
    try {
      cdp = await page.context().newCDPSession(page);
      await cdp.send("Page.enable").catch(() => {});
      const result = await cdp.send("Page.captureScreenshot", {
        format: "png",
        fromSurface,
        captureBeyondViewport: fullPage,
      });
      return {
        buffer: Buffer.from(result.data, "base64"),
        method: `cdp.Page.captureScreenshot(fromSurface=${fromSurface})`,
        warning: compactCaptureErrors(errors),
      };
    } catch (error) {
      errors.push(
        `cdp.Page.captureScreenshot(fromSurface=${fromSurface}): ${errorMessage(error)}`,
      );
    } finally {
      if (cdp) {
        await cdp.detach().catch(() => {});
      }
    }
  }

  for (const selector of ["body", "html"]) {
    try {
      return {
        buffer: await page.locator(selector).screenshot({ type: "png" }),
        method: `locator(${selector}).screenshot`,
        warning: compactCaptureErrors(errors),
      };
    } catch (error) {
      errors.push(`locator(${selector}).screenshot: ${errorMessage(error)}`);
    }
  }

  throw new Error(`Unable to capture browser screenshot. ${compactCaptureErrors(errors)}`);
}

async function captureScreenshots(page, args) {
  const requested = args.captures;
  if (!requested) {
    const requestedViewport = page.viewportSize?.() || null;
    return {
      captures: [{
        label: null,
        requestedViewport,
        metadata: typeof page.evaluate === "function"
          ? await viewportMetadata(page, requestedViewport)
          : { requestedViewport, effectiveViewport: requestedViewport },
        screenshot: await captureScreenshot(page),
      }],
      restoration: { requested: false, success: true },
      error: null,
    };
  }

  const originalViewport = page.viewportSize?.() || null;
  const original = await viewportMetadata(page, originalViewport);
  const result = { captures: [], restoration: { requested: true, success: false }, error: null };
  try {
    for (const capture of requested) {
      const current = page.viewportSize?.() || original.effectiveViewport;
      const viewport = {
        width: capture.viewportWidth ?? current.width,
        height: capture.viewportHeight ?? current.height,
      };
      await page.setViewportSize(viewport);
      await applyCaptureScroll(page, capture);
      if ((capture.settle_ms ?? 150) > 0) await page.waitForTimeout(capture.settle_ms ?? 150);
      const metadata = await viewportMetadata(page, viewport);
      const screenshot = await captureScreenshot(page);
      result.captures.push({ label: capture.label, requestedViewport: viewport, metadata, screenshot });
    }
  } catch (error) {
    result.error = `capture: ${errorMessage(error)}`;
  } finally {
    try {
      const restoreViewport = originalViewport || original.effectiveViewport;
      await page.setViewportSize(restoreViewport);
      await page.evaluate((scroll) => window.scrollTo(scroll.x, scroll.y), original.scroll);
      const restored = await viewportMetadata(page, originalViewport);
      const viewportMatches = restored.effectiveViewport.width === original.effectiveViewport.width && restored.effectiveViewport.height === original.effectiveViewport.height;
      const clientViewportMatches = restored.clientViewport?.width === original.clientViewport?.width && restored.clientViewport?.height === original.clientViewport?.height;
      const devicePixelRatioMatches = restored.devicePixelRatio === original.devicePixelRatio;
      const scrollMatches = restored.scroll.x === original.scroll.x && restored.scroll.y === original.scroll.y;
      result.restoration = { requested: true, success: viewportMatches && clientViewportMatches && devicePixelRatioMatches && scrollMatches, actual: restored, expected: original };
      if (!result.restoration.success) result.error ||= "restoration: viewport, device scale, or scroll position did not return to its initial value";
    } catch (error) {
      result.restoration = { requested: true, success: false, error: errorMessage(error) };
      result.error ||= `restoration: ${errorMessage(error)}`;
    }
  }
  return result;
}

async function viewportMetadata(page, requestedViewport) {
  return page.evaluate((requested) => {
    const root = document.documentElement;
    return {
      requestedViewport: requested,
      effectiveViewport: { width: window.innerWidth, height: window.innerHeight },
      clientViewport: { width: root.clientWidth, height: root.clientHeight },
      document: { width: root.scrollWidth, height: root.scrollHeight },
      devicePixelRatio: window.devicePixelRatio,
      scroll: { x: window.scrollX, y: window.scrollY },
    };
  }, requestedViewport);
}

async function applyCaptureScroll(page, capture) {
  if (capture.scrollY !== undefined) {
    await page.evaluate((scrollY) => window.scrollTo(window.scrollX, scrollY), capture.scrollY);
  } else if (capture.scroll === "top") {
    await page.evaluate(() => window.scrollTo(window.scrollX, 0));
  } else if (capture.scroll === "bottom") {
    await page.evaluate(() => window.scrollTo(window.scrollX, document.documentElement.scrollHeight));
  }
}

async function pageHints(page, offset = 0) {
  return page.evaluate((requestedOffset) => {
    const selector = "button,a[href],input,textarea,select,iframe,[role],[tabindex],[contenteditable='true'],[data-testid],[data-autosave-action]";
    const viewportWidth = typeof window !== "undefined" ? window.innerWidth : Number.MAX_SAFE_INTEGER;
    const viewportHeight = typeof window !== "undefined" ? window.innerHeight : Number.MAX_SAFE_INTEGER;
    const selectorIndex = { ids: new Map(), testIds: new Map(), labels: new Map(), tags: new Map() };
    for (const element of document.querySelectorAll("*")) {
      const tag = element.tagName.toLowerCase();
      selectorIndex.tags.set(tag, (selectorIndex.tags.get(tag) || 0) + 1);
      if (element.id) selectorIndex.ids.set(element.id, (selectorIndex.ids.get(element.id) || 0) + 1);
      const testId = element.getAttribute("data-testid");
      if (testId) selectorIndex.testIds.set(testId, (selectorIndex.testIds.get(testId) || 0) + 1);
      const label = element.getAttribute("aria-label");
      if (label) selectorIndex.labels.set(label, (selectorIndex.labels.get(label) || 0) + 1);
    }
    const all = Array.from(document.querySelectorAll(selector))
      .map((element, index) => {
        if (isHidden(element)) return null;
        const rect = element.getBoundingClientRect();
        if (rect.width <= 0 || rect.height <= 0) return null;
        const left = rect.left ?? rect.x;
        const top = rect.top ?? rect.y;
        const right = rect.right ?? left + rect.width;
        const bottom = rect.bottom ?? top + rect.height;
        const tag = element.tagName.toLowerCase();
        const role = (element.getAttribute("role") || ({ button: "button", a: "link", input: "textbox", textarea: "textbox", select: "combobox", iframe: "iframe" }[tag] || "element")).slice(0, 40);
        const valueBearingRole = ["textbox", "searchbox", "combobox", "spinbutton", "slider"].includes(role.toLowerCase());
        const valueBearingElement = tag === "input" || tag === "textarea" || tag === "select" || element.isContentEditable || valueBearingRole;
        const name = String(element.getAttribute("aria-label") || element.getAttribute("title") || (valueBearingElement ? "" : (element.innerText || element.textContent || "").replace(/\s+/g, " ").trim())).replace(/[\u0000-\u001f\u007f]/g, " ").slice(0, 80);
        const hints = [];
        const selectorMatches = [];
        if (element.id && element.id.length <= 80) {
          const value = `#${escapeIdentifier(element.id)}`;
          hints.push(value);
          selectorMatches.push({ selector: value, count: selectorIndex.ids.get(element.id) || 0 });
        }
        const testId = element.getAttribute("data-testid");
        if (testId && testId.length <= 80) {
          const value = `[data-testid="${escapeString(testId)}"]`;
          hints.push(value);
          selectorMatches.push({ selector: value, count: selectorIndex.testIds.get(testId) || 0 });
        }
        const label = element.getAttribute("aria-label");
        if (label && label.length <= 80) {
          const value = `[aria-label="${escapeString(label)}"]`;
          hints.push(value);
          selectorMatches.push({ selector: value, count: selectorIndex.labels.get(label) || 0 });
        }
        if (!hints.length) {
          hints.push(tag);
          selectorMatches.push({ selector: tag, count: selectorIndex.tags.get(tag) || 0 });
        }
        const selectors = hints.slice(0, 3);
        const disabled = Boolean(element.disabled || element.getAttribute("aria-disabled") === "true" || element.closest?.("[aria-disabled='true'],fieldset:disabled"));
        const offscreen = bottom <= 0 || right <= 0 || top >= viewportHeight || left >= viewportWidth;
        const centerX = Math.max(0, Math.min(viewportWidth - 1, left + rect.width / 2));
        const centerY = Math.max(0, Math.min(viewportHeight - 1, top + rect.height / 2));
        const topElement = document.elementFromPoint?.(centerX, centerY);
        const covered = Boolean(topElement && topElement !== element && !element.contains?.(topElement));
        const activeOverlay = isActiveOverlay(element);
        return {
          index,
          focused: element === document.activeElement,
          activeOverlay,
          summary: {
            role,
            name,
            focused: element === document.activeElement,
            disabled,
            covered,
            offscreen,
            ambiguous: selectorMatches.some((match) => match.count > 1),
            selectorMatches,
            box: { x: Math.round(left), y: Math.round(top), width: Math.round(rect.width), height: Math.round(rect.height) },
            selectors,
          },
        };
      })
      .filter(Boolean)
      .sort((left, right) => compareRank(rank(right), rank(left)) || left.index - right.index);
    const boundedOffset = Math.min(requestedOffset, all.length);
    const controls = all.slice(boundedOffset, boundedOffset + 24).map((entry) => entry.summary);
    return { offset: boundedOffset, total: all.length, omitted: Math.max(0, all.length - boundedOffset - controls.length), controls };

    function rank(entry) {
      const control = entry.summary;
      return [
        entry.focused ? 1 : 0,
        entry.activeOverlay ? 1 : 0,
        control.offscreen ? 0 : 1,
        control.covered ? 0 : 1,
        control.disabled ? 0 : 1,
        ["link", "button"].includes(control.role) ? 1 : 0,
      ];
    }
    function compareRank(left, right) {
      for (let index = 0; index < left.length; index += 1) {
        if (left[index] !== right[index]) return left[index] - right[index];
      }
      return 0;
    }
    function isHidden(element) {
      for (let current = element; current; current = current.parentElement) {
        if (current.hidden || current.inert || current.getAttribute?.("aria-hidden") === "true") return true;
        if (typeof window !== "undefined" && window.getComputedStyle) {
          const style = window.getComputedStyle(current);
          if (style.display === "none" || style.visibility === "hidden" || style.visibility === "collapse") return true;
        }
      }
      return false;
    }
    function isActiveOverlay(element) {
      const overlay = element.closest?.("dialog,[role='dialog'],[aria-modal='true'],[role='menu'],.modal");
      if (!overlay || overlay.hidden || overlay.inert || overlay.getAttribute("aria-hidden") === "true") return false;
      const rect = overlay.getBoundingClientRect();
      if (!rect || rect.width <= 0 || rect.height <= 0) return false;
      const style = typeof window !== "undefined" && window.getComputedStyle ? window.getComputedStyle(overlay) : null;
      return !style || (style.display !== "none" && style.visibility !== "hidden");
    }
    function escapeIdentifier(value) {
      return typeof CSS !== "undefined" && CSS.escape ? CSS.escape(value) : String(value).replace(/[^a-zA-Z0-9_-]/g, "\\$&");
    }
    function escapeString(value) {
      return String(value).replace(/[\u0000-\u001f\u007f"\\]/g, (character) => `\\${character.codePointAt(0).toString(16)} `);
    }
  }, offset);
}

async function saveCaptureArtifacts(stateDir, bundle, request) {
  if (bundle.error || !bundle.restoration.success) {
    return {
      success: false,
      directory: null,
      manifest: null,
      files: [],
      error: "capture or restoration failed; no artifact files were written",
    };
  }
  const screenshots = [];
  let runDir = null;
  try {
    const artifactsDir = path.join(stateDir, "artifacts");
    await ensurePrivateStateDirectory(artifactsDir);
    runDir = await fs.mkdtemp(path.join(artifactsDir, "capture-"));
    await fs.chmod(runDir, 0o700);
    for (let index = 0; index < bundle.captures.length; index += 1) {
      const capture = bundle.captures[index];
      const fileName = `capture-${String(index + 1).padStart(2, "0")}.png`;
      const filePath = path.join(runDir, fileName);
      await fs.writeFile(filePath, capture.screenshot.buffer, { flag: "wx", mode: 0o600 });
      await fs.chmod(filePath, 0o600);
      screenshots.push({ order: index + 1, label: capture.label, path: fileName, method: capture.screenshot.method, metadata: capture.metadata });
    }
    const manifestPath = path.join(runDir, "manifest.json");
    const manifest = { tool: request.tool, threadId: request.threadId, restoration: bundle.restoration, captures: screenshots };
    await fs.writeFile(manifestPath, JSON.stringify(manifest, null, 2), { flag: "wx", mode: 0o600 });
    await fs.chmod(manifestPath, 0o600);
    return {
      success: true,
      directory: path.relative(stateDir, runDir),
      manifest: path.relative(stateDir, manifestPath),
      files: [...screenshots.map((capture) => capture.path), "manifest.json"],
      captures: screenshots,
    };
  } catch (error) {
    const files = [];
    if (runDir) {
      for (const name of await fs.readdir(runDir).catch(() => [])) {
        const stat = await fs.lstat(path.join(runDir, name)).catch(() => null);
        if (stat?.isFile()) files.push(name);
      }
    }
    return {
      success: false,
      directory: runDir ? path.relative(stateDir, runDir) : null,
      manifest: null,
      files,
      error: errorMessage(error),
    };
  }
}

async function responseForPage(page, bundle, summaries, { artifactResult, error, pageHints: includePageHints, pageHintOffset, inspection }) {
  const lines = ["Browser observation", `url: ${page.url()}`];
  const title = await pageTitle(page);
  if (title) {
    lines.push(`title: ${title}`);
  }
  const viewport = page.viewportSize();
  if (viewport) {
    lines.push(`viewport: ${viewport.width}x${viewport.height}`);
  }
  if (summaries.length > 0) {
    lines.push("actions:");
    for (const summary of summaries) {
      lines.push(`- ${summary}`);
    }
  }
  if (includePageHints) {
    const hints = await pageHints(page, pageHintOffset);
    lines.push(`page_hints: ${JSON.stringify(hints)}`);
  }
  if (inspection) {
    lines.push(`inspection: ${JSON.stringify(inspection)}`);
  }
  for (const [index, capture] of bundle.captures.entries()) {
    const label = capture.label ? ` label=${JSON.stringify(capture.label)}` : "";
    lines.push(`capture[${index + 1}]${label}: ${JSON.stringify(capture.metadata)} method=${capture.screenshot.method}`);
    if (capture.screenshot.warning) lines.push(`capture_fallback[${index + 1}]: ${capture.screenshot.warning}`);
  }
  lines.push(`restoration: ${JSON.stringify(bundle.restoration)}`);
  if (artifactResult?.success) {
    lines.push(`artifact_manifest: ${artifactResult.manifest}`);
  } else if (artifactResult) {
    lines.push(`artifact_partial: ${JSON.stringify({
      directory: artifactResult.directory,
      files: artifactResult.files,
      complete_manifest: false,
    })}`);
  }
  if (error) lines.push(`visual_error: ${error}`);
  return {
    contentItems: [
      { type: "inputText", text: lines.join("\n") },
      ...bundle.captures.map((capture) => ({
        type: "inputImage",
        imageUrl: `data:image/png;base64,${capture.screenshot.buffer.toString("base64")}`,
        detail: "high",
      })),
    ],
    success: !error && bundle.restoration.success && (!bundle.captures.length || Boolean(bundle.captures[0].screenshot)),
  };
}

function pageTitle(page) {
  return page
    .title()
    .then((title) => title)
    .catch(() => "");
}

async function saveState(stateDir, page) {
  await fs.writeFile(
    path.join(stateDir, "state.json"),
    JSON.stringify({ url: page.url(), updatedAt: new Date().toISOString() }),
  );
}

async function readJsonOrNull(file) {
  try {
    return JSON.parse(await fs.readFile(file, "utf8"));
  } catch {
    return null;
  }
}

function timeoutMs(request) {
  return numberOrDefault(request.arguments?.timeout_secs, 30) * 1000;
}

function requiredNumber(value, field) {
  if (typeof value[field] !== "number") {
    throw new Error(`${field} is required`);
  }
  return value[field];
}

function numberOrDefault(value, fallback) {
  return typeof value === "number" && Number.isFinite(value) ? value : fallback;
}

function positiveIntegerOrUndefined(value) {
  return Number.isInteger(value) && value > 0 ? value : undefined;
}

function nonNegativeIntegerOrUndefined(value) {
  return Number.isInteger(value) && value >= 0 ? value : undefined;
}

function compactCaptureErrors(errors) {
  return errors
    .map((error) => error.split("\n")[0])
    .join(" | ")
    .slice(0, 500);
}

function errorMessage(error) {
  return String(error?.message || error);
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function envNumber(name, fallback) {
  const raw = process.env[name];
  if (!raw) {
    return fallback;
  }
  const parsed = Number(raw);
  return Number.isFinite(parsed) && parsed > 0 ? parsed : fallback;
}

function trimmedEnv(name) {
  const value = process.env[name];
  return value && value.trim() ? value.trim() : null;
}

function writeResponse(response) {
  process.stdout.write(JSON.stringify(response));
}
