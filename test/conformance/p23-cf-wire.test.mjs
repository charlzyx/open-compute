import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readdirSync, readFileSync } from "node:fs";
import test from "node:test";

const cfRoot = new URL("../../node_modules/cf/", import.meta.url);
const cfVersion = JSON.parse(
  readFileSync(new URL("package.json", cfRoot)),
).version;
const dist = new URL("dist/", cfRoot);
const deployModule = readdirSync(dist).find((name) => {
  if (!name.startsWith("deploy-context-") || !name.endsWith(".mjs"))
    return false;
  return readFileSync(new URL(name, dist), "utf8").includes("deployContainers");
});
assert.ok(deployModule, "the pinned cf Container deploy producer is missing");
const source = new URL(deployModule, dist);
const sourceSha256 = createHash("sha256")
  .update(readFileSync(source))
  .digest("hex");
const cf = await import(source.href);
function official(name) {
  const exported = Object.values(cf).find((value) => value?.name === name);
  assert.ok(exported, `pinned cf export ${name} is missing`);
  return exported;
}
const deploy = official("deployContainers");
const configure = official("configureOpenAPIForContainerPull");
const initialize = official("initContainersSharedContext");
const contract = JSON.parse(
  readFileSync(new URL("fixtures/containers-cf-wire.json", import.meta.url)),
);
const account = "00000000000000000000000000000001";
const namespace = "00000000000000000000000000000002";
const base = "https://p23-wire.invalid/client/v4";
const oldImage = `registry.example/app@sha256:${"a".repeat(64)}`;
const newImage = `registry.example/app@sha256:${"b".repeat(64)}`;
const container = {
  name: "container-app",
  class_name: "ContainerClass",
  max_instances: 4,
  instance_type: "lite",
  scheduling_policy: "default",
  constraints: { tiers: [1, 2] },
  rollout_active_grace_period: 0,
  rollout_kind: "full_auto",
  rollout_step_percentage: [10, 100],
  observability: { logs_enabled: true },
};
const worker = {
  containers: [container],
  durable_objects: {
    bindings: [{ name: "CONTAINER", class_name: container.class_name }],
  },
};

async function capture({
  existing,
  image = oldImage,
  config = container,
  failRollout = false,
}) {
  const requests = [];
  const originalFetch = globalThis.fetch;
  configure(account, "fixture-token", base);
  initialize({
    logger: {
      debug() {},
      debugWithSanitization() {},
      log() {},
      warn() {},
      error() {},
    },
    async fetchResult(_config, path) {
      assert.equal(
        path,
        `/accounts/${account}/workers/scripts/container-worker/versions/worker-version`,
      );
      requests.push({ method: "GET", path });
      return {
        resources: {
          bindings: [
            {
              type: "durable_object_namespace",
              class_name: container.class_name,
              namespace_id: namespace,
            },
          ],
        },
      };
    },
  });
  globalThis.fetch = async (url, options) => {
    const parsed = new URL(url);
    assert.equal(
      parsed.origin,
      new URL(base).origin,
      "the fixture must never use a real network endpoint",
    );
    const request = {
      method: options.method,
      path: parsed.pathname + parsed.search,
    };
    if (options.body !== undefined) request.body = JSON.parse(options.body);
    requests.push(request);
    let result;
    if (options.method === "GET")
      result = existing === undefined ? [] : [existing];
    else if (
      options.method === "POST" &&
      parsed.pathname.endsWith("/applications")
    ) {
      result = { ...request.body, id: "application-id" };
    } else result = { id: "rollout-id" };
    if (failRollout && parsed.pathname.endsWith("/rollouts")) {
      return Response.json(
        {
          success: false,
          errors: [{ code: 1001, message: "fixture rollout denied" }],
        },
        { status: 400 },
      );
    }
    return Response.json({ success: true, errors: [], messages: [], result });
  };
  try {
    await deploy(
      { ...worker, containers: [config] },
      [{ container: config, imageRef: { remoteDigest: image } }],
      {
        versionId: "worker-version",
        accountId: account,
        scriptName: "container-worker",
      },
    );
    return requests;
  } finally {
    globalThis.fetch = originalFetch;
  }
}

test("P23 pins the actual cf standard Container deploy producer", () => {
  assert.equal(cfVersion, contract.cfVersion);
  assert.equal(sourceSha256, contract.deployModuleSha256);
});

test("cf creates an application only after resolving the uploaded Worker Version", async () => {
  const requests = await capture({});
  assert.deepEqual(requests, contract.create);
});

test("cf modifies an application before creating a separate progressive rollout", async () => {
  const requests = await capture({
    existing: contract.application,
    image: newImage,
  });
  assert.deepEqual(requests, contract.update);
});

test("cf skips application mutation for an unchanged effective configuration", async () => {
  const requests = await capture({ existing: contract.application });
  assert.deepEqual(requests, contract.create.slice(0, 2));
});

test("cf rollout none prevents both configuration mutation and rollout", async () => {
  const requests = await capture({
    existing: contract.application,
    image: newImage,
    config: { ...container, rollout_kind: "none" },
  });
  assert.deepEqual(requests, contract.create.slice(0, 2));
});

test("cf rejects an existing application owned by a different namespace", async () => {
  await assert.rejects(
    capture({
      existing: {
        ...contract.application,
        durable_objects: { namespace_id: "other-namespace" },
      },
    }),
    /different durable object namespace/,
  );
});

test("cf propagates rollout failure after the successful application mutation", async () => {
  await assert.rejects(
    capture({
      existing: contract.application,
      image: newImage,
      failRollout: true,
    }),
    /fixture rollout denied/,
  );
});
