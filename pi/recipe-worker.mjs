import { mkdir } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import process from "node:process";
import {
  ModelRuntime,
  getAgentDir,
} from "@earendil-works/pi-coding-agent";
import { createNativeSearchObserver, nativeSearchPayload } from "./codex-native-search.mjs";
import { loadEpicure, pairwiseCritique, critiqueMessage, ingredientDiff } from "./epicure-scores.mjs";

const PROVIDER = "openai-codex";

const REASONING_EFFORTS = new Set(["low", "medium", "high"]);

/** Coerces a request's reasoningEffort to one of the supported values.
 *  Missing or blank means the host default ("low"); anything else is
 *  rejected so a mistyped setting fails loudly instead of silently
 *  running at a different effort than the user chose. */
export function normalizeEffort(value) {
  if (value === undefined || value === null) return "low";
  const effort = String(value).trim().toLowerCase();
  if (effort === "") return "low";
  if (!REASONING_EFFORTS.has(effort)) {
    throw new WorkerError("input", `Invalid reasoningEffort: ${value}`);
  }
  return effort;
}

export class WorkerError extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
  }
}

async function readRequest() {
  let input = "";
  for await (const chunk of process.stdin) input += chunk;
  try {
    const request = JSON.parse(input);
    if (!request || typeof request !== "object" || Array.isArray(request)) {
      throw new Error("invalid request");
    }
    if (request.command === "listModels") return request;
    if (request.command === "cleanMedia") {
      if (typeof request.prompt !== "string" || typeof request.systemPrompt !== "string") {
        throw new Error("missing cleaner prompt or systemPrompt");
      }
      return request;
    }
    if (typeof request.prompt !== "string" || typeof request.systemPrompt !== "string") {
      throw new Error("missing prompt or systemPrompt");
    }
    return request;
  } catch (error) {
    throw new WorkerError("input", `Invalid worker request: ${error.message}`);
  }
}

function outputJson(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}

/**
 * Reduce a provider's model catalogue to sorted, de-duplicated model ids,
 * newest first so the current generation (e.g. the 5.6 range) sits on top.
 */
export function catalogModelIds(models) {
  const seen = new Set();
  const ids = [];
  for (const model of models) {
    if (model && typeof model.id === "string" && !seen.has(model.id)) {
      seen.add(model.id);
      ids.push(model.id);
    }
  }
  return ids.sort((a, b) => b.localeCompare(a));
}

function extractJson(text) {
  const trimmed = text.trim();
  const fenced = trimmed.match(/^```(?:json)?\s*([\s\S]*?)\s*```$/i)?.[1]?.trim();
  const candidates = [trimmed, fenced].filter(Boolean);

  for (const candidate of candidates) {
    try {
      return JSON.parse(candidate);
    } catch {
      // A model occasionally prefixes an otherwise valid JSON object with one sentence.
    }
    const start = candidate.indexOf("{");
    if (start === -1) continue;
    for (let end = candidate.length; end > start; end -= 1) {
      try {
        return JSON.parse(candidate.slice(start, end));
      } catch {
        // Continue until the matching JSON object is found.
      }
    }
  }
  throw new WorkerError("output", "Pi did not return the required JSON response.");
}

function assistantText(message) {
  return (message?.content ?? [])
    .filter((part) => part.type === "text")
    .map((part) => part.text)
    .join("");
}

function sourceList(value, seenSources) {
  if (!Array.isArray(value)) return [];
  const urls = new Set();
  return value.flatMap((source) => {
    if (!source || typeof source.url !== "string" || !seenSources.has(source.url) || urls.has(source.url)) {
      return [];
    }
    urls.add(source.url);
    return [{
      title: typeof source.title === "string" && source.title.trim()
        ? source.title.trim()
        : seenSources.get(source.url),
      url: source.url,
    }];
  });
}

