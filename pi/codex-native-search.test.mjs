import assert from "node:assert/strict";
import test from "node:test";
import { completionContext } from "./recipe-worker.mjs";
import {
  createNativeSearchObserver,
  inspectNativeSearchResponse,
  nativeSearchPayload,
  normalizeSourceUrl,
  optionalSearchPayload,
} from "./codex-native-search.mjs";

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
  const state = { invoked: false, sources: new Map(), outputText: [], streamedText: [] };

  await inspectNativeSearchResponse(new Response(sse), state);

  assert.equal(state.invoked, true);
  assert.deepEqual([...state.sources], [["https://example.com/recipe", "Recipe"]]);
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

test("observer mirrors OMP's text URL fallback after a verified native search", async () => {
  const events = [
    { type: "response.web_search_call.completed", item_id: "ws_1" },
    {
      type: "response.output_item.done",
      item: {
        type: "message",
        content: [{
          type: "output_text",
          text: '{"sources":[{"url":"https://example.com/recipe"}]}',
          annotations: [],
        }],
      },
    },
  ];
  const observer = createNativeSearchObserver(async () => new Response(
    events.map((event) => `data: ${JSON.stringify(event)}\n\n`).join(""),
    { status: 200 },
  ), { allowUnverifiedFallback: true });

  await observer.fetch("https://example.test");
  const state = await observer.finish();

  assert.equal(state.invoked, true);
  assert.deepEqual([...state.sources], [["https://example.com/recipe", "https://example.com/recipe"]]);
});

test("observer ignores malformed SSE events without rejecting the response", async () => {
  const observer = createNativeSearchObserver(async () => new Response(
    "data: not-json\n\ndata: {\"type\":\"response.web_search_call.completed\"}\n\n",
    { status: 200 },
  ));

  await observer.fetch("https://example.test");
  const state = await observer.finish();

  assert.equal(state.invoked, true);
  assert.equal(state.parseErrors, 1);
});

test("observer does not accept text URL fallback unless explicitly enabled", async () => {
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

test("source URLs are normalized before citation matching", () => {
  assert.equal(normalizeSourceUrl("https://example.com/recipe/#step-1"), "https://example.com/recipe");
  assert.equal(normalizeSourceUrl("https://example.com/"), "https://example.com/");
  assert.equal(normalizeSourceUrl("ftp://example.com/recipe"), null);
});

test("recipe worker sends only its explicit non-coding completion context", () => {
  const context = completionContext({
    prompt: "Make a tomato pasta",
    systemPrompt: "You are a recipe assistant.",
  }, false);

  assert.equal(context.systemPrompt.startsWith("You are a recipe assistant."), true);
  assert.equal(context.systemPrompt.includes("coding assistant"), false);
  assert.deepEqual(context.tools, []);
  assert.equal(context.messages.length, 1);
  assert.equal(context.messages[0].role, "user");
  assert.equal(context.messages[0].content, "Make a tomato pasta");
});
