// Renders bench/gemma-cleaner/out/report.md from a results.json + examples.
// Kept separate from benchmark.mjs so a hand-assembled results file (e.g. one
// that merges a slow/flaky baseline) can still produce a report.
import { readFileSync, writeFileSync } from "node:fs";
import { EXAMPLES } from "./examples.mjs";

const results = JSON.parse(readFileSync("bench/gemma-cleaner/out/results.json", "utf8"));

const FILLER = [
  "hey guys", "welcome back", "like and subscribe", "hit that like", "follow",
  "subscribe", "link in bio", "link in my bio", "my blog", "save this", "tag me",
  "don't forget", "bye guys", "what's up everyone", "check the link", "full recipe",
  "#", "story", "printable recipe", "comment", "notification",
];
const fillerCount = (t) => FILLER.reduce((n, f) => (t.toLowerCase().includes(f) ? n + 1 : n), 0);

const out = [];
out.push("# Laguna Media-Cleaner Benchmark\n");
const model = results[0]?.model || "poolside/laguna-s-2.1-free";
out.push(`Model: \`${model}\`  |  Date: ${new Date().toISOString()}\n`);
out.push(
  "\n> Note: the production worker hardcodes `AI_GATEWAY_CLEANER_TIMEOUT_MS = 180_000`. " +
  "The low-reasoning baseline is sampled with an extended timeout, while " +
  "`reasoning_off` uses the production-shaped prompt with reasoning disabled.\n",
);

out.push("\n## Throughput & cost (per example)\n");
out.push("| variant | example | latency(ms) | prompt tok | completion tok | reasoning tok | total tok | upstream $ |");
out.push("|---|---|---:|---:|---:|---:|---:|---:|");
for (const r of results) {
  out.push(
    `| ${r.variant} | ${r.example} | ${r.latencyMs} | ${r.promptTokens} | ${r.completionTokens} | ${r.reasoningTokens} | ${r.totalTokens} | ${r.upstreamCost.toFixed(6)} |`,
  );
}

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
  // Preserve the VARIANTS order if present in results.
  const variantsHere = [...new Set(results.map((r) => r.variant))];
  for (const vid of variantsHere) {
    const r = results.find((x) => x.variant === vid && x.example === example.id);
    if (!r) continue;
    const label = vid;
    out.push(`\n#### AFTER — ${label}\n`);
    if (r.error) {
      out.push(`> ERROR: ${r.error}\n`);
    } else {
      out.push("```\n" + r.cleaned + "\n```\n");
    }
  }
}

const report = out.join("\n");
writeFileSync("bench/gemma-cleaner/out/report.md", report);
console.log("Wrote bench/gemma-cleaner/out/report.md");