/**
 * Grounds the model's claimed sources against the citations the search
 * surfaced. Exact URL membership is enforced whenever citations exist, so a
 * claim that matches nothing is dropped as unverifiable. The Responses API
 * only attaches url_citation annotations when the model emits inline citation
 * markers, which the JSON-only instruction set does not trigger; the search
 * runs and informs the recipe, but the API returns zero citations to verify
 * against. In that case (`acceptUnverifiable`) the claimed http(s) URLs are
 * accepted as-is — the model was prompted to cite only URLs from the search
 * results — so grounded generation still works.
 */
export function verifiedSources(claimed, citations, acceptUnverifiable) {
  const verified = sourceList(claimed, citations);
  if (verified.length > 0 || citations.size > 0 || !acceptUnverifiable) return verified;
  const urls = new Set();
  const fallback = [];
  for (const source of Array.isArray(claimed) ? claimed : []) {
    if (!source || typeof source.url !== "string") continue;
    const url = source.url.trim();
    if (!/^https?:\/\//.test(url) || urls.has(url)) continue;
    urls.add(url);
    fallback.push({
      title: typeof source.title === "string" && source.title.trim() ? source.title.trim() : url,
      url,
    });
  }
  return fallback;
}

export function recipePrompt(systemPrompt, searchEnabled) {
  const output = `Return only one JSON object with this shape:\n{
  "recipe": { ...the supplied recipe schema... },
  "sources": [{ "title": "source title", "url": "https://source.example" }]
}\nDo not use Markdown fences or add prose before or after the JSON.`;
  if (!searchEnabled) return `${systemPrompt}\n\n${output}\nUse an empty sources array.`;
  return `${systemPrompt}\n\nOpenAI's native web_search tool is enabled for this request. Research before preparing the recipe. Use only URLs cited by the native search response in sources; list every source that materially informed the recipe.\n\n${output}`;
}

export function completionContext(request, searchEnabled) {
  const systemPrompt = recipePrompt(request.systemPrompt, searchEnabled);

  // Use the low-level completion API with an explicit context. Do not use
  // AgentSession/createAgentSession here: those APIs assemble pi's default
  // coding-agent system prompt before sending the request.
  return {
    systemPrompt,
    messages: [{ role: "user", content: request.prompt, timestamp: Date.now() }],
    tools: [],
  };
}

const OPENAI_RESPONSE_TIMEOUT_MS = 600_000; // generation can take minutes
const AI_GATEWAY_CLEANER_TIMEOUT_MS = 180_000;

function apiCredentials(request) {
  const apiBaseUrl = typeof request.apiBaseUrl === "string" && request.apiBaseUrl.trim()
    ? request.apiBaseUrl.trim() : null;
  const apiKey = typeof request.apiKey === "string" && request.apiKey.trim()
    ? request.apiKey.trim() : null;
  if (!apiBaseUrl || !apiKey) {
    throw new WorkerError("worker", "Missing apiBaseUrl or apiKey for the API request.");
  }
  return { apiBaseUrl, apiKey };
}

/** POST JSON and return the parsed body. Throws WorkerError on transport
 *  failure or non-2xx (error body truncated to 300 chars; never includes the
 *  key). Shared by the Responses API path and the Messages API path. */
async function fetchJson(url, headers, body, fetchImpl, timeoutMs = OPENAI_RESPONSE_TIMEOUT_MS) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const response = await fetchImpl(url, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        ...headers,
      },
      body: JSON.stringify(body),
      signal: controller.signal,
    });
    if (!response.ok) {
      const detail = await response.text().catch(() => "");
      throw new WorkerError(
        "worker",
        `API returned HTTP ${response.status}${detail ? `: ${detail.slice(0, 300)}` : ""}`,
      );
    }
    return await response.json();
  } catch (error) {
    if (error instanceof WorkerError) throw error;
    throw new WorkerError("worker", `API request failed: ${error instanceof Error ? error.message : "unknown error"}`);
  } finally {
    clearTimeout(timer);
  }
}

