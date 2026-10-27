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
  CHAT_TOOLS,
  WEB_SEARCH_TOOL,
  ANSWER_TOOL,
  chatLibrary,
  chatSearchPayload,
  answerChat,
  hostedSearchAnswer,
  chatReply,
  chatWithTools,
  formatChatNotes,
  runChatTool,
  runWebSearchTool,
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
import type { ChatLibrary, ChatModelCall, Complete, CompletionOptions } from "./recipe-worker.ts";
import { EDIT_TOOLS, recipeDraft } from "./recipe-edits.ts";
import { WebSearchError } from "./web-search.ts";
import type { AssistantMessage, Context, Model, ToolCall } from "@earendil-works/pi-ai";

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

test("chatReply reads the answer and the change's label from the JSON reply", () => {
  const reply = chatReply("```json\n" + JSON.stringify({ reply: "Swapped the eggs for tofu.", label: "Egg-free" }) + "\n```");
  assert.deepEqual(reply, { reply: "Swapped the eggs for tofu.", label: "Egg-free" });
});

test("chatReply takes prose that ignored the format as the answer", () => {
  assert.deepEqual(chatReply("Use about 1 kg of fresh tomatoes {chopped}."), {
    reply: "Use about 1 kg of fresh tomatoes {chopped}.",
    label: "",
  });
});

test("chatReply refuses an empty answer as retryable output", () => {
  for (const text of ["", '{"reply":"  ","label":"Less salt"}']) {
    assert.throws(() => chatReply(text), (error) => {
      assert.ok(error instanceof WorkerError);
      assert.equal(error.code, "output");
      return true;
    });
  }
});

/** A chat library with a whole-recipe note, a note about the old version
 * with a photo, and two saved versions. */
function library(): ChatLibrary {
  return chatLibrary({
    notes: [
      { version: null, createdAt: "26 Oct 2026, 09:00", text: "Kids want it milder.", hasPhoto: false, photo: null },
      { version: 1, createdAt: "20 Oct 2026, 19:30", text: "Too salty.", hasPhoto: true, photo: "aGVsbG8=" },
      { version: "2", text: 5 },
      "junk",
    ],
    versions: [
      { version: 2, label: "Chat: Less salt", createdAt: "21 Oct 2026, 10:00", recipe: { title: "Dal" } },
      { version: 1, label: "Original recipe", createdAt: "19 Oct 2026, 10:00", recipe: { title: "Dal" } },
      { label: "no number" },
    ],
    currentVersion: 2,
  });
}

function toolCall(name: string, args: Record<string, unknown> = {}, id = `call-${name}`): ToolCall {
  return { type: "toolCall", id, name, arguments: args as ToolCall["arguments"] };
}

function reply(content: AssistantMessage["content"], stopReason: AssistantMessage["stopReason"] = "stop"): AssistantMessage {
  return { role: "assistant", content, stopReason, timestamp: 0 } as unknown as AssistantMessage;
}

function resultText(message: ReturnType<typeof runChatTool>): string {
  return message.content.filter((part) => part.type === "text").map((part) => (part as { text: string }).text).join("\n");
}

test("chatLibrary keeps well-formed notes and versions and drops the rest", () => {
  const parsed = library();
  assert.equal(parsed.notes.length, 3);
  assert.deepEqual(parsed.notes[2], { version: null, createdAt: "", text: "", hasPhoto: false, photo: null });
  assert.deepEqual(parsed.versions.map((entry) => entry.version), [2, 1]);
  assert.equal(parsed.currentVersion, 2);
  assert.deepEqual(chatLibrary(undefined), { notes: [], versions: [], currentVersion: null });
});

test("recipe_notes lists each note with what it is about and its photo", () => {
  const listing = formatChatNotes(library());
  assert.match(listing, /^Note 1 · written 26 Oct 2026, 09:00 · about the whole recipe\nKids want it milder\./);
  assert.match(listing, /Note 2 · written 20 Oct 2026, 19:30 · about version 1 \(Original recipe\) · has a photo \(note_photo\)\nToo salty\./);
  assert.equal(formatChatNotes(chatLibrary({ notes: [] })), "The cook has not kept any notes on this recipe.");
  const current = chatLibrary({ notes: [{ version: 2, text: "Better" }], versions: [{ version: 2, label: "Less salt" }], currentVersion: 2 });
  assert.match(formatChatNotes(current), /about version 2 \(Less salt\), which is the current recipe/);
});

