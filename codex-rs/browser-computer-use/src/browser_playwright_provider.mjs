import { createHash, randomUUID } from "node:crypto";
import fs from "node:fs/promises";
import { createRequire } from "node:module";
import os from "node:os";
import path from "node:path";
import { createInterface } from "node:readline";

const TOOL_OBSERVE = "browser_observe";
const TOOL_STEP = "browser_step";
const DEFAULT_PROFILE_LOCK_TIMEOUT_MS = 120_000;
const PROFILE_LOCK_POLL_MS = 250;
const CLEANUP_TIMEOUT_MS = 10_000;

let context;
let page;
let stateDir;
let lockDir;
let ownerToken;
let cancelRequested = false;
let activeRequestId = null;
let lastRequestId = null;
let initializationPromise;
let activeRequestTask;
let activeRequestSettled = true;

main().catch(async (error) => {
  await writeFrame({ kind: "fatal", error: errorMessage(error) }).catch(() => {});
  process.exitCode = 0;
});

async function main() {
  if (process.platform === "win32") {
    throw new Error("Native Browser profile state is unsupported on Windows; no state was changed.");
  }
  const input = createInterface({ input: process.stdin, crlfDelay: Infinity });
  for await (const line of input) {
    const message = JSON.parse(line);
    if (message.kind === "start") {
      if (initializationPromise || activeRequestId !== null) {
        throw new Error("Browser provider received more than one start frame.");
      }
      activeRequestId = message.requestId;
      initializationPromise = initialize(message.request);
      startRequestTask(message.requestId, message.request, /*firstRequest*/ true);
      continue;
    }
    if (message.kind === "request") {
      if (!initializationPromise || !activeRequestSettled || activeRequestId !== null) {
        throw new Error("Browser provider received a concurrent request.");
      }
      activeRequestId = message.requestId;
      startRequestTask(message.requestId, message.request, /*firstRequest*/ false);
      continue;
    }
    if (message.kind === "cancel") {
      if (message.requestId !== activeRequestId
          && !(activeRequestId === null && message.requestId === lastRequestId)) {
        throw new Error("Browser cancellation request identity mismatch.");
      }
      cancelRequested = true;
      if (await cleanupAndRelease(message.requestId)) {
        await writeReleased(message.requestId);
      }
      return;
    }
    if (message.kind === "close") {
      cancelRequested = true;
      if (await cleanupAndRelease(null)) {
        await writeReleased(null);
      }
      return;
    }
    throw new Error("Unsupported Browser provider protocol frame.");
  }
  // EOF is not proof that this process completed cleanup. Leave any acquired
  // owner lock intact so the next invocation fails closed.
}

function startRequestTask(requestId, request, firstRequest) {
  activeRequestSettled = false;
  activeRequestTask = (async () => {
    let idlePublished = false;
    let response;
    try {
      if (firstRequest) {
        const profile = await initializationPromise;
        if (cancelRequested) return;
        await writeFrame({
          kind: "ready",
          profileKey: profile.stateDir,
          generation: profile.generation,
        });
      }
      if (cancelRequested) return;
      try {
        response = await executeRequest(request, firstRequest);
      } catch (error) {
        response = {
          contentItems: [{
            type: "inputText",
            text: "Browser Playwright provider failed: " + errorMessage(error),
          }],
          success: false,
        };
      }
      if (!cancelRequested) {
        await writeFrame({ kind: "response", requestId, response });
        // The caller waits for this frame before sending its next request.
        // Enter idle before publishing the acknowledgement so the sender
        // cannot race the task's finally transition.
        activeRequestSettled = true;
        lastRequestId = requestId;
        activeRequestId = null;
        idlePublished = true;
        await writeFrame({ kind: "idle", requestId });
      }
    } catch (error) {
      if (!cancelRequested) {
        await writeFrame({ kind: "fatal", error: errorMessage(error) });
      }
    } finally {
      // Once idle is published, the next request may already be active. Do
      // not let this task's finally block overwrite its state.
      if (!idlePublished) {
        activeRequestSettled = true;
        lastRequestId = requestId;
        activeRequestId = null;
      }
    }
  })();
}