/** POST {base}/responses; returns the parsed JSON body. */
async function postResponses(apiBaseUrl, apiKey, body, fetchImpl) {
  const url = `${apiBaseUrl.replace(/\/+$/, "")}/responses`;
  return fetchJson(url, { Authorization: `Bearer ${apiKey}` }, body, fetchImpl);
}

/** Vercel AI Gateway's OpenAI-compatible chat-completions request used only
 * for reducing noisy local video evidence before final recipe generation. */
export async function aiGatewayChatCompletion(
  apiBaseUrl,
  apiKey,
  modelId,
  systemPrompt,
  prompt,
  fetchImpl = fetch,
  options = {},
) {
  const url = `${apiBaseUrl.replace(/\/+$/, "")}/chat/completions`;
  const headers = { Authorization: `Bearer ${apiKey}` };
  const body = {
    model: modelId,
    messages: [
      { role: "system", content: systemPrompt },
      { role: "user", content: prompt },
    ],
    // Reasoning is off by default for this extraction task. The caller may
    // select a Gateway-supported effort or omit the field for compatibility.
    reasoning: options.reasoning !== undefined ? options.reasoning : { effort: "none" },
    stream: false,
    max_tokens: options.maxTokens ?? 2048,
  };
  if (body.reasoning === null) delete body.reasoning;
  if (typeof options.temperature === "number") body.temperature = options.temperature;
  return fetchJson(url, headers, body, fetchImpl, AI_GATEWAY_CLEANER_TIMEOUT_MS);
}

function cleanerMessageText(data) {
  const message = data?.choices?.[0]?.message;
  if (!message || message.refusal) {
    throw new WorkerError("output", "AI Gateway refused the media-cleaner request.");
  }
  if (typeof message.content === "string") return message.content;
  if (Array.isArray(message.content)) {
    return message.content
      .filter((part) => part?.type === "text" && typeof part.text === "string")
      .map((part) => part.text)
      .join("");
  }
  throw new WorkerError("output", "AI Gateway returned no media-cleaner text.");
}

function cleanerScalar(value, max = 800) {
  if (typeof value !== "string") return "";
  return value.replaceAll("\u0000", " ").trim().slice(0, max);
}

function cleanerList(value) {
  const values = Array.isArray(value) ? value : typeof value === "string" ? [value] : [];
  return values
    .flatMap((item) => {
      if (typeof item === "string") return [item];
      if (!item || typeof item !== "object") return [];
      const direct = item.text ?? item.step ?? item.instruction ?? item.fact;
      if (direct) return [direct];
      const name = item.name ?? item.ingredient ?? "";
      const amount = item.amount ?? item.quantity ?? "";
      const unit = item.unit ?? "";
      return [
        [amount, unit, name].filter((part) => typeof part === "string" && part.trim()).join(" "),
      ];
    })
    .map((item) => cleanerScalar(item))
    .filter(Boolean)
    .slice(0, 80);
}

/** Converts the model's constrained JSON into a small, known-field-only text
 * block. Unknown response fields and prose are discarded rather than passed
 * through to the final recipe model. */
