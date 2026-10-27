import test from "node:test";
import assert from "node:assert/strict";
import { SEARCH_PROVIDERS, WebSearchError, formatSearchResults, webSearch, webSearchConfig } from "./web-search.ts";

interface Seen {
  url: string;
  init: RequestInit;
}

/** A fetch that records the request and answers with `body` and `status`. */
function fakeFetch(body: unknown, status = 200): { fetchImpl: typeof fetch; seen: Seen[] } {
  const seen: Seen[] = [];
  const fetchImpl = (async (url: string | URL | Request, init?: RequestInit) => {
    seen.push({ url: String(url), init: init ?? {} });
    return new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });
  }) as typeof fetch;
  return { fetchImpl, seen };
}

function header(init: RequestInit, name: string): string | undefined {
  return (init.headers as Record<string, string>)[name];
}

test("webSearchConfig keeps a known provider with a key and drops the rest", () => {
  assert.deepEqual(webSearchConfig({ provider: " exa ", apiKey: " k " }), { provider: "exa", apiKey: "k" });
  assert.equal(webSearchConfig({ provider: "bing", apiKey: "k" }), null);
  assert.equal(webSearchConfig({ provider: "brave", apiKey: "" }), null);
  assert.equal(webSearchConfig(null), null);
  assert.deepEqual(Object.keys(SEARCH_PROVIDERS).sort(), ["brave", "exa", "tavily"]);
});

test("Brave searches with its token header and joins the extra snippets", async () => {
  const { fetchImpl, seen } = fakeFetch({
    web: {
      results: [
        { title: "Dal <strong>tadka</strong>", url: "https://example.com/dal", description: "Temper the <strong>spices</strong> &amp; pour.", extra_snippets: ["Use ghee."] },
        { title: "No link", url: "ftp://example.com" },
      ],
    },
  });
  const results = await webSearch({ provider: "brave", apiKey: "brave-key" }, "dal tadka", fetchImpl);
  const url = new URL(seen[0].url);
  assert.equal(url.origin + url.pathname, "https://api.search.brave.com/res/v1/web/search");
  assert.equal(url.searchParams.get("q"), "dal tadka");
  assert.equal(header(seen[0].init, "X-Subscription-Token"), "brave-key");
  assert.deepEqual(results, [{ title: "Dal tadka", url: "https://example.com/dal", snippet: "Temper the spices & pour. … Use ghee." }]);
});

test("Exa posts the query with its key header and reads the highlights", async () => {
  const { fetchImpl, seen } = fakeFetch({
    results: [
      { title: "Paneer", url: "https://example.com/paneer", highlights: ["Press for 20 minutes.", "Salt the whey."] },
      { title: "", url: "https://example.com/text-only", text: "Full text." },
    ],
  });
  const results = await webSearch({ provider: "exa", apiKey: "exa-key" }, "pressing paneer", fetchImpl);
  assert.equal(seen[0].url, "https://api.exa.ai/search");
  assert.equal(seen[0].init.method, "POST");
  assert.equal(header(seen[0].init, "x-api-key"), "exa-key");
  const body = JSON.parse(String(seen[0].init.body));
  assert.equal(body.query, "pressing paneer");
  assert.ok(body.contents.highlights);
  assert.deepEqual(results, [
    { title: "Paneer", url: "https://example.com/paneer", snippet: "Press for 20 minutes. … Salt the whey." },
    { title: "https://example.com/text-only", url: "https://example.com/text-only", snippet: "Full text." },
  ]);
});

test("Tavily posts the query with a bearer key and reads the content", async () => {
  const { fetchImpl, seen } = fakeFetch({ results: [{ title: "Ghee", url: "https://example.com/ghee", content: "x".repeat(900) }] });
  const [result] = await webSearch({ provider: "tavily", apiKey: "tvly-key" }, "ghee smoke point", fetchImpl);
  assert.equal(seen[0].url, "https://api.tavily.com/search");
  assert.equal(header(seen[0].init, "Authorization"), "Bearer tvly-key");
  assert.equal(JSON.parse(String(seen[0].init.body)).query, "ghee smoke point");
  assert.equal(result.snippet.length, 701, "snippets are cut to 700 characters and an ellipsis");
});

