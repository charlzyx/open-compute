import { registerForwarding } from "cloudflare-internal:open-compute-forwarding";
import wrapped, {
  type PublicTransport,
} from "cloudflare-internal:wrapped-binding";
import {
  createRpcTransport,
  disposeRpcResult,
} from "../bindings/rpc-transport.js";
import { createCacheRuntime } from "../cache/facade.js";
import { createDurableObjectNamespace } from "../durable-objects/namespace.js";
import { registerOutputPublisher } from "../durable-objects/output-gate.js";
import {
  createDurableObjectStubPolicy as doStubPolicy,
  durableObjectChannelCallbacks,
} from "../durable-objects/stub.js";
import { QueuePublisher } from "../queues/publisher.js";
import {
  serviceChannelCallbacks,
  createServiceBinding as servicePolicy,
} from "../services/facade.js";
import { triggerWorkflowSchedule, WorkflowImpl } from "../workflows/facade.js";
import { runWorkflow, validateWorkflowClass } from "../workflows/runner.js";
import { PRIVATE_POLICY, type WorkerPolicy } from "./policy.js";
import { wrapDurableObject } from "./wrappers/durable-object.js";
import { createLoopbackEntrypoint } from "./wrappers/loopback.js";
import {
  createPolicyChannelCallbacks,
  validationHandler,
  wrapDefault,
  wrapDefaultService,
  wrapEntrypoint,
  type Environment,
} from "./wrappers/runtime.js";
import { createWorkflowEntrypoint } from "./wrappers/workflow.js";

export { registerServiceBinding } from "../services/facade.js";

const createNativeServiceStub = wrapped.createServiceRpcStub.bind(wrapped);
const copyPrivateTransport = wrapped.createPrivateTransport.bind(wrapped);
const createPrivateTransport = (transport: unknown): object =>
  createRpcTransport(copyPrivateTransport(transport), "transport");
const nativeFreeze = Object.freeze;
const validatePublicTransport = wrapped.validatePublicTransport.bind(wrapped);
const fetchPublicTransport = wrapped.fetchPublicTransport.bind(wrapped);
const connectPublicTransport = wrapped.connectPublicTransport.bind(wrapped);
const admitSubrequest = wrapped.admitSubrequest.bind(wrapped);

function createPublicTransport(transport: unknown): PublicTransport {
  validatePublicTransport(transport);
  return nativeFreeze({
    fetch: (request: Request) => fetchPublicTransport(transport, request),
    connect: (address: SocketAddress | string, options?: SocketOptions) =>
      connectPublicTransport(transport, address, options),
  });
}
const createNativeId = wrapped.createDurableObjectId.bind(wrapped);
const doFactories = {
  createId: createNativeId,
  createNamespace: wrapped.createDurableObjectNamespace.bind(wrapped),
  createStub: wrapped.createDurableObjectStub.bind(wrapped),
  createRpcStub: createNativeServiceStub,
  isRpcStub: wrapped.isRpcStub.bind(wrapped),
  createPrivateTransport,
  createPublicTransport,
  createRpcTransport: (transport: object) =>
    createRpcTransport(transport, "transport"),
  createUserRpcTransport: (transport: object) =>
    createRpcTransport(transport, "user"),
  disposeRpcResult,
};
const installQueuePolicy = wrapped.installQueuePolicy.bind(wrapped);
const queueCodec = { decodeV8: wrapped.decodeQueueV8.bind(wrapped) };

/** Give the private Service policy the native factory from this INTERNAL closure. */
export function createServiceBinding(env: { fetcher: unknown }): object {
  return servicePolicy(
    createNativeServiceStub,
    createPrivateTransport(env.fetcher),
    admitSubrequest,
    createPublicTransport(env.fetcher),
  );
}

/** Native channel extraction can only dispatch through an issued private binding policy. */
export function createBindingChannelCallbacks(policy: object) {
  const callbacks =
    serviceChannelCallbacks(policy) ?? durableObjectChannelCallbacks(policy);
  if (callbacks === null) throw new TypeError("SERVICE_BINDING_DENIED");
  return createPolicyChannelCallbacks(callbacks);
}

/** Native DO transfer retains immutable raw authority and the authenticated object identity. */
export function createDurableObjectStubPolicy(env: {
  fetcher: unknown;
  id: unknown;
}): object {
  return doStubPolicy(env, doFactories);
}

interface BindingFactory {
  readonly names: readonly string[];
  readonly create: (
    raw: unknown,
    durableObject: boolean,
    name: string,
  ) => object;
}

const nativeGet = Reflect.get;
const nativeDefine = Object.defineProperty;
const nativeEntries = Object.entries;
const nativeHasOwn = Object.hasOwn;

