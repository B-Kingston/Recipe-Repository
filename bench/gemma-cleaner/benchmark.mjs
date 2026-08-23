// Benchmark + before/after evaluation for the Laguna Vercel AI Gateway media
// cleaner (the text-cleaning step used by "From Video" imports).
//
// Pipeline mocked here:
//   extract_social_evidence()  -> MediaEvidence (title/desc/transcript/ocr)
//   [mocked: yt-dlp/ffmpeg/whisper/tesseract not installed]
//   cleaner_prompt(evidence)   -> user prompt  (ported 1:1 from src/media.rs)
//   cleanMedia() / aiGatewayChatCompletion()  -> REAL Laguna call via the
//                                                  actual worker code
//   parseAiGatewayCleaner()   -> cleaned_recipe_text
//
// We exercise the real worker path for the LLM call, vary the cleaner
// configuration, and measure: latency, prompt/completion/total tokens,
// reasoning tokens, upstream cost, and a recipe-fact quality score. Then we
// print a before/after comparison for each example.

import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import {
  aiGatewayChatCompletion,
  parseAiGatewayCleaner,
} from "../../pi/recipe-worker.mjs";
import { cleanerPrompt, PRODUCTION_SYSTEM_PROMPT, IMPROVED_SYSTEM_PROMPT } from "./cleaner_prompt.mjs";
import { EXAMPLES } from "./examples.mjs";

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

const KEY = process.env.AI_GATEWAY_API_KEY || "";
const BASE = process.env.AI_GATEWAY_BASE_URL?.trim() || "https://ai-gateway.vercel.sh/v1";
const MODEL =
  process.env.AI_GATEWAY_CLEANER_MODEL?.trim() || "poolside/laguna-s-2.1-free";

if (!KEY) {
  console.error("AI_GATEWAY_API_KEY not set in .env");
  process.exit(1);
}

// Variant definitions. `reasoning` -> {effort:string} | null(omit).
const VARIANTS = [
  {
    id: "baseline",
    label: "Baseline (low reasoning, 2048 tok)",
    systemPrompt: PRODUCTION_SYSTEM_PROMPT,
    options: { reasoning: { effort: "low" }, maxTokens: 2048 },
  },
  {
    id: "reasoning_off",
    label: "Reasoning OFF (production prompt)",
    systemPrompt: PRODUCTION_SYSTEM_PROMPT,
    options: { reasoning: { effort: "none" }, maxTokens: 2048 },
  },
  {
    id: "improved",
    label: "Improved prompt + reasoning OFF + temp 0",
    systemPrompt: IMPROVED_SYSTEM_PROMPT,
    options: { reasoning: { effort: "none" }, maxTokens: 2048, temperature: 0 },
  },
];

// ---- quality evaluation helpers -------------------------------------------

const FILLER = [
  "hey guys", "welcome back", "like and subscribe", "hit that like", "follow",
  "subscribe", "link in bio", "link in my bio", "my blog", "save this", "tag me",
  "don't forget", "bye guys", "what's up everyone", "check the link", "full recipe",
  "#", "story", "printable recipe", "comment", "notification",
];

function fillerCount(text) {
  const t = text.toLowerCase();
  return FILLER.reduce((n, f) => (t.includes(f) ? n + 1 : n), 0);
}

// Known recipe facts we expect the cleaner to preserve (for scoring only).
function knownAmounts(ev) {
  const join = (arr) => arr.join(" ");
  const ocr = ev.ocr.map((o) => o.text).join(" ");
  return join([ev.description, ev.audio_transcript, ocr]).toLowerCase();
}

