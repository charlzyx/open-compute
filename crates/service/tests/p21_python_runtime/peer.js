import { connect } from "cloudflare:sockets";
import { WorkerEntrypoint } from "cloudflare:workers";

export default class extends WorkerEntrypoint {
  async fetch(request) {
    const url = new URL(request.url);
    const path = url.pathname;
    if (path === "/search") {
      const payload = await request.json();
      const operation = payload.operation;
      const name = payload.name ?? "python-runtime-search";
      let result;
      if (operation === "namespace_list")
        result = await this.env.SEARCH.list(payload.options ?? {});
      else if (operation === "namespace_create")
        result = await (
          await this.env.SEARCH.create({
            id: name,
            index_method: { vector: true, keyword: true },
            score_threshold: 0,
            chunk: false,
          })
        ).info();
      else if (operation === "namespace_delete") {
        await this.env.SEARCH.delete(name);
        result = null;
      } else if (operation === "errors") {
        const rejected = {};
        for (const failure of ["query", "update", "missing", "isolation"]) {
          try {
            if (failure === "query")
              await this.env.SEARCH.get(name).search({
                query: "alpha",
                messages: [{ role: "user", content: "alpha" }],
              });
            else if (failure === "update")
              await this.env.SEARCH.get(name).update({ id: "renamed" });
            else if (failure === "missing")
              await this.env.SEARCH.get("missing-instance").info();
            else await this.env.ISOLATED_SEARCH.get(name).info();
          } catch (error) {
            rejected[failure] = error.message;
            continue;
          }
          rejected[failure] = null;
        }
        result = { rejected };
      } else {
        const instance =
          payload.target === "direct"
            ? this.env.DIRECT_SEARCH
            : this.env.SEARCH.get(name);
        if (operation === "info") result = await instance.info();
        else if (operation === "stats") result = await instance.stats();
        else if (operation === "upload") {
          const content =
            payload.contentKind === "stream"
              ? new Blob([payload.content]).stream()
              : payload.contentKind === "blob"
                ? new Blob([payload.content], { type: "text/plain" })
                : payload.content;
          result = await instance.items.uploadAndPoll(
            payload.filename,
            content,
            {
              metadata: { kind: payload.kind },
              pollIntervalMs: 50,
              timeoutMs: 30000,
            },
          );
        } else if (operation === "items")
          result = await instance.items.list(payload.options ?? {});
        else if (operation === "item_delete") {
          await instance.items.delete(payload.itemId);
          result = null;
        } else if (operation === "item") {
          const item = instance.items.get(payload.itemId);
          const download = await item.download();
          result = {
            info: await item.info(),
            logs: await item.logs(),
            chunks: await item.chunks(),
            download: {
              body: await new Response(download.body).text(),
              filename: download.filename,
              contentType: download.contentType,
              size: download.size,
            },
          };
        } else if (operation === "search" || operation === "multi_search")
          result = await (
            operation === "multi_search" ? this.env.SEARCH : instance
          ).search(payload.request);
        else if (operation === "chat")
          result = await instance.chatCompletions(payload.request);
        else if (operation === "stream")
          result = {
            body: await new Response(
              await instance.chatCompletions(payload.request),
            ).text(),
          };
        else if (operation === "jobs") result = await instance.jobs.list();
        else if (operation === "job_create")
          result = await instance.jobs.create({
            description: "Python binding parity",
          });
        else if (operation === "job") {
          const job = instance.jobs.get(payload.jobId);
          result = { info: await job.info(), logs: await job.logs() };
        } else if (operation === "job_cancel")
          result = await instance.jobs.get(payload.jobId).cancel();
        else throw new TypeError("unsupported test operation");
      }
      return Response.json(result);
    }
    if (path === "/artifacts") {
      const payload = await request.json();
      const binding = this.env.ARTIFACTS;
      const operation = payload.operation;
      const name = payload.name ?? "python-sdk";
      const issued = (result, field) => {
        const secret = result[field];
        if (
          typeof secret !== "string" ||
          !/^art_v1_[0-9a-f]{40}\?expires=[0-9]+$/.test(secret)
        )
          throw new TypeError("invalid issued Artifacts token");
        delete result[field];
        return { ...result, tokenShapeValid: true };
      };
      let result;
      if (operation === "create")
        result = issued(
          await binding.create(name, {
            description: payload.description ?? "Python binding parity",
          }),
          "token",
        );
      else if (operation === "list")
        result = await binding.list(payload.options ?? { limit: 50 });
      else if (operation === "delete") result = await binding.delete(name);
      else if (operation === "isolation") {
        try {
          await this.env.ISOLATED_ARTIFACTS.get("python-sdk");
          result = null;
        } catch (error) {
          result = {
            code: error.code,
            numericCode: error.numericCode,
            message: error.message,
          };
        }
      } else if (operation === "errors") {
        const rejected = {};
        for (const failure of [
          "duplicate",
          "missing",
          "name",
          "ttl",
          "import",
        ]) {
          try {
            if (failure === "duplicate") await binding.create("python-sdk");
            else if (failure === "missing")
              await binding.get("missing-repository");
            else if (failure === "name") await binding.create("../invalid");
            else if (failure === "ttl")
              await (await binding.get("python-sdk")).createToken("read", 0);
            else
              await binding.import({
                source: { url: "http://127.0.0.1/repo.git" },
                target: { name: "unsafe" },
              });
          } catch (error) {
            rejected[failure] = {
              code: error.code,
              numericCode: error.numericCode,
              message: error.message,
            };
            continue;
          }
          rejected[failure] = null;
        }
        result = { rejected };
      } else {
        const repo = await binding.get(name);
        if (operation === "inspect") {
          result = {
            repo: await repo.info(),
            tokens: await repo.listTokens(),
          };
        } else if (operation === "token")
          result = issued(
            await repo.createToken(payload.scope, 3600),
            "plaintext",
          );
        else if (operation === "revoke")
          result = await repo.revokeToken(payload.tokenId);
        else if (operation === "fork")
          result = issued(
            await repo.fork(payload.target, { defaultBranchOnly: true }),
            "token",
          );
        else throw new TypeError("unsupported test operation");
      }
      return Response.json(result);
    }
    if (path === "/ai") {
      const payload = await request.json();
      const document = (
        name = "sample.md",
        content = "# Native Python\n\nmarkdown bridge",
        mime = "text/markdown",
      ) => ({ name, blob: new Blob([content], { type: mime }) });
      const service = this.env.AI.toMarkdown();
      const operation = payload.operation ?? "single";
      let result;
      if (operation === "supported") result = await service.supported();
      else if (operation === "errors") {
        const rejected = {};
        for (const failure of ["document", "options", "inference"]) {
          try {
            if (failure === "document")
              await this.env.AI.toMarkdown(document("../sample.md"));
            else if (failure === "options")
              await this.env.AI.toMarkdown(document(), { gateway: {} });
            else
              await this.env.AI.run("@cf/unsupported", { prompt: "example" });
          } catch (error) {
            rejected[failure] = String(error);
            continue;
          }
          rejected[failure] = null;
        }
        return Response.json({ rejected });
      } else if (operation === "batch")
        result = await this.env.AI.toMarkdown([
          document(),
          document("bad.png", "invalid image", "image/png"),
        ]);
      else if (operation === "transform")
        result = await service.transform(
          document("sample.txt", "handle transform", "text/plain"),
        );
      else if (operation === "text")
        result = await service.transform(document(), {
          conversionOptions: { output: { format: "text" } },
        });
      else result = await this.env.AI.toMarkdown(document());
      return Response.json({
        result,
        aiGatewayLogId: this.env.AI.aiGatewayLogId,
      });
    }
    if (path === "/vectors") {
      const payload = await request.json();
      const operation = payload.operation;
      if (operation === "errors") {
        const rejected = {};
        for (const failure of ["vector", "topK", "batch"]) {
          try {
            if (failure === "vector") await this.env.VECTORS.query(["invalid"]);
            else if (failure === "topK")
              await this.env.VECTORS.query([1, 0, 0], { topK: 0 });
            else await this.env.VECTORS.insert([]);
          } catch (error) {
            rejected[failure] = String(error);
            continue;
          }
          rejected[failure] = null;
        }
        return Response.json({ rejected });
      }
      let result;
      if (operation === "describe") result = await this.env.VECTORS.describe();
      else if (["query", "queryById"].includes(operation))
        result = await this.env.VECTORS[operation](
          payload.value,
          payload.options ?? {},
        );
      else if (
        ["insert", "upsert", "getByIds", "deleteByIds"].includes(operation)
      )
        result = await this.env.VECTORS[operation](payload.value);
      else throw new TypeError("unsupported test operation");
      return Response.json(result);
    }
    if (path === "/images") {
      const payload = await request.json();
      const data = Uint8Array.from(atob(payload.source), (value) =>
        value.charCodeAt(0),
      );
      const stream = (bytes = data) => new Blob([bytes]).stream();
      if (payload.operation === "errors") {
        const rejected = {};
        for (const failure of ["input", "options", "decode"]) {
          try {
            if (failure === "input") this.env.IMAGES.input("invalid-stream");
            else if (failure === "options")
              await this.env.IMAGES.info(stream(), { unsupported: true });
            else
              await this.env.IMAGES.info(
                stream(new TextEncoder().encode("invalid-image")),
              );
          } catch (error) {
            rejected[failure] = String(error);
            continue;
          }
          rejected[failure] = null;
        }
        return Response.json({ rejected });
      }
      const info = await this.env.IMAGES.info(stream());
      let chain = this.env.IMAGES.input(stream()).transform({
        width: 4,
        height: 3,
        fit: "pad",
        background: "#102030ff",
      });
      if (payload.operation === "draw") {
        chain = chain
          .draw(stream(), { left: 1, top: 1, opacity: 1, composite: "over" })
          .transform({ rotate: 90 });
      }
      const result = await chain.output({
        format: payload.format ?? "image/png",
      });
      const response = payload.image
        ? new Response(result.image())
        : result.response({ headers: { "x-image-test": "custom" } });
      const bytes = new Uint8Array(await response.arrayBuffer());
      return Response.json({
        info,
        contentType: result.contentType(),
        status: response.status,
        header: response.headers.get("x-image-test"),
        responseContentType: response.headers.get("content-type"),
        bytes: btoa(String.fromCharCode(...bytes)),
      });
    }
    if (path === "/assets") {
      const headers = new Headers();
      for (const [query, header] of [
        ["etag", "if-none-match"],
        ["range", "range"],
      ])
        if (url.searchParams.has(query))
          headers.set(header, url.searchParams.get(query));
      const response = await this.env.ASSETS.fetch(
        "https://assets.example" +
          (url.searchParams.get("path") ?? "/message.txt"),
        {
          method: url.searchParams.get("method") ?? "GET",
          headers,
        },
      );
      const names = ["content-type", "content-length", "content-range", "etag"];
      return Response.json({
        status: response.status,
        body: await response.text(),
        headers: Object.fromEntries(
          names.map((name) => [name, response.headers.get(name)]),
        ),
      });
    }
    if (path === "/cache") {
      const cache =
        url.searchParams.get("namespace") === "named"
          ? await caches.open("runtime-cache-named")
          : caches.default;
      const key = "https://python-runtime-cache.invalid/value";
      const value = url.searchParams.get("value") ?? "cache-body";
      const method = url.searchParams.get("method") ?? "GET";
      const ignoreMethod = url.searchParams.get("ignore_method") === "true";
      const operation = url.searchParams.get("op") ?? "match";
      const headers = new Headers();
      for (const [query, header] of [
        ["range", "range"],
        ["etag", "if-none-match"],
      ])
        if (url.searchParams.has(query))
          headers.set(header, url.searchParams.get(query));
      const response = (status = 200, extra = {}) =>
        new Response(value, {
          status,
          headers: {
            "content-type": "text/plain",
            "cache-control": "public, max-age=3600",
            etag: `"${value}"`,
            "content-length": String(
              new TextEncoder().encode(value).byteLength,
            ),
            ...extra,
          },
        });
      if (operation === "put") {
        await cache.put(key, response());
        return Response.json({ stored: true });
      }
      if (operation === "delete")
        return Response.json({
          deleted: await cache.delete(new Request(key, { method, headers }), {
            ignoreMethod,
          }),
        });
      if (operation === "errors") {
        const rejected = {};
        for (const failure of ["method", "partial", "vary"]) {
          try {
            await cache.put(
              new Request(key, {
                method: failure === "method" ? "POST" : "GET",
              }),
              response(
                failure === "partial" ? 206 : 200,
                failure === "vary" ? { vary: "*" } : {},
              ),
            );
            rejected[failure] = false;
          } catch (error) {
            rejected[failure] = Boolean(String(error));
          }
        }
        return Response.json(rejected);
      }
      if (operation !== "match")
        throw new Error("unknown cache fixture operation");
      const cached = await cache.match(new Request(key, { method, headers }), {
        ignoreMethod,
      });
      if (cached === undefined) return Response.json({ found: false });
      const selected = [
        "content-type",
        "cache-control",
        "etag",
        "content-length",
        "content-range",
      ];
      return Response.json({
        found: true,
        status: cached.status,
        body: await cached.text(),
        headers: Object.fromEntries(
          selected.map((name) => [name, cached.headers.get(name)]),
        ),
      });
    }
    const key = "runtime/value";
    if (path === "/write") {
      const value = "javascript";
      await this.env.KV.put(key, value);
      await this.env.DB.prepare(
        "CREATE TABLE IF NOT EXISTS runtime_state (id INTEGER PRIMARY KEY, value TEXT NOT NULL)",
      ).run();
      await this.env.DB.prepare(
        "INSERT INTO runtime_state VALUES (1, ?) ON CONFLICT(id) DO UPDATE SET value=excluded.value",
      )
        .bind(value)
        .run();
      await this.env.BUCKET.put(key, value);
      return Response.json({ written: value });
    }
    if (path === "/read") {
      const object = await this.env.BUCKET.get(key);
      return Response.json({
        kv: await this.env.KV.get(key),
        d1: await this.env.DB.prepare(
          "SELECT value FROM runtime_state WHERE id=1",
        ).first("value"),
        r2: object === null ? null : await object.text(),
      });
    }
    if (path === "/background-read")
      return Response.json(await this.env.KV.get("runtime/background"));
    if (path === "/ffi") {
      const value = { unicode: "µ☁", nested: [null, true, 42, 1.25] };
      const jsNull = JSON.parse("null");
      const missing = Object.create(null).missing;
      const bytes = new Uint8Array([0, 1, 255]);
      const digest = new Uint8Array(
        await crypto.subtle.digest("SHA-256", bytes),
      );
      return Response.json(
        {
          value: structuredClone(value),
          jsNull,
          nullDistinctFromUndefined: jsNull !== missing,
          bytes: [...bytes],
          sha256: [...digest]
            .map((byte) => byte.toString(16).padStart(2, "0"))
            .join(""),
          mapped: [1, 2, 3].map((item) => item * 2),
        },
        { status: 201, headers: { "x-ffi": "native" } },
      );
    }
    if (path === "/outbound") {
      const response = await fetch(this.env.OUTBOUND_URL, {
        method: "POST",
        body: "µ☁",
        headers: { "x-caller": "javascript" },
      });
      return new Response(await response.text(), { status: response.status });
    }
    if (path === "/tcp") {
      const destination = new URL(this.env.OUTBOUND_URL);
      const socket = connect(
        { hostname: destination.hostname, port: Number(destination.port) },
        { allowHalfOpen: true },
      );
      const writer = socket.writable.getWriter();
      const reader = socket.readable.getReader();
      try {
        await writer.write(
          new TextEncoder().encode(
            "GET /tcp HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n",
          ),
        );
        await writer.close();
        const decoder = new TextDecoder();
        let response = "";
        while (true) {
          const part = await reader.read();
          if (part.done) break;
          response += decoder.decode(part.value, { stream: true });
        }
        response += decoder.decode();
        return Response.json({ response });
      } finally {
        reader.releaseLock();
        writer.releaseLock();
        await socket.close();
        await socket.closed;
      }
    }
    return new Response("missing route", { status: 404 });
  }
}
