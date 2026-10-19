import { bindings, defineConfig, defineContainer, exports } from "cf/config";

const name = process.env.OPEN_COMPUTE_CONTAINER_DIFFERENTIAL_NAME;
const image = process.env.OPEN_COMPUTE_CONTAINER_DIFFERENTIAL_IMAGE;
if (name === undefined || !/^oc-p23-full-[a-z0-9-]{1,30}$/.test(name))
  throw new Error("An explicit isolated differential Worker name is required");
if (image === undefined || image.length === 0)
  throw new Error("An explicit differential image is required");

const container = defineContainer({
  name,
  schedulingPolicy: "durable-object",
});

export default defineConfig({
  worker: {
    name,
    entrypoint: "./src/index.ts",
    compatibilityDate: "2026-09-08",
    workersDev: true,
    exports: {
      default: exports.worker(),
      SurfaceOutbound: exports.worker(),
      SurfaceContainer: exports.durableObject({ storage: "sqlite", container }),
    },
    env: {
      IMAGE: bindings.text(image),
      PROBE_KEY: bindings.secret(),
    },
  },
  containers: [container],
});
