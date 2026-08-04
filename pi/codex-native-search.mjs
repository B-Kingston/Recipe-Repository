const WEB_SEARCH_EVENT_PREFIX = "response.web_search_call";

function addSource(state, source) {
  if (!source || typeof source.url !== "string" || !/^https?:\/\//.test(source.url)) return;
  if (!state.sources.has(source.url)) {
    state.sources.set(
      source.url,
      typeof source.title === "string" && source.title.trim() ? source.title.trim() : source.url,
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
    state.invoked = true;
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

export function collectNativeSearchEvent(state, event) {
  if (!event || typeof event !== "object") return;
  if (typeof event.type === "string" && event.type.startsWith(WEB_SEARCH_EVENT_PREFIX)) {
    state.invoked = true;
  }
  if (event.type === "response.output_text.annotation.added" && event.annotation?.type === "url_citation") {
    addSource(state, event.annotation);
  }
  if (event.type === "response.output_text.delta" && typeof event.delta === "string") {
    state.streamedText.push(event.delta);
  }
  collectItem(state, event.item);
  for (const item of event.response?.output ?? []) collectItem(state, item);
}

export async function inspectNativeSearchResponse(response, state) {
  const text = await response.text();
  for (const block of text.split(/\r?\n\r?\n/)) {
    const data = block
      .split(/\r?\n/)
      .filter((line) => line.startsWith("data:"))
      .map((line) => line.slice(5).trim())
      .join("\n");
    if (!data || data === "[DONE]") continue;
    collectNativeSearchEvent(state, JSON.parse(data));
  }
}

export function createNativeSearchObserver(fetchImpl = globalThis.fetch) {
  const state = { invoked: false, sources: new Map(), outputText: [], streamedText: [] };
  let inspection = Promise.resolve();
  return {
    state,
    async fetch(input, init) {
      const response = await fetchImpl(input, init);
      if (response.ok) inspection = inspectNativeSearchResponse(response.clone(), state);
      return response;
    },
    async finish() {
      await inspection;
      if (state.sources.size === 0 && state.invoked) {
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
