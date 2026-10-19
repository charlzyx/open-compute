function failure(error: unknown) {
  return error instanceof Error
    ? {
        name: error.name,
        message: error.message,
        exitCode: Reflect.get(error, "exitCode"),
      }
    : { name: typeof error, message: String(error) };
}

async function output(
  container: Container,
  script: string,
  options?: ContainerExecOptions,
) {
  const process = await container.exec(["/bin/sh", "-c", script], options);
  const result = await process.output();
  return {
    stdout: new TextDecoder().decode(result.stdout),
    stderr: new TextDecoder().decode(result.stderr),
    exitCode: result.exitCode,
    isPty: process.isPty,
    stderrNull: process.stderr === null,
  };
}

async function signalOutput(container: Container, script: string) {
  const execution = new AbortController();
  const timer = new AbortController();
  try {
    return await Promise.race([
      output(container, script, { signal: execution.signal }).then(
        (value) => ({ outcome: "completed" as const, value }),
        (error: unknown) => ({
          outcome: "rejected" as const,
          error: failure(error),
        }),
      ),
      scheduler.wait(10_000, { signal: timer.signal }).then(() => {
        execution.abort();
        return { outcome: "pending" as const };
      }),
    ]);
  } finally {
    timer.abort();
  }
}

async function start(container: Container, image: string) {
  container.start({
    image,
    enableInternet: false,
    entrypoint: [
      "/bin/sh",
      "-c",
      "trap 'exit 0' TERM; while :; do sleep 1; done",
    ],
    labels: { purpose: "extended-surface-probe" },
  });
  return output(container, "printf ready");
}

