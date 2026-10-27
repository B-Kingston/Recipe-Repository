/**
 * Runs the Ask end-to-end corpus against a live deployment. See README.md.
 *
 *   node e2e/ask/run.ts                 # every case the configuration allows
 *   E2E_CASES=plain-question,deny node e2e/ask/run.ts
 *
 * Without credentials only the signed-out checks run. Credentials come from
 * the environment or a file (E2E_CREDENTIALS_FILE) and are never printed.
 */
import { readFileSync, writeFileSync } from "node:fs";
import process from "node:process";
import { CASES, CaseFailure, CaseSkipped, TEST_RECIPE, signedOutChecks } from "./cases.ts";
import type { Context, Settings } from "./cases.ts";
import { Client, readChat, readRecipe, text } from "./lib.ts";

interface Credentials {
  username?: string;
  password?: string;
  username2?: string;
  password2?: string;
}

/** Credentials from E2E_CREDENTIALS_FILE (JSON, or KEY=VALUE lines with the
 * same names as the variables), with the environment taking precedence. */
function credentials(): Credentials {
  const fromFile: Record<string, string> = {};
  const path = process.env.E2E_CREDENTIALS_FILE;
  if (path) {
    const raw = readFileSync(path, "utf8").trim();
    if (raw.startsWith("{")) {
      const json = JSON.parse(raw) as Record<string, unknown>;
      for (const [key, value] of Object.entries(json)) if (typeof value === "string") fromFile[key] = value;
    } else {
      for (const line of raw.split("\n")) {
        const match = line.match(/^\s*(?:export\s+)?([A-Z0-9_]+)\s*=\s*(.*?)\s*$/);
        if (match) fromFile[match[1]] = match[2].replace(/^(['"])(.*)\1$/, "$2");
      }
    }
  }
  const pick = (...names: string[]) => names.map((name) => process.env[name] || fromFile[name]).find(Boolean);
  return {
    username: pick("E2E_USERNAME", "username"),
    password: pick("E2E_PASSWORD", "password"),
    username2: pick("E2E_USERNAME_2", "username2"),
    password2: pick("E2E_PASSWORD_2", "password2"),
  };
}

function flag(name: string, fallback: boolean): boolean {
  const value = process.env[name];
  return value === undefined || value === "" ? fallback : !/^(0|false|no|off)$/i.test(value);
}

function list(name: string): string[] {
  return (process.env[name] ?? "").split(",").map((entry) => entry.trim()).filter(Boolean);
}

interface Outcome {
  name: string;
  result: "pass" | "fail" | "skip";
  detail: string;
  warnings: string[];
  seconds: number;
}

/** Makes the throwaway recipe: one generation (the only way to add a recipe),
 * then rewritten by hand into TEST_RECIPE, so the cases know its content. */
async function makeRecipe(owner: Client, settings: Settings): Promise<string> {
  const scratch = process.env.E2E_SCRATCH_RECIPE_ID;
  let id = scratch ?? "";
  if (!id) {
    const prompt = `A plain shakshuka for 4 with eggs, butter, onion, garlic, tomatoes, and salt. Title it "E2E Ask ${settings.runId}". Keep it short.`;
    const generated = await owner.post("/ai/generate", { prompt }, { timeoutMs: settings.generateTimeoutMs });
    if (generated.status !== 303 || !generated.location.startsWith("/ai/drafts/")) {
      throw new Error(`generating the test recipe answered ${generated.status}: ${text(generated.body).slice(0, 300)}`);
    }
    const applied = await owner.post(`${generated.location}/apply`, {});
    const match = applied.location.match(/^\/recipes\/([^/#?]+)/);
    if (applied.status !== 303 || !match) throw new Error(`saving the test recipe answered ${applied.status} → ${applied.location}`);
    id = match[1];
  }
  const path = `/recipes/${id}`;
  const before = readRecipe(await owner.page(path));
  for (const block of [...before.ingredients, ...before.steps]) {
    const reply = await owner.post(`${path}/blocks/${block.id}/delete`);
    if (reply.status !== 303) throw new Error(`clearing the test recipe answered ${reply.status}`);
  }
  const details = await owner.post(path, {
    title: `E2E Ask ${settings.runId}`,
    description: TEST_RECIPE.description,
    servings: TEST_RECIPE.servings,
    prep_minutes: TEST_RECIPE.prep,
    cook_minutes: TEST_RECIPE.cook,
  });
  if (details.status !== 303) throw new Error(`naming the test recipe answered ${details.status}`);
  for (const item of TEST_RECIPE.ingredients) {
    const form: Record<string, string> = { section: "ingredient", text: item.text, quantity: item.quantity, unit: item.unit };
    if ("optional" in item && item.optional) form.optional = "on";
    if ((await owner.post(`${path}/blocks`, form)).status !== 303) throw new Error("adding an ingredient failed");
  }
  for (const step of TEST_RECIPE.steps) {
    if ((await owner.post(`${path}/blocks`, { section: "step", text: step.text, step_ingredients: step.uses })).status !== 303) {
      throw new Error("adding a step failed");
    }
  }
  const made = readRecipe(await owner.page(path));
  if (made.ingredients.length !== TEST_RECIPE.ingredients.length || made.steps.length !== TEST_RECIPE.steps.length) {
    throw new Error("the test recipe did not come out as written");
  }
  return id;
}

function line(outcome: Outcome): string {
  const mark = { pass: "PASS", fail: "FAIL", skip: "SKIP" }[outcome.result];
  const warnings = outcome.warnings.map((warning) => `\n        warn: ${warning}`).join("");
  return `  ${mark}  ${outcome.name} (${outcome.seconds.toFixed(0)}s)${outcome.detail ? `\n        ${outcome.detail}` : ""}${warnings}`;
}

async function main(): Promise<number> {
  const base = process.env.E2E_BASE_URL || "http://minibuntu:3000";
  const settings: Settings = {
    answerTimeoutMs: Number(process.env.E2E_ANSWER_TIMEOUT_MS || 240_000),
    generateTimeoutMs: Number(process.env.E2E_GENERATE_TIMEOUT_MS || 360_000),
    expectSearch: flag("E2E_EXPECT_SEARCH", true),
    runId: `${new Date().toISOString().slice(0, 16).replace(/[-:T]/g, "")}-${Math.random().toString(36).slice(2, 6)}`,
  };
  console.log(`Ask e2e against ${base} (run ${settings.runId})`);

  let failures = 0;
  console.log("\nSigned out:");
  for (const result of await signedOutChecks(base)) {
    console.log(`  ${result.ok ? "PASS" : "FAIL"}  ${result.name}${result.ok ? "" : ` (${result.detail})`}`);
    if (!result.ok) failures += 1;
  }

  const login = credentials();
  if (!login.username || !login.password) {
    console.log("\nNo E2E_USERNAME / E2E_PASSWORD (or E2E_CREDENTIALS_FILE): the signed-in cases were not run.");
    return failures ? 1 : 0;
  }
  const owner = new Client(base, "owner");
  if (!(await owner.signIn(login.username, login.password))) {
    console.log("\nThe test account's sign-in was refused.");
    return 2;
  }
  let other: Client | null = null;
  if (login.username2 && login.password2) {
    other = new Client(base, "other");
    if (!(await other.signIn(login.username2, login.password2))) {
      console.log("\nThe second account's sign-in was refused.");
      await owner.signOut();
      return 2;
    }
  }

  const only = list("E2E_CASES");
  const skip = new Set(list("E2E_SKIP"));
  const unknown = only.filter((name) => !CASES.some((entry) => entry.name === name));
  if (unknown.length) {
    console.log(`\nUnknown case(s): ${unknown.join(", ")}. Known: ${CASES.map((entry) => entry.name).join(", ")}`);
    return 2;
  }
  const chosen = CASES.filter((entry) => (only.length === 0 ? !entry.optIn : only.includes(entry.name)) && !skip.has(entry.name));
  const outcomes: Outcome[] = [];
  let recipeId = "";
  let drawerWasOpen = false;
  try {
    const setupStarted = Date.now();
    recipeId = await makeRecipe(owner, settings);
    const first = readChat(await owner.page(`/recipes/${recipeId}`));
    drawerWasOpen = first.open;
    const opened = await owner.post(`/recipes/${recipeId}/chat/drawer`, { open: "on", back: `/recipes/${recipeId}` });
    if (opened.status === 404) throw new Error("the recipe chat is switched off for the test account (Settings → Recipe chat)");
    if (opened.status !== 303) throw new Error(`opening the drawer answered ${opened.status}`);
    console.log(`\nTest recipe /recipes/${recipeId} ready in ${((Date.now() - setupStarted) / 1000).toFixed(0)}s.\n\nCases:`);

    const context: Context = { owner, other, recipeId, settings, saved: new Map(), warn: () => {} };
    for (const entry of chosen) {
      const started = Date.now();
      const warnings: string[] = [];
      context.warn = (message) => warnings.push(message);
      let outcome: Outcome;
      try {
        if (entry.needsOther && !other) throw new CaseSkipped("needs a second account (E2E_USERNAME_2 / E2E_PASSWORD_2)");
        await entry.run(context);
        outcome = { name: entry.name, result: "pass", detail: "", warnings, seconds: 0 };
      } catch (error) {
        const skipped = error instanceof CaseSkipped;
        const message = error instanceof Error ? error.message : String(error);
        outcome = {
          name: entry.name,
          result: skipped ? "skip" : "fail",
          detail: error instanceof CaseFailure || skipped ? message : `error: ${message}`,
          warnings,
          seconds: 0,
        };
      }
      outcome.seconds = (Date.now() - started) / 1000;
      outcomes.push(outcome);
      console.log(line(outcome));
    }
  } catch (error) {
    console.log(`\nSetup failed: ${error instanceof Error ? error.message : String(error)}`);
    failures += 1;
  } finally {
    if (recipeId) {
      if (!drawerWasOpen) await owner.post(`/recipes/${recipeId}/chat/drawer`, { open: "off", back: `/recipes/${recipeId}` }).catch(() => undefined);
      if (flag("E2E_KEEP", false)) {
        console.log(`\nKept the test recipe: ${base.replace(/\/$/, "")}/recipes/${recipeId}`);
      } else {
        const deleted = await owner.post(`/recipes/${recipeId}/delete`).catch(() => null);
        const gone = (await owner.get(`/recipes/${recipeId}`).catch(() => null))?.status === 404;
        console.log(gone && deleted?.status === 303 ? "\nDeleted the test recipe (its chat, notes, and versions go with it)." : `\nCould not delete the test recipe /recipes/${recipeId}; delete it by hand.`);
        if (!gone) failures += 1;
      }
    }
    await owner.signOut().catch(() => undefined);
    await other?.signOut().catch(() => undefined);
  }

  const failed = outcomes.filter((outcome) => outcome.result === "fail").length;
  const counts = ["pass", "fail", "skip"].map((result) => `${outcomes.filter((outcome) => outcome.result === result).length} ${result}`).join(", ");
  const warned = outcomes.reduce((sum, outcome) => sum + outcome.warnings.length, 0);
  console.log(`\n${counts}${warned ? `, ${warned} warning(s)` : ""}.`);
  const report = process.env.E2E_REPORT;
  if (report) writeFileSync(report, `${JSON.stringify({ base, runId: settings.runId, outcomes }, null, 2)}\n`);
  return failures + failed > 0 ? 1 : 0;
}

main().then((code) => process.exit(code), (error: unknown) => {
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(2);
});
