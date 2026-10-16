import { mkdir } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import process from "node:process";
import {
  ModelRuntime,
  getAgentDir,
} from "@earendil-works/pi-coding-agent";
import {
  anthropicSearchPayload,
  createNativeSearchObserver,
  nativeSearchPayload,
  normalizeSourceUrl,
  optionalSearchPayload,
  webSearchCompleted,
} from "./native-search.mjs";
import { outputJson, readStdin, runMain } from "./cli.mjs";

/** Subscription providers the worker runs through pi's ModelRuntime, keyed by
 * pi provider id. The host stores each OAuth credential under the same key in
 * the per-request auth.json. Both go through completeSimple, so `reasoning` maps
 * to each model's own thinking control and a level the model lacks is clamped to
 * the nearest one it offers (Codex max runs as xhigh; Claude xhigh and max run
 * as high). */
const SUBSCRIPTIONS = {
  "openai-codex": {
    label: "Codex",
    notLoggedIn: "Pi is not logged in to ChatGPT. Authorise Codex from the Settings page.",
    defaultModel: "gpt-6-luna",
    // The observer reads search events from SSE through fetch, so an observed
    // Codex request must pin that transport; websocket would bypass it.
    searchOptions: (searchMode) => ({
      transport: "sse",
      onPayload: searchMode === "grounded" ? nativeSearchPayload : optionalSearchPayload,
    }),
  },
  anthropic: {
    label: "Claude",
    notLoggedIn: "Pi is not logged in to Claude. Authorise Claude from the Settings page.",
    defaultModel: "claude-sonnet-5-5",
    searchOptions: () => ({ onPayload: anthropicSearchPayload }),
  },
};

/** The trimmed string value, or null when it is missing or blank. */
function nonBlank(value) {
  return typeof value === "string" && value.trim() ? value.trim() : null;
}

/** The subscription entry for a request's piProvider; defaults to Codex. */
export function subscriptionOf(request) {
  const id = nonBlank(request.piProvider) ?? "openai-codex";
  if (!Object.hasOwn(SUBSCRIPTIONS, id)) {
    throw new WorkerError("configuration", `Unsupported pi provider: ${id}`);
  }
  return { id, ...SUBSCRIPTIONS[id] };
}

const REASONING_EFFORTS = new Set(["low", "medium", "high", "xhigh", "max"]);

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
  constructor(code, message, options = {}) {
    super(message);
    this.code = code;
    this.retryable = options.retryable === true;
  }
}

