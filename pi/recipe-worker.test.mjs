import test from "node:test";
import assert from "node:assert/strict";
import { WorkerError, aiGatewayChatCompletion, anthropicMessages, anthropicUrl, catalogModelIds, formatRecipeEvidence, normalizeEffort, openaiResponse, parseAiGatewayCleaner, parseAnthropicOutput, parseResponsesOutput, recipePrompt, searchModeOf, thinkingBudget, verifiedSources } from "./recipe-worker.mjs";

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

test("openaiResponse posts to /responses with web_search when search is enabled", async () => {
  let capturedUrl;
  let capturedInit;
  const fetchImpl = async (url, init) => {
    capturedUrl = url;
    capturedInit = init;
    return { ok: true, json: async () => ({ status: "completed" }) };
  };
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
  let capturedInit;
  const fetchImpl = async (url, init) => {
    capturedInit = init;
    return { ok: true, json: async () => ({}) };
  };
  await openaiResponse("https://api.openai.com/v1", "sk-test", "gpt-5.6-luna", "sys", "prompt", "off", "low", fetchImpl);
  const sent = JSON.parse(capturedInit.body);
  assert.ok(!("tools" in sent));
  assert.ok(!("tool_choice" in sent));
});

test("openaiResponse strips a trailing slash from the base URL", async () => {
  let capturedUrl;
  const fetchImpl = async (url) => {
    capturedUrl = url;
    return { ok: true, json: async () => ({}) };
  };
  await openaiResponse("https://api.openai.com/v1/", "sk-test", "gpt-5.6-luna", "sys", "prompt", "off", "low", fetchImpl);
  assert.equal(capturedUrl, "https://api.openai.com/v1/responses");
});

test("openaiResponse maps a non-2xx response to a WorkerError", async () => {
  const fetchImpl = async () => ({ ok: false, status: 401, text: async () => "bad key" });
  await assert.rejects(
    openaiResponse("https://api.openai.com/v1", "sk-test", "gpt-5.6-luna", "sys", "prompt", "off", "low", fetchImpl),
    (error) => {
      assert.ok(error instanceof WorkerError);
      assert.equal(error.code, "worker");
      assert.equal(error.retryable, false);
      assert.ok(error.message.includes("HTTP 401"));
      return true;
    },
  );
});

test("openaiResponse marks transient provider failures as retryable", async () => {
  const fetchImpl = async () => ({ status: 503, ok: false, text: async () => "busy" });
  await assert.rejects(
    openaiResponse("https://api.openai.com/v1", "sk-test", "gpt-5.6-luna", "sys", "prompt", "off", "low", fetchImpl),
    (error) => {
      assert.ok(error instanceof WorkerError);
      assert.equal(error.retryable, true);
      return true;
    },
  );
});

test("openaiResponse sends the chosen reasoning effort", async () => {
  let capturedInit;
  const fetchImpl = async (url, init) => {
    capturedInit = init;
    return { ok: true, json: async () => ({}) };
  };
  await openaiResponse("https://api.openai.com/v1", "sk-test", "gpt-5.6-luna", "sys", "prompt", "off", "high", fetchImpl);
  const sent = JSON.parse(capturedInit.body);
  assert.deepEqual(sent.reasoning, { effort: "high" });
});