test("note_photo returns the photo only when there is one the model can see", () => {
  const shown = runChatTool(library(), toolCall("note_photo", { note: 2 }), true);
  assert.equal(shown.isError, false);
  assert.deepEqual(shown.content[1], { type: "image", data: "aGVsbG8=", mimeType: "image/jpeg" });
  assert.equal(runChatTool(library(), toolCall("note_photo", { note: 1 }), true).isError, true);
  assert.equal(runChatTool(library(), toolCall("note_photo", { note: 9 }), true).isError, true);
  const blind = runChatTool(library(), toolCall("note_photo", { note: 2 }), false);
  assert.equal(blind.isError, true);
  assert.match(resultText(blind), /cannot view photos/);
  const withheld = chatLibrary({ notes: [{ version: null, text: "", hasPhoto: true, photo: null }] });
  assert.match(resultText(runChatTool(withheld, toolCall("note_photo", { note: 1 }), true)), /work from the note's text/);
});

test("recipe_version returns a saved version, and unknown versions and tools are errors", () => {
  const old = runChatTool(library(), toolCall("recipe_version", { version: 1 }), true);
  assert.deepEqual(JSON.parse(resultText(old)), {
    version: 1,
    label: "Original recipe",
    savedAt: "19 Oct 2026, 10:00",
    current: false,
    recipe: { title: "Dal" },
  });
  const missing = runChatTool(library(), toolCall("recipe_version", { version: 5 }), true);
  assert.equal(missing.isError, true);
  assert.match(resultText(missing), /Saved versions: 2, 1/);
  assert.equal(runChatTool(library(), toolCall("delete_recipe"), true).isError, true);
  assert.deepEqual(CHAT_TOOLS.map((tool) => tool.name), ["recipe_notes", "note_photo", "recipe_version"]);
});

test("chatWithTools runs the model's lookups, then returns its answer", async () => {
  const contexts: Context[] = [];
  const turns = [
    reply([toolCall("recipe_notes")], "toolUse"),
    reply([toolCall("note_photo", { note: 2 }), toolCall("recipe_version", { version: 1 }, "call-v1")], "toolUse"),
    reply([{ type: "text", text: '{"reply":"Use half the salt.","label":""}' }]),
  ];
  const call: ChatModelCall = async (context) => {
    contexts.push({ ...context, messages: [...context.messages] });
    return turns.shift()!;
  };
  const logged: string[] = [];
  const result = await chatWithTools(call, "system", "It was too salty", library(), { canSeePhotos: true, log: (line) => logged.push(line) });
  assert.equal(result.text, '{"reply":"Use half the salt.","label":""}');
  assert.deepEqual(result.toolCalls, ["recipe_notes", "note_photo", "recipe_version"]);
  assert.equal(contexts.length, 3);
  assert.deepEqual(contexts[0].tools, [...CHAT_TOOLS, ANSWER_TOOL]);
  // The last turn saw the question, both assistant turns, and three results.
  assert.deepEqual(contexts[2].messages.map((message) => message.role), ["user", "assistant", "toolResult", "assistant", "toolResult", "toolResult"]);
  assert.equal(logged.length, 3);
  assert.ok(logged.every((line) => !line.includes("aGVsbG8=")), "photo bytes are never logged");
});

test("chatWithTools stops a model that never answers and reports failed calls", async () => {
  const looping: ChatModelCall = async () => reply([toolCall("recipe_notes")], "toolUse");
  await assert.rejects(chatWithTools(looping, "s", "p", library(), { log: () => {} }), (error) => {
    assert.ok(error instanceof WorkerError);
    assert.equal(error.code, "output");
    return true;
  });
  const failing: ChatModelCall = async () => ({ ...reply([], "error"), errorMessage: "overloaded" }) as AssistantMessage;
  await assert.rejects(chatWithTools(failing, "s", "p", library(), { log: () => {} }), (error) => {
    assert.ok(error instanceof WorkerError);
    assert.equal(error.retryable, true);
    return true;
  });
});

test("chatSearchPayload uses the provider's own search only where it has one", () => {
  const responses = chatSearchPayload("openai-codex-responses", null)!;
  assert.deepEqual(responses({ tools: [{ type: "function", name: "recipe_notes" }] }).tools, [
    { type: "function", name: "recipe_notes" },
    { type: "web_search", search_context_size: "medium" },
  ]);
  assert.ok(chatSearchPayload("openai-responses", "openai"));
  const anthropic = chatSearchPayload("anthropic-messages", "anthropic")!;
  assert.deepEqual(anthropic({ tools: [{ name: "recipe_notes" }] }).tools, [
    { name: "recipe_notes" },
    { type: "web_search_20250305", name: "web_search", max_uses: 4 },
  ]);
  assert.ok(chatSearchPayload("anthropic-messages", null), "the Claude subscription searches itself");
  // pi-ai providers fall back to the search API, even on an Anthropic-shaped API.
  assert.equal(chatSearchPayload("anthropic-messages", "minimax"), null);
  assert.equal(chatSearchPayload("openai-completions", "openai-compatible"), null);
  assert.equal(chatSearchPayload("google-generative-ai", "google"), null);
});

test("chatWithTools offers web_search with a search key and answers it through the API", async () => {
  const contexts: Context[] = [];
  const turns = [
    reply([toolCall("search_web", { query: "asafoetida substitute" })], "toolUse"),
    reply([{ type: "text", text: '{"reply":"Use garlic powder.\\nhttps://example.com/hing","label":""}' }]),
  ];
  const call: ChatModelCall = async (context) => {
    contexts.push({ ...context, messages: [...context.messages] });
    return turns.shift()!;
  };
  const fetched: string[] = [];
  const fetchImpl = (async (url: string | URL | Request) => {
    fetched.push(String(url));
    return new Response(JSON.stringify({ results: [{ title: "Hing", url: "https://example.com/hing", content: "Garlic powder works." }] }));
  }) as typeof fetch;
  const logged: string[] = [];
  const result = await chatWithTools(call, "s", "No hing?", library(), {
    canSeePhotos: true,
    log: (line) => logged.push(line),
    search: { provider: "tavily", apiKey: "tvly-secret" },
    fetchImpl,
  });
  assert.deepEqual(result.toolCalls, ["search_web"]);
  assert.deepEqual(contexts[0].tools, [...CHAT_TOOLS, WEB_SEARCH_TOOL, ANSWER_TOOL]);
  assert.deepEqual(fetched, ["https://api.tavily.com/search"]);
  const searchResult = contexts[1].messages[2] as ReturnType<typeof runChatTool>;
  assert.match(resultText(searchResult), /1\. Hing\nhttps:\/\/example\.com\/hing\nGarlic powder works\./);
  assert.ok(logged.every((line) => !line.includes("tvly-secret")), "the search key is never logged");

  const without = await chatWithTools(async (context) => {
    assert.deepEqual(context.tools, [...CHAT_TOOLS, ANSWER_TOOL]);
    return reply([{ type: "text", text: "Plain." }]);
  }, "s", "p", library(), { log: () => {} });
  assert.equal(without.text, "Plain.");
});

test("a failed web search is an error result, not a failed chat", async () => {
  const rejected = (async () => new Response("{}", { status: 401 })) as typeof fetch;
  const logged: string[] = [];
  const output = await runWebSearchTool({ provider: "exa", apiKey: "k" }, toolCall("search_web", { query: "q" }), rejected, (line) => logged.push(line));
  assert.equal(output.isError, true);
  assert.match(resultText(output), /Exa rejected the search API key\. Answer without the web\./);
  assert.match(logged[0], /key rejected/);
  const empty = await runWebSearchTool({ provider: "exa", apiKey: "k" }, toolCall("search_web", { query: " " }), rejected, () => {});
  assert.equal(empty.isError, true);
});

test("chatWithTools offers the edit tools with a recipe and returns what they changed", async () => {
  const recipe = recipeDraft({
    title: "Dal",
    ingredients: [{ name: "salt", quantity: "2", unit: "tsp" }],
    steps: [{ text: "Simmer.", ingredients: ["2 tsp salt"] }],
  })!;
  const contexts: Context[] = [];
  const turns = [
    reply([
      toolCall("change_ingredient", { ingredient: 1, quantity: "1" }),
      toolCall("change_step", { step: 1, ingredients: ["1 tsp salt"] }, "call-step"),
    ], "toolUse"),
    reply([{ type: "text", text: '{"reply":"Halved the salt.","label":"Less salt"}' }]),
  ];
  const result = await chatWithTools(async (context) => {
    contexts.push({ ...context, messages: [...context.messages] });
    return turns.shift()!;
  }, "s", "Too salty", library(), { log: () => {}, recipe });
  assert.deepEqual(contexts[0].tools, [...CHAT_TOOLS, ...EDIT_TOOLS, ANSWER_TOOL]);
  assert.deepEqual(result.toolCalls, ["change_ingredient", "change_step"]);
  assert.deepEqual(result.proposal?.ingredients, [{ from: 0, name: "salt", quantity: "1", unit: "tsp", optional: false }]);
  assert.deepEqual(result.proposal?.steps[0].ingredients, ["1 tsp salt"]);
  assert.equal(recipe.ingredients[0].quantity, "2", "the recipe the host sent is not changed");

  const untouched = await chatWithTools(async () => reply([{ type: "text", text: "Fine as is." }]), "s", "p", library(), { log: () => {}, recipe });
  assert.equal(untouched.proposal, null);
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

// ---------------------------------------------------------------------------
// The recipe chat through pi-ai itself, with the provider's HTTP faked: what a
// hosted-search round sends on the next request, and how a paused turn ends.

/** A text/event-stream response of the given events, as the provider SDKs read it. */
function sse(events: Record<string, unknown>[]): Response {
  const body = events.map((event) => `event: ${String(event.type)}\ndata: ${JSON.stringify(event)}\n\n`).join("");
  return new Response(body, { status: 200, headers: { "content-type": "text/event-stream" } });
}

/** A fetch that answers each request with the next response and keeps the JSON bodies. */
function scriptedFetch(responses: Response[], bodies: Record<string, unknown>[]): typeof fetch {
  return (async (_url: string | URL | Request, init?: RequestInit) => {
    bodies.push(JSON.parse(String(init?.body ?? "{}")) as Record<string, unknown>);
    const next = responses.shift();
    if (!next) throw new Error("no scripted response left");
    return next;
  }) as typeof fetch;
}

const SHORTBREAD = {
  title: "Shortbread",
  ingredients: [{ name: "butter", quantity: "200", unit: "g" }, { name: "sugar", quantity: "100", unit: "g" }],
  steps: [{ text: "Cream the butter and sugar.", ingredients: ["200 g butter", "100 g sugar"] }],
};

const USAGE = { input_tokens: 10, output_tokens: 5, total_tokens: 15, input_tokens_details: { cached_tokens: 0 }, output_tokens_details: { reasoning_tokens: 0 } };

/** One Responses API turn whose output items are given in order. */
function responsesTurn(id: string, items: Record<string, unknown>[]): Response {
  const events: Record<string, unknown>[] = [{ type: "response.created", response: { id, status: "in_progress", output: [] } }];
  items.forEach((item, index) => {
    events.push({ type: "response.output_item.added", output_index: index, item: { ...item, ...(item.type === "function_call" ? { arguments: "" } : {}) } });
    events.push({ type: "response.output_item.done", output_index: index, item });
  });
  events.push({ type: "response.completed", response: { id, status: "completed", output: items, usage: USAGE } });
  return sse(events);
}

function answerItem(text: string): Record<string, unknown> {
  return { type: "message", id: "msg_2", role: "assistant", status: "completed", content: [{ type: "output_text", text, annotations: [] }] };
}

/** One Anthropic Messages turn: content blocks in order, then the stop reason. */
function anthropicStream(blocks: Record<string, unknown>[], stopReason: string): Response {
  const events: Record<string, unknown>[] = [{
    type: "message_start",
    message: { id: "msg_1", type: "message", role: "assistant", content: [], model: "claude-sonnet-5-5", stop_reason: null, usage: { input_tokens: 10, output_tokens: 0 } },
  }];
  blocks.forEach((block, index) => {
    if (block.type === "thinking") {
      events.push({ type: "content_block_start", index, content_block: { type: "thinking", thinking: "", signature: "" } });
      events.push({ type: "content_block_delta", index, delta: { type: "thinking_delta", thinking: block.thinking } });
      events.push({ type: "content_block_delta", index, delta: { type: "signature_delta", signature: block.signature } });
    } else if (block.type === "text") {
      events.push({ type: "content_block_start", index, content_block: { type: "text", text: "" } });
      events.push({ type: "content_block_delta", index, delta: { type: "text_delta", text: block.text } });
    } else if (block.type === "tool_use" || block.type === "server_tool_use") {
      events.push({ type: "content_block_start", index, content_block: { ...block, input: {} } });
      events.push({ type: "content_block_delta", index, delta: { type: "input_json_delta", partial_json: JSON.stringify(block.input) } });
    } else {
      events.push({ type: "content_block_start", index, content_block: block });
    }
    events.push({ type: "content_block_stop", index });
  });
  events.push({ type: "message_delta", delta: { stop_reason: stopReason }, usage: { output_tokens: 20 } });
  events.push({ type: "message_stop" });
  return sse(events);
}

const SEARCH_BLOCKS = [
  { type: "server_tool_use", id: "srvtoolu_1", name: "web_search", input: { query: "vegan butter shortbread" } },
  { type: "web_search_tool_result", tool_use_id: "srvtoolu_1", content: [{ type: "web_search_result", url: "https://example.com/vegan-butter", title: "Vegan butter", encrypted_content: "enc", page_age: null }] },
];

function anthropicChat(fetchImpl: typeof fetch): ChatModelCall {
  const models = piAiModels();
  const model = piAiModel(models, "anthropic", "claude-sonnet-5-5");
  const onPayload = chatSearchPayload(model.api, "anthropic")!;
  return (context) => models.completeSimple(model, context, { apiKey: "sk-ant-api03-test", reasoning: "low", maxRetries: 0, onPayload, fetch: fetchImpl });
}

test("a paused Anthropic turn is continued or failed, never saved as the answer", async () => {
  const bodies: Record<string, unknown>[] = [];
  const fetchImpl = scriptedFetch([
    anthropicStream([{ type: "text", text: "Let me look that up." }, ...SEARCH_BLOCKS], "pause_turn"),
    anthropicStream([{ type: "text", text: '{"reply":"Bake at 160C.","label":""}' }], "end_turn"),
  ], bodies);
  let outcome: string;
  try {
    const { text } = await chatWithTools(anthropicChat(fetchImpl), "system", "What temperature?", chatLibrary({}), { log: () => {} });
    outcome = chatReply(text).reply;
  } catch (error) {
    outcome = error instanceof WorkerError && error.retryable ? "retryable failure" : String(error);
  }
  assert.notEqual(outcome, "Let me look that up.");
});

test("pi-ai reports a paused Anthropic turn as a plain stop, keeping the raw reason", async () => {
  // Pins the behaviour the chat must handle itself: stopReason says "stop",
  // only rawStopReason says the server paused mid-search.
  const fetchImpl = scriptedFetch([anthropicStream([{ type: "text", text: "Let me look that up." }, ...SEARCH_BLOCKS], "pause_turn")], []);
  const message = await anthropicChat(fetchImpl)({ systemPrompt: "s", messages: [{ role: "user", content: "q", timestamp: 0 }], tools: CHAT_TOOLS });
  assert.equal(message.stopReason, "stop");
  assert.equal((message as AssistantMessage & { rawStopReason?: string }).rawStopReason, "pause_turn");
  assert.deepEqual(message.content.map((part) => part.type), ["text"], "the search blocks are not kept");
});

test("a model still editing when the rounds run out keeps its proposal", async () => {
  const recipe = recipeDraft({ ...SHORTBREAD, ingredients: Array.from({ length: 12 }, (_, at) => ({ name: `item ${at + 1}`, quantity: "1", unit: "" })) })!;
  let turn = 0;
  const oneAtATime: ChatModelCall = async () => {
    turn += 1;
    return reply([toolCall("change_ingredient", { ingredient: turn, quantity: "2" }, `call-${turn}`)], "toolUse");
  };
  const result = await chatWithTools(oneAtATime, "s", "Double everything", chatLibrary({}), { log: () => {}, recipe });
  assert.ok(result.proposal, "the edits made so far come back for the cook to review");
});

test("chatReply falls back to the text when the JSON has no usable reply", () => {
  const lines = chatReply(JSON.stringify({ reply: ["Use less salt.", "Taste first."], label: "" }));
  assert.match(lines.reply, /Use less salt/);
});

test("chatWithTools sends the edit tools only with a recipe and keeps search and edits apart", async () => {
  const contexts: Context[] = [];
  const turns = [
    reply([toolCall("search_web", { query: "vegan butter" }), toolCall("change_ingredient", { ingredient: 1, name: "vegan butter" }, "call-edit")], "toolUse"),
    reply([{ type: "text", text: '{"reply":"Swapped.","label":"Vegan butter"}' }]),
  ];
  const fetchImpl = (async () => new Response(JSON.stringify({ results: [] }))) as unknown as typeof fetch;
  const result = await chatWithTools(async (context) => {
    contexts.push({ ...context, messages: [...context.messages] });
    return turns.shift()!;
  }, "s", "p", chatLibrary({}), { log: () => {}, search: { provider: "exa", apiKey: "k" }, fetchImpl, recipe: recipeDraft(SHORTBREAD) });
  assert.deepEqual(contexts[0].tools?.map((tool) => tool.name), [...CHAT_TOOLS, WEB_SEARCH_TOOL, ...EDIT_TOOLS, ANSWER_TOOL].map((tool) => tool.name));
  const names = new Set(contexts[0].tools?.map((tool) => tool.name));
  assert.equal(names.size, contexts[0].tools?.length, "no two tools share a name");
  assert.equal(result.proposal?.ingredients[0].name, "vegan butter");
  assert.match(resultText(contexts[1].messages[2] as ReturnType<typeof runChatTool>), /No web results/);
  // An edit tool without a recipe is an unknown tool, not a crash.
  const noRecipe = await chatWithTools(async (context) => context.messages.length === 1
    ? reply([toolCall("remove_step", { step: 1 })], "toolUse")
    : reply([{ type: "text", text: "Done." }]), "s", "p", chatLibrary({}), { log: () => {} });
  assert.equal(noRecipe.proposal, null);
});

/** A chatModelCall stand-in with a scripted chat call and, optionally, a
 * hosted searcher. */
function fakeModelCall(call: ChatModelCall, hosted: ((query: string) => Promise<string>) | null, seen: boolean[]) {
  return async (_request: unknown, searching: boolean) => {
    seen.push(searching);
    return { call, canSeePhotos: false, hostedSearch: searching ? hosted : null };
  };
}

const CHAT_REQUEST = {
  prompt: "Find a vegan butter and use it",
  systemPrompt: "s",
  searchMode: "gapfill",
  webSearch: { provider: "exa", apiKey: "exa-key" },
  recipe: { title: "Shortbread", ingredients: [{ name: "butter", quantity: "200", unit: "g" }], steps: [{ text: "Mix.", ingredients: ["200 g butter"] }] },
};

test("answerChat answers web_search with the provider's own search when it has one", async () => {
  const contexts: Context[] = [];
  const call: ChatModelCall = async (context) => {
    contexts.push({ ...context, messages: [...context.messages] });
    return context.messages.length === 1
      ? reply([toolCall("search_web", { query: "vegan block butter" }), toolCall("change_ingredient", { ingredient: 1, name: "vegan block butter" }, "call-edit")], "toolUse")
      : reply([{ type: "text", text: '{"reply":"Swapped.","label":"Vegan butter"}' }]);
  };
  const queries: string[] = [];
  const seen: boolean[] = [];
  const result = await answerChat(CHAT_REQUEST as never, fakeModelCall(call, async (query) => {
    queries.push(query);
    return `Web search for "${query}":\nBlock vegan butter bakes like butter.\nhttps://example.com/vegan-butter`;
  }, seen) as never, () => {});
  assert.deepEqual(seen, [true]);
  assert.deepEqual(queries, ["vegan block butter"]);
  assert.equal(result.search, "hosted");
  assert.ok(contexts[0].tools?.some((tool) => tool.name === "search_web"));
  assert.match(resultText(contexts[1].messages[2] as ReturnType<typeof runChatTool>), /example\.com\/vegan-butter/);
  assert.equal(result.proposal?.ingredients[0].name, "vegan block butter");
});

test("answerChat uses the search key only without hosted search, and nothing with search off", async () => {
  const plain: ChatModelCall = async (context) => {
    assert.equal(context.tools?.some((tool) => tool.name === "search_web"), true);
    return reply([{ type: "text", text: '{"reply":"Yes.","label":"x"}' }]);
  };
  const viaKey = await answerChat(CHAT_REQUEST as never, fakeModelCall(plain, null, []) as never, () => {});
  assert.equal(viaKey.search, "api");
  assert.equal(viaKey.label, "", "no edits, no label");
  const off: ChatModelCall = async (context) => {
    assert.equal(context.tools?.some((tool) => tool.name === "search_web"), false);
    return reply([{ type: "text", text: "Yes." }]);
  };
  const seen: boolean[] = [];
  const quiet = await answerChat({ ...CHAT_REQUEST, searchMode: "off" } as never, fakeModelCall(off, async () => "unused", seen) as never, () => {});
  assert.deepEqual(seen, [false]);
  assert.equal(quiet.search, "none");
});

test("a failed hosted search is an error result the model answers around", async () => {
  const contexts: Context[] = [];
  const call: ChatModelCall = async (context) => {
    contexts.push({ ...context, messages: [...context.messages] });
    return context.messages.length === 1
      ? reply([toolCall("search_web", { query: "q" })], "toolUse")
      : reply([{ type: "text", text: '{"reply":"From what I know, 160C.","label":""}' }]);
  };
  const result = await answerChat(CHAT_REQUEST as never, fakeModelCall(call, async () => {
    throw new WebSearchError("The web search came back empty.");
  }, []) as never, () => {});
  assert.equal(result.reply, "From what I know, 160C.");
  const output = contexts[1].messages[2] as ReturnType<typeof runChatTool>;
  assert.equal(output.isError, true);
  assert.match(resultText(output), /came back empty\. Answer without the web\./);
});

test("hostedSearchAnswer makes one search request and reads its text", async () => {
  const bodies: Record<string, unknown>[] = [];
  const fetchImpl = scriptedFetch([
    anthropicStream([
      ...SEARCH_BLOCKS,
      { type: "text", text: 'Rest it <cite index="1-2">5 minutes</cite>.\nhttps://example.com/rest' },
    ], "end_turn"),
  ], bodies);
  const text = await hostedSearchAnswer(anthropicChat(fetchImpl), "shakshuka resting time");
  assert.equal(text, 'Web search for "shakshuka resting time":\nRest it 5 minutes.\nhttps://example.com/rest');
  assert.equal(bodies.length, 1, "one request, nothing replayed");
  const tools = bodies[0].tools as Record<string, unknown>[];
  assert.deepEqual(tools, [{ type: "web_search_20250305", name: "web_search", max_uses: 4 }], "only the hosted search tool");
});

test("hostedSearchAnswer refuses an empty, paused, or failed search", async () => {
  const empty = scriptedFetch([anthropicStream([...SEARCH_BLOCKS], "tool_use")], []);
  await assert.rejects(hostedSearchAnswer(anthropicChat(empty), "q"), (error) => error instanceof WebSearchError && /empty/.test(error.message));
  const paused = scriptedFetch([anthropicStream([{ type: "text", text: "Let me look." }, ...SEARCH_BLOCKS], "pause_turn")], []);
  await assert.rejects(hostedSearchAnswer(anthropicChat(paused), "q"), (error) => error instanceof WebSearchError && /paused/.test(error.message));
  const failed: ChatModelCall = async () => ({ ...reply([], "error"), errorMessage: "400 tool not supported" }) as AssistantMessage;
  await assert.rejects(hostedSearchAnswer(failed, "q"), /web search failed \(400 tool not supported\)/);
});

test("chatReply drops the cite tags Claude's web search puts around quotes", () => {
  const text = JSON.stringify({ reply: 'Cover for <cite index="1-10">about 5 minutes</cite>, or <cite index="2-11">4-7 minutes</cite>.', label: "" });
  assert.equal(chatReply(text).reply, "Cover for about 5 minutes, or 4-7 minutes.");
  assert.equal(chatReply("Use <CITE index=\"3\">ghee</CITE>.").reply, "Use ghee.");
});

test("chatWithTools asks once more after an empty turn, and caps searches per answer", async () => {
  const turns = [
    reply([], "toolUse"),
    reply([{ type: "text", text: '{"reply":"Rest it 5 minutes.","label":""}' }]),
  ];
  const logged: string[] = [];
  const recovered = await chatWithTools(async () => turns.shift()!, "s", "p", chatLibrary({}), { log: (line) => logged.push(line) });
  assert.equal(recovered.text, '{"reply":"Rest it 5 minutes.","label":""}');
  assert.match(logged[0], /no text: stop=toolUse .*asking again/);

  const queries: string[] = [];
  const results: string[] = [];
  let turn = 0;
  const searchy: ChatModelCall = async (context) => {
    const last = context.messages.at(-1) as ReturnType<typeof runChatTool> | undefined;
    if (last?.role === "toolResult") results.push(resultText(last));
    turn += 1;
    return turn <= 4 ? reply([toolCall("search_web", { query: `q${turn}` }, `call-${turn}`)], "toolUse") : reply([{ type: "text", text: "Done." }]);
  };
  await chatWithTools(searchy, "s", "p", chatLibrary({}), {
    log: () => {},
    hostedSearch: async (query) => { queries.push(query); return `found ${query}`; },
  });
  assert.deepEqual(queries, ["q1", "q2", "q3"], "the fourth search is not run");
  assert.match(results[3], /most searches for one answer \(3\)/);
});

test("the answer tool finishes the turn, with edits called alongside it", async () => {
  const recipe = recipeDraft({ title: "Dal", ingredients: [{ name: "salt", quantity: "2", unit: "tsp" }], steps: [{ text: "Simmer.", ingredients: ["2 tsp salt"] }] })!;
  let calls = 0;
  const result = await chatWithTools(async () => {
    calls += 1;
    return reply([
      toolCall("change_ingredient", { ingredient: 1, quantity: "1" }),
      toolCall("answer", { reply: "Halved the salt.", label: "Less salt" }, "call-answer"),
    ], "toolUse");
  }, "s", "Too salty", chatLibrary({}), { log: () => {}, recipe });
  assert.equal(calls, 1, "no further model call after answer");
  assert.deepEqual(chatReply(result.text), { reply: "Halved the salt.", label: "Less salt" });
  assert.deepEqual(result.toolCalls, ["change_ingredient"]);
  assert.equal(result.proposal?.ingredients[0].quantity, "1");

  // An empty answer and a made-up tool get pointed at a proper answer.
  const contexts: Context[] = [];
  const turns = [
    reply([toolCall("answer", { reply: " " }), toolCall("json", {}, "call-json")], "toolUse"),
    reply([toolCall("answer", { reply: "Done." }, "call-2")], "toolUse"),
  ];
  const second = await chatWithTools(async (context) => {
    contexts.push({ ...context, messages: [...context.messages] });
    return turns.shift()!;
  }, "s", "p", chatLibrary({}), { log: () => {} });
  assert.equal(chatReply(second.text).reply, "Done.");
  const results = contexts[1].messages.slice(2).map((message) => resultText(message as ReturnType<typeof runChatTool>));
  assert.match(results[0], /Give answer your reply/);
  assert.match(results[1], /There is no tool json\. When you are done, call answer/);
});
