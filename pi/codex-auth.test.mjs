import assert from "node:assert/strict";
import test from "node:test";
import {
  exchangeAuthorizationCode,
  pollDeviceFlow,
  startDeviceFlow,
} from "./codex-auth.mjs";

// A signed-looking JWT whose payload carries the chatgpt_account_id claim the
// Codex flow extracts. Signature bytes are irrelevant to the decoder.
function accountJwt(accountId) {
  const header = Buffer.from(JSON.stringify({ alg: "none" })).toString("base64url");
  const payload = Buffer.from(
    JSON.stringify({ "https://api.openai.com/auth": { chatgpt_account_id: accountId } }),
  ).toString("base64url");
  return `${header}.${payload}.signature`;
}

function mockFetch(routes) {
  const calls = [];
  const original = globalThis.fetch;
  globalThis.fetch = async (url, options) => {
    calls.push({ url, options });
    const route = routes[String(url)];
    if (!route) throw new Error(`Unexpected fetch: ${url}`);
    const response = typeof route === "function" ? route(options) : route;
    return new Response(JSON.stringify(response.body), { status: response.status ?? 200 });
  };
  return { calls, restore: () => { globalThis.fetch = original; } };
}

test("startDeviceFlow requests device codes and exposes the verification URI", async () => {
  const fake = mockFetch({
    "https://auth.openai.com/api/accounts/deviceauth/usercode": {
      body: { device_auth_id: "deviceauth_1", user_code: "6ZM9-9XMXN", interval: 5 },
    },
  });
  try {
    const device = await startDeviceFlow();

    assert.equal(device.deviceAuthId, "deviceauth_1");
    assert.equal(device.userCode, "6ZM9-9XMXN");
    assert.equal(device.verificationUri, "https://auth.openai.com/codex/device");
    assert.equal(device.intervalSeconds, 5);
    assert.equal(device.expiresInSeconds, 15 * 60);
    assert.deepEqual(JSON.parse(fake.calls[0].options.body), { client_id: "app_EMoamEEZ73f0CkXaXp7hrann" });
  } finally {
    fake.restore();
  }
});

test("pollDeviceFlow exchanges the code and returns a pi-compatible credential", async () => {
  const fake = mockFetch({
    "https://auth.openai.com/api/accounts/deviceauth/token": {
      body: { authorization_code: "authcode_1", code_verifier: "verifier_1" },
    },
    "https://auth.openai.com/oauth/token": {
      body: {
        access_token: accountJwt("acct_42"),
        refresh_token: "refresh_1",
        expires_in: 3600,
      },
    },
  });
  try {
    const result = await pollDeviceFlow({ deviceAuthId: "deviceauth_1", userCode: "6ZM9-9XMXN" });

    assert.equal(result.status, "complete");
    assert.equal(result.credential.type, "oauth");
    assert.equal(result.credential.accountId, "acct_42");
    assert.equal(result.credential.refresh, "refresh_1");
    assert.equal(typeof result.credential.expires, "number");
    // Redirect URI must match the device auth callback pi uses.
    const tokenCall = fake.calls.find((call) => call.url === "https://auth.openai.com/oauth/token");
    assert.equal(
      new URLSearchParams(tokenCall.options.body).get("redirect_uri"),
      "https://auth.openai.com/deviceauth/callback",
    );
  } finally {
    fake.restore();
  }
});

test("pollDeviceFlow maps pending, slow_down, and failed responses", async () => {
  const fake = mockFetch({
    "https://auth.openai.com/api/accounts/deviceauth/token": (options) => {
      const body = JSON.parse(options.body);
      if (body.user_code === "PEND-XXXXX") return { status: 403, body: { error: "deviceauth_authorization_pending" } };
      if (body.user_code === "SLOW-XXXXX") return { status: 400, body: { error: { code: "slow_down" } } };
      return { status: 500, body: { error: "boom" } };
    },
  });
  try {
    assert.deepEqual(await pollDeviceFlow({ deviceAuthId: "d", userCode: "PEND-XXXXX" }), { status: "pending" });
    assert.deepEqual(await pollDeviceFlow({ deviceAuthId: "d", userCode: "SLOW-XXXXX" }), { status: "slow_down" });
    assert.equal((await pollDeviceFlow({ deviceAuthId: "d", userCode: "FAIL-XXXXX" })).status, "failed");
  } finally {
    fake.restore();
  }
});

test("exchangeAuthorizationCode rejects tokens without an account claim", async () => {
  const fake = mockFetch({
    "https://auth.openai.com/oauth/token": {
      body: { access_token: "not-a-jwt", refresh_token: "r", expires_in: 3600 },
    },
  });
  try {
    await assert.rejects(
      exchangeAuthorizationCode("code", "verifier"),
      /accountId/,
    );
  } finally {
    fake.restore();
  }
});