export async function advancedProbe(
  container: Container,
  image: string,
  action: string,
  input: Record<string, unknown>,
) {
  switch (action) {
    case "inspect":
      return {
        running: container.running,
        info: await container.inspect(),
        images: container.images,
      };
    case "start-inspect":
      return {
        ready: await start(container, image),
        info: await container.inspect(),
        images: container.images,
      };
    case "exec-capabilities":
      return output(
        container,
        "command -v bash; command -v node; /bin/sh -c 'trap : 34'; printf 'shellTrapExit=%s\\n' \"$?\"",
      );
    case "signal-trap-capability":
      return signalOutput(
        container,
        `command -v bash; test -x /bin/bash; printf 'binBashExecutable=%s\\n' "$?"; test /bin/bash -ef /usr/bin/bash; printf 'bashSameFile=%s\\n' "$?"; /usr/bin/bash -c 'trap "printf selfTrap34; exit 23" 34 || exit 90; kill -34 $$; exit 91'`,
      );
    case "signal": {
      const signo = input.signo ?? 10;
      if (
        typeof signo !== "number" ||
        !Number.isInteger(signo) ||
        signo < 1 ||
        signo > 64
      )
        throw new TypeError("A signal from 1 to 64 is required");
      if (
        input.phase !== "prepare" &&
        input.phase !== "send" &&
        input.phase !== "direct" &&
        input.phase !== "observe"
      )
        throw new TypeError("A signal phase is required");
      const marker =
        input.phase === "prepare"
          ? `/tmp/oc-signal-${crypto.randomUUID()}`
          : input.marker;
      if (
        typeof marker !== "string" ||
        !/^\/tmp\/oc-signal-[0-9a-f-]{36}$/.test(marker)
      )
        throw new TypeError("A signal probe marker is required");
      if (input.phase === "prepare") {
        container.start({
          image,
          enableInternet: false,
          entrypoint: [
            "/usr/bin/bash",
            "-c",
            `trap 'printf trapped > ${marker}; exit 23' ${signo} || exit 90; printf ready > ${marker}; while :; do sleep 1; done`,
          ],
        });
        return {
          phase: "prepare",
          signo,
          marker,
          readiness: await signalOutput(
            container,
            `attempt=0; while [ "$attempt" -lt 100 ]; do if [ "$(cat ${marker} 2>/dev/null)" = ready ]; then printf 'ready\\n'; awk '/^Sig(Blk|Ign|Cgt):/ {print}' /proc/1/status; exit 0; fi; attempt=$((attempt+1)); sleep 0.05; done; exit 1`,
          ),
          running: container.running,
        };
      }
      const monitor = container.monitor().then(
        () => ({ outcome: "resolved" }),
        (error: unknown) => ({ outcome: "rejected", error: failure(error) }),
      );
      let signalReturned = false;
      let signalError;
      let directSend;
      if (input.phase === "send" || input.phase === "direct") {
        try {
          if (input.phase === "direct")
            directSend = await signalOutput(container, `kill -${signo} 1`);
          else container.signal(signo);
          signalReturned = true;
        } catch (error) {
          signalError = failure(error);
        }
      }
      const result = await Promise.race([
        monitor,
        scheduler.wait(2000).then(() => ({ outcome: "pending" })),
      ]);
      return {
        phase: input.phase,
        signo,
        signalReturned,
        signalError,
        directSend,
        monitor: result,
        running: container.running,
        processState: container.running
          ? await signalOutput(
              container,
              `printf 'marker='; cat ${marker}; printf '\\n'; awk '/^Sig(Blk|Ign|Cgt):/ {print}' /proc/1/status`,
            )
          : null,
      };
    }
    case "startup-env-names": {
      const environments: Record<string, Record<string, string>> = {
        baseline: { NORMAL_NAME: "normal" },
        hyphen: { "A-B": "hyphen" },
        dot: { "a.b": "dot" },
        unicode: { 变量: "unicode" },
        empty: { "": "empty-name" },
        long: { ["A".repeat(257)]: "long-name" },
      };
      if (typeof input.variant !== "string")
        throw new TypeError("An environment variant is required");
      const env = environments[input.variant];
      if (env === undefined) throw new TypeError("Unknown environment variant");
      container.start({
        image,
        enableInternet: false,
        entrypoint: ["node", "-e", "setInterval(()=>{},1000)"],
        env,
      });
      const monitor = container.monitor().then(
        () => ({ outcome: "resolved" }),
        (error: unknown) => ({ outcome: "rejected", error: failure(error) }),
      );
      try {
        const process = await container.exec([
          "node",
          "-e",
          'const fs=require("node:fs");const keys=["NORMAL_NAME","A-B","a.b","变量","","A".repeat(257)];const mainEnv=Object.fromEntries(fs.readFileSync("/proc/1/environ").toString().split("\\0").flatMap(entry=>{const i=entry.indexOf("=");return i>=0&&keys.includes(entry.slice(0,i))?[[entry.slice(0,i),entry.slice(i+1)]]:[]}));console.log(JSON.stringify({normal:process.env.NORMAL_NAME,hyphen:process.env["A-B"],dot:process.env["a.b"],unicode:process.env["变量"],empty:process.env[""],long:process.env["A".repeat(257)],mainEnv,main:fs.readFileSync("/proc/1/cmdline").toString().split("\\0")}))',
        ]);
        const result = await process.output();
        return {
          variant: input.variant,
          stdout: new TextDecoder().decode(result.stdout),
          stderr: new TextDecoder().decode(result.stderr),
          exitCode: result.exitCode,
          info: await container.inspect(),
          running: container.running,
        };
      } catch (error) {
        return {
          variant: input.variant,
          startupError: failure(error),
          monitor: await Promise.race([
            monitor,
            scheduler.wait(2000).then(() => ({ outcome: "pending" })),
          ]),
          info: await container.inspect(),
          running: container.running,
        };
      }
    }
    case "tcp-port": {
      container.start({
        image,
        enableInternet: false,
        entrypoint: [
          "node",
          "-e",
          "require('node:http').createServer((req,res)=>{let body='';req.on('data',part=>body+=part);req.on('end',()=>{res.setHeader('content-type','application/json');res.end(JSON.stringify({method:req.method,url:req.url,marker:req.headers['x-probe-marker'],body}))})}).listen(8080,'0.0.0.0')",
        ],
      });
      const ready = await output(
        container,
        "node -e \"const net=require('node:net');let attempts=0;function connect(){const socket=net.connect(8080,'127.0.0.1',()=>{socket.end();process.exit(0)});socket.on('error',()=>{socket.destroy();if(++attempts===100)process.exit(1);setTimeout(connect,50)})}connect()\"",
      );
      if (ready.exitCode !== 0)
        throw new Error("TCP probe server readiness failed");
      const port = container.getTcpPort(8080);
      const response = await port.fetch("http://container/probe?q=one", {
        method: "POST",
        headers: { "x-probe-marker": "port-probe" },
        body: "port-input",
      });
      const result: unknown = await response.json();
      return {
        status: response.status,
        response: result,
        running: container.running,
      };
    }
    case "monitor": {
      if (input.mode === "forced") {
        await start(container, image);
        const reason = new Error("surface-forced-reason");
        const result = container.monitor().then(
          () => ({ outcome: "resolved" }),
          (error: unknown) => ({
            outcome: "rejected",
            error: failure(error),
            exactReason: error === reason,
          }),
        );
        await container.destroy(reason);
        return { result: await result, running: container.running };
      }
      if (input.mode !== "zero" && input.mode !== "nonzero")
        throw new TypeError("Invalid monitor mode");
      container.start({
        image,
        enableInternet: false,
        entrypoint: [
          "/bin/sh",
          "-c",
          `sleep 1; exit ${input.mode === "zero" ? 0 : 7}`,
        ],
      });
      const result = await container.monitor().then(
        () => ({ outcome: "resolved" }),
        (error: unknown) => ({ outcome: "rejected", error: failure(error) }),
      );
      return { result, running: container.running };
    }
    case "inactivity-invalid": {
      if (typeof input.duration !== "number")
        throw new TypeError("A duration is required");
      try {
        await container.setInactivityTimeout(input.duration);
        return { accepted: true, running: container.running };
      } catch (error) {
        return {
          accepted: false,
          error: failure(error),
          running: container.running,
        };
      }
    }
    case "inactivity-start":
      await start(container, image);
      await container.setInactivityTimeout(1000);
      return { running: container.running };
    case "touch":
      return { running: container.running };
    case "idle-inspect":
      return { info: await container.inspect(), running: container.running };
    case "exec-defaults":
      await start(container, image);
      return {
        home: await output(container, "printf '%s' \"$HOME\""),
        pty: await output(container, "printf '%s' \"$TERM\"", { pty: true }),
        combined: await output(
          container,
          "printf stdout; printf stderr >&2; exit 7",
          { stderr: "combined" },
        ),
        ptyCombined: await output(container, "printf out; printf err >&2", {
          pty: true,
          stderr: "combined",
        }),
      };
    case "snapshot-default":
      await start(container, image);
      return container.snapshotContainer();
    case "snapshot-measure": {
      const scripts: Record<string, string> = {
        baseline: "rm -rf /tmp/surface-size; mkdir -m 750 /tmp/surface-size",
        one: "printf x > /tmp/surface-size/data; chmod 640 /tmp/surface-size/data",
        zero4096: "head -c 4096 /dev/zero > /tmp/surface-size/data",
        random4096: "head -c 4096 /dev/urandom > /tmp/surface-size/data",
        random65536: "head -c 65536 /dev/urandom > /tmp/surface-size/data",
        mode: "chmod 600 /tmp/surface-size/data",
        directory: "mkdir -m 711 /tmp/surface-size/empty",
      };
      if (typeof input.variant !== "string")
        throw new TypeError("A snapshot variant is required");
      const script = scripts[input.variant];
      if (script === undefined) throw new TypeError("Unknown snapshot variant");
      const prepared = await output(container, script);
      if (prepared.exitCode !== 0)
        throw new Error("Snapshot input preparation failed");
      return {
        variant: input.variant,
        snapshot: await container.snapshotContainer({ name: input.variant }),
        facts: await output(
          container,
          "find /tmp/surface-size -printf '%m:%s:%f\\n'; test ! -f /tmp/surface-size/data || sha256sum /tmp/surface-size/data",
        ),
        info: await container.inspect(),
      };
    }
    case "snapshot-restore-inspect": {
      if (typeof input.snapshotId !== "string")
        throw new TypeError("A snapshot ID is required");
      container.start({
        containerSnapshot: { id: input.snapshotId },
        enableInternet: false,
      });
      return {
        output: await output(
          container,
          "stat -c '%a:%s' /tmp/surface-size/data /tmp/surface-size/empty; sha256sum /tmp/surface-size/data",
        ),
        info: await container.inspect(),
        running: container.running,
      };
    }
    case "directory-restore": {
      container.start({
        image,
        enableInternet: false,
        directorySnapshots: [
          {
            snapshot: {
              id: "surface-nonexistent-directory-snapshot",
              size: 0,
              dir: "/tmp/surface-restore",
            },
            mountPoint: "/tmp/surface-restore",
          },
        ],
      });
      const result = await container.monitor().then(
        () => ({ outcome: "resolved" }),
        (error: unknown) => ({ outcome: "rejected", error: failure(error) }),
      );
      return { result, running: container.running };
    }
    case "directory-empty-mount": {
      container.start({
        image,
        enableInternet: false,
        entrypoint: ["/bin/sh", "-c", "while :; do sleep 1; done"],
        directorySnapshots: [{ mountPoint: "/mnt" }],
      });
      const monitor = container.monitor().then(
        () => ({ outcome: "resolved" }),
        (error: unknown) => ({ outcome: "rejected", error: failure(error) }),
      );
      try {
        return {
          output: await output(
            container,
            "test -d /mnt && printf 'mounted\\n'; find /mnt -mindepth 1 -maxdepth 1 -printf '%f\\n'; awk '$2 == \"/mnt\" {print $3}' /proc/mounts",
          ),
          info: await container.inspect(),
          running: container.running,
        };
      } catch (error) {
        return {
          startupError: failure(error),
          monitor: await Promise.race([
            monitor,
            scheduler.wait(2000).then(() => ({ outcome: "pending" })),
          ]),
          info: await container.inspect(),
          running: container.running,
        };
      }
    }
    default:
      throw new TypeError("Unknown probe action");
  }
}
