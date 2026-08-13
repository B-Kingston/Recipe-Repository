import test from "node:test";
import assert from "node:assert/strict";
import { WorkerError, catalogModelIds, critiquePass, normalizeEffort, openaiChat, openaiResponse, parseResponsesOutput, verifiedSources } from "./recipe-worker.mjs";

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
    true,
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
  await openaiResponse("https://api.openai.com/v1", "sk-test", "gpt-5.6-luna", "sys", "prompt", false, "low", fetchImpl);
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
  await openaiResponse("https://api.openai.com/v1/", "sk-test", "gpt-5.6-luna", "sys", "prompt", false, "low", fetchImpl);
  assert.equal(capturedUrl, "https://api.openai.com/v1/responses");
});

test("openaiResponse maps a non-2xx response to a WorkerError", async () => {
  const fetchImpl = async () => ({ ok: false, status: 401, text: async () => "bad key" });
  await assert.rejects(
    openaiResponse("https://api.openai.com/v1", "sk-test", "gpt-5.6-luna", "sys", "prompt", false, "low", fetchImpl),
    (error) => {
      assert.ok(error instanceof WorkerError);
      assert.equal(error.code, "worker");
      assert.ok(error.message.includes("HTTP 401"));
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
  await openaiResponse("https://api.openai.com/v1", "sk-test", "gpt-5.6-luna", "sys", "prompt", false, "high", fetchImpl);
  const sent = JSON.parse(capturedInit.body);
  assert.deepEqual(sent.reasoning, { effort: "high" });
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
      { type: "web_search_call", id: "ws_1", status: "completed", action: { type: "search", query: "x" } },
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

test("openaiChat posts multi-message input with no tools", async () => {
  let capturedUrl;
  let capturedInit;
  const fetchImpl = async (url, init) => {
    capturedUrl = url;
    capturedInit = init;
    return { ok: true, json: async () => ({ status: "completed" }) };
  };
  const messages = [
    { role: "user", content: "prompt", timestamp: Date.now() },
    { role: "assistant", content: "turn one", timestamp: Date.now() },
    { role: "user", content: "critique", timestamp: Date.now() },
  ];
  const body = await openaiChat("https://api.openai.com/v1", "sk-test", "gpt-5.6-luna", "sys", messages, "high", fetchImpl);
  assert.equal(capturedUrl, "https://api.openai.com/v1/responses");
  assert.deepEqual(capturedInit.headers.Authorization, "Bearer sk-test");
  const sent = JSON.parse(capturedInit.body);
  assert.equal(sent.model, "gpt-5.6-luna");
  // The Responses API rejects unknown fields (e.g. `timestamp`) on input
  // items; only role/content may be sent.
  assert.deepEqual(sent.input, [
    { role: "system", content: "sys" },
    { role: "user", content: "prompt" },
    { role: "assistant", content: "turn one" },
    { role: "user", content: "critique" },
  ]);
  assert.ok(!("tools" in sent));
  assert.ok(!("tool_choice" in sent));
  assert.deepEqual(sent.reasoning, { effort: "high" });
  assert.deepEqual(body, { status: "completed" });
});

test("critiquePass returns the revised recipe with the diff when the revision is valid", async () => {
  const turn1 = { ingredients: [{ name: "chicken" }, { name: "rice" }] };
  const messages = [];
  const pass = await critiquePass({
    prompt: "make it vegan",
    turn1Text: "turn one text",
    turn1Recipe: turn1,
    systemPrompt: "sys",
    log: () => {},
    callModel: async (msgs) => {
      messages.push(...msgs);
      return JSON.stringify({ recipe: { ingredients: [{ name: "Tofu" }, { name: "rice" }] } });
    },
  });
  assert.equal(pass.critique.total, 2);
  assert.equal(pass.critique.resolved, 2);
  assert.equal(pass.critique.pairCount, 1);
  assert.deepEqual(pass.critique.added, ["Tofu"]);
  assert.deepEqual(pass.critique.removed, ["chicken"]);
  assert.deepEqual(pass.recipe.ingredients, [{ name: "Tofu" }, { name: "rice" }]);
  assert.deepEqual(messages.map((m) => m.role), ["user", "assistant", "user"]);
  assert.equal(messages[0].content, "make it vegan");
  assert.equal(messages[1].content, "turn one text");
  assert.ok(messages[2].content.includes("Scored 2 of 2 ingredients (1 pairs)"));
});

test("critiquePass falls back to the turn-1 recipe on a garbage revision", async () => {
  const turn1 = { ingredients: [{ name: "chicken" }, { name: "rice" }] };
  const pass = await critiquePass({
    prompt: "p",
    turn1Text: "t",
    turn1Recipe: turn1,
    systemPrompt: "s",
    log: () => {},
    callModel: async () => "not json at all",
  });
  assert.deepEqual(pass.recipe, turn1);
  assert.equal(pass.critique, null);
});

test("critiquePass falls back to the turn-1 recipe when the model call returns null", async () => {
  const turn1 = { ingredients: [{ name: "chicken" }, { name: "rice" }] };
  const pass = await critiquePass({
    prompt: "p",
    turn1Text: "t",
    turn1Recipe: turn1,
    systemPrompt: "s",
    log: () => {},
    callModel: async () => null,
  });
  assert.deepEqual(pass.recipe, turn1);
  assert.equal(pass.critique, null);
});

test("critiquePass skips when fewer than two ingredients resolve", async () => {
  const turn1 = { ingredients: [{ name: "only one unresolvable ingredient" }] };
  let called = false;
  const pass = await critiquePass({
    prompt: "p",
    turn1Text: "t",
    turn1Recipe: turn1,
    systemPrompt: "s",
    log: () => {},
    callModel: async () => {
      called = true;
      return "{}";
    },
  });
  assert.deepEqual(pass.recipe, turn1);
  assert.equal(pass.critique, null);
  assert.equal(called, false);
});

test("critiquePass emits the critique and the revision result on stderr", async () => {
  const logged = [];
  await critiquePass({
    prompt: "p",
    turn1Text: "t",
    turn1Recipe: { ingredients: [{ name: "chicken" }, { name: "rice" }] },
    systemPrompt: "s",
    log: (line) => logged.push(line),
    callModel: async () => JSON.stringify({ recipe: { ingredients: [{ name: "Tofu" }, { name: "rice" }] } }),
  });
  assert.equal(logged.length, 2);
  const critiqueLine = JSON.parse(logged[0]);
  assert.equal(critiqueLine.event, "pairwise_critique");
  assert.ok(critiqueLine.turn2Message.includes("same schema as your draft above"));
  const resultLine = JSON.parse(logged[1]);
  assert.equal(resultLine.event, "pairwise_critique_result");
  assert.deepEqual(resultLine.added, ["Tofu"]);
  assert.deepEqual(resultLine.removed, ["chicken"]);
});