export function formatRecipeEvidence(value) {
  const root = value?.recipeEvidence && typeof value.recipeEvidence === "object"
    ? value.recipeEvidence
    : value;
  if (!root || typeof root !== "object" || Array.isArray(root)) {
    throw new WorkerError("output", "AI Gateway returned an invalid media-cleaner object.");
  }
  const title = cleanerScalar(root.title);
  const servings = cleanerScalar(root.servings);
  const ingredients = cleanerList(root.ingredients);
  const steps = cleanerList(root.steps);
  const timings = cleanerList(root.timings);
  const notes = cleanerList(root.relevant_notes ?? root.relevantNotes);
  if (ingredients.length === 0 && steps.length === 0) {
    throw new WorkerError("output", "AI Gateway found no recipe facts in the video evidence.");
  }
  const lines = [];
  if (title) lines.push(`Dish: ${title}`);
  if (servings) lines.push(`Servings: ${servings}`);
  if (ingredients.length > 0) {
    lines.push("Ingredients:");
    lines.push(...ingredients.map((item) => `- ${item}`));
  }
  if (steps.length > 0) {
    lines.push("Method:");
    lines.push(...steps.map((item, index) => `${index + 1}. ${item}`));
  }
  if (timings.length > 0) {
    lines.push("Timings and temperatures:");
    lines.push(...timings.map((item) => `- ${item}`));
  }
  if (notes.length > 0) {
    lines.push("Relevant recipe notes:");
    lines.push(...notes.map((item) => `- ${item}`));
  }
  const text = lines.join("\n").trim();
  if (text.length > 24_000) {
    throw new WorkerError("output", "AI Gateway returned too much media-cleaner text.");
  }
  return text;
}

export function parseAiGatewayCleaner(data) {
  const text = cleanerMessageText(data);
  return formatRecipeEvidence(extractJson(text));
}

export async function openaiResponse(apiBaseUrl, apiKey, modelId, systemPrompt, prompt, searchEnabled, reasoningEffort = "low", fetchImpl = fetch) {
  return postResponses(apiBaseUrl, apiKey, {
    model: modelId,
    input: [
      { role: "system", content: systemPrompt },
      { role: "user", content: prompt },
    ],
    ...(searchEnabled ? { tools: [{ type: "web_search" }], tool_choice: "required" } : {}),
    reasoning: { effort: reasoningEffort },
    text: { verbosity: "low" },
  }, fetchImpl);
}

/** Multi-message Responses API call for the critique-revision turn: system
 *  prompt plus arbitrary role messages, no tools (revision is never
 *  web-searched). Returns the parsed JSON body with the same error mapping as
 *  openaiResponse. Input items are reduced to role/content: the critique pass
 *  stamps its messages with `timestamp` for the pi SDK path, and the Responses
 *  API rejects unknown fields on input items (HTTP 400). */
export async function openaiChat(apiBaseUrl, apiKey, modelId, systemPrompt, messages, reasoningEffort = "low", fetchImpl = fetch) {
  return postResponses(apiBaseUrl, apiKey, {
    model: modelId,
    input: [
      { role: "system", content: systemPrompt },
      ...messages.map(({ role, content }) => ({ role, content })),
    ],
    reasoning: { effort: reasoningEffort },
    text: { verbosity: "low" },
  }, fetchImpl);
}

/** Reduces a Responses API body to what the worker needs.
 *  citations is a Map<url, title> built from url_citation annotations. */
export function parseResponsesOutput(data) {
  const output = Array.isArray(data?.output) ? data.output : [];
  let searched = false;
  let refused = false;
  const citations = new Map();
  const texts = [];
  for (const item of output) {
    if (item?.type === "web_search_call") {
      const action = item.action;
      const actionType = typeof action === "string" ? action : action?.type;
      if (actionType === "search") searched = true;
    } else if (item?.type === "message") {
      for (const part of item.content ?? []) {
        if (part?.type === "refusal") refused = true;
        if (part?.type === "output_text") texts.push(part.text ?? "");
        for (const annotation of part?.annotations ?? []) {
          // Documented shape: the annotation itself carries url/title
          // ({type:"url_citation", url, title}); the nested url_citation
          // object is accepted defensively for shape variants.
          const citation = annotation?.url_citation ?? annotation;
          if (citation && typeof citation.url === "string" && citation.url) {
            citations.set(citation.url, typeof citation.title === "string" ? citation.title : "");
          }
        }
      }
    }
  }
  return { text: texts.join(""), searched, refused, citations };
}

