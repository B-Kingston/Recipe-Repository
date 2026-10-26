import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import process from "node:process";
import {
  ModelRuntime,
  getAgentDir,
} from "@earendil-works/pi-coding-agent";
import { Type, createProvider } from "@earendil-works/pi-ai";
import type {
  Api,
  AssistantMessage,
  Context,
  ImageContent,
  Model,
  Models,
  MutableModels,
  SimpleStreamOptions,
  TextContent,
  Tool,
  ToolCall,
  ToolResultMessage,
} from "@earendil-works/pi-ai";
import { openAICompletionsApi } from "@earendil-works/pi-ai/api/openai-completions.lazy";
import { builtinModels } from "@earendil-works/pi-ai/providers/all";
import {
  actionSources,
  anthropicSearchPayload,
  createNativeSearchObserver,
  nativeSearchPayload,
  normalizeSourceUrl,
  optionalSearchPayload,
  webSearchCompleted,
} from "./native-search.ts";
import type { ResponsesItem, SourceLike } from "./native-search.ts";
import { outputJson, readStdin, runMain } from "./cli.ts";

/** A reasoning effort the host may ask for. */
export type ReasoningEffort = "low" | "medium" | "high" | "xhigh" | "max";

/** The search posture of a generation request; see searchModeOf. */
export type SearchMode = "off" | "grounded" | "gapfill";

/** WorkerError codes the host maps: "configuration" becomes AiNotConfigured,
 * "auth" (the provider refused the login) becomes AiLoginRefused and flags
 * the login in Settings, "output" is worth one retry, "input" and "worker"
 * are plain failures. */
export type WorkerErrorCode = "auth" | "configuration" | "input" | "output" | "worker";

/** The fields a stdin request may carry. The host sends each as a string or
 * leaves it out; values are still trimmed and checked before use. */
export interface RequestFields {
  /** "listModels", "checkLogin", "cleanMedia", "chat", or absent for recipe
   * generation. */
  command?: string;
  prompt?: string;
  systemPrompt?: string;
  model?: string;
  reasoningEffort?: string;
  searchMode?: string;
  /** The per-request auth.json holding a subscription's OAuth credential. */
  authPath?: string;
  /** The subscription: "openai-codex" (default) or "anthropic". */
  piProvider?: string;
  /** An endpoint spec ("openai", "anthropic", a pi-ai provider id, or
   * "openai-compatible"); present only for an API endpoint request. */
  provider?: string;
  apiBaseUrl?: string;
  apiKey?: string;
}

/** A request that carries a user prompt. */
export interface PromptRequest extends RequestFields {
  prompt: string;
}

/** A recipe generation, cleanMedia, or chat request. */
export interface GenerationRequest extends PromptRequest {
  systemPrompt: string;
}

/** A model discovery request, which needs no prompt. */
export interface ListModelsRequest extends RequestFields {
  command: "listModels";
}

/** A subscription login check, which needs no prompt. */
export interface CheckLoginRequest extends RequestFields {
  command: "checkLogin";
}

/** One parsed stdin request. */
export type WorkerRequest = ListModelsRequest | CheckLoginRequest | GenerationRequest;

/** Whether a parsed request is model discovery. */
function isListModelsRequest(request: WorkerRequest): request is ListModelsRequest {
  return request.command === "listModels";
}

/** Whether a parsed request is a login check. */
function isCheckLoginRequest(request: WorkerRequest): request is CheckLoginRequest {
  return request.command === "checkLogin";
}

/** A source the recipe cites. */
export interface RecipeSource {
  title: string;
  url: string;
}

/** The stdout line of a recipe generation. The recipe itself is validated by
 * the host against its schema. */
export interface GenerateResponse {
  recipe: object;
  sources: RecipeSource[];
}

/** The stdout line of listModels. */
export interface ListModelsResponse {
  models: string[];
}

/** The stdout line of cleanMedia. */
export interface CleanMediaResponse {
  cleanedText: string;
  model: string;
}

/** The model's chat answer: the reply to the cook's latest question, and a
 * recipe change when the cook asked for one. */
export interface ChatAnswer {
  reply: string;
  /** One instruction describing the whole change, or "" for a plain answer. */
  change: string;
  /** The model's one- or two-word name for the change, or "". The host checks
   * its length before showing it. */
  label: string;
}

/** The stdout line of chat: the answer, and the tools the model called. */
export interface ChatResponse extends ChatAnswer {
  toolCalls: string[];
}

/** A note the cook kept on the recipe, as the host sends it with a chat
 * request (newest first). */
export interface ChatNote {
  /** The saved version the note is about; null for the whole recipe. */
  version: number | null;
  createdAt: string;
  text: string;
  hasPhoto: boolean;
  /** The note's photo as a base64 JPEG, when the host sent it. */
  photo: string | null;
}

/** A saved version of the recipe, as the host sends it with a chat request. */
export interface ChatVersion {
  version: number;
  label: string;
  createdAt: string;
  recipe: unknown;
}

/** What the chat tools can read: the recipe's notes and saved versions. */
export interface ChatLibrary {
  notes: ChatNote[];
  versions: ChatVersion[];
  /** The version matching the current recipe; null when it has no history. */
  currentVersion: number | null;
}

/** The stdout line of checkLogin: the provider accepted a fresh token. */
export interface CheckLoginResponse {
  ok: true;
}

/** The stdout line of a failed request. */
export interface WorkerFailure {
  error: string;
  code: WorkerErrorCode;
  retryable: boolean;
}

/** One model call's reply text with the web-search citations it gathered. */
export interface Completion {
  label: string;
  text: string;
  /** Normalized URL to title. */
  citations: Map<string, string>;
  searched: boolean;
}

export interface CompletionOptions {
  systemPrompt: string;
  searchMode: SearchMode;
  reasoningEffort: ReasoningEffort;
}

export type Complete = (request: GenerationRequest, options: CompletionOptions) => Promise<Completion>;

interface Subscription {
  label: string;
  notLoggedIn: string;
  defaultModel: string;
  searchOptions: (searchMode: SearchMode) => Pick<SimpleStreamOptions, "transport" | "onPayload">;
}

type SubscriptionId = "openai-codex" | "anthropic";

/** Subscription providers the worker runs through pi's ModelRuntime, keyed by
 * pi provider id. The host stores each OAuth credential under the same key in
 * the per-request auth.json. Both go through completeSimple, so `reasoning` maps
 * to each model's own thinking control and a level the model lacks is clamped to
 * the nearest one it offers (Codex max runs as xhigh; Claude xhigh and max run
 * as high). */
