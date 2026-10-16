// Starts and completes the Anthropic (Claude Pro/Max) OAuth "copy code" flow
// and hands the resulting credential back to the caller, which persists it in
// the application database. Nothing is written to disk by this script.
//
// Subcommands (one JSON object per line on stdout):
//   start    -> {status:"ok", url, verifier}
//               The user opens url, signs in, and Anthropic shows a code.
//   exchange -> reads {input, verifier} on stdin; input is the pasted
//               code ("code#state") or redirect URL. Exchanges the code and
//               returns the anthropic OAuth credential.
//               -> {status:"complete", credential:{type,access,refresh,expires}}
//                  | {status:"failed", message}
//
// Endpoints and constants intentionally mirror pi-ai's Anthropic OAuth flow
// (auth/oauth/anthropic.js, copy code login) so the credential shape stays
// compatible with ModelRuntime's credential store and its token refresh.

import { createHash, randomBytes } from "node:crypto";
import { readStdin, outputJson, runMain } from "./cli.mjs";

const CLIENT_ID = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL = "https://claude.ai/oauth/authorize";
const TOKEN_URL = "https://platform.claude.com/v1/oauth/token";
const REDIRECT_URI = "https://platform.claude.com/oauth/code/callback";
const SCOPES = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
// pi-ai subtracts five minutes so a token is refreshed before it lapses.
const EXPIRY_MARGIN_MS = 5 * 60 * 1000;

const base64url = (buffer) => buffer.toString("base64url");

export function startFlow() {
  const verifier = base64url(randomBytes(32));
  const challenge = base64url(createHash("sha256").update(verifier).digest());
  // state is the PKCE verifier, as in pi-ai's copy-code login. Anthropic echoes
  // it back in the pasted code#state, which exchangeCode checks before exchanging.
  const params = new URLSearchParams({
    code: "true",
    client_id: CLIENT_ID,
    response_type: "code",
    redirect_uri: REDIRECT_URI,
    scope: SCOPES,
    code_challenge: challenge,
    code_challenge_method: "S256",
    state: verifier,
  });
  return { url: `${AUTHORIZE_URL}?${params}`, verifier };
}

/** Accepts what the user pastes: "code#state", a redirect URL, a query
 * string, or a bare code. */
export function parseAuthorizationInput(input) {
  const value = typeof input === "string" ? input.trim() : "";
  if (!value) return {};
  try {
    const url = new URL(value);
    return {
      code: url.searchParams.get("code") ?? undefined,
      state: url.searchParams.get("state") ?? undefined,
    };
  } catch {
    // Not a URL.
  }
  if (value.includes("#")) {
    const [code, state] = value.split("#", 2);
    return { code, state };
  }
  if (value.includes("code=")) {
    const params = new URLSearchParams(value);
    return { code: params.get("code") ?? undefined, state: params.get("state") ?? undefined };
  }
  return { code: value };
}

export async function exchangeCode({ input, verifier }, fetchImpl = fetch) {
  if (typeof verifier !== "string" || !verifier) throw new Error("Missing verifier");
  const { code, state } = parseAuthorizationInput(input);
  if (!code) return { status: "failed", message: "Paste the code Anthropic shows after you sign in." };
  if (state && state !== verifier) {
    return { status: "failed", message: "The code belongs to another sign-in. Start again." };
  }
  const response = await fetchImpl(TOKEN_URL, {
    method: "POST",
    headers: { "Content-Type": "application/json", Accept: "application/json" },
    body: JSON.stringify({
      grant_type: "authorization_code",
      client_id: CLIENT_ID,
      code,
      state: state || verifier,
      redirect_uri: REDIRECT_URI,
      code_verifier: verifier,
    }),
    signal: AbortSignal.timeout(30_000),
  });
  const text = await response.text().catch(() => "");
  if (!response.ok) {
    return {
      status: "failed",
      message: `Token exchange failed with status ${response.status}${text ? `: ${text.slice(0, 300)}` : ""}`,
    };
  }
  let token;
  try {
    token = JSON.parse(text);
  } catch {
    return { status: "failed", message: "Token exchange returned invalid JSON." };
  }
  if (typeof token?.access_token !== "string" || typeof token.refresh_token !== "string"
    || typeof token.expires_in !== "number") {
    return { status: "failed", message: "Token exchange response is missing tokens." };
  }
  return {
    status: "complete",
    credential: {
      type: "oauth",
      access: token.access_token,
      refresh: token.refresh_token,
      expires: Date.now() + token.expires_in * 1000 - EXPIRY_MARGIN_MS,
    },
  };
}

async function main() {
  const [subcommand] = process.argv.slice(2);
  if (subcommand === "start") {
    outputJson({ status: "ok", ...startFlow() });
  } else if (subcommand === "exchange") {
    let request;
    try {
      request = JSON.parse(await readStdin());
    } catch {
      throw new Error("Invalid exchange request");
    }
    outputJson(await exchangeCode(request ?? {}));
  } else {
    throw new Error(`Unknown subcommand: ${subcommand}`);
  }
}

if (process.argv[1] === import.meta.filename) runMain(main);