/** Parses one stdin request. listModels needs no prompt; every other command does. */
export function parseRequest(input) {
  let request;
  try {
    request = JSON.parse(input);
  } catch (error) {
    throw new WorkerError("input", `Invalid worker request: ${error.message}`);
  }
  if (!request || typeof request !== "object" || Array.isArray(request)) {
    throw new WorkerError("input", "Invalid worker request: invalid request");
  }
  if (request.command !== "listModels"
    && (typeof request.prompt !== "string" || typeof request.systemPrompt !== "string")) {
    throw new WorkerError("input", "Invalid worker request: missing prompt or systemPrompt");
  }
  return request;
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

/** Parses the first JSON object in a model reply, which may be fenced or have
 * prose around it. A JSON object ends at a closing brace, so only those
 * positions are tried, last first. */
function extractJson(text) {
  const trimmed = text.trim();
  const fenced = trimmed.match(/^```(?:json)?\s*([\s\S]*?)\s*```$/i)?.[1]?.trim() ?? "";
  for (const candidate of [trimmed, fenced]) {
    const start = candidate.indexOf("{");
    if (start === -1) continue;
    for (let end = candidate.lastIndexOf("}"); end > start; end = candidate.lastIndexOf("}", end - 1)) {
      try {
        return JSON.parse(candidate.slice(start, end + 1));
      } catch {
        // Not a complete object ending here; try the previous closing brace.
      }
    }
  }
  throw new WorkerError("output", "Pi did not return the required JSON response.");
}

function recipeObject(text) {
  const result = extractJson(text);
  if (!result || typeof result !== "object" || !result.recipe || typeof result.recipe !== "object") {
    throw new WorkerError("output", "Pi did not return a recipe object.");
  }
  return result;
}

function assistantText(message) {
  return (message?.content ?? [])
    .filter((part) => part.type === "text")
    .map((part) => part.text)
    .join("");
}

/**
 * Grounds the model's claimed sources against the citations the search
 * surfaced. Exact URL membership is enforced whenever citations exist, so a
 * claim that matches nothing is dropped as unverifiable. The Responses API
 * may expose sources on the search action, or attach url_citation annotations
 * when the model emits inline citation markers; the JSON-only instruction set
 * does not reliably trigger the latter. If neither is returned and
 * (`acceptUnverifiable`) is true, claimed http(s) URLs are accepted as-is for
 * optional gap-fill attribution. Grounded callers leave this fallback disabled.
 */
export function verifiedSources(claimed, citations, acceptUnverifiable) {
  const seen = new Set();
  const claims = [];
  for (const source of Array.isArray(claimed) ? claimed : []) {
    const url = normalizeSourceUrl(source?.url);
    if (!url || seen.has(url)) continue;
    seen.add(url);
    claims.push({ url, title: typeof source.title === "string" ? source.title.trim() : "" });
  }
  const verified = claims
    .filter(({ url }) => citations.has(url))
    .map(({ url, title }) => ({ title: title || citations.get(url) || url, url }));
  if (verified.length > 0 || citations.size > 0 || !acceptUnverifiable) return verified;
  return claims.map(({ url, title }) => ({ title: title || url, url }));
}

/** Tri-state search posture carried on generation requests: "grounded"
 *  must research and cite web sources, "gapfill" may research to fill gaps
 *  in untrusted imported evidence without requiring it, and any other
 *  value means plain generation without the web_search tool. */
export function searchModeOf(request) {
  const mode = typeof request?.searchMode === "string" ? request.searchMode.trim() : "";
  return mode === "grounded" || mode === "gapfill" || mode === "off" ? mode : "off";
}

export function recipePrompt(systemPrompt, searchMode = "off") {
  const output = `Return only one JSON object with this shape:\n{
  "recipe": { ...the supplied recipe schema... },
  "sources": [{ "title": "source title", "url": "https://source.example" }]
}\nDo not use Markdown fences or add prose before or after the JSON.`;
  if (searchMode === "gapfill") {
    return `${systemPrompt}\n\nThe native web_search tool is available but not required for this request. The material you are given comes from an imported social video and can be incomplete, garbled, or misleading; use web_search when it is needed to fill gaps or resolve contradictions, then list every source that materially informed the recipe.\n\n${output}`;
  }
  if (searchMode === "grounded") {
    return `${systemPrompt}\n\nOpenAI's native web_search tool is enabled for this request. Research before preparing the recipe. Use only URLs cited by the native search response in sources; list every source that materially informed the recipe.\n\n${output}`;
  }
  return `${systemPrompt}\n\n${output}\nUse an empty sources array.`;
}

export function completionContext(request, searchMode = "off") {
  const systemPrompt = recipePrompt(request.systemPrompt, searchMode);

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

/** POST JSON and return the parsed body. Throws WorkerError on transport
 *  failure or non-2xx (error body truncated to 300 chars; never includes the
 *  key). Shared by the Responses API path, the Messages API path and the
 *  AI Gateway cleaner. */
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
      const status = Number(response.status);
      const retryable = status === 408 || status === 409 || status === 425 || status === 429 || status >= 500;
      throw new WorkerError(
        "worker",
        `API returned HTTP ${response.status}${detail ? `: ${detail.slice(0, 300)}` : ""}`,
        { retryable },
      );
    }
    return await response.json();
  } catch (error) {
    if (error instanceof WorkerError) throw error;
    const retryable = !(error instanceof Error && error.name === "AbortError");
    throw new WorkerError(
      "worker",
      `API request failed: ${error instanceof Error ? error.message : "unknown error"}`,
      { retryable },
    );
  } finally {
    clearTimeout(timer);
  }
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

