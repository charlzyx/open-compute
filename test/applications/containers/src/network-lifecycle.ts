type NativeContainer = NonNullable<DurableObjectState["container"]>;

class FixtureCheckFailure extends Error {
  constructor(
    message: string,
    readonly output?: ExecOutput,
  ) {
    super(message);
  }
}

function check(
  condition: boolean,
  message: string,
  output?: ExecOutput,
): asserts condition {
  if (!condition) throw new FixtureCheckFailure(message, output);
}

function failureDiagnostic(error: unknown) {
  if (!(error instanceof FixtureCheckFailure))
    return {
      code: "FIXTURE_PHASE_EXECUTION_FAILED",
      ...(error instanceof Error && /\bDO_RPC_UNSUPPORTED\b/.test(error.message)
        ? { nativeErrorCode: "DO_RPC_UNSUPPORTED" }
        : {}),
    };
  const output = error.output;
  return {
    code: "FIXTURE_CHECK_FAILED",
    check: error.message,
    ...(output === undefined
      ? {}
      : {
          exitCode: output.exitCode,
          stdoutBytes: output.stdout.byteLength,
          stderrBytes: output.stderr.byteLength,
          // Only the fixed Python probe's stderr is exposed, with a byte limit.
          pythonStderr: new TextDecoder().decode(
            new Uint8Array(output.stderr).slice(0, 2048),
          ),
          stderrTruncated: output.stderr.byteLength > 2048,
        }),
  };
}

async function httpsProbe(native: NativeContainer, revision: string) {
  const process = await native.exec([
    "python3",
    "-c",
    [
      "import http.client,socket,ssl,sys",
      "context = ssl.create_default_context(cafile='/etc/cloudflare/certs/cloudflare-containers-ca.crt')",
      "assert context.check_hostname and context.verify_mode == ssl.CERT_REQUIRED",
      "raw = socket.create_connection(('203.0.113.1',443),timeout=10)",
      "tls = context.wrap_socket(raw,server_hostname='p23-egress.invalid')",
      "connection = http.client.HTTPSConnection('p23-egress.invalid',timeout=10,context=context)",
      "connection.sock = tls",
      "connection.request('POST','/https-probe',body='tls-body',headers={'X-P23-Request':'tls-header','Connection':'close'})",
      "response = connection.getresponse()",
      "assert response.status == 201",
      "assert response.getheader('X-P23-Reply') == 'tls-ok'",
      "assert response.read().decode() == 'https:' + sys.argv[1] + ':tls-body'",
      "assert tls.recv(1) == b''",
      "connection.close()",
      "print('https-verified',end='')",
    ].join("\n"),
    revision,
  ]);
  const output = await process.output();
  check(
    output.exitCode === 0 &&
      new TextDecoder().decode(output.stdout) === "https-verified" &&
      output.stderr.byteLength === 0,
    "HTTPS interception must verify the certificate, headers, body and close",
    output,
  );
  return {
    status: 201,
    header: "tls-ok",
    body: `https:${revision}:tls-body`,
    certificateVerified: true,
    closed: true,
  };
}