const SUBSCRIPTIONS: Record<SubscriptionId, Subscription> = {
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
function nonBlank(value: unknown): string | null {
  return typeof value === "string" && value.trim() ? value.trim() : null;
}

/** Whether an id names a SUBSCRIPTIONS entry, ignoring inherited Object keys. */
function isSubscriptionId(id: string): id is SubscriptionId {
  return Object.hasOwn(SUBSCRIPTIONS, id);
}

/** The subscription entry for a request's piProvider; defaults to Codex. */
export function subscriptionOf(request: RequestFields): Subscription & { id: SubscriptionId } {
  const id = nonBlank(request.piProvider) ?? "openai-codex";
  if (!isSubscriptionId(id)) {
    throw new WorkerError("configuration", `Unsupported pi provider: ${id}`);
  }
  return { id, ...SUBSCRIPTIONS[id] };
}

const REASONING_EFFORTS: ReadonlySet<string> = new Set<ReasoningEffort>(["low", "medium", "high", "xhigh", "max"]);

function isReasoningEffort(value: string): value is ReasoningEffort {
  return REASONING_EFFORTS.has(value);
}

/** Coerces a request's reasoningEffort to one of the supported values.
 *  Missing or blank means the host default ("low"); anything else is
 *  rejected so a mistyped setting fails loudly instead of silently
 *  running at a different effort than the user chose. */
export function normalizeEffort(value: unknown): ReasoningEffort {
  if (value === undefined || value === null) return "low";
  const effort = String(value).trim().toLowerCase();
  if (effort === "") return "low";
  if (!isReasoningEffort(effort)) {
    throw new WorkerError("input", `Invalid reasoningEffort: ${value}`);
  }
  return effort;
}

export class WorkerError extends Error {
  readonly code: WorkerErrorCode;
  readonly retryable: boolean;

  constructor(code: WorkerErrorCode, message: string, options: { retryable?: boolean } = {}) {
    super(message);
    this.code = code;
    this.retryable = options.retryable === true;
  }
}

/** Provider replies that mean the login itself was refused: an OAuth refresh
 *  the provider rejected (pi-ai's ModelsError code "oauth"), or a 401 or an
 *  invalid-key error from the API. Reconnecting or replacing the key fixes
 *  these, so the host reports them as a login problem, never retries them,
 *  and marks the login in Settings. */
const AUTH_FAILURE_PATTERNS: readonly RegExp[] = [
  /OAuth refresh failed/i,
  /invalid_grant/i,
  /\bHTTP 401\b/,
  /\bstatus[=: ]+401\b/i,
  /authentication_error/i,
  /invalid[ _-]?(?:x-)?api[ _-]?key/i,
  /incorrect api key/i,
  /\b401\b[^\n]{0,40}unauthori[sz]ed/i,
  /unauthori[sz]ed[^\n]{0,40}\b401\b/i,
];

/** Whether an error means the provider refused the login (see AUTH_FAILURE_PATTERNS). */
export function isAuthFailure(error: unknown): boolean {
  if (error instanceof WorkerError && error.code === "auth") return true;
  if (error instanceof Error && error.name === "ModelsError" && (error as Error & { code?: unknown }).code === "oauth") {
    return true;
  }
  const message = error instanceof Error ? error.message : String(error ?? "");
  return AUTH_FAILURE_PATTERNS.some((pattern) => pattern.test(message));
}

/** The stdout failure line for an error: `code` "auth" when the login was
 *  refused, else the WorkerError code or "worker". */
export function workerFailure(error: unknown): WorkerFailure {
  const known = error instanceof WorkerError;
  const auth = isAuthFailure(error);
  return {
    error: error instanceof Error ? error.message : "Pi worker failed.",
    code: auth ? "auth" : known ? error.code : "worker",
    // Output validation failures are worth one host-level retry; provider
    // transport errors carry their own status-aware retry decision. A refused
    // login fails the same way every time.
    retryable: !auth && known && (error.retryable || error.code === "output"),
  };
}

/** Parses one stdin request. listModels and checkLogin need no prompt; every
 *  other command does. */
export function parseRequest(input: string): WorkerRequest {
  let request: unknown;
  try {
    request = JSON.parse(input);
  } catch (error) {
    throw new WorkerError("input", `Invalid worker request: ${(error as SyntaxError).message}`);
  }
  if (!request || typeof request !== "object" || Array.isArray(request)) {
    throw new WorkerError("input", "Invalid worker request: invalid request");
  }
  const fields = request as RequestFields;
  if (fields.command !== "listModels" && fields.command !== "checkLogin"
    && (typeof fields.prompt !== "string" || typeof fields.systemPrompt !== "string")) {
    throw new WorkerError("input", "Invalid worker request: missing prompt or systemPrompt");
  }
  return fields as WorkerRequest;
}

/** Endpoint specs the worker calls with its own Responses/Messages code, which
 *  supports native web search. Every other spec runs through pi-ai. */
const NATIVE_SPECS: ReadonlySet<string | null | undefined> = new Set(["openai", "anthropic"]);
/** A self-hosted or third-party server speaking OpenAI Chat Completions
 *  (Ollama, llama.cpp, vLLM, LM Studio, LiteLLM, …). */
const OPENAI_COMPATIBLE_SPEC = "openai-compatible";

/** Whether a request targets a registered API endpoint rather than a subscription. */
function isEndpointRequest(request: RequestFields): boolean {
  return nonBlank(request.provider) !== null;
}

/** Whether an endpoint spec runs through pi-ai rather than the native paths. */
export function isPiAiSpec(spec: string | null | undefined): boolean {
  return !NATIVE_SPECS.has(spec);
}

/** A pi-ai model collection holding every built-in provider, plus a keyless-
 *  capable "openai-compatible" provider for custom Chat Completions servers. */
export function piAiModels(): MutableModels {
  const models = builtinModels();
  models.setProvider(createProvider({
    id: OPENAI_COMPATIBLE_SPEC,
    name: "OpenAI-compatible",
    auth: { apiKey: { name: "OpenAI-compatible", resolve: async () => ({ auth: {} }) } },
    models: [],
    api: openAICompletionsApi(),
  }));
  return models;
}

/** A provider's chat models in pi-ai's catalogue; none for a missing spec. */
function chatModels(models: Models, spec: string | null): Model<Api>[] {
  if (spec === null) return [];
  return models.getModels(spec).filter((model) => !model.type || model.type === "chat");
}

/** The pi-ai chat model an endpoint runs. A built-in provider's catalogue
 *  entry carries its real limits and thinking controls; a model id the
 *  catalogue does not know yet borrows the shape of one of the provider's
 *  models. A custom server gets a conservative Chat Completions model, since
 *  many such servers reject the developer role, reasoning_effort, and store. */
export function piAiModel(models: Models, spec: string | null, modelId: string, apiBaseUrl?: string | null): Model<Api> {
  if (spec === OPENAI_COMPATIBLE_SPEC) {
    if (!apiBaseUrl) throw new WorkerError("configuration", "Missing base URL for the OpenAI-compatible server.");
    return {
      id: modelId,
      name: modelId,
      api: "openai-completions",
      provider: OPENAI_COMPATIBLE_SPEC,
      baseUrl: apiBaseUrl.replace(/\/+$/, ""),
      reasoning: false,
      input: ["text"],
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
      contextWindow: 128_000,
      maxTokens: 16_384,
      compat: { supportsDeveloperRole: false, supportsReasoningEffort: false, supportsStore: false },
    };
  }
  const catalogue = chatModels(models, spec);
  if (catalogue.length === 0) {
    throw new WorkerError("configuration", `Unsupported AI provider: ${spec}`);
  }
  const known = catalogue.find((model) => model.id === modelId);
  const shape = known
    ?? catalogue.find((model) => model.api === "openai-completions")
    ?? catalogue[0];
  const model = known ? { ...shape } : { ...shape, id: modelId, name: modelId };
  if (apiBaseUrl) model.baseUrl = apiBaseUrl.replace(/\/+$/, "");
  return model;
}

/** A catalogue or models-list entry; only its id is read. */
interface ModelEntry {
  id?: unknown;
  created?: unknown;
}

/**
 * Reduce a provider's model catalogue to sorted, de-duplicated model ids,
 * newest first so the current generation (e.g. the 5.6 range) sits on top.
 */
export function catalogModelIds(models: readonly (ModelEntry | null | undefined)[]): string[] {
  const seen = new Set<string>();
  const ids: string[] = [];
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
function extractJson(text: string): unknown {
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

/** The model's recipe reply: a recipe object and the sources it claims. */
interface RecipeReply {
  recipe: object;
  sources?: unknown;
}

function recipeObject(text: string): RecipeReply {
  const result = extractJson(text) as { recipe?: unknown; sources?: unknown } | null;
  if (!result || typeof result !== "object" || !result.recipe || typeof result.recipe !== "object") {
    throw new WorkerError("output", "Pi did not return a recipe object.");
  }
  return result as RecipeReply;
}

function assistantText(message: AssistantMessage | null | undefined): string {
  return (message?.content ?? [])
    .filter((part): part is TextContent => part.type === "text")
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
export function verifiedSources(claimed: unknown, citations: Map<string, string>, acceptUnverifiable: boolean): RecipeSource[] {
  const seen = new Set<string>();
  const claims: RecipeSource[] = [];
  for (const source of Array.isArray(claimed) ? (claimed as (SourceLike | null)[]) : []) {
    const url = normalizeSourceUrl(source?.url);
    if (!url || seen.has(url)) continue;
    seen.add(url);
    claims.push({ url, title: typeof source?.title === "string" ? source.title.trim() : "" });
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
export function searchModeOf(request: RequestFields | null | undefined): SearchMode {
  const mode = typeof request?.searchMode === "string" ? request.searchMode.trim() : "";
  return mode === "grounded" || mode === "gapfill" || mode === "off" ? mode : "off";
}

export function recipePrompt(systemPrompt: string, searchMode: SearchMode = "off"): string {
  const output = `Return only one JSON object with this shape:\n{
  "recipe": { ...the supplied recipe schema... },
  "sources": [{ "title": "source title", "url": "https://source.example" }]
}\nDo not use Markdown fences or add prose before or after the JSON.`;
  if (searchMode === "gapfill") {
    return `${systemPrompt}\n\nThe native web_search tool is available but not required for this request. The material you are given comes from an imported social video and can be incomplete, garbled, or misleading; use web_search when it is needed to fill gaps or resolve contradictions, then list every source that materially informed the recipe.\n\n${output}`;
  }
  if (searchMode === "grounded") {
    return `${systemPrompt}\n\nThe native web_search tool is enabled for this request. Research before preparing the recipe. Use only URLs cited by the native search response in sources; list every source that materially informed the recipe.\n\n${output}`;
  }
  return `${systemPrompt}\n\n${output}\nUse an empty sources array.`;
}

export function completionContext(request: GenerationRequest, searchMode: SearchMode = "off"): Context {
  return plainContext(recipePrompt(request.systemPrompt, searchMode), request.prompt);
}

/** One system prompt and one user message. Use the low-level completion API
 *  with an explicit context. Do not use AgentSession/createAgentSession here:
 *  those APIs assemble pi's default coding-agent system prompt. */
function plainContext(systemPrompt: string, prompt: string): Context {
  return {
    systemPrompt,
    messages: [{ role: "user", content: prompt, timestamp: Date.now() }],
    tools: [],
  };
}

const OPENAI_RESPONSE_TIMEOUT_MS = 600_000; // generation can take minutes

/** POST JSON and return the parsed body. Throws WorkerError on transport
 *  failure or non-2xx (error body truncated to 300 chars; never includes the
 *  key).  Shared by the Responses API path, the Messages API path and model
 *  discovery. The body type is the one the API documents; callers still check
 *  each field they read. */
async function fetchJson<T>(
  url: string,
  headers: Record<string, string>,
  body: unknown,
  fetchImpl: typeof fetch,
  timeoutMs = OPENAI_RESPONSE_TIMEOUT_MS,
): Promise<T> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    // No body means a GET, as used by model discovery.
    const response = await fetchImpl(url, body === undefined
      ? { method: "GET", headers, signal: controller.signal }
      : {
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
    return await response.json() as T;
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

function cleanerScalar(value: unknown, max = 800): string {
  if (typeof value !== "string") return "";
  return value.replaceAll("\u0000", " ").trim().slice(0, max);
}

function cleanerList(value: unknown): string[] {
  const values: unknown[] = Array.isArray(value) ? value : typeof value === "string" ? [value] : [];
  return values
    .flatMap((item): unknown[] => {
      if (typeof item === "string") return [item];
      if (!item || typeof item !== "object") return [];
      const fields = item as Record<string, unknown>;
      const direct = fields.text ?? fields.step ?? fields.instruction ?? fields.fact;
      if (direct) return [direct];
      const name = fields.name ?? fields.ingredient ?? "";
      const amount = fields.amount ?? fields.quantity ?? "";
      const unit = fields.unit ?? "";
      return [
        [amount, unit, name].filter((part) => typeof part === "string" && part.trim()).join(" "),
      ];
    })
    .map((item) => cleanerScalar(item))
    .filter(Boolean)
    .slice(0, 80);
}

/** The cleaner's constrained JSON. Every field is checked before use. */
interface RecipeEvidence {
  title?: unknown;
  servings?: unknown;
  ingredients?: unknown;
  steps?: unknown;
  timings?: unknown;
  relevant_notes?: unknown;
  relevantNotes?: unknown;
}

/** Converts the model's constrained JSON into a small, known-field-only text
 * block. Unknown response fields and prose are discarded rather than passed
 * through to the final recipe model. */
export function formatRecipeEvidence(value: unknown): string {
  const wrapped = (value as { recipeEvidence?: unknown } | null | undefined)?.recipeEvidence;
  const root = wrapped && typeof wrapped === "object"
    ? wrapped
    : value;
  if (!root || typeof root !== "object" || Array.isArray(root)) {
    throw new WorkerError("output", "The video cleaner returned an invalid object.");
  }
  const evidence = root as RecipeEvidence;
  const title = cleanerScalar(evidence.title);
  const servings = cleanerScalar(evidence.servings);
  const ingredients = cleanerList(evidence.ingredients);
  const steps = cleanerList(evidence.steps);
  const timings = cleanerList(evidence.timings);
  const notes = cleanerList(evidence.relevant_notes ?? evidence.relevantNotes);
  if (ingredients.length === 0 && steps.length === 0) {
    throw new WorkerError("output", "The video cleaner found no recipe facts in the video evidence.");
  }
  const lines: string[] = [];
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
    throw new WorkerError("output", "The video cleaner returned too much text.");
  }
  return text;
}

/** The Responses API body fields the worker reads. */
export interface ResponsesBody {
  status?: unknown;
  incomplete_details?: { reason?: unknown } | null;
  output?: (ResponsesItem | null)[];
  [key: string]: unknown;
}

/** POST {base}/responses with bearer auth; returns the parsed JSON body. */
export async function openaiResponse(
  apiBaseUrl: string,
  apiKey: string,
  modelId: string,
  systemPrompt: string,
  prompt: string,
  searchMode: SearchMode,
  reasoningEffort: ReasoningEffort = "low",
  fetchImpl: typeof fetch = fetch,
): Promise<ResponsesBody> {
  return fetchJson<ResponsesBody>(
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

/** What a provider reply reduces to before the recipe is extracted. */
export interface ParsedOutput {
  text: string;
  searched: boolean;
  refused: boolean;
  /** Normalized URL to title. */
  citations: Map<string, string>;
}

/** Reduces a Responses API body to what the worker needs.
 *  citations is a Map<url, title> built from url_citation annotations. */
export function parseResponsesOutput(data: ResponsesBody | null | undefined): ParsedOutput {
  const output = Array.isArray(data?.output) ? data.output : [];
  let searched = false;
  let refused = false;
  const citations = new Map<string, string>();
  const texts: unknown[] = [];
  for (const item of output) {
    if (item?.type === "web_search_call") {
      if (webSearchCompleted(item)) searched = true;
      for (const source of actionSources(item)) {
        const url = normalizeSourceUrl(source?.url);
        if (url) citations.set(url, typeof source?.title === "string" ? source.title : "");
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
            citations.set(url, typeof citation?.title === "string" ? citation.title : "");
          }
        }
      }
    }
  }
  return { text: texts.join(""), searched, refused, citations };
}

const ANTHROPIC_VERSION = "2023-06-01";
/** The basic server-side web search tool. The dynamic-filtering versions
 *  (web_search_20260209 and later) route results through code execution, which
 *  Google Cloud, Azure-hosted Foundry and most proxies do not offer, and which
 *  keeps results out of the web_search_tool_result blocks the sources are
 *  verified against. */
const ANTHROPIC_WEB_SEARCH_TOOL = { type: "web_search_20250305", name: "web_search" };
/** A long server-side search turn stops with stop_reason "pause_turn"; the
 *  paused content is sent back unchanged so the model can finish. */
const ANTHROPIC_MAX_CONTINUATIONS = 5;
/** Extended-thinking budgets for models before the 4.6 generation, which have
 *  no adaptive thinking. They have no xhigh/max tiers, so those settings are
 *  capped at the highest budget rather than silently falling back to low. */
const ANTHROPIC_THINKING_BUDGETS: Record<ReasoningEffort, number> = { low: 1024, medium: 4096, high: 8192, xhigh: 8192, max: 8192 };
/** Output headroom added to the thinking budget for max_tokens. */
const ANTHROPIC_OUTPUT_HEADROOM = 8192;
/** max_tokens for adaptive-thinking models: a hard cap on thinking plus answer. */
const ANTHROPIC_ADAPTIVE_MAX_TOKENS = 32_000;
/** A 5-minute prompt-cache breakpoint. */
const ANTHROPIC_CACHE_CONTROL = { type: "ephemeral" };
const MODEL_LIST_TIMEOUT_MS = 30_000;
const MODEL_LIST_MAX_PAGES = 20;

/** A Messages API content block; a pause_turn continuation sends them back unchanged. */
export interface AnthropicBlock {
  type?: unknown;
  text?: unknown;
  citations?: (SourceLike | null)[] | null;
  content?: unknown;
  [key: string]: unknown;
}

/** One Messages API conversation turn. */
export interface AnthropicMessage {
  role: "user" | "assistant";
  content: string | AnthropicBlock[];
}

interface AnthropicUsage {
  input_tokens?: number;
  cache_creation_input_tokens?: number;
  cache_read_input_tokens?: number;
  output_tokens?: number;
}

/** The Messages API body fields the worker reads. */
export interface AnthropicBody {
  content?: AnthropicBlock[];
  stop_reason?: unknown;
  usage?: AnthropicUsage | null;
  [key: string]: unknown;
}

/** The thinking fields of a Messages API request. */
export interface AnthropicThinking {
  max_tokens: number;
  thinking: { type: "adaptive" } | { type: "enabled"; budget_tokens: number };
  output_config?: { effort: ReasoningEffort };
}

/** The Claude generation of a model id as { major, minor }, or null when the
 *  id is not a recognisable Claude id (for example a proxy alias). Reads both
 *  "claude-sonnet-5-5" / "claude-opus-4-5-20251101" and the older
 *  "claude-3-7-sonnet-20250219" order, with or without a platform prefix. */
export function claudeGeneration(modelId: unknown): { major: number; minor: number } | null {
  const id = String(modelId ?? "").toLowerCase();
  const match = id.match(/claude-[a-z]+-(\d+)(?:-(\d)(?=$|[-@.:]))?/) ?? id.match(/claude-(\d+)(?:-(\d))?-[a-z]/);
  return match ? { major: Number(match[1]), minor: Number(match[2] ?? 0) } : null;
}

/** The thinking fields of a Messages API request. Claude 4.6 and later use
 *  adaptive thinking steered by output_config.effort; manual budget_tokens is
 *  rejected from Opus 4.7 on. Earlier models keep effort-scaled budgets. An
 *  unrecognised id is treated as current. The 4.6 models have no xhigh effort,
 *  so xhigh runs as max there. */
export function anthropicThinking(modelId: unknown, reasoningEffort: unknown): AnthropicThinking {
  const effort = normalizeEffort(reasoningEffort);
  const generation = claudeGeneration(modelId);
  const adaptive = !generation || generation.major > 4 || (generation.major === 4 && generation.minor >= 6);
  if (adaptive) {
    const noXhigh = generation?.major === 4 && generation.minor === 6;
    return {
      max_tokens: ANTHROPIC_ADAPTIVE_MAX_TOKENS,
      thinking: { type: "adaptive" },
      output_config: { effort: effort === "xhigh" && noXhigh ? "max" : effort },
    };
  }
  const budget = ANTHROPIC_THINKING_BUDGETS[effort];
  return {
    max_tokens: budget + ANTHROPIC_OUTPUT_HEADROOM,
    thinking: { type: "enabled", budget_tokens: budget },
  };
}

/** A Messages API URL for a stored base URL: append /v1/<path>, unless the
 *  base already ends in /v1, then append /<path>. */
export function anthropicUrl(baseUrl: string, path = "messages"): string {
  const base = baseUrl.replace(/\/+$/, "");
  return /\/v1$/i.test(base) ? `${base}/${path}` : `${base}/v1/${path}`;
}

function anthropicHeaders(apiKey: string): Record<string, string> {
  return { "x-api-key": apiKey, "anthropic-version": ANTHROPIC_VERSION };
}

/** POST {base}/v1/messages with x-api-key auth and returns the parsed body,
 *  with the same error mapping as openaiResponse. The system prompt carries a
 *  prompt-cache breakpoint: it is identical for every recipe, so tools and
 *  system are read from the cache on later calls, including the host's retry
 *  and pause_turn continuations. A block-level breakpoint is used rather than
 *  the top-level automatic one because some Anthropic-spec hosts reject a
 *  top-level cache_control. */
export async function anthropicMessages(
  apiBaseUrl: string,
  apiKey: string,
  modelId: string,
  systemPrompt: string,
  messages: AnthropicMessage[],
  reasoningEffort: ReasoningEffort = "low",
  searchMode: SearchMode = "off",
  fetchImpl: typeof fetch = fetch,
): Promise<AnthropicBody> {
  const body = {
    model: modelId,
    ...anthropicThinking(modelId, reasoningEffort),
    system: [{ type: "text", text: systemPrompt, cache_control: ANTHROPIC_CACHE_CONTROL }],
    messages,
    ...(searchMode !== "off" ? { tools: [ANTHROPIC_WEB_SEARCH_TOOL] } : {}),
  };
  return fetchJson<AnthropicBody>(anthropicUrl(apiBaseUrl), anthropicHeaders(apiKey), body, fetchImpl);
}

/** Runs one Messages API turn to completion. A "pause_turn" stop is continued
 *  by sending the paused assistant content back unchanged; the content of all
 *  calls is returned in order, with the final stop_reason. Cache usage goes to
 *  stderr, which the host logs. */
export async function anthropicTurn(
  apiBaseUrl: string,
  apiKey: string,
  modelId: string,
  systemPrompt: string,
  prompt: string,
  reasoningEffort: ReasoningEffort = "low",
  searchMode: SearchMode = "off",
  fetchImpl: typeof fetch = fetch,
): Promise<{ content: AnthropicBlock[]; stop_reason: unknown }> {
  const assistant: AnthropicMessage & { content: AnthropicBlock[] } = { role: "assistant", content: [] };
  const messages: AnthropicMessage[] = [{ role: "user", content: prompt }];
  for (let call = 0; ; call += 1) {
    const data = await anthropicMessages(apiBaseUrl, apiKey, modelId, systemPrompt, messages, reasoningEffort, searchMode, fetchImpl);
    logAnthropicUsage(modelId, call, data?.usage);
    const content = Array.isArray(data?.content) ? data.content : [];
    assistant.content.push(...content);
    if (data?.stop_reason !== "pause_turn") {
      return { content: assistant.content, stop_reason: data?.stop_reason };
    }
    if (call === ANTHROPIC_MAX_CONTINUATIONS) {
      throw new WorkerError("worker", "Anthropic web search did not finish its turn.", { retryable: true });
    }
    if (messages.length === 1) messages.push(assistant);
  }
}

function logAnthropicUsage(model: string, call: number, usage: AnthropicUsage | null | undefined): void {
  if (!usage || typeof usage !== "object") return;
  console.error(JSON.stringify({
    event: "anthropicUsage",
    model,
    call,
    inputTokens: usage.input_tokens ?? 0,
    cacheCreationInputTokens: usage.cache_creation_input_tokens ?? 0,
    cacheReadInputTokens: usage.cache_read_input_tokens ?? 0,
    outputTokens: usage.output_tokens ?? 0,
  }));
}

/** Reduces a Messages API body to what the worker needs. Web search results
 *  arrive as web_search_result items inside a web_search_tool_result block; the
 *  answer's own citations are web_search_result_location entries on its text
 *  blocks. Both are the URLs the claimed sources are verified against. */
export function parseAnthropicOutput(data: AnthropicBody | null | undefined): ParsedOutput {
  const content = Array.isArray(data?.content) ? data.content : [];
  let searched = false;
  const citations = new Map<string, string>();
  const cite = (item: SourceLike) => {
    const url = normalizeSourceUrl(item.url);
    if (url) citations.set(url, typeof item.title === "string" ? item.title : "");
  };
  const texts: string[] = [];
  for (const block of content) {
    if (block?.type === "text") {
      if (typeof block.text === "string") texts.push(block.text);
      for (const citation of block.citations ?? []) {
        if (citation?.type === "web_search_result_location") cite(citation);
      }
    } else if (block?.type === "web_search_tool_result") {
      searched = true;
      for (const result of Array.isArray(block.content) ? (block.content as (SourceLike | null)[]) : []) {
        if (result?.type === "web_search_result") cite(result);
      }
    }
  }
  return { text: texts.join(""), searched, refused: data?.stop_reason === "refusal", citations };
}

/** A models list body: OpenAI-spec `data`, plus Anthropic's pagination fields. */
interface ModelsListBody {
  data?: (ModelEntry | null)[];
  has_more?: unknown;
  last_id?: unknown;
}

/** Model ids an API endpoint serves, from its models list. Anthropic-spec
 *  lists are paginated and already newest first; OpenAI-spec lists are sorted
 *  newest first by their created timestamp. */
export async function listEndpointModels(request: RequestFields, fetchImpl: typeof fetch = fetch): Promise<string[]> {
  const spec = nonBlank(request.provider);
  const apiBaseUrl = nonBlank(request.apiBaseUrl);
  const apiKey = nonBlank(request.apiKey);
  // A built-in pi-ai provider lists its own catalogue, which needs no network.
  if (isPiAiSpec(spec) && spec !== OPENAI_COMPATIBLE_SPEC) {
    return catalogModelIds(chatModels(piAiModels(), spec));
  }
  // A custom server may be keyless (a local Ollama or llama.cpp).
  if (!apiBaseUrl || (!apiKey && spec !== OPENAI_COMPATIBLE_SPEC)) {
    throw new WorkerError("input", "Missing apiBaseUrl or apiKey for model discovery.");
  }
  // The key check above already holds for every spec but a custom server.
  if (request.provider === "anthropic" && apiKey) {
    const models: (ModelEntry | null)[] = [];
    let afterId: string | undefined;
    for (let page = 0; page < MODEL_LIST_MAX_PAGES; page += 1) {
      const url = new URL(anthropicUrl(apiBaseUrl, "models"));
      url.searchParams.set("limit", "1000");
      if (afterId) url.searchParams.set("after_id", afterId);
      const body = await fetchJson<ModelsListBody | null>(url.toString(), anthropicHeaders(apiKey), undefined, fetchImpl, MODEL_LIST_TIMEOUT_MS);
      if (Array.isArray(body?.data)) models.push(...body.data);
      afterId = body?.has_more === true && typeof body.last_id === "string" ? body.last_id : undefined;
      if (!afterId) break;
    }
    return uniqueModelIds(models);
  }
  const body = await fetchJson<ModelsListBody | null>(
    `${apiBaseUrl.replace(/\/+$/, "")}/models`,
    apiKey ? { Authorization: `Bearer ${apiKey}` } : {},
    undefined,
    fetchImpl,
    MODEL_LIST_TIMEOUT_MS,
  );
  const models = Array.isArray(body?.data) ? [...body.data] : [];
  const created = (model: ModelEntry | null) => (typeof model?.created === "number" ? model.created : 0);
  models.sort((a, b) => created(b) - created(a) || String(a?.id).localeCompare(String(b?.id)));
  return uniqueModelIds(models);
}

function uniqueModelIds(models: readonly (ModelEntry | null)[]): string[] {
  return [...new Set(models.map((model) => model?.id).filter((id): id is string => typeof id === "string" && id.trim() !== ""))];
}

/** Checks a generated recipe against the search posture and returns it with
 * its verified sources. `citations` maps URL to title from the provider. */
function finishRecipe({ label, text, citations, searched, searchMode, acceptUnverifiable }: Completion & {
  searchMode: SearchMode;
  acceptUnverifiable: boolean;
}): GenerateResponse {
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
 * Returns the current model ids, newest first. An API endpoint request
 * (provider "openai" or "anthropic") reads the endpoint's own models list.
 * A subscription request refreshes the provider's catalogue from pi.dev (the
 * same source `pi update --models` uses). That refresh is credential-gated:
 * the host materialises the database-backed OAuth credential at `authPath`;
 * an expired token is refreshed in place in that file, so the host reads it
 * back to persist the refresh.
 */
async function listModels(request: RequestFields): Promise<void> {
  if (isEndpointRequest(request)) {
    outputJson({ models: await listEndpointModels(request) } satisfies ListModelsResponse);
    return;
  }
  const subscription = subscriptionOf(request);
  const modelRuntime = await subscriptionRuntime(request);
  await modelRuntime.refresh({ allowNetwork: true, force: true, signal: AbortSignal.timeout(20_000) });
  outputJson({ models: catalogModelIds(modelRuntime.getModels(subscription.id)) } satisfies ListModelsResponse);
}

/**
 * Refreshes a subscription's OAuth token now, whatever its expiry, so
 * Settings can prove the login still works. The token's expiry is cleared in
 * the per-request auth.json, which makes pi refresh it; the host reads the
 * new token back and stores it. A refused refresh throws pi's "oauth" error,
 * which the host reports as an expired login.
 */
async function checkLogin(request: CheckLoginRequest): Promise<void> {
  const subscription = subscriptionOf(request);
  const authPath = nonBlank(request.authPath);
  if (!authPath) throw new WorkerError("input", "Missing authPath for the subscription credential.");
  const auth = JSON.parse(await readFile(authPath, "utf8")) as Record<string, { type?: string; expires?: number } | undefined>;
  const credential = auth[subscription.id];
  if (credential?.type !== "oauth") {
    throw new WorkerError("configuration", subscription.notLoggedIn);
  }
  credential.expires = 0;
  await writeFile(authPath, JSON.stringify(auth), { mode: 0o600 });
  const modelRuntime = await subscriptionRuntime(request);
  if (!(await modelRuntime.getAuth(subscription.id))) {
    throw new WorkerError("configuration", subscription.notLoggedIn);
  }
  outputJson({ ok: true } satisfies CheckLoginResponse);
}

/** The pi runtime over the request's per-request auth.json. The SDK refreshes
 * an expired token in that file, and the host reads the refresh back. */
async function subscriptionRuntime(request: RequestFields): Promise<ModelRuntime> {
  const authPath = nonBlank(request.authPath);
  if (!authPath) throw new WorkerError("input", "Missing authPath for the subscription credential.");
  const agentDir = getAgentDir();
  await mkdir(agentDir, { recursive: true });
  return ModelRuntime.create({ authPath, modelsPath: join(agentDir, "models.json") });
}

/** Reduces imported video evidence to recipe-only facts with whichever
 * backend the host routed the request to (a subscription or an endpoint),
 * without web search. Only the known evidence fields come back. */
async function cleanMedia(request: GenerationRequest): Promise<void> {
  const completion = await complete(request, {
    systemPrompt: request.systemPrompt,
    searchMode: "off",
    reasoningEffort: normalizeEffort(request.reasoningEffort),
  });
  outputJson({ cleanedText: cleanedEvidence(completion.text), model: request.model ?? "" } satisfies CleanMediaResponse);
}

/** The cleaner's reply reduced to the known evidence fields. */
export function cleanedEvidence(text: string): string {
  return formatRecipeEvidence(extractJson(text));
}

/** Answers one recipe-chat turn with whichever backend the host routed the
 * request to, without web search. The host's system prompt asks for a JSON
 * object; see chatReply. */
/** Answers one recipe-chat turn through whichever backend the host routed
 * the request to, without web search. The model may call the chat tools to
 * read the recipe's notes and versions, which the host sent along; the final
 * reply is the JSON object chatReply reads. */
async function chat(request: GenerationRequest & { library?: unknown }): Promise<void> {
  const { call, canSeePhotos } = await chatModelCall(request);
  const { text, toolCalls } = await chatWithTools(
    call,
    request.systemPrompt,
    request.prompt,
    chatLibrary(request.library),
    canSeePhotos,
  );
  outputJson({ ...chatReply(text), toolCalls } satisfies ChatResponse);
}

/** One model call with the chat's running context. */
export type ChatModelCall = (context: Context) => Promise<AssistantMessage>;

/** The chat's model call through the request's backend. Every backend goes
 * through pi-ai here, the native OpenAI and Anthropic endpoints included,
 * since the chat needs tool calls rather than native web search. */
async function chatModelCall(request: PromptRequest): Promise<{ call: ChatModelCall; canSeePhotos: boolean }> {
  const reasoning = normalizeEffort(request.reasoningEffort);
  if (!isEndpointRequest(request)) {
    const subscription = subscriptionOf(request);
    const modelRuntime = await subscriptionRuntime(request);
    if (!(await modelRuntime.checkAuth(subscription.id))) {
      throw new WorkerError("configuration", subscription.notLoggedIn);
    }
    const modelId = nonBlank(request.model) ?? subscription.defaultModel;
    const model = modelRuntime.getModel(subscription.id, modelId);
    if (!model) throw new WorkerError("configuration", `Pi does not know the ${subscription.id}/${modelId} model.`);
    return {
      call: (context) => modelRuntime.completeSimple(model, context, { reasoning, maxRetries: 1 }),
      canSeePhotos: model.input.includes("image"),
    };
  }
  const spec = nonBlank(request.provider);
  const apiKey = nonBlank(request.apiKey);
  const modelId = nonBlank(request.model);
  if (!modelId) throw new WorkerError("configuration", "Choose a model for this AI provider in Settings.");
  if (!apiKey && spec !== OPENAI_COMPATIBLE_SPEC) {
    throw new WorkerError("configuration", "Missing API key for this AI provider.");
  }
  const models = piAiModels();
  const model = piAiModel(models, spec, modelId, nonBlank(request.apiBaseUrl));
  return {
    call: (context) => models.completeSimple(model, context, {
      apiKey: apiKey ?? "none",
      reasoning,
      maxRetries: 1,
      signal: AbortSignal.timeout(OPENAI_RESPONSE_TIMEOUT_MS),
    }),
    canSeePhotos: model.input.includes("image"),
  };
}

/** Model turns that may call tools before the chat must answer. */
const CHAT_TOOL_ROUNDS = 4;

/** The chat's read-only tools. They answer from the request's library, so a
 * lookup never leaves the worker. */
export const CHAT_TOOLS: Tool[] = [
  {
    name: "recipe_notes",
    description: "List the notes the cook has kept on this recipe, newest first: when each was written, whether it is about the whole recipe or one saved version, its text, and whether it has a photo.",
    parameters: Type.Object({}),
  },
  {
    name: "note_photo",
    description: "Look at the photo attached to one of the cook's notes.",
    parameters: Type.Object({
      note: Type.Integer({ minimum: 1, description: "The note's number from recipe_notes." }),
    }),
  },
  {
    name: "recipe_version",
    description: "Read one saved version of the recipe as JSON, to see exactly what the cook made when a note is about that version.",
    parameters: Type.Object({
      version: Type.Integer({ minimum: 1, description: "The version number." }),
    }),
  },
];

function finiteInteger(value: unknown): number | null {
  return typeof value === "number" && Number.isInteger(value) ? value : null;
}

/** The library the host sent, with anything malformed left out. */
export function chatLibrary(value: unknown): ChatLibrary {
  const fields = value && typeof value === "object" ? value as Record<string, unknown> : {};
  const notes = (Array.isArray(fields.notes) ? fields.notes : []).flatMap((note): ChatNote[] => {
    if (!note || typeof note !== "object") return [];
    const raw = note as Record<string, unknown>;
    const photo = typeof raw.photo === "string" && raw.photo ? raw.photo : null;
    return [{
      version: finiteInteger(raw.version),
      createdAt: trimmedString(raw.createdAt),
      text: typeof raw.text === "string" ? raw.text : "",
      hasPhoto: raw.hasPhoto === true || photo !== null,
      photo,
    }];
  });
  const versions = (Array.isArray(fields.versions) ? fields.versions : []).flatMap((entry): ChatVersion[] => {
    if (!entry || typeof entry !== "object") return [];
    const raw = entry as Record<string, unknown>;
    const version = finiteInteger(raw.version);
    if (version === null) return [];
    return [{ version, label: trimmedString(raw.label), createdAt: trimmedString(raw.createdAt), recipe: raw.recipe ?? null }];
  });
  return { notes, versions, currentVersion: finiteInteger(fields.currentVersion) };
}

/** What a note is about, in words. */
function noteSubject(library: ChatLibrary, version: number | null): string {
  if (version === null) return "about the whole recipe";
  const saved = library.versions.find((entry) => entry.version === version);
  const label = saved?.label ? ` (${saved.label})` : "";
  return version === library.currentVersion
    ? `about version ${version}${label}, which is the current recipe`
    : `about version ${version}${label}`;
}

/** The recipe_notes listing. */
export function formatChatNotes(library: ChatLibrary): string {
  if (library.notes.length === 0) return "The cook has not kept any notes on this recipe.";
  return library.notes.map((note, index) => {
    const header = [
      `Note ${index + 1}`,
      note.createdAt ? `written ${note.createdAt}` : "",
      noteSubject(library, note.version),
      note.hasPhoto ? "has a photo (note_photo)" : "",
    ].filter(Boolean).join(" · ");
    return note.text.trim() ? `${header}\n${note.text.trim()}` : `${header}\n(photo only)`;
  }).join("\n\n");
}

/** Runs one chat tool call against the library. */
export function runChatTool(library: ChatLibrary, call: ToolCall, canSeePhotos: boolean): ToolResultMessage {
  const result = (text: string, isError = false, image?: ImageContent): ToolResultMessage => ({
    role: "toolResult",
    toolCallId: call.id,
    toolName: call.name,
    content: image ? [{ type: "text", text }, image] : [{ type: "text", text }],
    isError,
    timestamp: Date.now(),
  });
  if (call.name === "recipe_notes") return result(formatChatNotes(library));
  if (call.name === "note_photo") {
    const number = finiteInteger(call.arguments.note);
    const note = number === null ? undefined : library.notes[number - 1];
    if (!note) return result(`There is no note ${String(call.arguments.note)}. Call recipe_notes for the list.`, true);
    if (!note.hasPhoto) return result(`Note ${number} has no photo.`, true);
    if (!canSeePhotos) return result("This model cannot view photos; work from the note's text.", true);
    if (!note.photo) return result(`The photo on note ${number} was too many to send; work from the note's text.`, true);
    return result(`The photo on note ${number}:`, false, { type: "image", data: note.photo, mimeType: "image/jpeg" });
  }
  if (call.name === "recipe_version") {
    const version = finiteInteger(call.arguments.version);
    const saved = library.versions.find((entry) => entry.version === version);
    if (!saved) {
      const known = library.versions.map((entry) => entry.version).join(", ") || "none";
      return result(`Version ${String(call.arguments.version)} is not available. Saved versions: ${known}.`, true);
    }
    return result(JSON.stringify({
      version: saved.version,
      label: saved.label,
      savedAt: saved.createdAt,
      current: saved.version === library.currentVersion,
      recipe: saved.recipe,
    }));
  }
  return result(`Unknown tool ${call.name}.`, true);
}

/** Runs the chat: the model may call the tools for up to CHAT_TOOL_ROUNDS
 * turns, then must answer. Each call is logged by name (never photo bytes). */
export async function chatWithTools(
  call: ChatModelCall,
  systemPrompt: string,
  prompt: string,
  library: ChatLibrary,
  canSeePhotos: boolean,
  log: (line: string) => void = (line) => process.stderr.write(`${line}\n`),
): Promise<{ text: string; toolCalls: string[] }> {
  const context: Context = {
    systemPrompt,
    messages: [{ role: "user", content: prompt, timestamp: Date.now() }],
    tools: CHAT_TOOLS,
  };
  const toolCalls: string[] = [];
  for (let round = 0; ; round += 1) {
    const response = await call(context);
    if (response.stopReason === "error" || response.stopReason === "aborted") {
      throw new WorkerError("worker", response.errorMessage || "AI provider request failed.", { retryable: true });
    }
    if (response.stopReason === "length") {
      throw new WorkerError("worker", "AI provider response was truncated by its output limit.");
    }
    const calls = response.content.filter((part): part is ToolCall => part.type === "toolCall");
    if (calls.length === 0) return { text: assistantText(response), toolCalls };
    if (round === CHAT_TOOL_ROUNDS) {
      throw new WorkerError("output", "The model kept calling tools without answering.");
    }
    context.messages.push(response);
    for (const toolCall of calls) {
      toolCalls.push(toolCall.name);
      log(`chat tool call: ${toolCall.name} ${JSON.stringify(toolCall.arguments)}`);
      const output = runChatTool(library, toolCall, canSeePhotos);
      if (round === CHAT_TOOL_ROUNDS - 1) {
        output.content.push({ type: "text", text: "That was the last lookup. Answer the cook now." });
      }
      context.messages.push(output);
    }
  }
}

function trimmedString(value: unknown): string {
  return typeof value === "string" ? value.trim() : "";
}

/** The model's chat reply. A JSON object gives the answer and any change; a
 * model that ignored the format and wrote prose has that prose taken as a
 * plain answer. A label without a change is dropped. */
export function chatReply(text: string): ChatAnswer {
  let parsed: unknown = null;
  try {
    parsed = extractJson(text);
  } catch {
    // Not JSON: the whole reply is the answer.
  }
  const fields = parsed && typeof parsed === "object" && !Array.isArray(parsed)
    ? parsed as Record<string, unknown>
    : null;
  const reply = fields ? trimmedString(fields.reply) : text.trim();
  if (!reply) throw new WorkerError("output", "The model returned an empty chat answer.");
  const change = fields ? trimmedString(fields.change) : "";
  const label = change && fields ? trimmedString(fields.label) : "";
  return { reply, change, label };
}

/** One model call through the request's backend: a subscription, a native
 * OpenAI/Anthropic endpoint, or a pi-ai provider. Returns the reply text with
 * any web-search citations it gathered. */
function complete(request: GenerationRequest, options: CompletionOptions): Promise<Completion> {
  if (!isEndpointRequest(request)) return completeViaSubscription(request, options);
  if (isPiAiSpec(request.provider)) return completeViaPiAi(request, options);
  return completeViaEndpoint(request, options);
}

/** A call against an OpenAI- or Anthropic-spec endpoint. The base URL and
 * key come from the host and are never logged. */
async function completeViaEndpoint(request: PromptRequest, { systemPrompt, searchMode, reasoningEffort }: CompletionOptions): Promise<Completion> {
  const apiBaseUrl = nonBlank(request.apiBaseUrl);
  const apiKey = nonBlank(request.apiKey);
  const model = nonBlank(request.model);
  if (!apiBaseUrl || !apiKey || !model) {
    throw new WorkerError("worker", "Missing apiBaseUrl, apiKey, or model for the API request.");
  }
  if (request.provider === "anthropic") {
    const data = await anthropicTurn(apiBaseUrl, apiKey, model, systemPrompt, request.prompt, reasoningEffort, searchMode);
    if (data.stop_reason === "max_tokens") {
      throw new WorkerError("worker", "Anthropic response was truncated by max_tokens.");
    }
    const { text, searched, refused, citations } = parseAnthropicOutput(data);
    if (refused) throw new WorkerError("worker", "Anthropic refused the request.");
    return { label: "Anthropic", text, citations, searched };
  }
  const data = await openaiResponse(apiBaseUrl, apiKey, model, systemPrompt, request.prompt, searchMode, reasoningEffort);
  if (data.status && data.status !== "completed") {
    throw new WorkerError("worker", `OpenAI response incomplete: ${data.incomplete_details?.reason ?? "unknown reason"}`);
  }
  const { text, searched, refused, citations } = parseResponsesOutput(data);
  if (refused) throw new WorkerError("worker", "OpenAI refused the request.");
  return { label: "OpenAI", text, citations, searched };
}

/** A call through any pi-ai provider (Google, Mistral, Groq, OpenRouter, a
 * custom Chat Completions server, …) with the endpoint's own key. These
 * providers have no web search here. A blank key is only valid for a custom
 * server. */
export async function completeViaPiAi(
  request: PromptRequest,
  { systemPrompt, reasoningEffort }: Pick<CompletionOptions, "systemPrompt" | "reasoningEffort">,
  models: Models = piAiModels(),
  fetchImpl: typeof fetch | undefined = undefined,
): Promise<Completion> {
  const spec = nonBlank(request.provider);
  const apiKey = nonBlank(request.apiKey);
  const modelId = nonBlank(request.model);
  if (!modelId) throw new WorkerError("configuration", "Choose a model for this AI provider in Settings.");
  if (!apiKey && spec !== OPENAI_COMPATIBLE_SPEC) {
    throw new WorkerError("configuration", "Missing API key for this AI provider.");
  }
  const model = piAiModel(models, spec, modelId, nonBlank(request.apiBaseUrl));
  const response = await models.completeSimple(model, plainContext(systemPrompt, request.prompt), {
    apiKey: apiKey ?? "none",
    reasoning: reasoningEffort,
    maxRetries: 1,
    signal: AbortSignal.timeout(OPENAI_RESPONSE_TIMEOUT_MS),
    ...(fetchImpl ? { fetch: fetchImpl } : {}),
  });
  if (response.stopReason === "error" || response.stopReason === "aborted") {
    throw new WorkerError("worker", response.errorMessage || "AI provider request failed.", { retryable: true });
  }
  if (response.stopReason === "length") {
    throw new WorkerError("worker", "AI provider response was truncated by its output limit.");
  }
  return { label: model.provider, text: assistantText(response), citations: new Map(), searched: false };
}

/** A call through a pi subscription (ChatGPT for Codex, Claude Pro/Max). */
async function completeViaSubscription(request: PromptRequest, { systemPrompt, searchMode, reasoningEffort }: CompletionOptions): Promise<Completion> {
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
  const response = await modelRuntime.completeSimple(model, plainContext(systemPrompt, request.prompt), {
    reasoning: reasoningEffort,
    maxRetries: 1,
    ...(observer ? { fetch: observer.fetch, ...subscription.searchOptions(searchMode) } : {}),
  });
  if (response.stopReason === "error" || response.stopReason === "aborted") {
    throw new WorkerError("worker", response.errorMessage || "Pi model request failed.");
  }
  const observed = observer ? await observer.finish() : { invoked: false, sources: new Map<string, string>() };
  return { label: subscription.label, text: assistantText(response), citations: observed.sources, searched: observed.invoked };
}

/** Recipe generation through the request's backend. pi-ai providers have no
 * web search, so they always generate as "off" (the host downgrades grounded
 * requests; gap-fill sources are optional). Only a native endpoint's gap-fill
 * may keep unverifiable claimed sources. */
export async function generateRecipe(
  request: GenerationRequest,
  searchMode: SearchMode,
  reasoningEffort: ReasoningEffort,
  completeImpl: Complete = complete,
): Promise<GenerateResponse> {
  const piAi = isEndpointRequest(request) && isPiAiSpec(request.provider);
  const effective = piAi ? "off" : searchMode;
  const completion = await completeImpl(request, {
    systemPrompt: recipePrompt(request.systemPrompt, effective),
    searchMode: effective,
    reasoningEffort,
  });
  return finishRecipe({
    ...completion,
    searchMode: effective,
    acceptUnverifiable: isEndpointRequest(request) && !piAi && searchMode === "gapfill",
  });
}

async function main(): Promise<void> {
  const request = parseRequest(await readStdin());
  if (isListModelsRequest(request)) return listModels(request);
  if (isCheckLoginRequest(request)) return checkLogin(request);
  if (request.command === "cleanMedia") return cleanMedia(request);
  if (request.command === "chat") return chat(request);
  outputJson(await generateRecipe(request, searchModeOf(request), normalizeEffort(request.reasoningEffort)));
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  runMain(main, workerFailure);
}
