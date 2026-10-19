import { DurableObject, WorkerEntrypoint } from "cloudflare:workers";
import { execPtyProbe } from "./exec-probe";
import { networkProbe } from "./network";
import { advancedProbe } from "./probes";

interface Env {
  IMAGE: string;
  PROBE_KEY: string;
}

declare global {
  namespace Cloudflare {
    interface GlobalProps {
      mainModule: typeof import("./index");
      durableNamespaces: "SurfaceContainer";
    }
  }
}

function record(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function descriptors(value: object) {
  const result: Record<string, Record<string, boolean | string>> = {};
  for (
    let prototype = value;
    prototype !== null;
    prototype = Object.getPrototypeOf(prototype)
  ) {
    for (const [name, descriptor] of Object.entries(
      Object.getOwnPropertyDescriptors(prototype),
    )) {
      if (name === "constructor" || Object.hasOwn(result, name)) continue;
      result[name] = {
        kind: descriptor.get === undefined ? typeof descriptor.value : "getter",
        enumerable: descriptor.enumerable === true,
        configurable: descriptor.configurable === true,
        writable: descriptor.writable === true,
        setter: descriptor.set !== undefined,
      };
    }
    if (Object.getPrototypeOf(prototype) === Object.prototype) break;
  }
  return result;
}

function errorResult(error: unknown) {
  return error instanceof Error
    ? { name: error.name, message: error.message }
    : { name: typeof error, message: String(error) };
}

export class SurfaceOutbound extends WorkerEntrypoint<Env> {
  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (
      url.hostname !== "surface-egress.invalid" ||
      url.pathname !== "/https-probe" ||
      request.method !== "POST" ||
      request.headers.get("X-Probe-Request") !== "tls-header"
    )
      return new Response("Invalid intercepted request", { status: 400 });
    return new Response(`https:${await request.text()}`, {
      status: 201,
      headers: { "X-Probe-Reply": "tls-ok" },
    });
  }
}

export class SurfaceContainer extends DurableObject<Env> {
  async fetch(request: Request): Promise<Response> {
    const container = this.ctx.container;
    if (container === undefined)
      throw new Error("Container attachment is missing");
    const input: unknown = await request.json();
    if (!record(input) || typeof input.action !== "string")
      return new Response("Invalid probe input", { status: 400 });
    try {
      return Response.json(await this.probe(container, input.action, input));
    } catch (error) {
      return Response.json({ error: errorResult(error) });
    }
  }

  private async probe(
    container: Container,
    action: string,
    input: Record<string, unknown>,
  ) {
    switch (action) {
      case "describe":
        return {
          running: container.running,
          images: container.images,
          descriptors: descriptors(container),
        };
      case "start": {
        container.start({
          image: this.env.IMAGE,
          enableInternet: false,
          entrypoint: [
            "/bin/sh",
            "-c",
            "trap 'exit 0' TERM; while :; do sleep 1; done",
          ],
          env: { PROBE_STARTUP: "startup-only" },
          labels: { purpose: "surface-probe" },
        });
        this.ctx.waitUntil(container.monitor().catch(() => {}));
        const process = await container.exec(["/bin/sh", "-c", "printf ready"]);
        const output = await process.output();
        return {
          running: container.running,
          stdout: new TextDecoder().decode(output.stdout),
          exitCode: output.exitCode,
        };
      }
      case "exec": {
        const process = await container.exec(
          [
            "/bin/sh",
            "-c",
            'cat; printf \':%s:%s\' "$PROBE_STARTUP" "$PROBE_EXEC"; printf err >&2; exit 7',
          ],
          {
            stdin: "pipe",
            stdout: "pipe",
            stderr: "pipe",
            cwd: "/tmp",
            user: "0",
            env: { PROBE_EXEC: "exec-only" },
          },
        );
        if (process.stdin === null) throw new Error("stdin pipe is missing");
        const writer = process.stdin.getWriter();
        await writer.write(new TextEncoder().encode("input"));
        await writer.close();
        const output = await process.output();
        return {
          processDescriptors: descriptors(process),
          outputDescriptors: descriptors(output),
          pidPositive: process.pid > 0,
          isPty: process.isPty,
          exitCode: await process.exitCode,
          stdout: new TextDecoder().decode(output.stdout),
          stderr: new TextDecoder().decode(output.stderr),
          outputExitCode: output.exitCode,
        };
      }
      case "snapshot": {
        const process = await container.exec([
          "/bin/sh",
          "-c",
          "mkdir -p /tmp/probe-snapshot; printf saved > /tmp/probe-snapshot/state; chmod 640 /tmp/probe-snapshot/state",
        ]);
        const output = await process.output();
        if (output.exitCode !== 0)
          throw new Error("Snapshot input preparation failed");
        const snapshot = await container.snapshotContainer({
          name: "surface-checkpoint",
        });
        await this.ctx.storage.put("containerSnapshot", snapshot);
        return snapshot;
      }
      case "restore": {
        if (!record(input.snapshot) || typeof input.snapshot.id !== "string")
          throw new TypeError("A snapshot ID is required");
        container.start({
          containerSnapshot: { id: input.snapshot.id },
          enableInternet: false,
        });
        this.ctx.waitUntil(container.monitor().catch(() => {}));
        const process = await container.exec([
          "/bin/sh",
          "-c",
          "cat /tmp/probe-snapshot/state; stat -c %a /tmp/probe-snapshot/state",
        ]);
        const output = await process.output();
        return {
          stdout: new TextDecoder().decode(output.stdout),
          exitCode: output.exitCode,
          running: container.running,
        };
      }
      case "inactivity-abort":
        await advancedProbe(
          container,
          this.env.IMAGE,
          "inactivity-start",
          input,
        );
        this.ctx.abort("surface-inactivity-reset");
        return { unexpectedAbortReturn: true };
      case "inactivity-disconnect":
        if (!container.running)
          throw new Error("A running container is required");
        await container.setInactivityTimeout(1000);
        this.ctx.abort("surface-inactivity-reset");
        return { unexpectedAbortReturn: true };
      case "inactivity":
        await container.setInactivityTimeout(1000n);
        return { running: container.running };
      case "stop":
        await container.destroy();
        return { running: container.running };
      case "network":
        return networkProbe(
          container,
          this.env.IMAGE,
          this.ctx.exports.SurfaceOutbound({ props: {} }),
        );
      case "exec-pty":
        return execPtyProbe(container);
      default:
        return advancedProbe(container, this.env.IMAGE, action, input);
    }
  }
}

export default {
  async fetch(
    request: Request,
    env: Env,
    ctx: ExecutionContext,
  ): Promise<Response> {
    if (request.headers.get("authorization") !== `Bearer ${env.PROBE_KEY}`)
      return new Response("Unauthorized", { status: 401 });
    if (request.method !== "POST")
      return new Response("Method not allowed", { status: 405 });
    const name = new URL(request.url).pathname.slice(1);
    if (!/^[a-z][a-z0-9-]{0,31}$/.test(name))
      return new Response("Invalid object", { status: 400 });
    try {
      return await ctx.exports.SurfaceContainer.getByName(name).fetch(request);
    } catch (error) {
      return Response.json(
        { dispatchError: errorResult(error) },
        { status: 500 },
      );
    }
  },
} satisfies ExportedHandler<Env>;