test("openaiResponse offers web_search without forcing it in gap-fill mode", async () => {
  let capturedInit;
  const fetchImpl = async (url, init) => {
    capturedInit = init;
    return { ok: true, json: async () => ({}) };
  };
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

test("searchModeOf accepts only the three known modes", () => {
  assert.equal(searchModeOf({}), "off");
  assert.equal(searchModeOf({ searchMode: "nonsense" }), "off");
  assert.equal(searchModeOf({ searchMode: " grounded " }), "grounded");
  assert.equal(searchModeOf({ searchMode: "gapfill" }), "gapfill");
});

test("aiGatewayChatCompletion uses the requested chat-completions model and reasoning", async () => {
  let capturedUrl;
  let capturedInit;
  const fetchImpl = async (url, init) => {
    capturedUrl = url;
    capturedInit = init;
    return { ok: true, json: async () => ({ choices: [{ message: { content: "ok" } }] }) };
  };
  await aiGatewayChatCompletion("https://ai-gateway.vercel.sh/v1/", "vg-test", "poolside/laguna-s-2.1-free", "sys", "raw evidence", fetchImpl);
  assert.equal(capturedUrl, "https://ai-gateway.vercel.sh/v1/chat/completions");
  assert.equal(capturedInit.headers.Authorization, "Bearer vg-test");
  const sent = JSON.parse(capturedInit.body);
  assert.equal(sent.model, "poolside/laguna-s-2.1-free");
  assert.deepEqual(sent.messages, [
    { role: "system", content: "sys" },
    { role: "user", content: "raw evidence" },
  ]);
  assert.deepEqual(sent.reasoning, { effort: "none" });
  assert.equal(sent.stream, false);
  assert.equal(sent.max_tokens, 2048);
  assert.ok(!("temperature" in sent));
});

test("aiGatewayChatCompletion honours reasoning/maxTokens/temperature options and omits reasoning when null", async () => {
  let capturedInit;
  const fetchImpl = async (_url, init) => {
    capturedInit = init;
    return { ok: true, json: async () => ({ choices: [{ message: { content: "ok" } }] }) };
  };
  // Off + tuned max_tokens and a deterministic temperature.
  await aiGatewayChatCompletion(
    "https://ai-gateway.vercel.sh/v1/", "vg-test", "poolside/laguna-s-2.1-free",
    "sys", "raw evidence", fetchImpl,
    { reasoning: { effort: "none" }, maxTokens: 1024, temperature: 0 },
  );
  let sent = JSON.parse(capturedInit.body);
  assert.deepEqual(sent.reasoning, { effort: "none" });
  assert.equal(sent.max_tokens, 1024);
  assert.equal(sent.temperature, 0);
  // null reasoning means the field is omitted entirely (some models reject it).
  await aiGatewayChatCompletion(
    "https://ai-gateway.vercel.sh/v1/", "vg-test", "poolside/laguna-s-2.1-free",
    "sys", "raw evidence", fetchImpl, { reasoning: null },
  );
  sent = JSON.parse(capturedInit.body);
  assert.ok(!("reasoning" in sent), "null reasoning should omit the field");
  assert.equal(sent.max_tokens, 2048, "unset maxTokens keeps the default");
});

test("parseAiGatewayCleaner keeps recipe fields and discards cleaner prose and unknown fields", () => {
  const cleaned = parseAiGatewayCleaner({
    choices: [{ message: { content: "```json\n" + JSON.stringify({
      title: "Chilli tofu",
      ingredients: [{ quantity: "200", unit: "g", name: "tofu" }, "1 tbsp oil"],
      steps: ["Crisp the tofu"],
      timings: ["Fry for 5 minutes"],
      relevant_notes: ["Serve hot"],
      rambling: "Follow me for more recipes",
    }) + "\n```" } }],
  });
  assert.equal(cleaned, "Dish: Chilli tofu\nIngredients:\n- 200 g tofu\n- 1 tbsp oil\nMethod:\n1. Crisp the tofu\nTimings and temperatures:\n- Fry for 5 minutes\nRelevant recipe notes:\n- Serve hot");
  assert.ok(!cleaned.includes("Follow me"));
});

test("formatRecipeEvidence rejects cleaner output without ingredients or steps", () => {
  assert.throws(() => formatRecipeEvidence({ title: "A story", relevant_notes: ["Like and follow"] }), (error) => {
    assert.ok(error instanceof WorkerError);
    assert.equal(error.code, "output");
    return true;
  });
});

test("anthropicUrl maps base URLs to the Messages endpoint", () => {
  assert.equal(anthropicUrl("https://api.anthropic.com"), "https://api.anthropic.com/v1/messages");
  assert.equal(anthropicUrl("https://api.anthropic.com/"), "https://api.anthropic.com/v1/messages");
  assert.equal(anthropicUrl("https://api.anthropic.com/v1"), "https://api.anthropic.com/v1/messages");
  assert.equal(anthropicUrl("https://proxy.example.com/anthropic/v1/"), "https://proxy.example.com/anthropic/v1/messages");
});

test("anthropicMessages posts to /v1/messages with x-api-key auth", async () => {
  let capturedUrl;
  let capturedInit;
  const fetchImpl = async (url, init) => {
    capturedUrl = url;
    capturedInit = init;
    return { ok: true, json: async () => ({ type: "message", content: [] }) };
  };
  const body = await anthropicMessages(
    "https://api.anthropic.com",
    "sk-ant-test",
    "claude-sonnet-4-5",
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
  assert.equal(sent.model, "claude-sonnet-4-5");
  assert.equal(sent.system, "sys");
  assert.deepEqual(sent.messages, [{ role: "user", content: "prompt" }]);
  assert.deepEqual(sent.tools, [{ type: "web_search_20250305" }]);
  assert.deepEqual(body, { type: "message", content: [] });
});

test("anthropicMessages scales the thinking budget with effort and covers it in max_tokens", async () => {
  let capturedInit;
  const fetchImpl = async (url, init) => {
    capturedInit = init;
    return { ok: true, json: async () => ({}) };
  };
  await anthropicMessages("https://api.anthropic.com", "k", "m", "s", [{ role: "user", content: "p" }], "low", "off", fetchImpl);
  let sent = JSON.parse(capturedInit.body);
  assert.deepEqual(sent.thinking, { type: "enabled", budget_tokens: 1024 });
  assert.equal(sent.max_tokens, 1024 + 8192);
  await anthropicMessages("https://api.anthropic.com", "k", "m", "s", [{ role: "user", content: "p" }], "high", "off", fetchImpl);
  sent = JSON.parse(capturedInit.body);
  assert.deepEqual(sent.thinking, { type: "enabled", budget_tokens: 8192 });
  assert.equal(sent.max_tokens, 8192 + 8192);
  assert.ok(!("tools" in sent));
});

test("thinkingBudget defaults unknown or blank efforts to low", () => {
  assert.equal(thinkingBudget(undefined), 1024);
  assert.equal(thinkingBudget(""), 1024);
  assert.equal(thinkingBudget("medium"), 4096);
  assert.equal(thinkingBudget("high"), 8192);
  assert.equal(thinkingBudget("xhigh"), 8192);
  assert.equal(thinkingBudget("max"), 8192);
});

test("anthropicMessages maps a non-2xx response to a WorkerError", async () => {
  const fetchImpl = async () => ({ ok: false, status: 401, text: async () => "bad key" });
  await assert.rejects(
    anthropicMessages("https://api.anthropic.com", "sk-ant-test", "m", "s", [{ role: "user", content: "p" }], "low", "off", fetchImpl),
    (error) => {
      assert.ok(error instanceof WorkerError);
      assert.equal(error.code, "worker");
      assert.ok(error.message.includes("HTTP 401"));
      return true;
    },
  );
});

test("parseAnthropicOutput extracts text and web-search citations", () => {
  const { text, searched, refused, citations } = parseAnthropicOutput({
    stop_reason: "end_turn",
    content: [
      { type: "text", text: "Here is the recipe " },
      { type: "text", text: "and more." },
      {
        type: "web_search_tool_result",
        tool_use_id: "toolu_1",
        content: [
          { type: "text", text: "search summary" },
          {
            type: "search_result",
            title: "Sourdough basics",
            source: { type: "url", url: "https://example.com/sourdough" },
          },
          {
            type: "search_result",
            url: "https://example.org/old-shape",
            title: "Legacy shape",
          },
        ],
      },
    ],
  });
  assert.equal(text, "Here is the recipe and more.");
  assert.equal(searched, true);
  assert.equal(refused, false);
  assert.equal(citations.get("https://example.com/sourdough"), "Sourdough basics");
  assert.equal(citations.get("https://example.org/old-shape"), "Legacy shape");
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
