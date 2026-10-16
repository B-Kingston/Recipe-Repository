// Dumps the FULL Vercel AI Gateway /chat/completions HTTP response envelope
// for a cleaner run (id, model, created, choices, usage, system_fingerprint,
// etc.), not just the message content the worker parses.
import { readFileSync } from "node:fs";
import {
  aiGatewayChatCompletion,
} from "../../pi/recipe-worker.mjs";
import { cleanerPrompt, IMPROVED_SYSTEM_PROMPT } from "../gemma-cleaner/cleaner_prompt.mjs";
import { EXAMPLES } from "../gemma-cleaner/examples.mjs";

function loadEnv(path) {
  try {
    for (const line of readFileSync(path, "utf8").split("\n")) {
      const m = line.match(/^\s*([A-Z0-9_]+)\s*=\s*(.*)\s*$/i);
      if (m && !process.env[m[1]]) {
        let v = m[2];
        if ((v.startsWith('"') && v.endsWith('"')) || (v.startsWith("'") && v.endsWith("'"))) v = v.slice(1, -1);
        process.env[m[1]] = v;
      }
    }
  } catch {}
}
loadEnv(".env");
loadEnv("../../.env");

const KEY = process.env.AI_GATEWAY_API_KEY || "";
const BASE = process.env.AI_GATEWAY_BASE_URL?.trim() || "https://ai-gateway.vercel.sh/v1";
const MODEL = process.env.MODEL_B?.trim() || process.env.MODEL?.trim() || "poolside/laguna-s-2.1-free";
const SYSTEM_PROMPT = IMPROVED_SYSTEM_PROMPT;
const OPTIONS = { reasoning: { effort: "none" }, maxTokens: 2048 };

const example = EXAMPLES.find((e) => e.id === (process.env.EXAMPLE || "ig-post-DZNQT3Pt3Ja")) || EXAMPLES[1];
const data = await aiGatewayChatCompletion(
  BASE, KEY, MODEL, SYSTEM_PROMPT, cleanerPrompt(example.evidence), undefined, OPTIONS,
);
console.log("MODEL:", MODEL, "EXAMPLE:", example.id);
console.log(JSON.stringify(data, null, 2));
