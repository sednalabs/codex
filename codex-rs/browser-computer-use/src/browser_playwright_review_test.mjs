import assert from "node:assert/strict";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { __test as providerTest } from "./browser_playwright_provider.mjs";
import { __test } from "./browser_playwright_review.mjs";

test("interaction summaries do not expose typed form or contenteditable values", () => {
  const editable = fakeElement("div", {
    attributes: { contenteditable: "true", "data-testid": "editor" },
    innerText: "draft body should not be exposed",
    textContent: "draft body should not be exposed",
    isContentEditable: true,
  });
  const input = fakeElement("input", {
    attributes: {
      name: "token",
      type: "password",
      value: "secret-token",
    },
  });
  const button = fakeElement("input", {
    attributes: {
      type: "submit",
      value: "Publish live",
    },
  });
  const roleTextbox = fakeElement("div", {
    attributes: {
      "aria-label": "Summary",
      role: "textbox",
    },
    innerText: "role textbox body should not be exposed",
    textContent: "role textbox body should not be exposed",
  });
  const document = fakeDocument([editable, input, button, roleTextbox]);

  const controls = __test.summarizeControlsForTest(document, { width: 1440, height: 1000 }, "*");

  assert.equal(controls[0].role, "textbox");
  assert.equal(controls[0].name, "");
  assert(!JSON.stringify(controls).includes("draft body should not be exposed"));
  assert.equal(controls[1].role, "textbox");
  assert.equal(controls[1].name, "token");
  assert(!JSON.stringify(controls).includes("secret-token"));
  assert.equal(controls[2].role, "button");
  assert.equal(controls[2].name, "Publish live");
  assert.equal(controls[3].role, "textbox");
  assert.equal(controls[3].name, "Summary");
  assert(!JSON.stringify(controls).includes("role textbox body should not be exposed"));
});

test("provider clears Chromium tab session restore without deleting saved browser state", async () => {
  const stateDir = await fs.mkdtemp(path.join(os.tmpdir(), "codex-browser-state-"));
  try {
    for (const restorePath of providerTest.browserSessionRestorePaths(stateDir)) {
      if (path.basename(restorePath) === "Sessions") {
        await fs.mkdir(restorePath, { recursive: true });
        await fs.writeFile(path.join(restorePath, "Session_1"), "stale tab");
      } else {
        await fs.mkdir(path.dirname(restorePath), { recursive: true });
        await fs.writeFile(restorePath, "stale tab");
      }
    }
    const providerStatePath = path.join(stateDir, "state.json");
    const profileDataPath = path.join(stateDir, "Default", "Local Storage", "leveldb", "LOCK");
    await fs.writeFile(providerStatePath, JSON.stringify({ url: "http://localhost:4321" }));
    await fs.mkdir(path.dirname(profileDataPath), { recursive: true });
    await fs.writeFile(profileDataPath, "");

    await providerTest.clearBrowserSessionRestore(stateDir);

    assert.throws(
      () => providerTest.profilePath(path.resolve(stateDir), ["..", "outside"]),
      /outside profile/,
    );
    for (const restorePath of providerTest.browserSessionRestorePaths(stateDir)) {
      await assert.rejects(fs.access(restorePath), { code: "ENOENT" });
    }
    assert.equal(
      await fs.readFile(providerStatePath, "utf8"),
      JSON.stringify({ url: "http://localhost:4321" }),
    );
    await fs.access(profileDataPath);
  } finally {
    await fs.rm(stateDir, { recursive: true, force: true });
  }
});

function fakeDocument(elements) {
  for (const element of elements) {
    element.ownerDocument = {
      elementFromPoint: () => element,
    };
  }
  return {
    querySelectorAll: () => elements,
  };
}

function fakeElement(tagName, { attributes = {}, innerText = "", textContent = "", isContentEditable = false } = {}) {
  const element = {
    disabled: false,
    id: attributes.id || "",
    innerText,
    isContentEditable,
    name: attributes.name || "",
    ownerDocument: null,
    tagName: tagName.toUpperCase(),
    textContent,
    closest: () => null,
    contains: (other) => other === element,
    getAttribute: (name) => attributes[name] ?? null,
    getAttributeNames: () => Object.keys(attributes),
    getBoundingClientRect: () => ({
      x: 10,
      y: 20,
      left: 10,
      top: 20,
      right: 110,
      bottom: 50,
      width: 100,
      height: 30,
    }),
  };
  return element;
}
