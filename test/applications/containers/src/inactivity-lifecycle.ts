import { DurableObject } from "cloudflare:workers";
import { directoryAction } from "./directory-snapshots";

// This class deliberately has no SDK constructor, monitor(), or waitUntil().
export class RawIdleContainer extends DurableObject {
  async fetch(request: Request): Promise<Response> {
    const native = this.ctx.container;
    if (native === undefined)
      throw new Error("native container attachment is required");
    const url = new URL(request.url);
    if (url.pathname.startsWith("/inactivity/directory/")) {
      url.pathname = url.pathname.replace(
        "/inactivity/directory/",
        "/named/directory/",
      );
      return directoryAction(native, undefined, new Request(url, request));
    }
    if (url.pathname === "/inactivity/start") {
      const duration = url.searchParams.get("duration");
      if (duration !== "1000" && duration !== "120000")
        return new Response("invalid inactivity duration", { status: 400 });
      if (native.running) throw new Error("raw idle object must start fresh");
      native.start({
        enableInternet: false,
        entrypoint: ["python3", "-c", "import time\nwhile True: time.sleep(1)"],
      });
      const ready = await native.exec([
        "python3",
        "-c",
        "print('ready',end='')",
      ]);
      const output = await ready.output();
      if (
        output.exitCode !== 0 ||
        output.stderr.byteLength !== 0 ||
        new TextDecoder().decode(output.stdout) !== "ready"
      )
        throw new Error("raw idle main must be ready before configuration");
      await native.setInactivityTimeout(Number(duration));
      return Response.json({
        objectId: this.ctx.id.toString(),
        running: native.running,
        ready: "ready",
      });
    }
    if (url.pathname === "/inactivity/stop") await native.destroy();
    else if (url.pathname !== "/inactivity/state")
      return new Response("unknown inactivity action", { status: 404 });
    return Response.json({
      objectId: this.ctx.id.toString(),
      info: await native.inspect(),
      running: native.running,
    });
  }
}
