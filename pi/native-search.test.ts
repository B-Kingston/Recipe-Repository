import assert from "node:assert/strict";
import test from "node:test";
import {
  anthropicSearchPayload,
  createNativeSearchObserver,
  inspectNativeSearchResponse,
  nativeSearchPayload,
  normalizeSourceUrl,
  optionalSearchPayload,
  responsesSearchPayload,
  webSearchCompleted,
} from "./native-search.ts";

test("nativeSearchPayload forces OpenAI hosted web search", () => {
  const original = { model: "gpt-test", tools: [{ type: "function", name: "old" }] };
  const payload = nativeSearchPayload(original);

  assert.deepEqual(payload.tools, [{ type: "web_search", search_context_size: "high" }]);
  assert.deepEqual(payload.tool_choice, { type: "web_search" });
  assert.deepEqual(original.tools, [{ type: "function", name: "old" }]);
});

test("optionalSearchPayload attaches web_search but leaves tool_choice free", () => {
  const payload = optionalSearchPayload({ model: "gpt-test", tools: [] });

  assert.deepEqual(payload.tools, [{ type: "web_search", search_context_size: "high" }]);
  assert.ok(!("tool_choice" in payload));
});

test("responsesSearchPayload adds web_search beside the function tools", () => {
  const original = { model: "gpt-test", tools: [{ type: "function", name: "recipe_notes" }] };
  const payload = responsesSearchPayload(original);

  assert.deepEqual(payload.tools, [{ type: "function", name: "recipe_notes" }, { type: "web_search", search_context_size: "medium" }]);
  assert.ok(!("tool_choice" in payload));
  assert.equal(original.tools.length, 1);
  assert.deepEqual(responsesSearchPayload({}).tools, [{ type: "web_search", search_context_size: "medium" }]);
});

test("SSE inspection requires a native call and collects citation annotations", async () => {
  const events = [
    { type: "response.web_search_call.completed", item_id: "ws_1" },
    {
      type: "response.output_item.done",
      item: {
        type: "message",
        content: [{
          type: "output_text",
          text: "answer",
          annotations: [
            { type: "url_citation", title: "Recipe", url: "https://example.com/recipe" },
            { type: "url_citation", title: "Duplicate", url: "https://example.com/recipe" },
          ],
        }],
      },
    },
  ];
  const sse = events.map((event) => `data: ${JSON.stringify(event)}\n\n`).join("");
  const state = { invoked: false, sources: new Map() };

  await inspectNativeSearchResponse(new Response(sse), state);

  assert.equal(state.invoked, true);
  assert.deepEqual([...state.sources], [["https://example.com/recipe", "Recipe"]]);
});

test("a web_search_call counts once it completed, not while it is still running", () => {
  assert.equal(webSearchCompleted({ type: "web_search_call", status: "completed" }), true);
  assert.equal(webSearchCompleted({ type: "web_search_call", action: "search" }), true, "a shape without a status");
  assert.equal(webSearchCompleted({ type: "web_search_call", status: "in_progress", action: { type: "search" } }), false);
  assert.equal(webSearchCompleted({ type: "web_search_call", status: "searching", action: { type: "search" } }), false);
  assert.equal(webSearchCompleted({ type: "web_search_call", status: "failed", action: { type: "search" } }), false);
});

test("anthropicSearchPayload keeps existing tools and never forces tool_choice", () => {
  const original = { model: "claude-test", tools: [{ name: "fn" }], thinking: { type: "adaptive" } };
  const payload = anthropicSearchPayload(original);

  assert.deepEqual(payload.tools, [{ name: "fn" }, { type: "web_search_20250305", name: "web_search" }]);
  assert.ok(!("tool_choice" in payload));
  assert.deepEqual(original.tools, [{ name: "fn" }]);
  assert.deepEqual(anthropicSearchPayload({}, 5).tools, [{ type: "web_search_20250305", name: "web_search", max_uses: 5 }]);
});

