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
  "./exec-users-lifecycle":
    "export const execUsersLifecycle = () => { throw new Error('unexpected exec user probe'); };",
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

function fixture(snapshotContainer, path = "/named/snapshot") {
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
  const response = object.fetch(new Request(`http://fixture${path}`));
  response.phases = phases;
  return response;
}

test("default snapshot capture calls the native method with no arguments", async () => {
  let calls = 0;
  const handle = { id: "default-snapshot", size: 123 };
  const response = await fixture((...args) => {
    assert.equal(args.length, 0);
    calls++;
    return Promise.resolve(handle);
  }, "/named/snapshot-default");
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), handle);
  assert.equal(calls, 1);
});

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

let mainCode = await compileRuntime(
  "../../../test/applications/containers/src/index.ts",
);
for (const [name, replacement] of Object.entries({
  "@cloudflare/containers":
    "export class Container { constructor(ctx,env) { this.ctx=ctx; this.env=env; } async startAndWaitForPorts() {} } export class ContainerProxy {}",
  "./exec-lifecycle": "export const execLifecycle = () => {};",
  "./exec-users-lifecycle": "export const execUsersLifecycle = () => {};",
  "./inactivity-lifecycle": "export class RawIdleContainer {}",
  "./named-images": "export class NamedImageContainer {}",
  "./network-lifecycle": "export const networkLifecycle = () => {};",
})) {
  assert.ok(mainCode.includes(JSON.stringify(name)), name);
  mainCode = mainCode.replaceAll(
    JSON.stringify(name),
    JSON.stringify(url(replacement)),
  );
}
const { FixtureContainer } = await import(url(mainCode));

for (const [message, category] of [
  ["Container operation failed.", "CONTAINER_OPERATION_FAILED"],
  ["The Container is not running.", "CONTAINER_NOT_RUNNING"],
  ["Container execution authority changed.", "CONTAINER_AUTHORITY_CHANGED"],
  ["Container port connection closed unexpectedly.", "CONTAINER_PORT_CLOSED"],
  ["Container port connection read aborted.", "CONTAINER_PORT_READ_ABORTED"],
  ["private unexpected error with credentials and topology", null],
]) {
  test(`main HTTP diagnostic exposes only fixed category ${category}`, async () => {
    const object = new FixtureContainer(
      {
        id: { equals: () => false },
        storage: { sql: { exec() {} } },
        container: {
          getTcpPort: () => ({
            fetch: async () => {
              throw new Error(message);
            },
          }),
        },
      },
      { OBJECTS: { idFromName: () => null } },
    );
    const response = await object.fetch(new Request("http://fixture/"));
    assert.equal(response.status, 500);
    const body = await response.json();
    assert.equal(body.phase, "main-http");
    assert.equal(body.nativeErrorCode, category);
    assert.ok(!JSON.stringify(body).includes(message));
  });
}
