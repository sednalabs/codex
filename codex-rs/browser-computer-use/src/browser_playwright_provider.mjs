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
  const request = JSON.parse(await readStdin());
  const { chromium } = loadPlaywright();
  const stateDir = await browserStateDir();
  await withProfileLock(stateDir, async () => {
    const headless = playwrightHeadless();
    const viewport = viewportFromRequest(request);
    const context = await chromium.launchPersistentContext(stateDir, {
      ...launchOptions({ headless, viewport }),
      headless,
      viewport,
    });

    try {
      const page = await activePage(context);
      await restoreOrNavigate(page, request);

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
      const screenshot = await captureScreenshot(page);
      await saveState(stateDir, page);
      writeResponse(await responseForPage(page, screenshot, summaries));
    } finally {
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

async function browserStateDir() {
  const configured = process.env.CODEX_BROWSER_PLAYWRIGHT_STATE_DIR;
  const dir =
    configured && configured.trim()
      ? configured
      : path.join(os.homedir(), ".codex", "browser-computer-use-playwright");
  await fs.mkdir(dir, { recursive: true });
  return dir;
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

function captureMode() {
  const mode = (
    process.env.CODEX_BROWSER_PLAYWRIGHT_CAPTURE_MODE || CAPTURE_VIEWPORT
  ).toLowerCase();
  return mode === CAPTURE_FULL_PAGE ? CAPTURE_FULL_PAGE : CAPTURE_VIEWPORT;
}

async function activePage(context) {
  const existing = context.pages().find((page) => !page.isClosed());
  return existing || context.newPage();
}

async function restoreOrNavigate(page, request) {
  const explicitUrl = request.arguments?.url;
  if (explicitUrl) {
    await page.goto(explicitUrl, { waitUntil: "domcontentloaded", timeout: timeoutMs(request) });
    return;
  }

  const statePath = path.join(await browserStateDir(), "state.json");
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

async function responseForPage(page, screenshot, summaries) {
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
  lines.push(`capture: ${screenshot.method}`);
  if (screenshot.warning) {
    lines.push(`capture_fallback: ${screenshot.warning}`);
  }
  return {
    contentItems: [
      { type: "inputText", text: lines.join("\n") },
      {
        type: "inputImage",
        imageUrl: `data:image/png;base64,${screenshot.buffer.toString("base64")}`,
        detail: "high",
      },
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