function scoreCleaned(cleaned, evidence) {
  // Parse the cleaned text back into structured facts.
  const ingredients = [...cleaned.matchAll(/^- (.+)$/gm)].map((m) => m[1].toLowerCase());
  const steps = [...cleaned.matchAll(/^\d+\. (.+)$/gm)].map((m) => m[1].toLowerCase());
  const corpus = knownAmounts(evidence);

  // How many of the cleaner's ingredient lines contain a digit/unit amount?
  const amountRe = /\d|cup|tbsp|tsp|clove|handful|banana|egg|peanut|chicken|cream|parmesan|tomato|spinach|butter|garlic|min|°c|degree/i;
  const ingredientLinesWithAmount = ingredients.filter((i) => amountRe.test(i)).length;

  // Filler that leaked into the cleaned output (should be 0).
  const leakedFiller = fillerCount(cleaned);

  // Did the cleaner preserve the dish title from the source?
  const titleKept = /pancake|tuscan|chicken pasta/i.test(cleaned);

  return {
    ingredientLines: ingredients.length,
    stepLines: steps.length,
    ingredientLinesWithAmount,
    leakedFiller,
    titleKept,
    structureOk: /ingredients:/i.test(cleaned) && /method:/i.test(cleaned),
  };
}

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

// The production worker hardcodes AI_GATEWAY_CLEANER_TIMEOUT_MS = 180_000.
// To capture the low-reasoning quality data point with a generous ceiling,
// sample it here with an extended timeout.
async function directAiGateway(prompt, systemPrompt, timeoutMs) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const res = await fetch(`${BASE.replace(/\/+$/, "")}/chat/completions`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: `Bearer ${KEY}`,
      },
      body: JSON.stringify({
        model: MODEL,
        messages: [
          { role: "system", content: systemPrompt },
          { role: "user", content: prompt },
        ],
        reasoning: { effort: "low" },
        stream: false,
        max_tokens: 2048,
      }),
      signal: controller.signal,
    });
    return await res.json();
  } finally {
    clearTimeout(timer);
  }
}

// ---- run ------------------------------------------------------------------

async function callVariant(variant, prompt) {
  if (variant.id === "baseline") {
    // Sampled with an extended timeout (production would abort at 180s).
    return directAiGateway(prompt, variant.systemPrompt, 540_000);
  }
  return aiGatewayChatCompletion(
    BASE, KEY, MODEL, variant.systemPrompt, prompt, undefined, variant.options,
  );
}

async function runOne(variant, example, attempt = 0) {
  const evidence = example.evidence;
  const prompt = cleanerPrompt(evidence); // exact production prompt builder
  const started = Date.now();
  let data, rawText, cleaned, error;
  try {
    data = await callVariant(variant, prompt);
    rawText = data?.choices?.[0]?.message?.content ?? "";
    cleaned = parseAiGatewayCleaner(data); // real worker parse + format
  } catch (e) {
    const msg = e?.code ? `${e.code}: ${e.message}` : String(e);
    // Retry once on transient free-tier glitches (e.g. empty/non-JSON body).
    if (attempt === 0 && !msg.startsWith("configuration")) {
      await sleep(4000);
      return runOne(variant, example, attempt + 1);
    }
    error = msg;
  }
  const ms = Date.now() - started;
  const usage = data?.usage ?? {};
  const upstreamCost = usage.cost_details?.upstream_inference_cost ?? usage.cost ?? 0;
  const reasoningTokens = usage.completion_tokens_details?.reasoning_tokens ?? 0;

  const score = cleaned ? scoreCleaned(cleaned, evidence) : null;
  return {
    variant: variant.id,
    example: example.id,
    latencyMs: ms,
    promptTokens: usage.prompt_tokens ?? 0,
    completionTokens: usage.completion_tokens ?? 0,
    totalTokens: usage.total_tokens ?? 0,
    reasoningTokens,
    upstreamCost,
    model: data?.model ?? MODEL,
    error: error ?? null,
    rawText: error ? null : rawText,
    cleaned: error ? null : cleaned,
    score,
  };
}

