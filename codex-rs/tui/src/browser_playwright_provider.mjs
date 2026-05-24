import fs from "node:fs/promises";
import { createRequire } from "node:module";
import os from "node:os";
import path from "node:path";

const TOOL_OBSERVE = "browser_observe";
const TOOL_STEP = "browser_step";
const CAPTURE_VIEWPORT = "viewport";
const CAPTURE_FULL_PAGE = "full_page";

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
  const context = await chromium.launchPersistentContext(stateDir, {
    ...launchOptions(),
    headless: playwrightHeadless(),
    viewport: viewportFromRequest(request),
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
    const screenshot = await page.screenshot({
      type: "png",
      fullPage: captureMode() === CAPTURE_FULL_PAGE,
    });
    await saveState(stateDir, page);
    writeResponse(await responseForPage(page, screenshot, summaries));
  } finally {
    await context.close().catch(() => {});
  }
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

function playwrightHeadless() {
  const raw = (process.env.CODEX_BROWSER_PLAYWRIGHT_HEADLESS || "1").toLowerCase();
  return !["0", "false", "no", "off"].includes(raw);
}

function launchOptions() {
  const options = {};
  const executablePath = trimmedEnv("CODEX_BROWSER_PLAYWRIGHT_EXECUTABLE_PATH");
  if (executablePath) {
    options.executablePath = executablePath;
  }
  const channel = trimmedEnv("CODEX_BROWSER_PLAYWRIGHT_CHANNEL");
  if (channel && !executablePath) {
    options.channel = channel;
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
    case "scroll":
      await page.mouse.wheel(numberOrDefault(action.scroll_x, 0), numberOrDefault(action.scroll_y, 720));
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
    await locator.click({ timeout: timeoutMs({ arguments: action }) });
  } else {
    await page.mouse.click(requiredNumber(action, "x"), requiredNumber(action, "y"));
  }
}

async function typeText(page, action) {
  const text = action.text || "";
  const locator = locatorFromAction(page, action);
  if (locator) {
    await locator.fill(text, { timeout: timeoutMs({ arguments: action }) });
  } else {
    await page.keyboard.type(text);
  }
}

async function keypress(page, action) {
  const key = Array.isArray(action.keys) && action.keys.length > 0 ? action.keys.join("+") : action.key;
  if (!key) {
    throw new Error("keypress requires key or keys");
  }
  await page.keyboard.press(key);
}

async function selectOption(page, action) {
  const locator = locatorFromAction(page, action);
  if (!locator) {
    throw new Error("select requires selector");
  }
  await locator.selectOption(action.value || action.text || action.label || "");
}

async function drag(page, action) {
  await page.mouse.move(requiredNumber(action, "x1"), requiredNumber(action, "y1"));
  await page.mouse.down();
  await page.mouse.move(requiredNumber(action, "x2"), requiredNumber(action, "y2"));
  await page.mouse.up();
}

async function hover(page, action) {
  const locator = locatorFromAction(page, action);
  if (locator) {
    await locator.hover({ timeout: timeoutMs({ arguments: action }) });
  } else {
    await page.mouse.move(requiredNumber(action, "x"), requiredNumber(action, "y"));
  }
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
    return page.getByText(selector.text).first();
  }
  if (selector.label) {
    return page.getByLabel(selector.label).first();
  }
  if (selector.role) {
    return page.getByRole(selector.role, selector.name ? { name: selector.name } : undefined).first();
  }
  return null;
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
  return {
    contentItems: [
      { type: "inputText", text: lines.join("\n") },
      {
        type: "inputImage",
        imageUrl: `data:image/png;base64,${screenshot.toString("base64")}`,
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