test("a rejected key and a failing API are WebSearchErrors", async () => {
  await assert.rejects(webSearch({ provider: "exa", apiKey: "bad" }, "q", fakeFetch({}, 401).fetchImpl), (error) => {
    assert.ok(error instanceof WebSearchError);
    assert.equal(error.auth, true);
    assert.match(error.message, /Exa rejected/);
    return true;
  });
  await assert.rejects(webSearch({ provider: "brave", apiKey: "k" }, "q", fakeFetch({}, 500).fetchImpl), (error) => {
    assert.ok(error instanceof WebSearchError);
    assert.equal(error.auth, false);
    return true;
  });
  const offline = (async () => { throw new Error("ENOTFOUND"); }) as typeof fetch;
  await assert.rejects(webSearch({ provider: "tavily", apiKey: "k" }, "q", offline), /could not be reached/);
});

test("formatSearchResults numbers the results with their URLs", () => {
  assert.equal(formatSearchResults("q", []), 'No web results for "q".');
  assert.equal(
    formatSearchResults("ghee", [{ title: "Ghee", url: "https://example.com", snippet: "Clarified butter." }]),
    'Web results for "ghee":\n\n1. Ghee\nhttps://example.com\nClarified butter.',
  );
});

test("a search failure never repeats the key, whatever went wrong", async () => {
  for (const provider of Object.keys(SEARCH_PROVIDERS)) {
    const apiKey = "key-SECRET\u0000tail";
    await assert.rejects(webSearch({ provider, apiKey }, "q", globalThis.fetch), (error) => {
      assert.ok(error instanceof WebSearchError);
      assert.ok(!error.message.includes("SECRET"), `${provider}: ${error.message}`);
      return true;
    });
  }
});

test("status and network failures name the provider, not the key", async () => {
  for (const [provider, status] of [["brave", 429], ["exa", 500], ["tavily", 403]] as const) {
    const { fetchImpl } = fakeFetch({ error: "nope" }, status);
    await assert.rejects(webSearch({ provider, apiKey: "key-SECRET" }, "q", fetchImpl), (error) => {
      assert.ok(error instanceof WebSearchError);
      assert.equal(error.auth, status === 403);
      assert.ok(!error.message.includes("SECRET"));
      return true;
    });
  }
  const unreachable = (async () => { throw new TypeError("fetch failed"); }) as unknown as typeof fetch;
  await assert.rejects(webSearch({ provider: "exa", apiKey: "key-SECRET" }, "q", unreachable), /Exa could not be reached \(fetch failed\)\./);
  const { fetchImpl: html } = fakeFetch(null);
  const notJson = (async () => new Response("<html>", { status: 200 })) as unknown as typeof fetch;
  await assert.rejects(webSearch({ provider: "brave", apiKey: "k" }, "q", notJson), /not JSON/);
  assert.deepEqual(await webSearch({ provider: "brave", apiKey: "k" }, "q", html), [], "an empty body is no results");
});

test("results keep only web URLs, plain text, and at most the result count", async () => {
  const many = Array.from({ length: 10 }, (_, at) => ({ title: `<b>Result</b> ${at} &amp; more`, url: `https://example.com/${at}`, content: "x".repeat(900) }));
  const { fetchImpl } = fakeFetch({ results: [{ title: "Bad", url: "javascript:alert(1)", content: "" }, { title: "", url: "ftp://example.com", content: "" }, ...many] });
  const results = await webSearch({ provider: "tavily", apiKey: "k" }, "q", fetchImpl);
  assert.equal(results.length, 6);
  assert.ok(results.every((result) => result.url.startsWith("https://")));
  assert.equal(results[0].title, "Result 0 & more");
  assert.ok(results[0].snippet.length <= 701 && results[0].snippet.endsWith("…"));
  assert.equal(formatSearchResults("q", []), "No web results for \"q\".");
});
