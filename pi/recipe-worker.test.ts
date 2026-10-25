import test from "node:test";
import assert from "node:assert/strict";
import {
  WorkerError,
  anthropicMessages,
  anthropicThinking,
  anthropicTurn,
  anthropicUrl,
  claudeGeneration,
  catalogModelIds,
  completionContext,
  formatRecipeEvidence,
  cleanedEvidence,
  completeViaPiAi,
  generateRecipe,
  isAuthFailure,
  isPiAiSpec,
  normalizeEffort,
  openaiResponse,
  parseAnthropicOutput,
  parseRequest,
  parseResponsesOutput,
  piAiModel,
  piAiModels,
  recipePrompt,
  searchModeOf,
  subscriptionOf,
  listEndpointModels,
  verifiedSources,
  workerFailure,
} from "./recipe-worker.ts";
import type { Complete, CompletionOptions } from "./recipe-worker.ts";
import type { Model } from "@earendil-works/pi-ai";

/** What a fake fetch receives: the worker sends string headers and a JSON string body. */
interface SentInit {
  method: string;
  headers: Record<string, string>;
  body: string;
}

/** A fake fetch whose responses carry only the fields the worker reads. */
function fakeFetch(respond: (url: string, init: SentInit) => Promise<Partial<Response>>): typeof fetch {
  return respond as unknown as typeof fetch;
}

test("catalogModelIds lists the 5.6 range first, newest to oldest", () => {
  const models = [
    { id: "gpt-5.4" },
    { id: "gpt-5.6-sol" },
    { id: "gpt-5.3-codex-spark" },
    { id: "gpt-5.6-terra" },
    { id: "gpt-5.5" },
    { id: "gpt-5.4-mini" },
    { id: "gpt-5.6-luna" },
  ];
  assert.deepEqual(catalogModelIds(models), [
    "gpt-5.6-terra",
    "gpt-5.6-sol",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.4-mini",
    "gpt-5.4",
    "gpt-5.3-codex-spark",
  ]);
});

test("catalogModelIds dedupes ids and drops entries without a string id", () => {
  const models = [
    { id: "gpt-5.6-sol" },
    { id: "gpt-5.6-sol" },
    { name: "no id" },
    {},
    { id: "gpt-5.4-mini" },
  ];
  assert.deepEqual(catalogModelIds(models), ["gpt-5.6-sol", "gpt-5.4-mini"]);
});

test("subscriptionOf defaults to Codex and maps Claude's piProvider", () => {
  assert.equal(subscriptionOf({}).id, "openai-codex");
  assert.equal(subscriptionOf({ piProvider: "  " }).id, "openai-codex");
  assert.equal(subscriptionOf({ piProvider: "anthropic" }).label, "Claude");
});

test("subscriptionOf rejects unknown providers, including inherited Object keys", () => {
  for (const piProvider of ["google", "constructor", "__proto__"]) {
    assert.throws(() => subscriptionOf({ piProvider }), (error) => {
      assert.ok(error instanceof WorkerError);
      assert.equal(error.code, "configuration");
      return true;
    });
  }
});

test("parseRequest accepts listModels without a prompt but requires one otherwise", () => {
  assert.deepEqual(parseRequest('{"command":"listModels"}'), { command: "listModels" });
  assert.deepEqual(
    parseRequest('{"prompt":"p","systemPrompt":"s"}'),
    { prompt: "p", systemPrompt: "s" },
  );
  for (const input of ["[]", "null", '"text"', '{"prompt":"p"}', "not json"]) {
    assert.throws(() => parseRequest(input), (error) => {
      assert.ok(error instanceof WorkerError);
      assert.equal(error.code, "input");
      return true;
    });
  }
});