const ANTHROPIC_VERSION = "2023-06-01";
const ANTHROPIC_WEB_SEARCH_TOOL = "web_search_20250305";
/** Thinking budget tokens per reasoning effort; max_tokens must exceed it. */
const ANTHROPIC_THINKING_BUDGETS = { low: 1024, medium: 4096, high: 8192 };
/** Output headroom added to the thinking budget for max_tokens. */
const ANTHROPIC_OUTPUT_HEADROOM = 8192;

export function thinkingBudget(reasoningEffort) {
  return ANTHROPIC_THINKING_BUDGETS[normalizeEffort(reasoningEffort)] ?? ANTHROPIC_THINKING_BUDGETS.low;
}

/** The Messages API path for a stored base URL: append /v1/messages, unless
 *  the base already ends in /v1, then append /messages. */
export function anthropicUrl(baseUrl) {
  const base = baseUrl.replace(/\/+$/, "");
  return /\/v1$/i.test(base) ? `${base}/messages` : `${base}/v1/messages`;
}

/** POST {base}/v1/messages with x-api-key auth. Extended thinking is always
 *  enabled with an effort-scaled budget; the web_search tool is attached when
 *  search is enabled. max_tokens covers the budget plus output headroom.
 *  Returns the parsed JSON body with the same error mapping as openaiResponse. */
export async function anthropicMessages(apiBaseUrl, apiKey, modelId, systemPrompt, messages, reasoningEffort = "low", searchEnabled = false, fetchImpl = fetch) {
  const budget = thinkingBudget(reasoningEffort);
  const body = {
    model: modelId,
    max_tokens: budget + ANTHROPIC_OUTPUT_HEADROOM,
    system: systemPrompt,
    messages,
    thinking: { type: "enabled", budget_tokens: budget },
    ...(searchEnabled ? { tools: [{ type: ANTHROPIC_WEB_SEARCH_TOOL }] } : {}),
  };
  return fetchJson(anthropicUrl(apiBaseUrl), {
    "x-api-key": apiKey,
    "anthropic-version": ANTHROPIC_VERSION,
  }, body, fetchImpl);
}

/** Reduces a Messages API body to what the worker needs. Web search results
 *  arrive as content blocks of type web_search_tool_result, each holding
 *  search_result items with source.url / title; those become the citations
 *  the claimed sources are verified against. */
export function parseAnthropicOutput(data) {
  const content = Array.isArray(data?.content) ? data.content : [];
  let searched = false;
  const citations = new Map();
  const texts = [];
  for (const block of content) {
    if (block?.type === "text") {
      if (typeof block.text === "string") texts.push(block.text);
    } else if (block?.type === "web_search_tool_result") {
      searched = true;
      for (const item of Array.isArray(block.content) ? block.content : []) {
        if (item?.type !== "search_result") continue;
        const source = item.source;
        const url = source && source.type === "url" && typeof source.url === "string"
          ? source.url
          : typeof item.url === "string" ? item.url : "";
        if (url) citations.set(url, typeof item.title === "string" && item.title ? item.title : "");
      }
    }
  }
  return { text: texts.join(""), searched, refused: data?.stop_reason === "refusal", citations };
}

/**
 * Two-turn critique pass: scores the turn-1 recipe's ingredient pairings with
 * the bundled epicure model, sends the compressed critique to the model on a
 * second turn (no web search), and returns the revision. Fully fail-soft —
 * any data, call, or parse failure returns the turn-1 recipe with
 * critique: null, so the pass can never break generation.
 * @returns {Promise<{recipe: object, critique: object|null}>}
 */
