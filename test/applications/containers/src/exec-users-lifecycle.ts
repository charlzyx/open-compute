import { collect } from "./exec-options-lifecycle";

type NativeContainer = NonNullable<DurableObjectState["container"]>;

function check(condition: boolean, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

const probe = [
  "import os,json",
  "fields=dict(line.split(':',1) for line in open('/proc/self/status') if ':' in line)",
  "main=dict(line.split(':',1) for line in open('/proc/1/status') if ':' in line)",
  "uid,gid=os.geteuid(),os.getegid(); readable=False; switched=False",
  "try:",
  " with open(os.environ['OC_EXEC_USER_PROBE'],'rb') as f: readable=f.read()==b'exec-user-fixture'",
  "except PermissionError: pass",
  "try: os.setuid(0); switched=os.geteuid()==0",
  "except PermissionError: pass",
  "print(json.dumps({'uid':uid,'gid':gid,'mainUid':int(main['Uid'].split()[0]),'mainGid':int(main['Gid'].split()[0]),'capEff':fields['CapEff'].strip(),'capBnd':fields['CapBnd'].strip(),'mainCapEff':main['CapEff'].strip(),'canRead':readable,'canSetRoot':switched}))",
].join("\n");

export async function execUsersLifecycle(
  native: NativeContainer,
  durableObject: boolean,
): Promise<Response> {
  check(native.running, "exec user checks require a running main");
  const deadline = Date.now() + 60000;
  const path = `/tmp/exec-user-fixture-${crypto.randomUUID()}`;
  const env = { OC_EXEC_USER_PROBE: path };
  const decoder = new TextDecoder("utf-8", { fatal: true, ignoreBOM: false });
  const setup = await collect(
    native,
    "import os; p=os.environ['OC_EXEC_USER_PROBE']; f=os.open(p,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600); os.write(f,b'exec-user-fixture'); os.close(f); os.chmod(p,0)",
    { user: "0:0", env },
    () => {},
    deadline,
  );
  check(setup.output.exitCode === 0, "root-owned exec fixture creation failed");
  try {
    const run = async (user?: string) => {
      const result = await collect(
        native,
        probe,
        { env, ...(user === undefined ? {} : { user }) },
        () => {},
        deadline,
      );
      check(
        result.output.exitCode === 0 &&
          result.output.stderr.byteLength === 0 &&
          result.output.stdout.byteLength <= 2048,
        "exec user probe must complete with bounded JSON",
      );
      const value: unknown = JSON.parse(decoder.decode(result.output.stdout));
      check(isRecord(value), "exec user probe must return an object");
      for (const field of ["uid", "gid", "mainUid", "mainGid"]) {
        const id = value[field];
        check(
          typeof id === "number" && Number.isInteger(id) && id >= 0,
          "exec user probe returned an invalid ID",
        );
      }
      for (const field of ["capEff", "capBnd", "mainCapEff"]) {
        const mask = value[field];
        check(
          typeof mask === "string" && /^[0-9a-f]{16}$/.test(mask),
          "exec user probe returned an invalid capability set",
        );
      }
      check(
        typeof value.canRead === "boolean" &&
          typeof value.canSetRoot === "boolean",
        "exec user probe returned invalid permission results",
      );
      return value;
    };
    const root = await run("0:0");
    const inherited = await run();
    const pair = await run("65534:65534");
    const bare = await run("65534");
    check(
      root.uid === 0 &&
        root.gid === 0 &&
        root.canRead === true &&
        root.canSetRoot === true &&
        root.capEff === root.capBnd,
      "explicit root must retain only the container capability bound",
    );
    check(
      inherited.uid === inherited.mainUid &&
        inherited.gid === inherited.mainGid &&
        inherited.uid !== 0,
      "omitted exec user must preserve the non-root image user",
    );
    check(pair.uid === 65534 && pair.gid === 65534, "numeric UID:GID changed");
    check(
      bare.uid === 0 &&
        bare.gid === 0 &&
        bare.canRead === true &&
        bare.canSetRoot === true,
      "bare numeric UID must execute as root",
    );
    for (const nonRoot of [inherited, pair]) {
      check(
        nonRoot.canRead === durableObject &&
          nonRoot.canSetRoot === durableObject &&
          nonRoot.capEff ===
            (durableObject ? root.capEff : "0000000000000000") &&
          nonRoot.mainCapEff ===
            (durableObject ? root.capEff : "0000000000000000"),
        "process capabilities must follow the persisted scheduling policy",
      );
    }
    for (const user of ["root", "nobody:nogroup"]) {
      let allocated = false;
      let denied = false;
      try {
        await collect(
          native,
          "pass",
          { user },
          () => {
            allocated = true;
          },
          deadline,
        );
      } catch (error) {
        if (
          allocated ||
          Date.now() >= deadline ||
          !(error instanceof Error) ||
          error instanceof DOMException
        )
          throw error;
        denied = true;
      }
      check(
        denied,
        "user and group names must reject before process allocation",
      );
      check(
        (await run("0:0")).canRead === true,
        "rejected exec must preserve the live container",
      );
    }
    check(native.running, "exec user checks must preserve their main process");
    return Response.json({
      imageUserPreserved: true,
      numericPairPreserved: true,
      bareUidRoot: true,
      namedUsersRejected: true,
      nonRootCapabilities: durableObject,
      mainCapabilities: durableObject,
      rootBoundPreserved: true,
    });
  } finally {
    const cleanup = await collect(
      native,
      "import os; os.unlink(os.environ['OC_EXEC_USER_PROBE'])",
      { user: "0:0", env },
      () => {},
      deadline,
    );
    check(
      cleanup.output.exitCode === 0,
      "root-owned exec fixture cleanup failed",
    );
  }
}
