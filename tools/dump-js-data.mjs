// Dump the pure-data exports of the upstream JS 9router into crates/nr-data/data/.
// Re-run after merging upstream to pick up new providers/models/constants:
//   node tools/dump-js-data.mjs ../9router
// Functions can't be serialized: each output file lists them under "functions"
// so the Rust port knows which logic must be hand-written.
import fs from "node:fs";
import path from "node:path";
import { register } from "node:module";
import { pathToFileURL, fileURLToPath } from "node:url";

const SRC = path.resolve(process.argv[2] || "../9router");
const OUT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../crates/nr-data/data");

// Resolve the Next.js "@/" and "open-sse" aliases and extensionless imports like the bundler does.
const srcUrl = pathToFileURL(path.join(SRC, "src")).href;
const rootUrl = pathToFileURL(SRC).href;
register("data:text/javascript," + encodeURIComponent(`
export async function resolve(spec, ctx, next) {
  if (spec.startsWith("@/")) spec = ${JSON.stringify(srcUrl)} + "/" + spec.slice(2);
  else if (spec === "open-sse" || spec.startsWith("open-sse/")) spec = ${JSON.stringify(rootUrl)} + "/" + (spec === "open-sse" ? "open-sse/index.js" : spec);
  try { return await next(spec, ctx); } catch (e) {
    if (!/\\.[cm]?js$/.test(spec) && (spec.startsWith(".") || spec.startsWith("file:"))) {
      for (const ext of [".js", "/index.js"]) { try { return await next(spec + ext, ctx); } catch {} }
    }
    throw e;
  }
}`));

// Modules whose exports are (mostly) static data.
const GROUPS = [
  "open-sse/config",
  "open-sse/translator/schema",
  "open-sse/providers",
  "open-sse/providers/models",
  "src/shared/constants",
];
const EXTRA = ["open-sse/providers/registry/index.js"];
// Pure re-export aggregators: their data already lands via the modules they re-export.
const SKIP = new Set(["src/shared/constants/index.js", "src/shared/constants/config.js", "open-sse/translator/schema/index.js"]);

function toJson(value, fns, at, seen = new WeakSet()) {
  if (typeof value === "function") { fns.push(at); return undefined; }
  if (value instanceof RegExp) return { __regex: value.source, flags: value.flags };
  if (value instanceof Map) return Object.fromEntries([...value].map(([k, v]) => [String(k), toJson(v, fns, `${at}.${k}`, seen)]));
  if (value instanceof Set) return [...value].map((v, i) => toJson(v, fns, `${at}[${i}]`, seen));
  if (typeof value === "bigint") return value.toString();
  if (value && typeof value === "object") {
    if (seen.has(value)) return "__circular__";
    seen.add(value);
    const out = Array.isArray(value) ? [] : {};
    for (const [k, v] of Object.entries(value)) {
      const j = toJson(v, fns, `${at}.${k}`, seen);
      if (j !== undefined) out[k] = j;
    }
    seen.delete(value);
    return out;
  }
  return value;
}

const files = [
  ...GROUPS.flatMap((g) => fs.readdirSync(path.join(SRC, g)).filter((f) => f.endsWith(".js")).map((f) => `${g}/${f}`)),
  ...EXTRA,
].filter((f) => !SKIP.has(f));

let ok = 0;
const failed = [];
for (const rel of files) {
  try {
    const mod = await import(pathToFileURL(path.join(SRC, rel)).href);
    const functions = [];
    const exports = {};
    for (const [name, value] of Object.entries(mod)) {
      const j = toJson(value, functions, name);
      if (j !== undefined) exports[name] = j;
    }
    const dest = path.join(OUT, rel.replace(/\.js$/, ".json"));
    fs.mkdirSync(path.dirname(dest), { recursive: true });
    fs.writeFileSync(dest, JSON.stringify({ source: rel, exports, functions }, null, 1) + "\n");
    ok++;
  } catch (e) {
    failed.push(`${rel}: ${e.message.split("\n")[0]}`);
  }
}
console.log(`dumped ${ok}/${files.length} modules to ${OUT}`);
if (failed.length) console.log("failed:\n  " + failed.join("\n  "));
