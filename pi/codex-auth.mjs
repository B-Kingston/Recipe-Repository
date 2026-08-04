// Starts and completes the OpenAI Codex (ChatGPT) OAuth device flow and
// hands the resulting credential back to the caller, which persists it in
// the application database. Nothing is written to disk by this script.
//
// Subcommands (one JSON object per line on stdout):
//   start  -> {status:"ok", deviceAuthId, userCode, verificationUri,
//              intervalSeconds, expiresInSeconds}
//   poll   -> reads {deviceAuthId, userCode} on stdin; performs one poll
//              attempt; on success exchanges the authorization code and
//              returns the openai-codex OAuth credential.
//              -> {status:"complete", credential:{...}}
//                 | {status:"pending"} | {status:"slow_down"}
//                 | {status:"failed", message}
//
// Endpoints and constants intentionally mirror pi-ai's OpenAI Codex OAuth
// flow (auth/oauth/openai-codex.ts) so the credential shape stays compatible
// with ModelRuntime's credential store.

const CLIENT_ID = "app_EMoamEEZ73f0CkXaXp7hrann";
const DEVICE_USER_CODE_URL = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL = "https://auth.openai.com/api/accounts/deviceauth/token";
const TOKEN_URL = "https://auth.openai.com/oauth/token";
const DEVICE_VERIFICATION_URI = "https://auth.openai.com/codex/device";
const DEVICE_REDIRECT_URI = "https://auth.openai.com/deviceauth/callback";
const DEVICE_CODE_TIMEOUT_SECONDS = 15 * 60;
const ACCOUNT_CLAIM_PATH = "https://api.openai.com/auth";

function outputJson(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}

async function readStdin() {
  let input = "";
  for await (const chunk of process.stdin) input += chunk;
  return input;
}

async function requestJson(url, options) {
  const response = await fetch(url, options);
  const text = await response.text().catch(() => "");
  let json = null;
  try {
    json = JSON.parse(text);
  } catch {
    // Non-JSON body; text is still surfaced in error messages.
  }
  return { response, json, text };
}

export async function startDeviceFlow() {
  const { response, json } = await requestJson(DEVICE_USER_CODE_URL, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ client_id: CLIENT_ID }),
  });
  if (!response.ok) {
    throw new Error(`Device code request failed with status ${response.status}`);
  }
  const interval = typeof json?.interval === "string" ? Number(json.interval.trim()) : json?.interval;
  if (
    !json?.device_auth_id ||
    !json.user_code ||
    typeof interval !== "number" ||
    !Number.isFinite(interval) ||
    interval < 0
  ) {
    throw new Error(`Invalid device code response: ${JSON.stringify(json)}`);
  }
  return {
    deviceAuthId: json.device_auth_id,
    userCode: json.user_code,
    verificationUri: DEVICE_VERIFICATION_URI,
    intervalSeconds: interval,
    expiresInSeconds: DEVICE_CODE_TIMEOUT_SECONDS,
  };
}

function decodeAccountId(accessToken) {
  const parts = accessToken.split(".");
  if (parts.length !== 3) return null;
  let payload;
  try {
    payload = JSON.parse(Buffer.from(parts[1], "base64url").toString("utf8"));
  } catch {
    return null;
  }
  const accountId = payload?.[ACCOUNT_CLAIM_PATH]?.chatgpt_account_id;
  return typeof accountId === "string" && accountId.length > 0 ? accountId : null;
}

export async function exchangeAuthorizationCode(code, codeVerifier) {
  const { response, json } = await requestJson(TOKEN_URL, {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      grant_type: "authorization_code",
      client_id: CLIENT_ID,
      code,
      code_verifier: codeVerifier,
      redirect_uri: DEVICE_REDIRECT_URI,
    }).toString(),
  });
  if (!response.ok) {
    throw new Error(`Token exchange failed with status ${response.status}`);
  }
  if (!json?.access_token || !json.refresh_token || typeof json.expires_in !== "number") {
    throw new Error("Token response missing access_token, refresh_token, or expires_in");
  }
  const accountId = decodeAccountId(json.access_token);
  if (!accountId) {
    throw new Error("Failed to extract accountId from token");
  }
  return {
    type: "oauth",
    access: json.access_token,
    refresh: json.refresh_token,
    expires: Date.now() + json.expires_in * 1000,
    accountId,
  };
}

export async function pollDeviceFlow(device) {
  const { response, json, text } = await requestJson(DEVICE_TOKEN_URL, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      device_auth_id: device.deviceAuthId,
      user_code: device.userCode,
    }),
  });
  if (response.ok) {
    if (!json?.authorization_code || !json.code_verifier) {
      return { status: "failed", message: "Invalid device auth token response" };
    }
    try {
      const credential = await exchangeAuthorizationCode(
        json.authorization_code,
        json.code_verifier,
      );
      return { status: "complete", credential };
    } catch (error) {
      return { status: "failed", message: error instanceof Error ? error.message : String(error) };
    }
  }
  if (response.status === 403 || response.status === 404) {
    return { status: "pending" };
  }
  const errorCode = typeof json?.error === "object" ? json.error?.code : json?.error;
  if (errorCode === "deviceauth_authorization_pending") {
    return { status: "pending" };
  }
  if (errorCode === "slow_down") {
    return { status: "slow_down" };
  }
  return {
    status: "failed",
    message: `Device auth failed with status ${response.status}${text ? `: ${text}` : ""}`,
  };
}

async function main() {
  const [subcommand] = process.argv.slice(2);
  if (subcommand === "start") {
    outputJson({ status: "ok", ...(await startDeviceFlow()) });
  } else if (subcommand === "poll") {
    let device;
    try {
      device = JSON.parse(await readStdin());
    } catch {
      throw new Error("Invalid poll request");
    }
    if (!device?.deviceAuthId || !device.userCode) {
      throw new Error("Missing deviceAuthId or userCode");
    }
    outputJson(await pollDeviceFlow(device));
  } else {
    throw new Error(`Unknown subcommand: ${subcommand}`);
  }
}

if (process.argv[1] === import.meta.filename) {
  main().catch((error) => {
    outputJson({
      status: "error",
      code: "internal",
      message: error instanceof Error ? error.message : String(error),
    });
    process.exitCode = 1;
  });
}