test("openaiResponse posts to /responses with web_search when search is enabled", async () => {
  let capturedUrl!: string;
  let capturedInit!: SentInit;
  const fetchImpl = fakeFetch(async (url, init) => {
    capturedUrl = url;
    capturedInit = init;
    return { ok: true, json: async () => ({ status: "completed" }) };
  });
  const body = await openaiResponse(
    "https://api.openai.com/v1",
    "sk-test",
    "gpt-5.6-luna",
    "sys",
    "prompt",
    "grounded",
    "low",
    fetchImpl,
  );
  assert.equal(capturedUrl, "https://api.openai.com/v1/responses");
  assert.equal(capturedInit.method, "POST");
  assert.equal(capturedInit.headers["Content-Type"], "application/json");
  assert.equal(capturedInit.headers.Authorization, "Bearer sk-test");
  const sent = JSON.parse(capturedInit.body);
  assert.equal(sent.model, "gpt-5.6-luna");
  assert.deepEqual(sent.input[0], { role: "system", content: "sys" });
  assert.deepEqual(sent.input[1], { role: "user", content: "prompt" });
  assert.deepEqual(sent.tools, [{ type: "web_search" }]);
  assert.equal(sent.tool_choice, "required");
  assert.deepEqual(sent.reasoning, { effort: "low" });
  assert.deepEqual(sent.text, { verbosity: "low" });
  assert.deepEqual(body, { status: "completed" });
});

test("openaiResponse omits tools and tool_choice when search is disabled", async () => {
  let capturedInit!: SentInit;
  const fetchImpl = fakeFetch(async (url, init) => {
    capturedInit = init;
    return { ok: true, json: async () => ({}) };
  });
  await openaiResponse("https://api.openai.com/v1", "sk-test", "gpt-5.6-luna", "sys", "prompt", "off", "low", fetchImpl);
  const sent = JSON.parse(capturedInit.body);
  assert.ok(!("tools" in sent));
  assert.ok(!("tool_choice" in sent));
});

test("openaiResponse strips a trailing slash from the base URL", async () => {
  let capturedUrl!: string;
  const fetchImpl = fakeFetch(async (url) => {
    capturedUrl = url;
    return { ok: true, json: async () => ({}) };
  });
  await openaiResponse("https://api.openai.com/v1/", "sk-test", "gpt-5.6-luna", "sys", "prompt", "off", "low", fetchImpl);
  assert.equal(capturedUrl, "https://api.openai.com/v1/responses");
});

test("openaiResponse maps a non-2xx response to a WorkerError and marks transient failures retryable", async () => {
  const rejects = (status: number) => openaiResponse(
    "https://api.openai.com/v1",
    "sk-test",
    "gpt-5.6-luna",
    "sys",
    "prompt",
    "off",
    "low",
    fakeFetch(async () => ({ ok: false, status, text: async () => "bad key" })),
  );
  await assert.rejects(rejects(401), (error) => {
    assert.ok(error instanceof WorkerError);
    assert.equal(error.code, "worker");
    assert.equal(error.retryable, false);
    assert.ok(error.message.includes("HTTP 401"));
    return true;
  });
  await assert.rejects(rejects(503), (error) => {
    assert.ok(error instanceof WorkerError);
    assert.equal(error.retryable, true);
    return true;
  });
});

test("openaiResponse offers web_search without forcing it in gap-fill mode", async () => {
  let capturedInit!: SentInit;
  const fetchImpl = fakeFetch(async (url, init) => {
    capturedInit = init;
    return { ok: true, json: async () => ({}) };
  });
  await openaiResponse("https://api.openai.com/v1", "sk-test", "gpt-5.6-luna", "sys", "prompt", "gapfill", "low", fetchImpl);
  const sent = JSON.parse(capturedInit.body);
  assert.deepEqual(sent.tools, [{ type: "web_search" }]);
  assert.equal(sent.tool_choice, "auto");
});

test("recipePrompt tail matches the request's search mode", () => {
  assert.ok(recipePrompt("sys", "off").includes("Use an empty sources array."));
  assert.ok(recipePrompt("sys", "grounded").includes("Research before preparing the recipe."));
  const gapfill = recipePrompt("sys", "gapfill");
  assert.ok(gapfill.includes("available but not required"));
  assert.ok(gapfill.includes("imported social video"));
});

test("completionContext sends only the explicit non-coding context", () => {
  const context = completionContext({
    prompt: "Make a tomato pasta",
    systemPrompt: "You are a recipe assistant.",
  }, "off");

  assert.equal(context.systemPrompt?.startsWith("You are a recipe assistant."), true);
  assert.equal(context.systemPrompt?.includes("coding assistant"), false);
  assert.deepEqual(context.tools, []);
  assert.equal(context.messages.length, 1);
  assert.equal(context.messages[0].role, "user");
  assert.equal(context.messages[0].content, "Make a tomato pasta");
});

