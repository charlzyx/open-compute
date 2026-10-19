import { bindings, defineConfig, defineContainer, exports } from "cf/config";
import { preparedImage } from "./build-input.ts";

const container = defineContainer({
  name: "containers-fixture",
  image: {
    reference: preparedImage(process.env.OPEN_COMPUTE_TEST_CONTAINERS_INPUT),
  },
  instanceType: "lite",
  maxInstances: 2,
});
const idleContainer = defineContainer({
  name: "containers-idle-fixture",
  image: {
    reference: preparedImage(process.env.OPEN_COMPUTE_TEST_CONTAINERS_INPUT),
  },
  instanceType: "lite",
  maxInstances: 2,
});

const namedContainer = defineContainer({
  name: "containers-named-fixture",
  schedulingPolicy: "durable-object",
  images: {
    base: {
      reference: preparedImage(
        process.env.OPEN_COMPUTE_TEST_CONTAINERS_INPUT,
        0,
      ),
    },
    alternate: {
      reference: preparedImage(
        process.env.OPEN_COMPUTE_TEST_CONTAINERS_INPUT,
        1,
      ),
    },
  },
});

export default defineConfig({
  worker: {
    name: "containers-fixture",
    entrypoint: "./src/index.ts",
    compatibilityDate: "2026-09-08",
    workersDev: false,
    exports: {
      default: exports.worker(),
      ContainerProxy: exports.worker(),
      FixtureContainer: exports.durableObject({ storage: "sqlite", container }),
      NamedImageContainer: exports.durableObject({
        storage: "sqlite",
        container: namedContainer,
      }),
      RawIdleContainer: exports.durableObject({
        storage: "sqlite",
        container: idleContainer,
      }),
    },
    env: {
      CODE_REVISION: bindings.text("one"),
      OBJECTS: bindings.durableObject({
        worker: "containers-fixture",
        exportName: "FixtureContainer",
      }),
      NAMED_OBJECTS: bindings.durableObject({
        worker: "containers-fixture",
        exportName: "NamedImageContainer",
      }),
      IDLE_OBJECTS: bindings.durableObject({
        worker: "containers-fixture",
        exportName: "RawIdleContainer",
      }),
    },
  },
  containers: [container, idleContainer, namedContainer],
});
