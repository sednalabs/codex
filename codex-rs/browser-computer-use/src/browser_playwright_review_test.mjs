import assert from "node:assert/strict";
import test from "node:test";
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