test("searchModeOf accepts only the three known modes", () => {
  assert.equal(searchModeOf({}), "off");
  assert.equal(searchModeOf({ searchMode: "nonsense" }), "off");
  assert.equal(searchModeOf({ searchMode: " grounded " }), "grounded");
  assert.equal(searchModeOf({ searchMode: "gapfill" }), "gapfill");
});

test("cleanedEvidence keeps recipe fields and discards cleaner prose and unknown fields", () => {
  const cleaned = cleanedEvidence("```json\n" + JSON.stringify({
      title: "Chilli tofu",
      ingredients: [{ quantity: "200", unit: "g", name: "tofu" }, "1 tbsp oil"],
      steps: ["Crisp the tofu"],
      timings: ["Fry for 5 minutes"],
      relevant_notes: ["Serve hot"],
      rambling: "Follow me for more recipes",
    }) + "\n```");
  assert.equal(cleaned, "Dish: Chilli tofu\nIngredients:\n- 200 g tofu\n- 1 tbsp oil\nMethod:\n1. Crisp the tofu\nTimings and temperatures:\n- Fry for 5 minutes\nRelevant recipe notes:\n- Serve hot");
  assert.ok(!cleaned.includes("Follow me"));
});

test("cleanedEvidence finds the JSON object between prose that contains braces", () => {
  const object = JSON.stringify({ title: "Soup", steps: ["Simmer {gently}"] });
  const cleaned = cleanedEvidence(`Here it is: ${object} (see {ref})`);
  assert.equal(cleaned, "Dish: Soup\nMethod:\n1. Simmer {gently}");
});

test("formatRecipeEvidence rejects cleaner output without ingredients or steps", () => {
  assert.throws(() => formatRecipeEvidence({ title: "A story", relevant_notes: ["Like and follow"] }), (error) => {
    assert.ok(error instanceof WorkerError);
    assert.equal(error.code, "output");
    return true;
  });
});

test("anthropicUrl maps base URLs to the Messages API paths", () => {
  assert.equal(anthropicUrl("https://api.anthropic.com"), "https://api.anthropic.com/v1/messages");
  assert.equal(anthropicUrl("https://api.anthropic.com/"), "https://api.anthropic.com/v1/messages");
  assert.equal(anthropicUrl("https://api.anthropic.com/v1"), "https://api.anthropic.com/v1/messages");
  assert.equal(anthropicUrl("https://proxy.example.com/anthropic/v1/"), "https://proxy.example.com/anthropic/v1/messages");
  assert.equal(anthropicUrl("https://api.anthropic.com", "models"), "https://api.anthropic.com/v1/models");
});

test("anthropicMessages posts to /v1/messages with a cached system prompt and a named web_search tool", async () => {
  let capturedUrl!: string;
  let capturedInit!: SentInit;
  const fetchImpl = fakeFetch(async (url, init) => {
    capturedUrl = url;
    capturedInit = init;
    return { ok: true, json: async () => ({ type: "message", content: [] }) };
  });
  const body = await anthropicMessages(
    "https://api.anthropic.com",
    "sk-ant-test",
    "claude-sonnet-5-5",
    "sys",
    [{ role: "user", content: "prompt" }],
    "medium",
    "grounded",
    fetchImpl,
  );
  assert.equal(capturedUrl, "https://api.anthropic.com/v1/messages");
  assert.equal(capturedInit.method, "POST");
  assert.equal(capturedInit.headers["Content-Type"], "application/json");
  assert.equal(capturedInit.headers["x-api-key"], "sk-ant-test");
  assert.equal(capturedInit.headers["anthropic-version"], "2023-06-01");
  const sent = JSON.parse(capturedInit.body);
  assert.equal(sent.model, "claude-sonnet-5-5");
  assert.deepEqual(sent.system, [{ type: "text", text: "sys", cache_control: { type: "ephemeral" } }]);
  assert.ok(!("cache_control" in sent));
  assert.deepEqual(sent.messages, [{ role: "user", content: "prompt" }]);
  assert.deepEqual(sent.tools, [{ type: "web_search_20250305", name: "web_search" }]);
  assert.deepEqual(sent.thinking, { type: "adaptive" });
  assert.deepEqual(sent.output_config, { effort: "medium" });
  assert.deepEqual(body, { type: "message", content: [] });
});