export async function critiquePass({ prompt, turn1Text, turn1Recipe, systemPrompt, callModel, log = console.error }) {
  try {
    const data = loadEpicure();
    const critique = pairwiseCritique(turn1Recipe, data);
    if (!critique) {
      log(JSON.stringify({ event: "pairwise_critique", skipped: true, reason: "fewer than two matched ingredients" }));
      return { recipe: turn1Recipe, critique: null };
    }
    const message = critiqueMessage(critique);
    log(JSON.stringify({ event: "pairwise_critique", ...critique, turn2Message: message }));
    const text = await callModel([
      { role: "user", content: prompt, timestamp: Date.now() },
      { role: "assistant", content: turn1Text, timestamp: Date.now() },
      { role: "user", content: message, timestamp: Date.now() },
    ]);
    if (text === null) {
      log(JSON.stringify({ event: "pairwise_critique", skipped: true, reason: "revision call failed or was refused" }));
      return { recipe: turn1Recipe, critique: null };
    }
    let revised;
    try {
      revised = extractJson(text);
    } catch {
      log(JSON.stringify({ event: "pairwise_critique", skipped: true, reason: "revision response was not valid JSON" }));
      return { recipe: turn1Recipe, critique: null };
    }
    if (!revised || typeof revised !== "object" || !revised.recipe || typeof revised.recipe !== "object") {
      log(JSON.stringify({ event: "pairwise_critique", skipped: true, reason: "revision response lacked a recipe object" }));
      return { recipe: turn1Recipe, critique: null };
    }
    const diff = ingredientDiff(turn1Recipe, revised.recipe);
    log(JSON.stringify({ event: "pairwise_critique_result", ...diff }));
    return { recipe: revised.recipe, critique: { ...critique, ...diff } };
  } catch (error) {
    log(JSON.stringify({
      event: "pairwise_critique",
      skipped: true,
      reason: error instanceof Error ? error.message : "unknown error",
    }));
    return { recipe: turn1Recipe, critique: null };
  }
}

/**
 * Refreshes the openai-codex provider catalogue from pi.dev (the same source
 * `pi update --models` uses) and returns the current model ids, newest first.
 * The refresh is credential-gated: the host materialises the database-backed
 * Codex credential at `authPath`; an expired token is refreshed in place in
 * that file, so the host reads it back to persist the refresh.
 */
async function listModels(request) {
  const agentDir = process.env.PI_CODING_AGENT_DIR || getAgentDir();
  await mkdir(agentDir, { recursive: true });
  const authPath = typeof request.authPath === "string" && request.authPath.trim()
    ? request.authPath
    : join(agentDir, "auth.json");
  const modelRuntime = await ModelRuntime.create({
    authPath,
    modelsPath: join(agentDir, "models.json"),
  });
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 20_000);
  try {
    await modelRuntime.refresh({ allowNetwork: true, force: true, signal: controller.signal });
  } finally {
    clearTimeout(timer);
  }
  outputJson({ models: catalogModelIds(modelRuntime.getModels(PROVIDER)) });
}

async function cleanMedia(request) {
  const apiKey = process.env.AI_GATEWAY_API_KEY?.trim();
  if (!apiKey) {
    throw new WorkerError("configuration", "AI_GATEWAY_API_KEY is not configured.");
  }
  const apiBaseUrl = process.env.AI_GATEWAY_BASE_URL?.trim() || "https://ai-gateway.vercel.sh/v1";
  const modelId = typeof request.model === "string" && request.model.trim()
    ? request.model.trim()
    : process.env.AI_GATEWAY_CLEANER_MODEL?.trim() || "poolside/laguna-s-2.1-free";
  // Optional tuning knobs forwarded from the host (ai.rs).
  const options = {
    reasoning: typeof request.reasoning === "object" && request.reasoning ? request.reasoning : undefined,
    maxTokens: typeof request.maxTokens === "number" ? request.maxTokens : undefined,
    temperature: typeof request.temperature === "number" ? request.temperature : undefined,
  };
  for (const key of Object.keys(options)) {
    if (options[key] === undefined) delete options[key];
  }
  const data = await aiGatewayChatCompletion(
    apiBaseUrl,
    apiKey,
    modelId,
    request.systemPrompt,
    request.prompt,
    undefined,
    options,
  );
  outputJson({ cleanedText: parseAiGatewayCleaner(data), model: modelId });
}

