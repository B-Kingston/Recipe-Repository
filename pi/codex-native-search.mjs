const MAX_SSE_BUFFER_CHARS = 1_048_576;

/** Canonicalizes URLs before citation matching. Search providers and models
 * frequently disagree only about a trailing slash or fragment. */
export function normalizeSourceUrl(value) {
  if (typeof value !== "string" || !value.trim()) return null;
  try {
    const url = new URL(value.trim());
    if (url.protocol !== "http:" && url.protocol !== "https:") return null;
    url.hash = "";
    if (url.pathname.length > 1) url.pathname = url.pathname.replace(/\/+$/, "");
    return url.toString();
  } catch {
    return null;
  }
}

function addSource(state, source) {
  const url = normalizeSourceUrl(source?.url);
  if (!url) return;
  if (!state.sources.has(url)) {
    state.sources.set(
      url,
      typeof source.title === "string" && source.title.trim() ? source.title.trim() : url,
    );
  }
}

function collectContent(state, content) {
  if (!Array.isArray(content)) return;
  for (const part of content) {
    if (part?.type === "output_text" && typeof part.text === "string") state.outputText.push(part.text);
    if (!Array.isArray(part?.annotations)) continue;
    for (const annotation of part.annotations) {
      if (annotation?.type === "url_citation") addSource(state, annotation);
    }
  }
}

function collectItem(state, item) {
  if (!item || typeof item !== "object") return;
  if (item.type === "web_search_call") {
    const actionType = typeof item.action === "string" ? item.action : item.action?.type;
    const completed = item.status === "completed" || (actionType === "search" && item.status !== "failed");
    if (completed) state.invoked = true;
    for (const source of item.action?.sources ?? []) addSource(state, source);
  }
  collectContent(state, item.content);
}

export function nativeSearchPayload(payload) {
  if (!payload || typeof payload !== "object" || Array.isArray(payload)) {
    throw new Error("Pi produced an invalid Codex request payload.");
  }
  return {
    ...payload,
    tools: [{ type: "web_search", search_context_size: "high" }],
    tool_choice: { type: "web_search" },
  };
}

/** Attaches the hosted web_search tool without forcing its use; gap-fill
 * generation over imported video evidence may stay unsourced when the
 * cleaned evidence alone answers the request. */
export function optionalSearchPayload(payload) {
  if (!payload || typeof payload !== "object" || Array.isArray(payload)) {
    throw new Error("Pi produced an invalid Codex request payload.");
  }
  return {
    ...payload,
    tools: [{ type: "web_search", search_context_size: "high" }],
  };
}

export function collectNativeSearchEvent(state, event) {
  if (!event || typeof event !== "object") return;
  if (event.type === "response.web_search_call.completed") state.invoked = true;
  if (event.type === "response.output_text.annotation.added" && event.annotation?.type === "url_citation") {
    addSource(state, event.annotation);
  }
  if (event.type === "response.output_text.delta" && typeof event.delta === "string") {
    state.streamedText.push(event.delta);
  }
  collectItem(state, event.item);
  for (const item of event.response?.output ?? []) collectItem(state, item);
}

function inspectSseBlock(block, state) {
  const data = block
    .split("\n")
    .filter((line) => line.startsWith("data:"))
    .map((line) => line.slice(5).trim())
    .join("\n");
  if (!data || data === "[DONE]") return;
  try {
    collectNativeSearchEvent(state, JSON.parse(data));
  } catch {
    // The provider may add comments or a non-JSON diagnostic event. The model
    // response remains usable; a missing completed search will still fail the
    // grounded-mode check in the worker.
    state.parseErrors = (state.parseErrors ?? 0) + 1;
  }
}

/** Inspects the cloned SSE response incrementally so a large search response
 * is never duplicated as one giant string. */
export async function inspectNativeSearchResponse(response, state) {
  if (!response.body) return;
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  try {
    while (true) {
      const { done, value } = await reader.read();
      buffer += decoder.decode(value ?? new Uint8Array(), { stream: !done });
      buffer = buffer.replace(/\r\n?/g, "\n");
      let boundary = buffer.indexOf("\n\n");
      while (boundary !== -1) {
        inspectSseBlock(buffer.slice(0, boundary), state);
        buffer = buffer.slice(boundary + 2);
        boundary = buffer.indexOf("\n\n");
      }
      // Bound only an incomplete event. Complete events are processed first so
      // a large text delta cannot evict the earlier search-completed event.
      if (buffer.length > MAX_SSE_BUFFER_CHARS) {
        state.parseErrors = (state.parseErrors ?? 0) + 1;
        buffer = buffer.slice(-MAX_SSE_BUFFER_CHARS);
      }
      if (done) break;
    }
    if (buffer.trim()) inspectSseBlock(buffer, state);
  } finally {
    reader.releaseLock();
  }
}

export function createNativeSearchObserver(fetchImpl = globalThis.fetch, options = {}) {
  const state = { invoked: false, sources: new Map(), outputText: [], streamedText: [], parseErrors: 0 };
  const inspections = [];
  return {
    state,
    async fetch(input, init) {
      const response = await fetchImpl(input, init);
      if (response.ok) {
        try {
          const inspection = inspectNativeSearchResponse(response.clone(), state)
            .catch(() => { state.parseErrors += 1; });
          inspections.push(inspection);
        } catch {
          state.parseErrors += 1;
        }
      }
      return response;
    },
    async finish() {
      await Promise.all(inspections);
      // Regex extraction is retained only for optional gap-fill attribution.
      // Grounded mode must have provider-owned citation/search metadata.
      if (options.allowUnverifiedFallback && state.sources.size === 0 && state.invoked) {
        const text = state.outputText.join("\n") || state.streamedText.join("");
        for (const match of text.matchAll(/https?:\/\/[^\s"'<>\\]+/g)) {
          let url = match[0];
          while (/[.,!?;:\])}]+$/.test(url)) url = url.slice(0, -1);
          addSource(state, { url, title: url });
        }
      }
      return state;
    },
  };
}