function loadPlaywright() {
  const require = createRequire(import.meta.url);
  return require("playwright");
}

async function initialize(firstRequest) {
  validateVisualArguments(firstRequest.arguments || {});
  const { chromium } = loadPlaywright();
  const profile = await browserProfile(firstRequest);
  stateDir = profile.stateDir;
  lockDir = await acquireProfileLock(stateDir);
  ownerToken = randomUUID();
  await fs.writeFile(
    path.join(lockDir, "owner.json"),
    JSON.stringify({ generation: ownerToken, pid: process.pid, startedAt: new Date().toISOString() }),
    { flag: "wx", mode: 0o600 },
  );
  const headless = playwrightHeadless();
  const viewport = viewportFromRequest(firstRequest);
  context = await chromium.launchPersistentContext(stateDir, {
    ...launchOptions({ headless, viewport }),
    headless,
    viewport,
  });
  if (cancelRequested) return { stateDir, generation: ownerToken };
  page = await activePage(context);
  if (cancelRequested) return { stateDir, generation: ownerToken };
  await restoreOrNavigate(page, firstRequest, stateDir);
  return { stateDir, generation: ownerToken };
}

async function executeRequest(request, firstRequest) {
  validateVisualArguments(request.arguments || {});
  if (!firstRequest && request.arguments?.url) {
    await page.goto(request.arguments.url, {
      waitUntil: "domcontentloaded",
      timeout: timeoutMs(request),
    });
  }
  if (cancelRequested) throw new Error("Browser request cancelled.");
  const summaries = [];
  if (request.tool === TOOL_STEP) {
    const actions = canonicalActions(request.arguments);
    if (actions.length === 0) {
      throw new Error("browser_step requires an action or non-empty actions array.");
    }
    for (const action of actions) {
      summaries.push(await runAction(page, action));
      if (cancelRequested) throw new Error("Browser request cancelled.");
    }
  } else if (request.tool !== TOOL_OBSERVE) {
    throw new Error("Unsupported browser tool " + request.tool);
  }
  if (cancelRequested) throw new Error("Browser request cancelled.");
  await page.waitForLoadState("domcontentloaded", { timeout: 10_000 }).catch(() => {});
  if (cancelRequested) throw new Error("Browser request cancelled.");
  const screenshot = await captureScreenshot(page);
  if (cancelRequested) throw new Error("Browser request cancelled.");
  await saveState(stateDir, page);
  if (cancelRequested) throw new Error("Browser request cancelled.");
  return responseForPage(page, screenshot, summaries);
}

async function cleanupAndRelease(requestId) {
  const deadline = Date.now() + CLEANUP_TIMEOUT_MS;
  if (requestId !== null
      && requestId !== activeRequestId
      && !(activeRequestId === null && requestId === lastRequestId)) {
    return false;
  }
  if (!initializationPromise) return false;
  if (!await settleBefore(initializationPromise, deadline, /*acceptFailure*/ false)) return false;
  if (!lockDir || !ownerToken || !stateDir) return false;
  if (context && !await settleBefore(context.close(), deadline, /*acceptFailure*/ false)) return false;
  if (activeRequestTask && !activeRequestSettled) {
    if (!await settleBefore(activeRequestTask, deadline, /*acceptFailure*/ true)) return false;
  }
  if (Date.now() >= deadline) return false;
  try {
    await releaseProfileLock(lockDir, ownerToken);
    return true;
  } catch {
    return false;
  }
}

async function settleBefore(promise, deadline, acceptFailure) {
  const remaining = deadline - Date.now();
  if (remaining <= 0) return false;
  let timer;
  const timerPromise = new Promise((resolve) => {
    timer = setTimeout(() => resolve(false), remaining);
  });
  const result = await Promise.race([
    Promise.resolve(promise).then(
      () => true,
      () => acceptFailure,
    ),
    timerPromise,
  ]);
  clearTimeout(timer);
  return result === true;
}