/** POST {base}/responses with bearer auth; returns the parsed JSON body. */
export async function openaiResponse(apiBaseUrl, apiKey, modelId, systemPrompt, prompt, searchMode, reasoningEffort = "low", fetchImpl = fetch) {
  return fetchJson(
    `${apiBaseUrl.replace(/\/+$/, "")}/responses`,
    { Authorization: `Bearer ${apiKey}` },
    {
      model: modelId,
      input: [
        { role: "system", content: systemPrompt },
        { role: "user", content: prompt },
      ],
      ...(searchMode !== "off"
        ? { tools: [{ type: "web_search" }], tool_choice: searchMode === "grounded" ? "required" : "auto" }
        : {}),
      reasoning: { effort: reasoningEffort },
      text: { verbosity: "low" },
    },
    fetchImpl,
  );
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
      if (webSearchCompleted(item)) searched = true;
      for (const source of item.action?.sources ?? []) {
        const url = normalizeSourceUrl(source?.url);
        if (url) citations.set(url, typeof source.title === "string" ? source.title : "");
      }
    } else if (item?.type === "message") {
      for (const part of item.content ?? []) {
        if (part?.type === "refusal") refused = true;
        if (part?.type === "output_text") texts.push(part.text ?? "");
        for (const annotation of part?.annotations ?? []) {
          // Documented shape: the annotation itself carries url/title
          // ({type:"url_citation", url, title}); the nested url_citation
          // object is accepted defensively for shape variants.
          const citation = annotation?.url_citation ?? annotation;
          const url = normalizeSourceUrl(citation?.url);
          if (url) {
            citations.set(url, typeof citation.title === "string" ? citation.title : "");
          }
        }
      }
    }
  }
  return { text: texts.join(""), searched, refused, citations };
}

const ANTHROPIC_VERSION = "2023-06-01";
const ANTHROPIC_WEB_SEARCH_TOOL = "web_search_20250305";
/** Thinking budget tokens per reasoning effort; max_tokens must exceed it.
 * Anthropic has no xhigh/max tiers, so those shared settings are capped at
 * its highest available budget rather than silently falling back to low. */
const ANTHROPIC_THINKING_BUDGETS = { low: 1024, medium: 4096, high: 8192, xhigh: 8192, max: 8192 };
/** Output headroom added to the thinking budget for max_tokens. */
const ANTHROPIC_OUTPUT_HEADROOM = 8192;

export function thinkingBudget(reasoningEffort) {
  return ANTHROPIC_THINKING_BUDGETS[normalizeEffort(reasoningEffort)];
}

/** The Messages API path for a stored base URL: append /v1/messages, unless
 *  the base already ends in /v1, then append /messages. */
export function anthropicUrl(baseUrl) {
  const base = baseUrl.replace(/\/+$/, "");
  return /\/v1$/i.test(base) ? `${base}/messages` : `${base}/v1/messages`;
}

/** POST {base}/v1/messages with x-api-key auth. Extended thinking is always
 *  enabled with an effort-scaled budget; the web_search tool is attached unless
 *  searchMode is "off". max_tokens covers the budget plus output headroom.
 *  Returns the parsed JSON body with the same error mapping as openaiResponse. */
export async function anthropicMessages(apiBaseUrl, apiKey, modelId, systemPrompt, messages, reasoningEffort = "low", searchMode = "off", fetchImpl = fetch) {
  const budget = thinkingBudget(reasoningEffort);
  const body = {
    model: modelId,
    max_tokens: budget + ANTHROPIC_OUTPUT_HEADROOM,
    system: systemPrompt,
    messages,
    thinking: { type: "enabled", budget_tokens: budget },
    ...(searchMode !== "off" ? { tools: [{ type: ANTHROPIC_WEB_SEARCH_TOOL }] } : {}),
  };
  return fetchJson(anthropicUrl(apiBaseUrl), {
    "x-api-key": apiKey,
    "anthropic-version": ANTHROPIC_VERSION,
  }, body, fetchImpl);
}

/** Reduces a Messages API body to what the worker needs. Web search results
 *  arrive as web_search_result items inside a web_search_tool_result block; the
 *  answer's own citations are web_search_result_location entries on its text
 *  blocks. Both are the URLs the claimed sources are verified against. */