test("Anthropic SSE marks search invoked only on a result block and collects sources", async () => {
  const events = [
    { type: "content_block_start", index: 0, content_block: { type: "server_tool_use", name: "web_search" } },
    {
      type: "content_block_start",
      index: 1,
      content_block: {
        type: "web_search_tool_result",
        content: [
          { type: "web_search_result", url: "https://example.com/a/", title: "A" },
          { type: "web_search_result", url: "ftp://example.com/x", title: "bad scheme" },
        ],
      },
    },
    { type: "content_block_delta", index: 2, delta: { type: "text_delta", text: "{\"recipe\"" } },
    {
      type: "content_block_delta",
      index: 2,
      delta: { type: "citations_delta", citation: { type: "web_search_result_location", url: "https://example.com/b", title: "B" } },
    },
  ];
  const sse = events.map((event) => `event: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`).join("");
  const state = { invoked: false, sources: new Map() };

  await inspectNativeSearchResponse(new Response(sse), state);

  assert.equal(state.invoked, true);
  assert.deepEqual([...state.sources], [["https://example.com/a", "A"], ["https://example.com/b", "B"]]);
});

test("Anthropic server_tool_use alone does not count as a completed search", async () => {
  const event = { type: "content_block_start", index: 0, content_block: { type: "server_tool_use", name: "web_search" } };
  const state = { invoked: false, sources: new Map() };

  await inspectNativeSearchResponse(new Response(`data: ${JSON.stringify(event)}\n\n`), state);

  assert.equal(state.invoked, false);
});

test("observer leaves non-search responses ungrounded", async () => {
  const observer = createNativeSearchObserver(async () => new Response(
    `data: ${JSON.stringify({ type: "response.completed", response: { output: [] } })}\n\n`,
    { status: 200 },
  ));

  await observer.fetch("https://example.test");
  const state = await observer.finish();

  assert.equal(state.invoked, false);
  assert.equal(state.sources.size, 0);
});

test("observer ignores malformed SSE events without rejecting the response", async () => {
  const observer = createNativeSearchObserver(async () => new Response(
    "data: not-json\n\ndata: {\"type\":\"response.web_search_call.completed\"}\n\n",
    { status: 200 },
  ));

  await observer.fetch("https://example.test");
  const state = await observer.finish();

  assert.equal(state.invoked, true);
});

test("observer reads URLs only from citations, never from the reply text", async () => {
  const observer = createNativeSearchObserver(async () => new Response(
    [
      { type: "response.web_search_call.completed" },
      {
        type: "response.output_item.done",
        item: { type: "message", content: [{ type: "output_text", text: "https://example.com/recipe", annotations: [] }] },
      },
    ].map((event) => `data: ${JSON.stringify(event)}\n\n`).join(""),
    { status: 200 },
  ));

  await observer.fetch("https://example.test");
  const state = await observer.finish();

  assert.equal(state.invoked, true);
  assert.equal(state.sources.size, 0);
});

test("observer reports the retried response, not a search the abandoned attempt ran", async () => {
  const searched = [
    { type: "response.web_search_call.completed" },
    { type: "response.output_text.annotation.added", annotation: { type: "url_citation", url: "https://example.com/a", title: "A" } },
  ];
  const responses = [
    new Response(searched.map((event) => `data: ${JSON.stringify(event)}\n\n`).join(""), { status: 200 }),
    new Response(`data: ${JSON.stringify({ type: "response.completed", response: { output: [] } })}\n\n`, { status: 200 }),
  ];
  const observer = createNativeSearchObserver(async () => responses.shift()!);

  await observer.fetch("https://example.test");
  await observer.fetch("https://example.test");
  const state = await observer.finish();

  assert.equal(state.invoked, false);
  assert.equal(state.sources.size, 0);
});

test("source URLs are normalized before citation matching", () => {
  assert.equal(normalizeSourceUrl("https://example.com/recipe/#step-1"), "https://example.com/recipe");
  assert.equal(normalizeSourceUrl("https://example.com/"), "https://example.com/");
  assert.equal(normalizeSourceUrl("ftp://example.com/recipe"), null);
});

test("search payload helpers reject non-object payloads and name their provider", () => {
  assert.throws(() => nativeSearchPayload(null), /invalid Codex request payload/);
  assert.throws(() => optionalSearchPayload([]), /invalid Codex request payload/);
  assert.throws(() => anthropicSearchPayload("x"), /invalid Anthropic request payload/);
});