/** Install binding policies and remaining JS capabilities before tenant evaluation. */
function materializeEnvironment(
  publicEnvironment: Environment,
  privateEnvironment: Environment,
  policy: WorkerPolicy,
): void {
  if (policy.validation) return;
  if (policy.durableObject) {
    privateEnvironment.__OPEN_COMPUTE_PRIVATE_ALARM_INDEX =
      createPrivateTransport(
        privateEnvironment.__OPEN_COMPUTE_PRIVATE_ALARM_INDEX,
      );
  }
  const factories: BindingFactory[] = [
    {
      names: policy.bindings
        .filter((binding) => binding.kind === "do_namespace")
        .map((binding) => binding.name),
      create: (raw) => createDurableObjectNamespace(raw, doFactories),
    },
    {
      names: policy.bindings
        .filter((binding) => binding.kind === "workflow")
        .map((binding) => binding.name),
      create: (raw, durableObject, name) =>
        new WorkflowImpl(raw, durableObject, name),
    },
  ];
  for (const binding of policy.bindings) {
    if (binding.kind !== "queue_producer") continue;
    const queue: unknown = nativeGet(publicEnvironment, binding.name);
    if (queue === null || typeof queue !== "object")
      throw new TypeError("QUEUE_INVARIANT_VIOLATION");
    const publisher = new QueuePublisher(
      queue,
      nativeGet(privateEnvironment, binding.name),
      policy.durableObject,
      binding.name,
      queueCodec,
    );
    installQueuePolicy(queue, {
      send: publisher.send.bind(publisher),
      sendBatch: publisher.sendBatch.bind(publisher),
      metrics: publisher.metrics.bind(publisher),
    });
  }
  for (const factory of factories) {
    for (const name of factory.names) {
      const value = factory.create(
        nativeGet(privateEnvironment, name),
        policy.durableObject,
        name,
      );
      if (value instanceof WorkflowImpl) registerOutputPublisher(value, value);
      nativeDefine(publicEnvironment, name, {
        value,
        enumerable: true,
        configurable: true,
        writable: true,
      });
    }
  }
  registerForwarding(publicEnvironment, privateEnvironment, policy);
}

/** The native loader invokes this factory before evaluating the original tenant main. */
export default function hostPolicy(
  publicEnvironment: Environment,
  privateEnvironment: Environment,
): (namespace: Environment) => Environment {
  const value: unknown = nativeGet(privateEnvironment, PRIVATE_POLICY);
  if (value === null || typeof value !== "object" || Array.isArray(value))
    throw new TypeError("VERSION_INVARIANT_VIOLATION");
  const policy = value as WorkerPolicy;
  materializeEnvironment(publicEnvironment, privateEnvironment, policy);
  const cache = createCacheRuntime(
    policy.automaticCacheEnabled,
    policy.cacheFailOpen,
    privateEnvironment,
    policy.entrypointName ?? "default",
  );
  return (namespace) => {
    const output: Environment = {};
    for (const [name, value] of nativeEntries(namespace))
      nativeDefine(output, name, {
        value,
        enumerable: true,
        configurable: true,
        writable: true,
      });
    const python: unknown = nativeGet(namespace, "pythonEntrypoints");
    const pythonEntrypoints: Environment | undefined =
      python !== null && typeof python === "object" ? { ...python } : undefined;
    const tenant =
      pythonEntrypoints === undefined
        ? output
        : { ...output, ...pythonEntrypoints };
    const selected = policy.entrypointName ?? "default";
    const assign = (name: string, value: unknown) => {
      if (
        pythonEntrypoints !== undefined &&
        nativeHasOwn(pythonEntrypoints, name)
      )
        pythonEntrypoints[name] = value;
      else
        nativeDefine(output, name, {
          value,
          enumerable: true,
          configurable: true,
          writable: true,
        });
    };
    if (policy.validation && !policy.workflow) {
      output.default = validationHandler(
        tenant,
        selected,
        policy.durableObject,
      );
      return output;
    }
    const alreadyWrapped = [
      ...policy.automaticCacheEntrypoints,
      ...(selected === "default" ? [] : [selected]),
    ];
    output.__OpenComputeLoopbackService = createLoopbackEntrypoint(
      tenant,
      wrapEntrypoint,
      alreadyWrapped,
    );
    if (policy.workflow) {
      output.__OpenComputeWorkflow = createWorkflowEntrypoint(
        tenant[selected],
        runWorkflow,
        validateWorkflowClass,
        cache,
      );
    } else if (policy.durableObject) {
      assign(
        selected,
        wrapDurableObject(
          tenant[selected],
          privateEnvironment,
          selected,
          cache,
        ),
      );
    } else if (selected !== "default") {
      assign(selected, wrapEntrypoint(tenant[selected], selected, cache));
    } else {
      for (const name of policy.automaticCacheEntrypoints)
        assign(
          name,
          wrapEntrypoint(
            tenant[name],
            name,
            createCacheRuntime(
              true,
              policy.cacheFailOpen,
              privateEnvironment,
              name,
            ),
          ),
        );
    }
    if (!policy.durableObject && !policy.workflow)
      output.__OpenComputeDefaultService = wrapDefaultService(
        tenant.default,
        cache,
      );
    if (
      nativeHasOwn(tenant, "default") &&
      !(policy.durableObject && selected === "default")
    ) {
      const scheduled = policy.scheduledTargets.some(
        (target) => target.workflowBindings.length > 0,
      )
        ? { targets: policy.scheduledTargets, trigger: triggerWorkflowSchedule }
        : undefined;
      output.default = wrapDefault(tenant.default, cache, scheduled);
    }
    if (pythonEntrypoints !== undefined)
      output.pythonEntrypoints = pythonEntrypoints;
    return output;
  };
}
