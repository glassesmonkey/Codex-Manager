import test from "node:test";
import assert from "node:assert/strict";

import { formatResetLabel } from "../utils/format.js";
import { buildProgressLine } from "./dashboard-progress.js";

class FakeClassList {
  constructor() {
    this.tokens = new Set();
  }

  setFromString(value) {
    this.tokens.clear();
    for (const token of String(value || "").split(/\s+/)) {
      if (token) this.tokens.add(token);
    }
  }

  add(...tokens) {
    for (const token of tokens) {
      if (token) this.tokens.add(token);
    }
  }

  contains(token) {
    return this.tokens.has(token);
  }
}

class FakeElement {
  constructor(tagName = "div") {
    this.tagName = tagName.toUpperCase();
    this.children = [];
    this.classList = new FakeClassList();
    this.style = {};
    this._textContent = "";
    this._className = "";
  }

  set className(value) {
    this._className = String(value || "");
    this.classList.setFromString(this._className);
  }

  get className() {
    return this._className;
  }

  set textContent(value) {
    this._textContent = String(value ?? "");
    this.children = [];
  }

  get textContent() {
    if (this.children.length === 0) {
      return this._textContent;
    }
    return this._textContent + this.children.map((child) => child.textContent).join("");
  }

  appendChild(node) {
    this.children.push(node);
    return node;
  }
}

class FakeDocument {
  createElement(tagName) {
    return new FakeElement(tagName);
  }
}

test("buildProgressLine includes reset hint when timestamp is available", () => {
  const previousDocument = globalThis.document;
  globalThis.document = new FakeDocument();
  const resetsAt = 1762812000;

  try {
    const wrapped = buildProgressLine("5小时用量", 18, resetsAt, true);
    assert.equal(wrapped.children.length, 2);
    assert.equal(wrapped.children[0].children[0].textContent, "5小时用量 82%");
    assert.equal(wrapped.children[0].classList.contains("secondary"), true);
    assert.equal(wrapped.children[1].textContent, formatResetLabel(resetsAt));
  } finally {
    globalThis.document = previousDocument;
  }
});
