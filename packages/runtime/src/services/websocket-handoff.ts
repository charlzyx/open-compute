/** Private response header carrying Service fetch handles to the native socket bridge. */
export const SERVICE_WEBSOCKET_HANDOFF_HEADER =
  "x-open-compute-service-websocket-handoffs";
const SERVICE_WEBSOCKET_HANDLE =
  /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const MAX_SERVICE_WEBSOCKET_HANDOFFS = 16;
function failure(code: string): Error {
  const error = Object.assign(new Error(code), { stableCode: code });
  error.stack = `Error: ${code}`;
  return error;
}

export function parseServiceWebSocketHandoffs(raw: string): string[] {
  const handles = raw.split(",");
  if (
    handles.length < 1 ||
    handles.length > MAX_SERVICE_WEBSOCKET_HANDOFFS ||
    handles.some((handle) => !SERVICE_WEBSOCKET_HANDLE.test(handle)) ||
    new Set(handles).size !== handles.length
  ) {
    throw failure("SERVICE_UNAVAILABLE");
  }
  return handles;
}

export function responseWithHeaders(
  response: Response,
  headers: Headers,
): Response {
  const result = new Response(response.body, response);
  const value = headers.get(SERVICE_WEBSOCKET_HANDOFF_HEADER);
  if (value === null) result.headers.delete(SERVICE_WEBSOCKET_HANDOFF_HEADER);
  else result.headers.set(SERVICE_WEBSOCKET_HANDOFF_HEADER, value);
  return result;
}

/** Add the current trusted Service fetch operation to a native WebSocket response. */
export function appendServiceWebSocketHandoff(
  response: Response,
  handle: string,
): Response {
  if (!response.webSocket || !SERVICE_WEBSOCKET_HANDLE.test(handle)) {
    throw failure("SERVICE_UNAVAILABLE");
  }
  const raw = response.headers.get(SERVICE_WEBSOCKET_HANDOFF_HEADER);
  const handles = raw === null ? [] : parseServiceWebSocketHandoffs(raw);
  if (
    handles.includes(handle) ||
    handles.length >= MAX_SERVICE_WEBSOCKET_HANDOFFS
  ) {
    throw failure("SERVICE_UNAVAILABLE");
  }
  const headers = new Headers(response.headers);
  headers.set(SERVICE_WEBSOCKET_HANDOFF_HEADER, [...handles, handle].join(","));
  return responseWithHeaders(response, headers);
}

/** Read strict trusted handoff handles at the final loader-host boundary. */
export function serviceWebSocketHandoffHandles(
  response: Response,
): readonly string[] {
  const raw = response.headers.get(SERVICE_WEBSOCKET_HANDOFF_HEADER);
  if (raw === null) return [];
  if (!response.webSocket) throw failure("SERVICE_UNAVAILABLE");
  return parseServiceWebSocketHandoffs(raw);
}
