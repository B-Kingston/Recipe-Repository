/**
 * Web search through a search API, for chat models that have no web search of
 * their own. Each provider is one adapter that turns a query into a request
 * and the response into `{title, url, snippet}` results; the tool the model
 * sees, the result formatting, and the error handling are shared. Adding a
 * provider is one SEARCH_PROVIDERS entry (and its key in WEB_SEARCH_PROVIDERS
 * in src/main.rs, which offers it in Settings).
 */

/** The account's search key, as the host sends it. */
export interface WebSearchConfig {
  provider: string;
  apiKey: string;
}

/** One search result, the same for every provider. */
export interface SearchResult {
  title: string;
  url: string;
  snippet: string;
}

interface SearchAdapter {
  label: string;
  request(query: string, apiKey: string, count: number): { url: string; init: RequestInit };
  results(body: unknown): SearchResult[];
}

/** Results the model gets per search. */
export const SEARCH_RESULT_COUNT = 6;
/** Characters of each result's snippet the model gets. */
const SNIPPET_CHARS = 700;
const SEARCH_TIMEOUT_MS = 20_000;

function record(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : {};
}

function list(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

function text(value: unknown): string {
  return typeof value === "string" ? value : "";
}

/** Plain text from a snippet that may carry markup (Brave bolds matches). */
function plain(value: string): string {
  return value
    .replace(/<[^>]*>/g, "")
    .replace(/&amp;/g, "&").replace(/&quot;/g, "\"").replace(/&#39;/g, "'").replace(/&lt;/g, "<").replace(/&gt;/g, ">")
    .replace(/\s+/g, " ")
    .trim();
}

function jsonPost(url: string, headers: Record<string, string>, body: unknown): { url: string; init: RequestInit } {
  return {
    url,
    init: { method: "POST", headers: { "Content-Type": "application/json", Accept: "application/json", ...headers }, body: JSON.stringify(body) },
  };
}

/** The search APIs, keyed as the host stores them. */
export const SEARCH_PROVIDERS: Record<string, SearchAdapter> = {
  brave: {
    label: "Brave Search",
    request: (query, apiKey, count) => ({
      url: `https://api.search.brave.com/res/v1/web/search?${new URLSearchParams({ q: query, count: String(count), extra_snippets: "true" })}`,
      init: { headers: { Accept: "application/json", "X-Subscription-Token": apiKey } },
    }),
    results: (body) => list(record(record(body).web).results).map((entry) => {
      const result = record(entry);
      const snippets = [text(result.description), ...list(result.extra_snippets).map(text)];
      return { title: text(result.title), url: text(result.url), snippet: snippets.filter(Boolean).join(" … ") };
    }),
  },
  exa: {
    label: "Exa",
    request: (query, apiKey, count) => jsonPost("https://api.exa.ai/search", { "x-api-key": apiKey }, {
      query,
      numResults: count,
      type: "auto",
      contents: { highlights: { maxCharacters: SNIPPET_CHARS } },
    }),
    results: (body) => list(record(body).results).map((entry) => {
      const result = record(entry);
      const snippet = list(result.highlights).map(text).filter(Boolean).join(" … ") || text(result.text);
      return { title: text(result.title), url: text(result.url), snippet };
    }),
  },
  tavily: {
    label: "Tavily",
    request: (query, apiKey, count) => jsonPost("https://api.tavily.com/search", { Authorization: `Bearer ${apiKey}` }, {
      query,
      max_results: count,
      search_depth: "basic",
    }),
    results: (body) => list(record(body).results).map((entry) => {
      const result = record(entry);
      return { title: text(result.title), url: text(result.url), snippet: text(result.content) };
    }),
  },
};

/** The account's search key when it names a provider the worker knows. */
export function webSearchConfig(value: unknown): WebSearchConfig | null {
  const fields = record(value);
  const provider = text(fields.provider).trim();
  const apiKey = text(fields.apiKey).trim();
  return provider in SEARCH_PROVIDERS && apiKey ? { provider, apiKey } : null;
}

/** A search API failure, worded for the model. `auth` marks a rejected key. */
export class WebSearchError extends Error {
  readonly auth: boolean;
  constructor(message: string, auth = false) {
    super(message);
    this.auth = auth;
  }
}

/** `text` with every copy of the key replaced, so an error that quotes a
 * request header (undici does, for an invalid one) never carries the key
 * into a log or the model's context. */
export function redact(text: string, key: string): string {
  // Real keys are long; redacting a short string would mangle ordinary words.
  return key.length >= 8 ? text.split(key).join("[key]") : text;
}

/** Runs one query through the configured provider. Results without a URL are
 * dropped, and snippets are plain text cut to SNIPPET_CHARS. */
export async function webSearch(
  config: WebSearchConfig,
  query: string,
  fetchImpl: typeof fetch = globalThis.fetch,
): Promise<SearchResult[]> {
  const adapter = SEARCH_PROVIDERS[config.provider];
  if (!adapter) throw new WebSearchError(`Unknown search provider ${config.provider}.`);
  // A key travels in a header: anything but printable ASCII is not a key,
  // and fetch would quote it back in its error.
  if (!/^[\x21-\x7e]+$/.test(config.apiKey)) {
    throw new WebSearchError(`${adapter.label} cannot use the saved search key: it has spaces or characters a key never has.`, true);
  }
  const { url, init } = adapter.request(query, config.apiKey, SEARCH_RESULT_COUNT);
  let response: Response;
  try {
    response = await fetchImpl(url, { ...init, signal: AbortSignal.timeout(SEARCH_TIMEOUT_MS) });
  } catch (error) {
    const reason = redact(error instanceof Error ? error.message : String(error), config.apiKey);
    throw new WebSearchError(`${adapter.label} could not be reached (${reason}).`);
  }
  if (response.status === 401 || response.status === 403) {
    throw new WebSearchError(`${adapter.label} rejected the search API key.`, true);
  }
  if (!response.ok) throw new WebSearchError(`${adapter.label} search failed with HTTP ${response.status}.`);
  let body: unknown;
  try {
    body = await response.json();
  } catch {
    throw new WebSearchError(`${adapter.label} returned a response that is not JSON.`);
  }
  return adapter.results(body)
    .filter((result) => /^https?:\/\//.test(result.url))
    .slice(0, SEARCH_RESULT_COUNT)
    .map((result) => {
      const snippet = plain(result.snippet);
      return {
        title: plain(result.title) || result.url,
        url: result.url,
        snippet: snippet.length > SNIPPET_CHARS ? `${snippet.slice(0, SNIPPET_CHARS).trimEnd()}…` : snippet,
      };
    });
}

/** The web_search tool result: numbered results with their URLs. */
export function formatSearchResults(query: string, results: SearchResult[]): string {
  if (results.length === 0) return `No web results for "${query}".`;
  return [`Web results for "${query}":`, ...results.map((result, index) =>
    `${index + 1}. ${result.title}\n${result.url}${result.snippet ? `\n${result.snippet}` : ""}`)].join("\n\n");
}