export function parseAnthropicOutput(data) {
  const content = Array.isArray(data?.content) ? data.content : [];
  let searched = false;
  const citations = new Map();
  const cite = (item) => {
    const url = normalizeSourceUrl(item.url);
    if (url) citations.set(url, typeof item.title === "string" ? item.title : "");
  };
  const texts = [];
  for (const block of content) {
    if (block?.type === "text") {
      if (typeof block.text === "string") texts.push(block.text);
      for (const citation of block.citations ?? []) {
        if (citation?.type === "web_search_result_location") cite(citation);
      }
    } else if (block?.type === "web_search_tool_result") {
      searched = true;
      for (const result of Array.isArray(block.content) ? block.content : []) {
        if (result?.type === "web_search_result") cite(result);
      }
    }
  }
  return { text: texts.join(""), searched, refused: data?.stop_reason === "refusal", citations };
}

/** Checks a generated recipe against the search posture and returns it with
 * its verified sources. `citations` maps URL to title from the provider. */
function finishRecipe({ label, text, citations, searched, searchMode, acceptUnverifiable }) {
  if (searchMode === "grounded" && !searched) {
    throw new WorkerError("output", `${label} returned a recipe without running web search.`);
  }
  const result = recipeObject(text);
  const sources = verifiedSources(result.sources, citations, acceptUnverifiable);
  if (searchMode === "grounded" && sources.length === 0) {
    throw new WorkerError("output", `${label} did not use any valid web-search sources.`);
  }
  return { recipe: result.recipe, sources };
}

/**
 * Refreshes the subscription provider's catalogue from pi.dev (the same source
 * `pi update --models` uses) and returns the current model ids, newest first.
 * The refresh is credential-gated: the host materialises the database-backed
 * OAuth credential at `authPath`; an expired token is refreshed in place in
 * that file, so the host reads it back to persist the refresh.
 */
async function listModels(request) {
  const subscription = subscriptionOf(request);
  const modelRuntime = await subscriptionRuntime(request);
  await modelRuntime.refresh({ allowNetwork: true, force: true, signal: AbortSignal.timeout(20_000) });
  outputJson({ models: catalogModelIds(modelRuntime.getModels(subscription.id)) });
}

/** The pi runtime over the request's per-request auth.json. The SDK refreshes
 * an expired token in that file, and the host reads the refresh back. */
async function subscriptionRuntime(request) {
  const authPath = nonBlank(request.authPath);
  if (!authPath) throw new WorkerError("input", "Missing authPath for the subscription credential.");
  const agentDir = getAgentDir();
  await mkdir(agentDir, { recursive: true });
  return ModelRuntime.create({ authPath, modelsPath: join(agentDir, "models.json") });
}

async function cleanMedia(request) {
  const apiKey = nonBlank(process.env.AI_GATEWAY_API_KEY);
  if (!apiKey) throw new WorkerError("configuration", "AI_GATEWAY_API_KEY is not configured.");
  const apiBaseUrl = nonBlank(process.env.AI_GATEWAY_BASE_URL) ?? "https://ai-gateway.vercel.sh/v1";
  const modelId = nonBlank(request.model)
    ?? nonBlank(process.env.AI_GATEWAY_CLEANER_MODEL)
    ?? "poolside/laguna-s-2.1-free";
  const data = await aiGatewayChatCompletion(apiBaseUrl, apiKey, modelId, request.systemPrompt, request.prompt, fetch, {
    reasoning: request.reasoning && typeof request.reasoning === "object" ? request.reasoning : undefined,
    maxTokens: typeof request.maxTokens === "number" ? request.maxTokens : undefined,
    temperature: typeof request.temperature === "number" ? request.temperature : undefined,
  });
  outputJson({ cleanedText: parseAiGatewayCleaner(data), model: modelId });
}

/** Recipe generation against an OpenAI- or Anthropic-spec endpoint. The base
 * URL and key come from the host and are never logged. */
