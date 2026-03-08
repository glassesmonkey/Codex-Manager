import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const appsRoot = path.resolve(__dirname, "..");
const indexHtml = fs.readFileSync(path.join(appsRoot, "index.html"), "utf8");
const mainJs = fs.readFileSync(path.join(appsRoot, "src", "main.js"), "utf8");

test("route strategy UI includes expiry-first option", () => {
  assert.ok(indexHtml.includes('<option value="expiry_first">'), "missing expiry_first option");
  assert.ok(indexHtml.includes("7天到期优先"), "missing expiry_first label");
});

test("route strategy payload parsing preserves expiry-first fallback", () => {
  assert.ok(
    mainJs.includes("function resolveRouteStrategyFromPayload(payload, fallback = ROUTE_STRATEGY_ORDERED)"),
    "missing route strategy payload fallback",
  );
  assert.ok(
    mainJs.includes('return normalizeRouteStrategy(fallback);'),
    "missing fallback preservation logic",
  );
});