async function main() {
  const request = await readRequest();
  if (request.command === "listModels") {
    await listModels(request);
    return;
  }
  if (request.command === "cleanMedia") {
    await cleanMedia(request);
    return;
  }
  const agentDir = process.env.PI_CODING_AGENT_DIR || getAgentDir();
  const searchEnabled = Boolean(request.searchEnabled);
  const modelId = typeof request.model === "string" && request.model.trim()
    ? request.model.trim()
    : "gpt-5.4-mini";
  const reasoningEffort = normalizeEffort(request.reasoningEffort);
  if (request.provider === "openai") {
    const { apiBaseUrl, apiKey } = apiCredentials(request);
    const systemPrompt = recipePrompt(request.systemPrompt, searchEnabled);
    const data = await openaiResponse(apiBaseUrl, apiKey, modelId, systemPrompt, request.prompt, searchEnabled, reasoningEffort);
    if (data.status && data.status !== "completed") {
      throw new WorkerError("worker", `OpenAI response incomplete: ${data.incomplete_details?.reason ?? "unknown reason"}`);
    }
    const { text, searched, refused, citations } = parseResponsesOutput(data);
    if (refused) throw new WorkerError("worker", "OpenAI refused the request.");
    if (searchEnabled && !searched) {
      throw new WorkerError("output", "OpenAI returned a recipe without running web search.");
    }
    const result = extractJson(text);
    if (!result || typeof result !== "object" || !result.recipe || typeof result.recipe !== "object") {
      throw new WorkerError("output", "Pi did not return a recipe object.");
    }
    const sources = verifiedSources(result.sources, citations, true);
    if (searchEnabled && sources.length === 0) {
      throw new WorkerError("output", "OpenAI did not use any valid web-search sources.");
    }
    const pass = request.pairwiseCritique === true
      ? await critiquePass({
          prompt: request.prompt,
          turn1Text: text,
          turn1Recipe: result.recipe,
          systemPrompt: request.systemPrompt,
          callModel: async (messages) => {
            try {
              const revisionData = await openaiChat(apiBaseUrl, apiKey, modelId, systemPrompt, messages, reasoningEffort);
              const { text: revisionText, refused: revisionRefused } = parseResponsesOutput(revisionData);
              return revisionRefused ? null : revisionText;
            } catch {
              return null;
            }
          },
        })
      : { recipe: result.recipe, critique: null };
    outputJson(pass.critique
      ? { recipe: pass.recipe, sources, critique: pass.critique }
      : { recipe: pass.recipe, sources });
    return;
  }
  if (request.provider === "anthropic") {
    const { apiBaseUrl, apiKey } = apiCredentials(request);
    const systemPrompt = recipePrompt(request.systemPrompt, searchEnabled);
    const data = await anthropicMessages(
      apiBaseUrl,
      apiKey,
      modelId,
      systemPrompt,
      [{ role: "user", content: request.prompt }],
      reasoningEffort,
      searchEnabled,
    );
    if (data.stop_reason === "max_tokens") {
      throw new WorkerError("worker", "Anthropic response was truncated by max_tokens.");
    }
    const { text, searched, refused, citations } = parseAnthropicOutput(data);
    if (refused) throw new WorkerError("worker", "Anthropic refused the request.");
    if (searchEnabled && !searched) {
      throw new WorkerError("output", "Anthropic returned a recipe without running web search.");
    }
    const result = extractJson(text);
    if (!result || typeof result !== "object" || !result.recipe || typeof result.recipe !== "object") {
      throw new WorkerError("output", "Pi did not return a recipe object.");
    }
    const sources = verifiedSources(result.sources, citations, true);
    if (searchEnabled && sources.length === 0) {
      throw new WorkerError("output", "Anthropic did not use any valid web-search sources.");
    }
    const pass = request.pairwiseCritique === true
      ? await critiquePass({
          prompt: request.prompt,
          turn1Text: text,
          turn1Recipe: result.recipe,
          systemPrompt: request.systemPrompt,
          callModel: async (messages) => {
            try {
              const revisionData = await anthropicMessages(
                apiBaseUrl,
                apiKey,
                modelId,
                systemPrompt,
                messages.map(({ role, content }) => ({ role, content })),
                reasoningEffort,
                false,
              );
              const { text: revisionText, refused: revisionRefused } = parseAnthropicOutput(revisionData);
              return revisionRefused ? null : revisionText;
            } catch {
              return null;
            }
          },
        })
      : { recipe: result.recipe, critique: null };
    outputJson(pass.critique
      ? { recipe: pass.recipe, sources, critique: pass.critique }
      : { recipe: pass.recipe, sources });
    return;
  }
  await mkdir(agentDir, { recursive: true });

  // The host supplies a per-request auth.json (its database-backed credential)
  // through `authPath`; the SDK refreshes tokens in place in that file.
  const authPath = typeof request.authPath === "string" && request.authPath.trim()
    ? request.authPath
    : join(agentDir, "auth.json");
  const modelRuntime = await ModelRuntime.create({
    authPath,
    modelsPath: join(agentDir, "models.json"),
  });
  if (!(await modelRuntime.checkAuth(PROVIDER))) {
    throw new WorkerError(
      "configuration",
      "Pi is not logged in to ChatGPT. Authorise Codex from the Settings page.",
    );
  }
  const model = modelRuntime.getModel(PROVIDER, modelId);
  if (!model) {
    throw new WorkerError("configuration", `Pi does not know the ${PROVIDER}/${modelId} model.`);
  }

  const observer = searchEnabled ? createNativeSearchObserver() : null;
  const context = completionContext(request, searchEnabled);
  const response = await modelRuntime.complete(
    model,
    context,
    {
      reasoningEffort,
      textVerbosity: "low",
      transport: "sse",
      maxRetries: 1,
      ...(observer ? {
        fetch: observer.fetch,
        onPayload: nativeSearchPayload,
      } : {}),
    },
  );
  if (response.stopReason === "error" || response.stopReason === "aborted") {
    throw new WorkerError("worker", response.errorMessage || "Pi model request failed.");
  }
  const observed = observer ? await observer.finish() : { invoked: false, sources: new Map() };
  if (searchEnabled && !observed.invoked) {
    throw new WorkerError("output", "Codex returned a recipe without running native web search.");
  }

  const turn1Text = assistantText(response);
  const result = extractJson(turn1Text);
  if (!result || typeof result !== "object" || !result.recipe || typeof result.recipe !== "object") {
    throw new WorkerError("output", "Pi did not return a recipe object.");
  }
  const sources = sourceList(result.sources, observed.sources);
  if (searchEnabled && sources.length === 0) {
    throw new WorkerError("output", "Pi did not use any valid web-search sources.");
  }
  const pass = request.pairwiseCritique === true
    ? await critiquePass({
        prompt: request.prompt,
        turn1Text,
        turn1Recipe: result.recipe,
        systemPrompt: request.systemPrompt,
        callModel: async (messages) => {
          const revision = await modelRuntime.complete(model, {
            systemPrompt: recipePrompt(request.systemPrompt, false),
            messages,
            tools: [],
          }, { reasoningEffort, textVerbosity: "low", transport: "sse", maxRetries: 1 });
          if (revision.stopReason === "error" || revision.stopReason === "aborted") return null;
          return assistantText(revision);
        },
      })
    : { recipe: result.recipe, critique: null };
  outputJson(pass.critique
    ? { recipe: pass.recipe, sources, critique: pass.critique }
    : { recipe: pass.recipe, sources });
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  main().catch((error) => {
    const workerError = error instanceof WorkerError
      ? error
      : new WorkerError("worker", error instanceof Error ? error.message : "Pi worker failed.");
    outputJson({ error: workerError.message, code: workerError.code });
    process.exitCode = 1;
  });
}