test("claudeGeneration reads both model id orders and platform prefixes", () => {
  assert.deepEqual(claudeGeneration("claude-sonnet-5-5"), { major: 5, minor: 5 });
  assert.deepEqual(claudeGeneration("claude-opus-4-5-20251101"), { major: 4, minor: 5 });
  assert.deepEqual(claudeGeneration("claude-opus-4-20250514"), { major: 4, minor: 0 });
  assert.deepEqual(claudeGeneration("claude-3-7-sonnet-20250219"), { major: 3, minor: 7 });
  assert.deepEqual(claudeGeneration("anthropic.claude-sonnet-4-6"), { major: 4, minor: 6 });
  assert.equal(claudeGeneration("my-proxy-alias"), null);
});

test("anthropicThinking uses adaptive effort from 4.6 on and budgets before it", () => {
  assert.deepEqual(anthropicThinking("claude-opus-5-5", "xhigh"), {
    max_tokens: 32_000,
    thinking: { type: "adaptive" },
    output_config: { effort: "xhigh" },
  });
  // The 4.6 models have no xhigh effort.
  assert.deepEqual(anthropicThinking("claude-sonnet-4-6", "xhigh").output_config, { effort: "max" });
  // An unrecognised id is treated as a current model.
  assert.deepEqual(anthropicThinking("proxy-model", "").output_config, { effort: "low" });
  assert.deepEqual(anthropicThinking("claude-haiku-4-5", "low"), {
    max_tokens: 1024 + 8192,
    thinking: { type: "enabled", budget_tokens: 1024 },
  });
  assert.deepEqual(anthropicThinking("claude-sonnet-4-5-20250929", "max").thinking, { type: "enabled", budget_tokens: 8192 });
});

test("anthropicTurn continues a pause_turn with the paused content and joins all content", async () => {
  const sent: { messages: unknown }[] = [];
  const replies = [
    { stop_reason: "pause_turn", content: [{ type: "server_tool_use", id: "s1", name: "web_search", input: {} }] },
    { stop_reason: "pause_turn", content: [{ type: "web_search_tool_result", tool_use_id: "s1", content: [] }] },
    { stop_reason: "end_turn", content: [{ type: "text", text: "done" }] },
  ];
  const fetchImpl = fakeFetch(async (url, init) => {
    sent.push(JSON.parse(init.body));
    return { ok: true, json: async () => replies[sent.length - 1] };
  });
  const result = await anthropicTurn("https://api.anthropic.com", "k", "claude-opus-5-5", "s", "p", "low", "grounded", fetchImpl);
  assert.equal(sent.length, 3);
  assert.deepEqual(sent[0].messages, [{ role: "user", content: "p" }]);
  assert.deepEqual(sent[2].messages, [
    { role: "user", content: "p" },
    { role: "assistant", content: [...replies[0].content, ...replies[1].content] },
  ]);
  assert.equal(result.stop_reason, "end_turn");
  assert.deepEqual(result.content, [...replies[0].content, ...replies[1].content, ...replies[2].content]);
});

test("anthropicTurn gives up on a turn that keeps pausing", async () => {
  let calls = 0;
  const fetchImpl = fakeFetch(async () => {
    calls += 1;
    return { ok: true, json: async () => ({ stop_reason: "pause_turn", content: [] }) };
  });
  await assert.rejects(
    anthropicTurn("https://api.anthropic.com", "k", "claude-opus-5-5", "s", "p", "low", "grounded", fetchImpl),
    (error) => error instanceof WorkerError && error.retryable,
  );
  assert.equal(calls, 6);
});

