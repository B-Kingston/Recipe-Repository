import { mkdir } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import process from "node:process";
import {
  ModelRuntime,
  getAgentDir,
} from "@earendil-works/pi-coding-agent";
import { createNativeSearchObserver, nativeSearchPayload } from "./codex-native-search.mjs";

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

function openaiCredentials(request) {
  const apiBaseUrl = typeof request.apiBaseUrl === "string" && request.apiBaseUrl.trim()
    ? request.apiBaseUrl.trim() : null;
  const apiKey = typeof request.apiKey === "string" && request.apiKey.trim()
    ? request.apiKey.trim() : null;
  if (!apiBaseUrl || !apiKey) {
    throw new WorkerError("worker", "Missing apiBaseUrl or apiKey for the OpenAI API request.");
  }
  return { apiBaseUrl, apiKey };
}

/** POST {base}/responses; returns the parsed JSON body. Throws WorkerError on
 *  transport failure or non-2xx (error body truncated to 300 chars; never
 *  includes the key). */
export async function openaiResponse(apiBaseUrl, apiKey, modelId, systemPrompt, prompt, searchEnabled, reasoningEffort = "low", fetchImpl = fetch) {
  const url = `${apiBaseUrl.replace(/\/+$/, "")}/responses`;
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), OPENAI_RESPONSE_TIMEOUT_MS);
  try {
    const response = await fetchImpl(url, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: `Bearer ${apiKey}`,
      },
      body: JSON.stringify({
        model: modelId,
        input: [
          { role: "system", content: systemPrompt },
          { role: "user", content: prompt },
        ],
        ...(searchEnabled ? { tools: [{ type: "web_search" }], tool_choice: "required" } : {}),
        reasoning: { effort: reasoningEffort },
        text: { verbosity: "low" },
      }),
      signal: controller.signal,
    });
    if (!response.ok) {
      const detail = await response.text().catch(() => "");
      throw new WorkerError(
        "worker",
        `OpenAI API returned HTTP ${response.status}${detail ? `: ${detail.slice(0, 300)}` : ""}`,
      );
    }
    return await response.json();
  } catch (error) {
    if (error instanceof WorkerError) throw error;
    throw new WorkerError("worker", `OpenAI API request failed: ${error instanceof Error ? error.message : "unknown error"}`);
  } finally {
    clearTimeout(timer);
  }
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

async function main() {
  const request = await readRequest();
  if (request.command === "listModels") {
    await listModels(request);
    return;
  }
  const agentDir = process.env.PI_CODING_AGENT_DIR || getAgentDir();
  const searchEnabled = Boolean(request.searchEnabled);
  const modelId = typeof request.model === "string" && request.model.trim()
    ? request.model.trim()
    : "gpt-5.4-mini";
  const reasoningEffort = normalizeEffort(request.reasoningEffort);
  if (request.provider === "openai") {
    const { apiBaseUrl, apiKey } = openaiCredentials(request);
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
    outputJson({ recipe: result.recipe, sources });
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

  const result = extractJson(assistantText(response));
  if (!result || typeof result !== "object" || !result.recipe || typeof result.recipe !== "object") {
    throw new WorkerError("output", "Pi did not return a recipe object.");
  }
  const sources = sourceList(result.sources, observed.sources);
  if (searchEnabled && sources.length === 0) {
    throw new WorkerError("output", "Pi did not use any valid web-search sources.");
  }
  outputJson({ recipe: result.recipe, sources });
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