async function websocketProbe(native: NativeContainer) {
  const process = await native.exec([
    "python3",
    "-c",
    [
      "import base64,hashlib,socket,struct",
      "server = socket.socket()",
      "server.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)",
      "server.settimeout(10)",
      "server.bind(('0.0.0.0',8081))",
      "server.listen(1)",
      "print('ready',flush=True)",
      "connection,_ = server.accept()",
      "connection.settimeout(10)",
      "stream = connection.makefile('rb')",
      "assert stream.readline(8193) == b'GET /websocket HTTP/1.1\\r\\n'",
      "headers = {}",
      "for _ in range(32):",
      " line = stream.readline(8193)",
      " assert line.endswith(b'\\r\\n') and len(line) <= 8192",
      " if line == b'\\r\\n': break",
      " name,value = line.decode('ascii').split(':',1)",
      " assert name.lower() not in headers",
      " headers[name.lower()] = value.strip()",
      "else: raise AssertionError('too many headers')",
      "assert headers['upgrade'].lower() == 'websocket'",
      "assert 'upgrade' in [value.strip().lower() for value in headers['connection'].split(',')]",
      "assert headers['sec-websocket-version'] == '13'",
      "assert headers['x-p23-request'] == 'websocket-header'",
      "key = headers['sec-websocket-key']",
      "assert len(base64.b64decode(key,validate=True)) == 16",
      "accept = base64.b64encode(hashlib.sha1((key+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest())",
      "connection.sendall(b'HTTP/1.1 101 Switching Protocols\\r\\nUpgrade: websocket\\r\\nConnection: Upgrade\\r\\nX-P23-Reply: websocket-ok\\r\\nSec-WebSocket-Accept: '+accept+b'\\r\\n\\r\\n')",
      "def frame(opcode):",
      " header = stream.read(2)",
      " assert len(header) == 2 and header[0] == 0x80|opcode",
      " assert header[1]&0x80 and header[1]&0x7f < 126",
      " mask = stream.read(4)",
      " payload = stream.read(header[1]&0x7f)",
      " assert len(mask) == 4 and len(payload) == header[1]&0x7f",
      " return bytes(value^mask[index%4] for index,value in enumerate(payload))",
      "assert frame(1) == b'websocket-body'",
      "body = b'echo:websocket-body'",
      "connection.sendall(bytes([0x81,len(body)])+body)",
      "close = frame(8)",
      "assert close == struct.pack('!H',1000)+b'finished'",
      "connection.sendall(bytes([0x88,len(close)])+close)",
      "stream.close()",
      "connection.close()",
      "server.close()",
    ].join("\n"),
  ]);
  check(
    process.stdout !== null && process.stderr !== null,
    "WebSocket server requires output",
  );
  const reader = process.stdout.getReader();
  const stderr = new Response(process.stderr).text();
  let socket: WebSocket | null = null;
  let completed = false;
  try {
    let ready = "";
    while (!ready.endsWith("\n")) {
      const chunk = await reader.read();
      check(
        !chunk.done && chunk.value instanceof Uint8Array,
        "WebSocket server must become ready",
      );
      ready += new TextDecoder().decode(chunk.value);
      check(ready.length <= 6, "WebSocket server readiness must be bounded");
    }
    check(ready === "ready\n", "WebSocket server readiness must match");
    const response = await native
      .getTcpPort(8081)
      .fetch("http://container/websocket", {
        headers: { Upgrade: "websocket", "X-P23-Request": "websocket-header" },
      });
    check(
      response.status === 101 &&
        response.headers.get("X-P23-Reply") === "websocket-ok",
      "getTcpPort must preserve WebSocket upgrade and response headers",
    );
    socket = response.webSocket;
    check(socket !== null, "getTcpPort must return the upgraded WebSocket");
    const message = new Promise<unknown>((resolve, reject) => {
      socket?.addEventListener("message", (event) => resolve(event.data), {
        once: true,
      });
      socket?.addEventListener(
        "error",
        () => reject(new Error("WebSocket message failed")),
        { once: true },
      );
      socket?.addEventListener(
        "close",
        () => reject(new Error("WebSocket closed before its message")),
        { once: true },
      );
    });
    const closed = new Promise<CloseEvent>((resolve, reject) => {
      socket?.addEventListener("close", resolve, { once: true });
      socket?.addEventListener(
        "error",
        () => reject(new Error("WebSocket close failed")),
        { once: true },
      );
    });
    socket.accept();
    socket.send("websocket-body");
    check(
      (await message) === "echo:websocket-body",
      "WebSocket response body must match",
    );
    socket.close(1000, "finished");
    const close = await closed;
    check(
      close.code === 1000 && close.reason === "finished" && close.wasClean,
      "WebSocket close handshake must be preserved",
    );
    const exitCode = await process.exitCode;
    check(exitCode === 0, "WebSocket server must exit normally");
    check((await reader.read()).done, "WebSocket server output must reach EOF");
    check((await stderr) === "", "WebSocket server must not report errors");
    completed = true;
    return {
      header: "websocket-ok",
      body: "echo:websocket-body",
      closeCode: close.code,
      closeReason: close.reason,
      wasClean: close.wasClean,
      exitCode,
    };
  } finally {
    reader.releaseLock();
    if (!completed) {
      process.kill(9);
    }
  }
}

async function httpRoutingProbe(
  native: NativeContainer,
  specific: Fetcher,
  fallback: Fetcher,
) {
  await native.interceptOutboundHttp("p23-specific.invalid", specific);
  await native.interceptAllOutboundHttp(fallback);
  const process = await native.exec([
    "python3",
    "-c",
    [
      "import http.client,socket",
      "for host,expected in [('p23-specific.invalid','http-specific'),('p23-fallback.invalid','http-fallback')]:",
      " connection = socket.create_connection(('203.0.113.1',80),timeout=10)",
      " connection.sendall(('GET /http-routing HTTP/1.1\\r\\nHost: '+host+'\\r\\nConnection: close\\r\\n\\r\\n').encode())",
      " response = http.client.HTTPResponse(connection)",
      " response.begin()",
      " assert response.status == 200",
      " assert response.read().decode() == expected",
      " assert connection.recv(1) == b''",
      " connection.close()",
      "print('http-routing-verified',end='')",
    ].join("\n"),
  ]);
  const output = await process.output();
  check(
    output.exitCode === 0 &&
      new TextDecoder().decode(output.stdout) === "http-routing-verified" &&
      output.stderr.byteLength === 0,
    "Per-host and catch-all HTTP routes must reach different bindings",
    output,
  );
  return { matched: "http-specific", unmatched: "http-fallback", closed: true };
}

export async function networkLifecycle(
  native: NativeContainer,
  revision: string,
  specific: Fetcher,
  fallback: Fetcher,
): Promise<Response> {
  let phase: "https" | "websocket" | "http" = "https";
  try {
    const https = await httpsProbe(native, revision);
    phase = "websocket";
    const websocket = await websocketProbe(native);
    phase = "http";
    const http = await httpRoutingProbe(native, specific, fallback);
    return Response.json({ https, websocket, http });
  } catch (error) {
    return Response.json(
      { phase, diagnostic: failureDiagnostic(error) },
      { status: 500 },
    );
  }
}
