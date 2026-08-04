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

class WorkerError extends Error {
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
      reasoningEffort: "low",
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
