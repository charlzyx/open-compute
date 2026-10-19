import assert from "node:assert/strict";
import test from "node:test";
import { setImmediate } from "node:timers/promises";
import { compileRuntime } from "../../../packages/runtime/tests/compiled-runtime.mjs";

const url = (code) =>
  `data:text/javascript;base64,${Buffer.from(code).toString("base64")}`;
let code = await compileRuntime(
  "../../../test/applications/containers/src/named-images.ts",
);
for (const [name, replacement] of Object.entries({
  "cloudflare:workers":
    "export class DurableObject { constructor(ctx) { this.ctx = ctx; } }",
  "./exec-options-lifecycle":
    "export const execOptionsLifecycle = () => { throw new Error('unexpected exec'); };",
  "./signals-lifecycle":
    "export const signalsLifecycle = () => { throw new Error('unexpected signals'); };",
})) {
  assert.ok(code.includes(JSON.stringify(name)), name);
  code = code.replaceAll(
    JSON.stringify(name),
    JSON.stringify(url(replacement)),
  );
}
const { NamedImageContainer } = await import(url(code));

function fixture(snapshotContainer) {
  const native = {
    running: true,
    images: { base: "base", alternate: "alternate" },
    getTcpPort: (port) => {
      assert.equal(port, 8080);
      return { fetch: async () => new Response("ready") };
    },
    snapshotContainer,
  };
  const phases = [];
  const object = new NamedImageContainer({
    container: native,
    storage: {
      sql: {
        exec(query, phase, value) {
          if (query.startsWith("INSERT INTO snapshot_diagnostic_phase"))
            phases.push({ phase, value });
        },
      },
    },
  });
  const response = object.fetch(new Request("http://fixture/named/snapshot"));
  response.phases = phases;
  return response;
}

test("snapshot response headers precede completion, but its JSON waits for the real handle", async () => {
  let complete;
  const pending = new Promise((resolve) => {
    complete = resolve;
  });
  let calls = 0;
  const responsePromise = fixture((options) => {
    assert.deepEqual(options, { name: "fixture-saved" });
    calls++;
    return pending;
  });
  const response = await Promise.race([
    responsePromise,
    setImmediate().then(() => null),
  ]);
  if (response === null) {
    complete({ id: "cleanup", size: 1 });
    await responsePromise;
    assert.fail("snapshot completion withheld response headers");
  }
  assert.deepEqual(responsePromise.phases, [
    { phase: "CAPTURE_REQUESTED", value: undefined },
  ]);
  assert.equal(response.status, 200);
  assert.equal(response.headers.get("content-type"), "application/json");
  let settled = false;
  const body = response.json().finally(() => {
    settled = true;
  });
  await setImmediate();
  assert.equal(
    settled,
    false,
    "a pending snapshot must not produce a successful handle",
  );
  const handle = { id: "completed-snapshot", name: "fixture-saved", size: 123 };
  complete(handle);
  assert.deepEqual(await body, handle);
  assert.equal(calls, 1);
  assert.deepEqual(responsePromise.phases, [
    { phase: "CAPTURE_REQUESTED", value: undefined },
    {
      phase: "CAPTURE_RETURNED",
      value: new TextEncoder().encode(JSON.stringify(handle)).byteLength,
    },
  ]);
});

for (const synchronous of [false, true]) {
  test(`snapshot ${synchronous ? "synchronous" : "asynchronous"} failure rejects its body without exposing the exception`, async () => {
    const responsePromise = fixture(() => {
      const error = new Error("private snapshot exception");
      if (synchronous) throw error;
      return Promise.reject(error);
    });
    const response = await responsePromise;
    await assert.rejects(response.json(), {
      message: "snapshot fixture capture failed",
    });
    assert.deepEqual(responsePromise.phases, [
      { phase: "CAPTURE_REQUESTED", value: undefined },
      { phase: "CAPTURE_REJECTED", value: null },
    ]);
  });
}
