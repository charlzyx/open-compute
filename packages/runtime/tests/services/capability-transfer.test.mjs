import assert from "node:assert/strict";
import test from "node:test";
import { compileRuntime, moduleUrl } from "../compiled-runtime.mjs";

const cloudflare = moduleUrl("export const waitUntil = () => {};");
const errors = moduleUrl("export const bindingError = code => Error(code);");
const deadline = moduleUrl(
  "export const serviceDeadline = action => action();",
);
const { activateServiceCapabilities } = await import(
  moduleUrl(
    await compileRuntime("services/capability-transfer.ts", {
      "cloudflare:workers": cloudflare,
      "../loader/shared.js": errors,
      "./deadline.js": deadline,
    }),
  )
);

test("capability activation uses its owning transport and retains repeated aliases once", async () => {
  const raw = Object.defineProperty({}, "activate", {
    get() {
      throw Error("raw private member");
    },
  });
  let released = 0;
  const retention = {
    release() {
      released++;
    },
  };
  const envelope = {
    __openComputeServiceCapability: 1,
    kind: "function",
    handle: raw,
  };
  const calls = [];
  await activateServiceCapabilities(
    [envelope, envelope],
    async () => retention,
    Date.now() + 1000,
    (source) => {
      assert.equal(source, raw);
      const transport = {
        activate(value) {
          assert.equal(this, transport);
          calls.push(value);
        },
      };
      return transport;
    },
  );
  assert.deepEqual(calls, [retention]);
  assert.equal(released, 0);
});

test("a failed owning activation releases its admitted retention and keeps the original error", async () => {
  let released = 0;
  const failure = Error("activation failed");
  const retention = {
    release() {
      released++;
    },
  };
  const envelope = {
    __openComputeServiceCapability: 1,
    kind: "target",
    handle: {},
  };
  await assert.rejects(
    activateServiceCapabilities(
      envelope,
      async () => retention,
      Date.now() + 1000,
      () => ({
        activate() {
          throw failure;
        },
      }),
    ),
    (error) => error === failure,
  );
  assert.equal(released, 1);
});
