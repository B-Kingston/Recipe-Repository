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

/** Whether a Responses API web_search_call item ran to completion. */
export function webSearchCompleted(item) {
  const actionType = typeof item.action === "string" ? item.action : item.action?.type;
  return item.status === "completed" || (actionType === "search" && item.status !== "failed");
}

function collectItem(state, item) {
  if (!item || typeof item !== "object") return;
  if (item.type === "web_search_call") {
    if (webSearchCompleted(item)) state.invoked = true;
    for (const source of item.action?.sources ?? []) addSource(state, source);
  }
  collectContent(state, item.content);
}

function requestPayload(payload, provider) {
  if (!payload || typeof payload !== "object" || Array.isArray(payload)) {
    throw new Error(`Pi produced an invalid ${provider} request payload.`);
  }
  return payload;
}

/** Forces the hosted web_search tool onto a Codex Responses payload. */
export function nativeSearchPayload(payload) {
  return {
    ...requestPayload(payload, "Codex"),
    tools: [{ type: "web_search", search_context_size: "high" }],
    tool_choice: { type: "web_search" },
  };
}

/** Attaches the hosted web_search tool without forcing its use; gap-fill
 * generation over imported video evidence may stay unsourced when the
 * cleaned evidence alone answers the request. */
export function optionalSearchPayload(payload) {
  return {
    ...requestPayload(payload, "Codex"),
    tools: [{ type: "web_search", search_context_size: "high" }],
  };
}

/** Attaches Anthropic's server-side web_search tool to a Messages API payload.
 * The tool is never forced with tool_choice: Anthropic rejects a forced tool
 * while extended thinking is on. Grounded mode relies on the prompt and on the
 * worker's check that a search result arrived. */
export function anthropicSearchPayload(payload) {
  const body = requestPayload(payload, "Anthropic");
  const tools = Array.isArray(body.tools) ? body.tools : [];
  return { ...body, tools: [...tools, { type: "web_search_20250305", name: "web_search" }] };
}

/** Anthropic SSE: web_search_tool_result blocks carry the search results;
 * citations_delta events carry the citations on the answer text. */
function collectAnthropicEvent(state, event) {
  const block = event.type === "content_block_start" ? event.content_block : null;
  if (block?.type === "web_search_tool_result" && Array.isArray(block.content)) {
    state.invoked = true;
    for (const result of block.content) {
      if (result?.type === "web_search_result") addSource(state, result);
    }
  }
  if (event.type === "content_block_delta") {
    const delta = event.delta;
    if (delta?.type === "text_delta" && typeof delta.text === "string") state.streamedText.push(delta.text);
    if (delta?.type === "citations_delta" && delta.citation?.type === "web_search_result_location") {
      addSource(state, delta.citation);
    }
  }
}

function collectNativeSearchEvent(state, event) {
  if (!event || typeof event !== "object") return;
  if (event.type === "content_block_start" || event.type === "content_block_delta") {
    collectAnthropicEvent(state, event);
    return;
  }
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
        inspections.push(
          inspectNativeSearchResponse(response.clone(), state).catch(() => {
            state.parseErrors += 1;
          }),
        );
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