async function main() {
  const results = [];
  const out = [];
  const skipBaseline = process.env.SKIP_BASELINE === "1";
  for (const variant of VARIANTS) {
    if (skipBaseline && variant.id === "baseline") continue;
    for (const example of EXAMPLES) {
      process.stdout.write(`running ${variant.id} x ${example.id} ... `);
      const r = await runOne(variant, example);
      if (r.error) {
        console.log(`ERROR ${r.error}`);
      } else {
        console.log(
          `ok ${r.latencyMs}ms tok=${r.totalTokens} (reasoning ${r.reasoningTokens}) cost=$${r.upstreamCost.toFixed(6)}`,
        );
      }
      results.push(r);
      await sleep(2500); // be gentle with the free-tier rate limit
    }
  }

  // Persist raw results for later diffing.
  mkdirSync("bench/gemma-cleaner/out", { recursive: true });
  writeFileSync("bench/gemma-cleaner/out/results.json", JSON.stringify(results, null, 2));

  // ---- human report ------------------------------------------------------
  out.push("# Laguna Media-Cleaner Benchmark\n");
  out.push(`Model: \`${MODEL}\`  |  Date: ${new Date().toISOString()}\n`);

  out.push(
    "\n> Note: the production worker hardcodes `AI_GATEWAY_CLEANER_TIMEOUT_MS = 180_000`. " +
    "The low-reasoning baseline is sampled with an extended timeout, while " +
    "`reasoning_off` uses the production-shaped prompt with reasoning disabled.\n",
  );

  // Metrics summary table.
  out.push("\n## Throughput & cost (per example)\n");
  out.push("| variant | example | latency(ms) | prompt tok | completion tok | reasoning tok | total tok | upstream $ |");
  out.push("|---|---|---:|---:|---:|---:|---:|---:|");
  for (const r of results) {
    out.push(
      `| ${r.variant} | ${r.example} | ${r.latencyMs} | ${r.promptTokens} | ${r.completionTokens} | ${r.reasoningTokens} | ${r.totalTokens} | ${r.upstreamCost.toFixed(6)} |`,
    );
  }

  // Quality summary table.
  out.push("\n## Cleaning quality\n");
  out.push("| variant | example | struct | title | ingr lines | step lines | ingr w/ amount | leaked filler |");
  out.push("|---|---|---|---|---:|---:|---:|---:|");
  for (const r of results) {
    const s = r.score;
    if (!s) { out.push(`| ${r.variant} | ${r.example} | ERR | - | - | - | - | - |`); continue; }
    out.push(
      `| ${r.variant} | ${r.example} | ${s.structureOk ? "ok" : "BAD"} | ${s.titleKept ? "yes" : "no"} | ${s.ingredientLines} | ${s.stepLines} | ${s.ingredientLinesWithAmount}/${s.ingredientLines} | ${s.leakedFiller} |`,
    );
  }

  // Per-example before/after.
  for (const example of EXAMPLES) {
    out.push(`\n## ${example.label}\n`);
    out.push(`URL: ${example.url}\n`);
    const ev = example.evidence;
    out.push("### BEFORE (raw extracted evidence)\n");
    out.push("**Title:** " + ev.title);
    out.push("\n**Description:** " + ev.description);
    out.push("\n**Audio transcript (Whisper):** " + ev.audio_transcript);
    out.push("\n**OCR:** " + ev.ocr.map((o) => `[${o.timestamp_seconds}s] ${o.text}`).join("  "));
    out.push("");
    for (const variant of VARIANTS) {
      const r = results.find((x) => x.variant === variant.id && x.example === example.id);
      if (!r) continue;
      out.push(`\n#### AFTER — ${variant.label}\n`);
      if (r.error) {
        out.push(`> ERROR: ${r.error}\n`);
      } else {
        out.push("```\n" + r.cleaned + "\n```\n");
      }
    }
  }

  const report = out.join("\n");
  writeFileSync("bench/gemma-cleaner/out/report.md", report);
  console.log("\nWrote bench/gemma-cleaner/out/results.json and report.md");
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
