const httpsClient = String.raw`
const assert = require('node:assert/strict');
const fs = require('node:fs');
const https = require('node:https');
const request = https.request({
  hostname: 'surface-egress.invalid', port: 443, path: '/https-probe', method: 'POST',
  ca: fs.readFileSync('/etc/cloudflare/certs/cloudflare-containers-ca.crt'),
  rejectUnauthorized: true, servername: 'surface-egress.invalid', family: 4,
  lookup: (_host, _options, callback) => callback(null, '203.0.113.1', 4),
  headers: { 'X-Probe-Request': 'tls-header', Connection: 'close' },
}, response => {
  const socket = response.socket;
  assert.equal(socket.authorized, true);
  assert.equal(response.statusCode, 201);
  assert.equal(response.headers['x-probe-reply'], 'tls-ok');
  let body = '';
  response.setEncoding('utf8');
  response.on('data', part => { body += part; assert.ok(body.length <= 128); });
  response.on('end', () => {
    assert.equal(body, 'https:tls-body');
    socket.once('close', () => process.stdout.write('https-verified'));
  });
});
request.setTimeout(10000, () => request.destroy(new Error('TLS request timed out')));
request.on('error', error => { console.error(error.message); process.exitCode = 1; });
request.end('tls-body');
`;

const websocketServer = String.raw`
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const net = require('node:net');
const server = net.createServer(socket => {
  server.close();
  socket.setTimeout(10000, () => socket.destroy(new Error('WebSocket timed out')));
  let buffer = Buffer.alloc(0), stage = 0;
  socket.on('data', part => {
    buffer = Buffer.concat([buffer, part]);
    assert.ok(buffer.length <= 8192);
    if (stage === 0) {
      const end = buffer.indexOf('\r\n\r\n');
      if (end < 0) return;
      const lines = buffer.subarray(0, end).toString('ascii').split('\r\n');
      assert.equal(lines.shift(), 'GET /websocket HTTP/1.1');
      const headers = {};
      for (const line of lines) {
        const colon = line.indexOf(':');
        assert.ok(colon > 0);
        const name = line.slice(0, colon).toLowerCase();
        assert.equal(headers[name], undefined);
        headers[name] = line.slice(colon + 1).trim();
      }
      assert.equal(headers.upgrade.toLowerCase(), 'websocket');
      assert.ok(headers.connection.toLowerCase().split(',').map(x => x.trim()).includes('upgrade'));
      assert.equal(headers['sec-websocket-version'], '13');
      assert.equal(headers['x-probe-request'], 'websocket-header');
      assert.equal(Buffer.from(headers['sec-websocket-key'], 'base64').length, 16);
      const accept = crypto.createHash('sha1').update(headers['sec-websocket-key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
      socket.write('HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nX-Probe-Reply: websocket-ok\r\nSec-WebSocket-Accept: ' + accept + '\r\n\r\n');
      buffer = buffer.subarray(end + 4);
      stage = 1;
    }
    while (buffer.length >= 2) {
      assert.equal(buffer[0], stage === 1 ? 0x81 : 0x88);
      assert.ok(buffer[1] & 0x80);
      const length = buffer[1] & 0x7f;
      assert.ok(length < 126);
      if (buffer.length < length + 6) return;
      const mask = buffer.subarray(2, 6);
      const payload = Buffer.from(buffer.subarray(6, length + 6));
      for (let i = 0; i < length; i++) payload[i] ^= mask[i % 4];
      buffer = buffer.subarray(length + 6);
      if (stage === 1) {
        assert.equal(payload.toString(), 'websocket-body');
        const body = Buffer.from('echo:websocket-body');
        socket.write(Buffer.concat([Buffer.from([0x81, body.length]), body]));
        stage = 2;
      } else {
        assert.equal(payload.readUInt16BE(0), 1000);
        assert.equal(payload.subarray(2).toString(), 'finished');
        assert.equal(buffer.length, 0);
        socket.end(Buffer.concat([Buffer.from([0x88, payload.length]), payload]));
        stage = 3;
      }
    }
  });
  socket.on('error', error => { console.error(error.message); process.exitCode = 1; });
  socket.on('close', () => assert.equal(stage, 3));
}).listen(8081, '0.0.0.0', () => process.stdout.write('ready\n'));
`;

function check(condition: boolean, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

export async function networkProbe(
  container: Container,
  image: string,
  outbound: Fetcher,
) {
  await container.interceptOutboundHttps("surface-egress.invalid", outbound);
  container.start({
    image,
    enableInternet: false,
    entrypoint: ["node", "-e", "setInterval(()=>{},1000)"],
  });
  const https = await container.exec(["node", "-e", httpsClient]);
  const tls = await https.output();
  check(
    tls.exitCode === 0 &&
      new TextDecoder().decode(tls.stdout) === "https-verified" &&
      tls.stderr.byteLength === 0,
    "HTTPS must verify the injected CA, hostname, response and close",
  );
  const process = await container.exec(["node", "-e", websocketServer]);
  check(
    process.stdout !== null && process.stderr !== null,
    "Missing server output",
  );
  const reader = process.stdout.getReader();
  const stderr = new Response(process.stderr).text();
  let completed = false;
  let socket: WebSocket | null = null;
  try {
    let ready = "";
    while (!ready.endsWith("\n")) {
      const chunk = await reader.read();
      check(
        !chunk.done && chunk.value instanceof Uint8Array,
        "Server exited before readiness",
      );
      ready += new TextDecoder().decode(chunk.value);
      check(ready.length <= 6, "Server readiness exceeded its limit");
    }
    check(ready === "ready\n", "Server readiness did not match");
    const response = await container
      .getTcpPort(8081)
      .fetch("http://container/websocket", {
        headers: {
          Upgrade: "websocket",
          "X-Probe-Request": "websocket-header",
        },
      });
    check(
      response.status === 101 &&
        response.headers.get("X-Probe-Reply") === "websocket-ok",
      "WebSocket upgrade did not match",
    );
    socket = response.webSocket;
    check(socket !== null, "Missing upgraded WebSocket");
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
        () => reject(new Error("WebSocket closed before message")),
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
      "WebSocket body did not match",
    );
    socket.close(1000, "finished");
    const close = await closed;
    check(
      close.code === 1000 && close.reason === "finished" && close.wasClean,
      "WebSocket close did not match",
    );
    const exitCode = await process.exitCode;
    check(
      exitCode === 0 && (await reader.read()).done && (await stderr) === "",
      "WebSocket server did not exit cleanly",
    );
    completed = true;
    return {
      https: {
        certificateVerified: true,
        status: 201,
        header: "tls-ok",
        body: "https:tls-body",
        closed: true,
      },
      websocket: {
        status: 101,
        header: "websocket-ok",
        body: "echo:websocket-body",
        closeCode: close.code,
        closeReason: close.reason,
        wasClean: close.wasClean,
        exitCode,
      },
    };
  } finally {
    reader.releaseLock();
    if (!completed) {
      socket?.close();
      process.kill(9);
    }
  }
}