async function generateViaEndpoint(request, searchMode, reasoningEffort) {
  const apiBaseUrl = nonBlank(request.apiBaseUrl);
  const apiKey = nonBlank(request.apiKey);
  const model = nonBlank(request.model);
  if (!apiBaseUrl || !apiKey || !model) {
    throw new WorkerError("worker", "Missing apiBaseUrl, apiKey, or model for the API request.");
  }
  const systemPrompt = recipePrompt(request.systemPrompt, searchMode);
  const acceptUnverifiable = searchMode === "gapfill";
  if (request.provider === "anthropic") {
    const data = await anthropicMessages(
      apiBaseUrl,
      apiKey,
      model,
      systemPrompt,
      [{ role: "user", content: request.prompt }],
      reasoningEffort,
      searchMode,
    );
    if (data.stop_reason === "max_tokens") {
      throw new WorkerError("worker", "Anthropic response was truncated by max_tokens.");
    }
    const { text, searched, refused, citations } = parseAnthropicOutput(data);
    if (refused) throw new WorkerError("worker", "Anthropic refused the request.");
    return finishRecipe({ label: "Anthropic", text, citations, searched, searchMode, acceptUnverifiable });
  }
  const data = await openaiResponse(apiBaseUrl, apiKey, model, systemPrompt, request.prompt, searchMode, reasoningEffort);
  if (data.status && data.status !== "completed") {
    throw new WorkerError("worker", `OpenAI response incomplete: ${data.incomplete_details?.reason ?? "unknown reason"}`);
  }
  const { text, searched, refused, citations } = parseResponsesOutput(data);
  if (refused) throw new WorkerError("worker", "OpenAI refused the request.");
  return finishRecipe({ label: "OpenAI", text, citations, searched, searchMode, acceptUnverifiable });
}

/** Recipe generation through a pi subscription (ChatGPT for Codex, Claude Pro/Max). */
async function generateViaSubscription(request, searchMode, reasoningEffort) {
  const subscription = subscriptionOf(request);
  const modelRuntime = await subscriptionRuntime(request);
  if (!(await modelRuntime.checkAuth(subscription.id))) {
    throw new WorkerError("configuration", subscription.notLoggedIn);
  }
  const modelId = nonBlank(request.model) ?? subscription.defaultModel;
  const model = modelRuntime.getModel(subscription.id, modelId);
  if (!model) {
    throw new WorkerError("configuration", `Pi does not know the ${subscription.id}/${modelId} model.`);
  }

  const observer = searchMode === "off"
    ? null
    : createNativeSearchObserver(globalThis.fetch, { allowUnverifiedFallback: searchMode === "gapfill" });
  const response = await modelRuntime.completeSimple(model, completionContext(request, searchMode), {
    reasoning: reasoningEffort,
    maxRetries: 1,
    ...(observer ? { fetch: observer.fetch, ...subscription.searchOptions(searchMode) } : {}),
  });
  if (response.stopReason === "error" || response.stopReason === "aborted") {
    throw new WorkerError("worker", response.errorMessage || "Pi model request failed.");
  }
  const observed = observer ? await observer.finish() : { invoked: false, sources: new Map() };
  return finishRecipe({
    label: subscription.label,
    text: assistantText(response),
    citations: observed.sources,
    searched: observed.invoked,
    searchMode,
    acceptUnverifiable: false,
  });
}

async function main() {
  const request = parseRequest(await readStdin());
  if (request.command === "listModels") return listModels(request);
  if (request.command === "cleanMedia") return cleanMedia(request);
  const searchMode = searchModeOf(request);
  const reasoningEffort = normalizeEffort(request.reasoningEffort);
  const endpoint = request.provider === "openai" || request.provider === "anthropic";
  outputJson(endpoint
    ? await generateViaEndpoint(request, searchMode, reasoningEffort)
    : await generateViaSubscription(request, searchMode, reasoningEffort));
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  runMain(main, (error) => {
    const known = error instanceof WorkerError;
    return {
      error: error instanceof Error ? error.message : "Pi worker failed.",
      code: known ? error.code : "worker",
      // Output validation failures are worth one host-level retry; provider
      // transport errors carry their own status-aware retry decision.
      retryable: known && (error.retryable || error.code === "output"),
    };
  });
}
