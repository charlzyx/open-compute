import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { posix } from "node:path";
import test from "node:test";
import { parseAst } from "rolldown/parseAst";

function dependencies(source, name) {
  return parseAst(source, { sourceType: "module" }, name).body.flatMap(
    (node) => {
      if (
        node.type !== "ImportDeclaration" &&
        node.type !== "ExportNamedDeclaration" &&
        node.type !== "ExportAllDeclaration"
      )
        return [];
      return node.source ? [node.source.value] : [];
    },
  );
}

function problems(modules) {
  const result = [];
  for (const [name, source] of modules) {
    for (const specifier of dependencies(source, name)) {
      if (
        specifier.startsWith("cloudflare-internal:") ||
        specifier.startsWith("node-internal:")
      ) {
        result.push({ kind: "internal", name, specifier });
        continue;
      }
      if (specifier.startsWith("cloudflare:") || specifier.startsWith("node:"))
        continue;
      const target = specifier.startsWith(".")
        ? posix.normalize(posix.join(posix.dirname(name), specifier))
        : specifier;
      if (!modules.has(target))
        result.push({ kind: "missing", name, specifier, target });
    }
  }
  return result;
}

test("ordinary host module inventories have complete emitted import and export closure", async () => {
  const root = new URL("../../", import.meta.url);
  const config = await readFile(new URL("config.capnp", root), "utf8");
  for (const worker of ["loaderHostWorker", "doHostWorker"]) {
    const start = config.indexOf(`const ${worker} :Workerd.Worker = (`);
    assert.notEqual(start, -1, worker);
    const end = config.indexOf("  bindings = [", start);
    assert.ok(end > start, worker);
    const entries = [
      ...config
        .slice(start, end)
        .matchAll(/\(name = "([^"]+)", esModule = embed "([^"]+)"\)/g),
    ];
    assert.ok(entries.length > 0, worker);
    const modules = new Map();
    for (const [, name, path] of entries) {
      assert.equal(modules.has(name), false, `${worker}: duplicate ${name}`);
      modules.set(name, await readFile(new URL(path, root), "utf8"));
    }
    assert.deepEqual(problems(modules), [], worker);
    const missing = new Map(modules);
    missing.delete("services/websocket-handoff.js");
    assert.ok(problems(missing).some((item) => item.kind === "missing"));
    const internal = new Map(modules);
    internal.set(
      "services/websocket-handoff.js",
      'export { default } from "cloudflare-internal:wrapped-binding";',
    );
    assert.ok(problems(internal).some((item) => item.kind === "internal"));
  }
});