test("listEndpointModels follows Anthropic pagination and keeps its newest-first order", async () => {
  const urls: string[] = [];
  const pages = [
    { data: [{ id: "claude-opus-5-5" }, { id: "claude-sonnet-5-5" }], has_more: true, last_id: "claude-sonnet-5-5" },
    { data: [{ id: "claude-haiku-4-5" }], has_more: false, last_id: "claude-haiku-4-5" },
  ];
  const fetchImpl = fakeFetch(async (url, init) => {
    urls.push(url);
    assert.equal(init.method, "GET");
    assert.equal(init.headers["x-api-key"], "k");
    assert.equal(init.headers["anthropic-version"], "2023-06-01");
    return { ok: true, json: async () => pages[urls.length - 1] };
  });
  const models = await listEndpointModels({ provider: "anthropic", apiBaseUrl: "https://api.anthropic.com/", apiKey: "k" }, fetchImpl);
  assert.deepEqual(models, ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-4-5"]);
  assert.equal(urls[0], "https://api.anthropic.com/v1/models?limit=1000");
  assert.equal(urls[1], "https://api.anthropic.com/v1/models?limit=1000&after_id=claude-sonnet-5-5");
});

test("listEndpointModels sorts an OpenAI-spec list newest first", async () => {
  let captured!: { url: string; init: SentInit };
  const fetchImpl = fakeFetch(async (url, init) => {
    captured = { url, init };
    return {
      ok: true,
      json: async () => ({ data: [{ id: "old", created: 1 }, { id: "new", created: 3 }, { id: "mid", created: 2 }, { id: "new", created: 3 }] }),
    };
  });
  const models = await listEndpointModels({ provider: "openai", apiBaseUrl: "https://api.openai.com/v1", apiKey: "sk" }, fetchImpl);
  assert.deepEqual(models, ["new", "mid", "old"]);
  assert.equal(captured.url, "https://api.openai.com/v1/models");
  assert.equal(captured.init.headers.Authorization, "Bearer sk");
});

test("listEndpointModels requires a base URL and key", async () => {
  await assert.rejects(listEndpointModels({ provider: "openai", apiKey: "sk" }), (error) => error instanceof WorkerError && error.code === "input");
});

test("parseAnthropicOutput reads web_search_result items and text citations", () => {
  const { text, searched, refused, citations } = parseAnthropicOutput({
    stop_reason: "end_turn",
    content: [
      { type: "text", text: "Here is the recipe ", citations: null },
      {
        type: "text",
        text: "and more.",
        citations: [{ type: "web_search_result_location", url: "https://example.org/cited", title: "Cited", cited_text: "x", encrypted_index: "e" }],
      },
      {
        type: "web_search_tool_result",
        tool_use_id: "srvtoolu_1",
        content: [
          { type: "web_search_result", url: "https://example.com/sourdough/", title: "Sourdough basics", encrypted_content: "e", page_age: null },
          { type: "web_search_result", url: "ftp://example.com/x", title: "bad scheme", encrypted_content: "e", page_age: null },
        ],
      },
    ],
  });
  assert.equal(text, "Here is the recipe and more.");
  assert.equal(searched, true);
  assert.equal(refused, false);
  assert.deepEqual([...citations], [
    ["https://example.org/cited", "Cited"],
    ["https://example.com/sourdough", "Sourdough basics"],
  ]);
});

test("parseAnthropicOutput flags refusals and missing searches", () => {
  const refused = parseAnthropicOutput({ stop_reason: "refusal", content: [] });
  assert.equal(refused.refused, true);
  const noSearch = parseAnthropicOutput({ stop_reason: "end_turn", content: [{ type: "text", text: "hi" }] });
  assert.equal(noSearch.searched, false);
  assert.equal(noSearch.citations.size, 0);
});

test("normalizeEffort defaults missing or blank to low", () => {
  assert.equal(normalizeEffort(undefined), "low");
  assert.equal(normalizeEffort(null), "low");
  assert.equal(normalizeEffort(""), "low");
  assert.equal(normalizeEffort("  "), "low");
});

test("normalizeEffort accepts the supported values case-insensitively", () => {
  assert.equal(normalizeEffort("low"), "low");
  assert.equal(normalizeEffort("Medium"), "medium");
  assert.equal(normalizeEffort(" HIGH "), "high");
  assert.equal(normalizeEffort(" XHIGH "), "xhigh");
  assert.equal(normalizeEffort(" MAX "), "max");
});

test("normalizeEffort rejects unknown values", () => {
  assert.throws(() => normalizeEffort("extreme"), (error) => {
    assert.ok(error instanceof WorkerError);
    assert.equal(error.code, "input");
    return true;
  });
});

test("parseResponsesOutput extracts text, search flag, and citations", () => {
  const { text, searched, refused, citations } = parseResponsesOutput({
    output: [
      {
        type: "web_search_call",
        id: "ws_1",
        status: "completed",
        action: {
          type: "search",
          query: "x",
          sources: [{ title: "Search result", url: "https://example.com/result/#section" }],
        },
      },
      {
        type: "message",
        id: "msg_1",
        status: "completed",
        role: "assistant",
        content: [
          {
            type: "output_text",
            text: "hello",
            annotations: [{ type: "url_citation", url: "https://example.com/x", title: "X" }],
          },
        ],
      },
    ],
  });
  assert.equal(text, "hello");
  assert.equal(searched, true);
  assert.equal(refused, false);
  assert.equal(citations.get("https://example.com/x"), "X");
  assert.equal(citations.get("https://example.com/result"), "Search result");
});

test("parseResponsesOutput flags refusal parts", () => {
  const { refused } = parseResponsesOutput({
    output: [{ type: "message", content: [{ type: "refusal" }] }],
  });
  assert.equal(refused, true);
});

test("parseResponsesOutput accepts a string-form web_search_call action", () => {
  const { searched } = parseResponsesOutput({
    output: [{ type: "web_search_call", action: "search" }],
  });
  assert.equal(searched, true);
});

test("parseResponsesOutput concatenates multiple output_text parts", () => {
  const { text } = parseResponsesOutput({
    output: [
      {
        type: "message",
        content: [
          { type: "output_text", text: "part one " },
          { type: "output_text", text: "part two" },
        ],
      },
    ],
  });
  assert.equal(text, "part one part two");
});

test("verifiedSources keeps claimed sources that match citations", () => {
  const citations = new Map([
    ["https://example.com/a", "A"],
    ["https://example.com/b", "B"],
  ]);
  const sources = verifiedSources(
    [
      { title: "A", url: "https://example.com/a" },
      { title: "B", url: "https://example.com/b" },
      { title: "Uncited", url: "https://example.com/c" },
      { title: "Dup", url: "https://example.com/a" },
    ],
    citations,
    true,
  );
  assert.deepEqual(sources, [
    { title: "A", url: "https://example.com/a" },
    { title: "B", url: "https://example.com/b" },
  ]);
});

test("verifiedSources drops all claims when citations exist but nothing matches", () => {
  const citations = new Map([["https://example.com/a", "A"]]);
  const sources = verifiedSources(
    [{ title: "Hallucinated", url: "https://example.net/x" }],
    citations,
    true,
  );
  assert.deepEqual(sources, []);
});

test("verifiedSources accepts claimed http(s) URLs when the API returns no citations", () => {
  const sources = verifiedSources(
    [
      { title: "A", url: "https://example.com/a" },
      { title: "", url: "http://example.org/b" },
      { title: "Bad scheme", url: "ftp://example.com/c" },
      { title: "Dup", url: "https://example.com/a" },
    ],
    new Map(),
    true,
  );
  assert.deepEqual(sources, [
    { title: "A", url: "https://example.com/a" },
    { title: "http://example.org/b", url: "http://example.org/b" },
  ]);
});

test("verifiedSources rejects unverifiable claims when the fallback is disabled", () => {
  const sources = verifiedSources(
    [{ title: "A", url: "https://example.com/a" }],
    new Map(),
    false,
  );
  assert.deepEqual(sources, []);
});

/** A Chat Completions SSE stream that streams one assistant text reply. */
function chatCompletionStream(text: string) {
  const chunks = [
    { id: "c1", object: "chat.completion.chunk", created: 1, model: "m", choices: [{ index: 0, delta: { role: "assistant", content: text }, finish_reason: null }] },
    { id: "c1", object: "chat.completion.chunk", created: 1, model: "m", choices: [{ index: 0, delta: {}, finish_reason: "stop" }] },
  ];
  const body = chunks.map((chunk) => `data: ${JSON.stringify(chunk)}\n\n`).join("") + "data: [DONE]\n\n";
  return new Response(body, { status: 200, headers: { "Content-Type": "text/event-stream" } });
}

const RECIPE_REPLY = JSON.stringify({ recipe: { title: "Toast" }, sources: [{ title: "x", url: "https://x.example" }] });

test("isPiAiSpec keeps OpenAI and Anthropic on the native search paths", () => {
  assert.equal(isPiAiSpec("openai"), false);
  assert.equal(isPiAiSpec("anthropic"), false);
  assert.equal(isPiAiSpec("google"), true);
  assert.equal(isPiAiSpec("openai-compatible"), true);
});

test("piAiModel uses the catalogue entry, borrows a shape for unknown ids, and rejects unknown providers", () => {
  const models = piAiModels();
  const known = models.getModels("groq")[0];
  assert.equal(piAiModel(models, "groq", known.id).contextWindow, known.contextWindow);
  const unknown = piAiModel(models, "groq", "brand-new-model");
  assert.equal(unknown.id, "brand-new-model");
  assert.equal(unknown.provider, "groq");
  assert.throws(() => piAiModel(models, "no-such-provider", "m"), (error) => error instanceof WorkerError && error.code === "configuration");
  const custom = piAiModel(models, "openai-compatible", "llama3", "http://ollama:11434/v1/");
  assert.equal(custom.baseUrl, "http://ollama:11434/v1");
  assert.equal((custom as Model<"openai-completions">).compat?.supportsDeveloperRole, false);
  assert.throws(() => piAiModel(models, "openai-compatible", "llama3"), (error) => error instanceof WorkerError && error.code === "configuration");
});

test("completeViaPiAi sends the key and system prompt to a built-in provider", async () => {
  const calls: { url: string; headers: Headers; body: { model: string; messages: unknown } }[] = [];
  const fetchImpl = fakeFetch(async (url, init) => {
    calls.push({ url: String(url), headers: new Headers(init.headers), body: JSON.parse(init.body) });
    return chatCompletionStream(RECIPE_REPLY);
  });
  const result = await completeViaPiAi(
    { provider: "groq", apiKey: "gsk-test", model: "llama-3.3-70b-versatile", prompt: "Toast" },
    { systemPrompt: "Cook. Use an empty sources array.", reasoningEffort: "low" },
    piAiModels(),
    fetchImpl,
  );
  assert.equal(result.text, RECIPE_REPLY);
  assert.equal(result.searched, false);
  assert.equal(calls.length, 1);
  assert.equal(calls[0].url, "https://api.groq.com/openai/v1/chat/completions");
  assert.equal(calls[0].headers.get("authorization"), "Bearer gsk-test");
  assert.equal(calls[0].body.model, "llama-3.3-70b-versatile");
  assert.match(JSON.stringify(calls[0].body.messages), /Use an empty sources array/);
});

test("completeViaPiAi reaches a keyless custom server at its own base URL", async () => {
  const urls: string[] = [];
  const fetchImpl = fakeFetch(async (url) => {
    urls.push(String(url));
    return chatCompletionStream(RECIPE_REPLY);
  });
  const result = await completeViaPiAi(
    { provider: "openai-compatible", apiBaseUrl: "http://ollama:11434/v1", apiKey: "", model: "llama3", prompt: "Toast" },
    { systemPrompt: "Cook.", reasoningEffort: "low" },
    piAiModels(),
    fetchImpl,
  );
  assert.equal(result.text, RECIPE_REPLY);
  assert.deepEqual(urls, ["http://ollama:11434/v1/chat/completions"]);
});

test("completeViaPiAi requires a model, and a key for built-in providers", async () => {
  const options = { systemPrompt: "s", reasoningEffort: "low" } as const;
  await assert.rejects(
    completeViaPiAi({ provider: "groq", apiKey: "k", model: "", prompt: "p" }, options),
    (error) => error instanceof WorkerError && error.code === "configuration",
  );
  await assert.rejects(
    completeViaPiAi({ provider: "groq", apiKey: "", model: "m", prompt: "p" }, options),
    (error) => error instanceof WorkerError && error.code === "configuration",
  );
});

test("completeViaPiAi maps a provider error to a retryable WorkerError", async () => {
  const fetchImpl = fakeFetch(async () => new Response(JSON.stringify({ error: { message: "overloaded" } }), { status: 503, headers: { "Content-Type": "application/json" } }));
  await assert.rejects(
    completeViaPiAi({ provider: "groq", apiKey: "k", model: "llama-3.3-70b-versatile", prompt: "p" }, { systemPrompt: "s", reasoningEffort: "low" }, piAiModels(), fetchImpl),
    (error) => error instanceof WorkerError && error.code === "worker" && error.retryable,
  );
});

test("listEndpointModels reads a built-in provider's catalogue without a request", async () => {
  const fetchImpl = fakeFetch(async () => { throw new Error("no network expected"); });
  const models = await listEndpointModels({ provider: "groq", apiKey: "k" }, fetchImpl);
  assert.ok(models.includes("llama-3.3-70b-versatile"));
});

test("listEndpointModels lists a keyless custom server without an Authorization header", async () => {
  let headers!: Record<string, string>;
  const fetchImpl = fakeFetch(async (url, init) => {
    headers = init.headers;
    return new Response(JSON.stringify({ data: [{ id: "llama3" }] }), { status: 200 });
  });
  const models = await listEndpointModels({ provider: "openai-compatible", apiBaseUrl: "http://ollama:11434/v1" }, fetchImpl);
  assert.deepEqual(models, ["llama3"]);
  assert.equal(headers.Authorization, undefined);
});

test("generateRecipe runs pi-ai providers without search and drops their unverifiable sources", async () => {
  let seen!: CompletionOptions;
  const completeImpl: Complete = async (_request, options) => {
    seen = options;
    return { label: "groq", text: RECIPE_REPLY, citations: new Map(), searched: false };
  };
  const result = await generateRecipe({ provider: "groq", prompt: "p", systemPrompt: "s" }, "gapfill", "low", completeImpl);
  assert.equal(seen.searchMode, "off");
  assert.match(seen.systemPrompt, /Use an empty sources array/);
  assert.deepEqual(result, { recipe: { title: "Toast" }, sources: [] });
});

test("generateRecipe keeps a native endpoint's gap-fill sources when the API returns no citations", async () => {
  let seen!: CompletionOptions;
  const completeImpl: Complete = async (_request, options) => {
    seen = options;
    return { label: "OpenAI", text: RECIPE_REPLY, citations: new Map(), searched: true };
  };
  const result = await generateRecipe({ provider: "openai", prompt: "p", systemPrompt: "s" }, "gapfill", "low", completeImpl);
  assert.equal(seen.searchMode, "gapfill");
  assert.deepEqual(result.sources, [{ title: "x", url: "https://x.example/" }]);
});

test("generateRecipe requires a search on grounded subscription requests", async () => {
  const completeImpl = async () => ({ label: "Codex", text: RECIPE_REPLY, citations: new Map(), searched: false });
  await assert.rejects(
    generateRecipe({ piProvider: "openai-codex", prompt: "p", systemPrompt: "s" }, "grounded", "low", completeImpl),
    (error) => error instanceof WorkerError && error.code === "output",
  );
});

test("isAuthFailure recognises refused logins and keys", () => {
  const modelsError = (message: string, code: string): Error =>
    Object.assign(new Error(message), { name: "ModelsError", code });
  const refused: unknown[] = [
    new Error("OAuth refresh failed for anthropic: Anthropic token refresh request failed. status=400; body={\"error\": \"invalid_grant\"}"),
    modelsError("OAuth refresh failed for openai-codex", "oauth"),
    new WorkerError("worker", "API returned HTTP 401: {\"error\":{\"type\":\"authentication_error\"}}"),
    new Error("401 Unauthorized"),
    new Error("Incorrect API key provided: sk-...abcd"),
    new Error("invalid x-api-key"),
  ];
  for (const error of refused) assert.equal(isAuthFailure(error), true, String(error));
  const other: unknown[] = [
    new WorkerError("worker", "API returned HTTP 429: rate limited", { retryable: true }),
    new WorkerError("worker", "API returned HTTP 500"),
    new WorkerError("configuration", "Pi is not logged in to Claude. Authorise Claude from the Settings page."),
    modelsError("Credential store modify failed for anthropic", "auth"),
    new Error("Anthropic response was truncated by max_tokens."),
  ];
  for (const error of other) assert.equal(isAuthFailure(error), false, String(error));
});

test("workerFailure reports a refused login as a non-retryable auth failure", () => {
  assert.deepEqual(
    workerFailure(new WorkerError("worker", "API returned HTTP 401: bad key", { retryable: true })),
    { error: "API returned HTTP 401: bad key", code: "auth", retryable: false },
  );
  assert.deepEqual(
    workerFailure(new WorkerError("output", "bad json")),
    { error: "bad json", code: "output", retryable: true },
  );
  assert.deepEqual(
    workerFailure(new Error("boom")),
    { error: "boom", code: "worker", retryable: false },
  );
});

test("parseRequest accepts checkLogin without a prompt", () => {
  assert.equal(parseRequest(JSON.stringify({ command: "checkLogin" })).command, "checkLogin");
});
