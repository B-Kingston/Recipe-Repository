import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import test from "node:test";
import { exchangeCode, parseAuthorizationInput, startFlow } from "./claude-auth.mjs";

test("startFlow issues a PKCE S256 challenge for its verifier, sent as state", () => {
  const { url, verifier } = startFlow();
  const params = new URL(url).searchParams;

  assert.equal(params.get("state"), verifier);
  assert.equal(params.get("code_challenge_method"), "S256");
  assert.equal(params.get("code_challenge"), createHash("sha256").update(verifier).digest("base64url"));
  assert.equal(params.get("redirect_uri"), "https://platform.claude.com/oauth/code/callback");
  assert.notEqual(startFlow().verifier, verifier);
});

test("parseAuthorizationInput accepts code#state, redirect URLs, and bare codes", () => {
  assert.deepEqual(parseAuthorizationInput(" abc#xyz "), { code: "abc", state: "xyz" });
  assert.deepEqual(
    parseAuthorizationInput("https://platform.claude.com/oauth/code/callback?code=abc&state=xyz"),
    { code: "abc", state: "xyz" },
  );
  assert.deepEqual(parseAuthorizationInput("abc"), { code: "abc" });
  assert.deepEqual(parseAuthorizationInput(""), {});
});

test("exchangeCode rejects a code from another sign-in without calling Anthropic", async () => {
  let called = false;
  const result = await exchangeCode({ input: "abc#other", verifier: "mine" }, async () => {
    called = true;
  });

  assert.equal(result.status, "failed");
  assert.equal(called, false);
});

test("exchangeCode returns an oauth credential that expires before the token does", async () => {
  let body;
  const before = Date.now();
  const result = await exchangeCode({ input: "abc#mine", verifier: "mine" }, async (_url, init) => {
    body = JSON.parse(init.body);
    return new Response(JSON.stringify({ access_token: "sk-ant-oat-1", refresh_token: "r1", expires_in: 3600 }));
  });

  assert.equal(body.code_verifier, "mine");
  assert.equal(body.state, "mine");
  assert.equal(result.status, "complete");
  assert.equal(result.credential.type, "oauth");
  assert.equal(result.credential.access, "sk-ant-oat-1");
  assert.equal(result.credential.refresh, "r1");
  assert.ok(result.credential.expires <= before + 3600_000 - 5 * 60_000 + 1000);
  assert.ok(result.credential.expires >= before + 3600_000 - 5 * 60_000);
});

test("exchangeCode reports a rejected code as failed", async () => {
  const result = await exchangeCode(
    { input: "abc", verifier: "mine" },
    async () => new Response("invalid_grant", { status: 400 }),
  );

  assert.equal(result.status, "failed");
  assert.match(result.message, /400/);
});

test("exchangeCode refuses a token response without both tokens and an expiry", async () => {
  const result = await exchangeCode(
    { input: "abc", verifier: "mine" },
    async () => new Response(JSON.stringify({ access_token: "sk-ant-oat-1" })),
  );

  assert.equal(result.status, "failed");
  assert.match(result.message, /missing tokens/);
});