async function writeReleased(requestId) {
  await writeFrame({
    kind: "released",
    profileKey: stateDir,
    generation: ownerToken,
    requestId,
  });
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
  return { stateDir: await fs.realpath(profileDir) };
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

async function acquireProfileLock(stateDir) {
  const lockDir = path.join(stateDir, ".codex-provider.lock");
  const deadline = Date.now() + lockTimeoutMs();
  while (true) {
    try {
      await fs.mkdir(lockDir);
      return lockDir;
    } catch (error) {
      if (error?.code !== "EEXIST") {
        throw error;
      }
      if (Date.now() >= deadline) {
        throw new Error(
          `Timed out waiting for browser profile lock at ${lockDir}. The existing owner is unknown, so the profile remains unavailable.`,
        );
      }
      await sleep(PROFILE_LOCK_POLL_MS);
    }
  }
}

async function releaseProfileLock(lockDir, generation) {
  const ownerPath = path.join(lockDir, "owner.json");
  const owner = JSON.parse(await fs.readFile(ownerPath, "utf8"));
  if (owner.generation !== generation) {
    throw new Error("Browser profile lock owner changed before release.");
  }
  await fs.unlink(ownerPath);
  await fs.rmdir(lockDir);
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
  return {
    width: envNumber("CODEX_BROWSER_PLAYWRIGHT_VIEWPORT_WIDTH", 1280),
    height: envNumber("CODEX_BROWSER_PLAYWRIGHT_VIEWPORT_HEIGHT", 720),
  };
}

function validateVisualArguments(args) {
  if (!args || typeof args !== "object" || Array.isArray(args)) {
    throw new Error("Browser arguments must be an object.");
  }
  if (args.url !== undefined && typeof args.url !== "string") {
    throw new Error("url must be a string.");
  }
  if (args.actions !== undefined && (!Array.isArray(args.actions) || args.actions.some((action) => !action || typeof action !== "object" || Array.isArray(action)))) {
    throw new Error("actions must be an array of action objects.");
  }
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
  try {
    return { buffer: await page.screenshot({ type: "png" }), method: "page.screenshot" };
  } catch (error) {
    errors.push("page.screenshot: " + errorMessage(error));
  }
  for (const fromSurface of [true, false]) {
    let cdp = null;
    try {
      cdp = await page.context().newCDPSession(page);
      await cdp.send("Page.enable").catch(() => {});
      const result = await cdp.send("Page.captureScreenshot", { format: "png", fromSurface });
      return { buffer: Buffer.from(result.data, "base64"), method: "cdp.Page.captureScreenshot", warning: errors.join(" | ").slice(0, 500) };
    } catch (error) {
      errors.push("cdp.Page.captureScreenshot: " + errorMessage(error));
    } finally {
      if (cdp) await cdp.detach().catch(() => {});
    }
  }
  for (const selector of ["body", "html"]) {
    try {
      return { buffer: await page.locator(selector).screenshot({ type: "png" }), method: "locator(" + selector + ").screenshot", warning: errors.join(" | ").slice(0, 500) };
    } catch (error) {
      errors.push("locator(" + selector + ").screenshot: " + errorMessage(error));
    }
  }
  throw new Error("Unable to capture browser screenshot. " + errors.map((error) => error.split("\n")[0]).join(" | ").slice(0, 500));
}

async function responseForPage(page, screenshot, summaries) {
  const lines = ["Browser observation", "url: " + page.url()];
  const title = await pageTitle(page);
  if (title) lines.push("title: " + title);
  const viewport = page.viewportSize();
  if (viewport) lines.push("viewport: " + viewport.width + "x" + viewport.height);
  if (summaries.length > 0) {
    lines.push("actions:");
    for (const summary of summaries) lines.push("- " + summary);
  }
  return {
    contentItems: [
      { type: "inputText", text: lines.join("\n") },
      { type: "inputImage", imageUrl: "data:image/png;base64," + screenshot.buffer.toString("base64"), detail: "high" },
    ],
    success: true,
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

function writeFrame(frame) {
  return new Promise((resolve, reject) => {
    process.stdout.write(JSON.stringify(frame) + "\n", (error) => {
      if (error) reject(error);
      else resolve();
    });
  });
}
