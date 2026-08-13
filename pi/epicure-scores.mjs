import { readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

/**
 * Local port of the KAIKAKU-AI epicure-mcp ingredient model (MIT license,
 * data bundled under ./epicure-data/). Resolution order is a faithful port of
 * the upstream matcher.py (exact → modifier-stripped exact → word-boundary
 * substring → stripped substring → word overlap), plus one added final step:
 * stemming the query before a second word-overlap pass, so LLM text like
 * "tomatoes" reaches the "tomato" node. Scoring is the upstream pairing_score:
 * cosine (dot product on the unit-normalized 300-dim rows), reported as the
 * percentile of each pair within each ingredient's own affinity distribution.
 */

export const EPICURE_DATA_DIR = fileURLToPath(new URL("./epicure-data/", import.meta.url));

const DIM = 300;
export const EXPECTED_ROWS = 1790;

export const FLAG_PERCENTILE = 20;
export const MAX_FLAGGED_PAIRS = 5;
export const MIN_RESOLVED = 2;
export const MIN_RESOLVED_FOR_WEAKEST = 3;

/** Modifier vocabulary copied verbatim from epicure-mcp src/epicure_mcp/matcher.py. */
export const MODIFIERS = new Set([
  "ground",
  "fresh",
  "dried",
  "smoked",
  "raw",
  "whole",
  "chopped",
  "minced",
  "sliced",
  "diced",
  "crushed",
  "grated",
  "shredded",
  "frozen",
  "canned",
  "roasted",
  "toasted",
  "blanched",
  "steamed",
  "fried",
  "grilled",
  "baked",
  "boiled",
  "poached",
  "braised",
  "pickled",
  "fermented",
  "unsalted",
  "salted",
  "sweetened",
  "unsweetened",
  "organic",
  "boneless",
  "skinless",
  "lean",
  "light",
  "dark",
  "baby",
  "young",
  "aged",
  "powdered",
  "flaked",
  "dehydrated",
  "concentrated",
  "hot",
  "warm",
  "cold",
  "plain",
  "natural",
  "pure",
  "cooked",
  "uncooked",
  "prepared",
]);

export function normalize(text) {
  return String(text)
    .toLowerCase()
    .replace(/_/g, " ")
    .replace(/-/g, " ")
    .replace(/\s+/g, " ")
    .trim();
}

/** Singularization fallback: tomatoes → tomato, olives → olive, cherries →
 *  cherry, dishes → dish. The es-drop only fires after a sibilant (ss/sh/ch/
 *  x/z) so "olives" becomes "olive", never "oliv", and "fish" is untouched. */
export function stem(word) {
  const w = String(word).trim();
  if (w.length > 4 && w.endsWith("ies")) return `${w.slice(0, -3)}y`;
  if (w.length > 4 && w.endsWith("oes")) return w.slice(0, -2);
  if (w.length > 3 && /(ss|sh|ch|x|z)es$/.test(w)) return w.slice(0, -2);
  if (
    w.length > 3
    && w.endsWith("s")
    && !w.endsWith("ss")
    && !w.endsWith("sh")
    && !w.endsWith("ch")
    && !w.endsWith("x")
    && !w.endsWith("z")
  ) {
    return w.slice(0, -1);
  }
  return w;
}

function stripModifiers(text) {
  const words = text.split(" ").filter(Boolean);
  const kept = words.filter((word) => !MODIFIERS.has(word));
  return kept.length > 0 ? kept.join(" ") : text;
}

function round1(value) {
  return Math.round(value * 10) / 10;
}

/** Minimal quote-aware CSV line parser (pandas to_csv quoting). */
function parseCsvLine(line) {
  const fields = [];
  let field = "";
  let inQuotes = false;
  for (let i = 0; i < line.length; i += 1) {
    const ch = line[i];
    if (inQuotes) {
      if (ch === '"') {
        if (line[i + 1] === '"') {
          field += '"';
          i += 1;
        } else {
          inQuotes = false;
        }
      } else {
        field += ch;
      }
    } else if (ch === '"') {
      inQuotes = true;
    } else if (ch === ",") {
      fields.push(field);
      field = "";
    } else {
      field += ch;
    }
  }
  fields.push(field);
  return fields;
}

let cached = null;

/**
 * Lazy singleton loader for the bundled epicure data. Throws when a file is
 * missing or the shape is wrong so a corrupted bundle fails loudly (the
 * critique pass then skips rather than emitting garbage).
 */
export function loadEpicure(dir = EPICURE_DATA_DIR) {
  if (cached) return cached;

  const embLines = readFileSync(join(dir, "embeddings.csv"), "utf8").split(/\r?\n/).filter((line) => line.trim() !== "");
  const ingLines = readFileSync(join(dir, "ingredient_list.csv"), "utf8").split(/\r?\n/).filter((line) => line.trim() !== "");
  const conLines = readFileSync(join(dir, "consolidated_nodes.csv"), "utf8").split(/\r?\n/).filter((line) => line.trim() !== "");

  const header = parseCsvLine(embLines[0]);
  if (header.length !== DIM + 1 || header[0] !== "node_id") {
    throw new Error(`epicure embeddings: unexpected header with ${header.length} columns`);
  }

  const nodeIds = [];
  const embeddings = new Float32Array((embLines.length - 1) * DIM);
  for (let r = 1; r < embLines.length; r += 1) {
    const parts = parseCsvLine(embLines[r]);
    const nodeId = Number(parts[0]);
    if (!Number.isInteger(nodeId) || parts.length !== DIM + 1) {
      throw new Error(`epicure embeddings: malformed row ${r}`);
    }
    nodeIds.push(nodeId);
    const offset = (r - 1) * DIM;
    let normSq = 0;
    for (let d = 0; d < DIM; d += 1) {
      const value = Number(parts[d + 1]);
      embeddings[offset + d] = value;
      normSq += value * value;
    }
    const norm = Math.sqrt(normSq);
    if (norm > 0) {
      for (let d = 0; d < DIM; d += 1) embeddings[offset + d] /= norm;
    }
  }
  if (nodeIds.length !== EXPECTED_ROWS) {
    throw new Error(`epicure embeddings: expected ${EXPECTED_ROWS} rows, got ${nodeIds.length}`);
  }

  const rowByNodeId = new Map();
  nodeIds.forEach((nodeId, index) => rowByNodeId.set(nodeId, index));

  const nameByNodeId = new Map();
  const nodeIdByExactName = new Map();
  for (let r = 1; r < ingLines.length; r += 1) {
    const parts = parseCsvLine(ingLines[r]);
    const nodeId = Number(parts[0]);
    const name = parts[1];
    if (!Number.isInteger(nodeId) || !name || !name.trim()) {
      throw new Error(`epicure ingredient list: malformed row ${r}`);
    }
    nameByNodeId.set(nodeId, name);
    nodeIdByExactName.set(normalize(name), nodeId);
  }
  if (nameByNodeId.size !== EXPECTED_ROWS) {
    throw new Error(`epicure ingredient list: expected ${EXPECTED_ROWS} rows, got ${nameByNodeId.size}`);
  }

  // Combined lookup in upstream order: canonical names first, then alias
  // variants (first key wins, mirroring matcher.py's "if key not in lookup").
  const lookup = new Map();
  for (const [key, nodeId] of nodeIdByExactName) {
    lookup.set(key, { nodeId, name: nameByNodeId.get(nodeId) });
  }
  const aliases = new Map();
  for (let r = 1; r < conLines.length; r += 1) {
    const parts = parseCsvLine(conLines[r]);
    const nodeId = Number(parts[0]);
    const finalName = parts[1];
    const rawNames = parts[3];
    if (!Number.isInteger(nodeId)) {
      throw new Error(`epicure consolidated nodes: malformed row ${r}`);
    }
    const variants = rawNames.match(/'(?:[^'\\]|\\.)*'/g) || [];
    for (const variant of variants) {
      const key = normalize(variant.slice(1, -1));
      if (key && !lookup.has(key)) {
        lookup.set(key, { nodeId, name: finalName || nameByNodeId.get(nodeId) });
        aliases.set(key, nodeId);
      }
    }
  }

  cached = { embeddings, nodeIds, rowByNodeId, nameByNodeId, nodeIdByExactName, aliases, lookup };
  return cached;
}

function substringMatch(query, lookup) {
  const qWords = query.split(" ").filter(Boolean);
  let best = null;
  for (const [key, entry] of lookup) {
    const kWords = key.split(" ").filter(Boolean);
    if (kWords.length >= qWords.length || !kWords.every((word) => qWords.includes(word))) continue;
    const leftover = qWords.filter((word) => !kWords.includes(word));
    if (!leftover.every((word) => MODIFIERS.has(word))) continue;
    if (!best || kWords.length > best.kWords.length) best = { kWords, entry };
  }
  if (!best) return null;
  return { nodeId: best.entry.nodeId, name: best.entry.name };
}

function wordOverlapMatch(query, lookup) {
  const qWords = query.split(" ").filter(Boolean);
  const qSet = new Set(qWords);
  let best = null;
  for (const [key, entry] of lookup) {
    const kWords = key.split(" ").filter(Boolean);
    const kSet = new Set(kWords);
    const common = [...qSet].filter((word) => kSet.has(word));
    const meaningful = common.filter((word) => !MODIFIERS.has(word));
    if (meaningful.length === 0) continue;
    if (meaningful.length < 2 && (qWords.length > 1 || kWords.length > 1)) continue;
    const ratio = Math.min(common.length / qWords.length, common.length / kWords.length);
    if (!best || ratio > best.ratio) best = { ratio, entry };
  }
  if (!best) return null;
  return { nodeId: best.entry.nodeId, name: best.entry.name };
}

/**
 * Resolves a recipe-ingredient name to an epicure node, following the
 * upstream matcher order: exact → modifier-stripped exact → word-boundary
 * substring → stripped substring → word overlap → stemmed word overlap.
 * Returns { nodeId, name } with the canonical name, or null.
 */
export function resolveIngredient(name, data) {
  if (typeof name !== "string" || name.trim() === "") return null;
  const q = normalize(name);
  const exact = data.lookup.get(q);
  if (exact) return { nodeId: exact.nodeId, name: exact.name };

  const stripped = stripModifiers(q);
  if (stripped !== q) {
    const hit = data.lookup.get(stripped);
    if (hit) return { nodeId: hit.nodeId, name: hit.name };
  }

  const sub = substringMatch(q, data.lookup);
  if (sub) return sub;

  if (stripped !== q) {
    const subStripped = substringMatch(stripped, data.lookup);
    if (subStripped) return subStripped;
  }

  const overlap = wordOverlapMatch(q, data.lookup);
  if (overlap) return overlap;

  const stemmed = q.split(" ").map(stem).join(" ");
  if (stemmed !== q) {
    const stemOverlap = wordOverlapMatch(stemmed, data.lookup);
    if (stemOverlap) return stemOverlap;
  }

  return null;
}

function dot(embeddings, offsetA, offsetB) {
  let sum = 0;
  for (let d = 0; d < DIM; d += 1) sum += embeddings[offsetA + d] * embeddings[offsetB + d];
  return sum;
}

function percentileInRow(row, selfIndex, affinity, count) {
  let atMost = 0;
  for (let k = 0; k < count; k += 1) {
    if (k !== selfIndex && row[k] <= affinity) atMost += 1;
  }
  return (atMost / (count - 1)) * 100;
}

/**
 * Scores every resolved ingredient pair of a recipe against the bundled
 * model. Returns the critique object, or null when fewer than MIN_RESOLVED
 * distinct ingredients match. Percentile = the pair's rank (0-100) within
 * each ingredient's own affinity distribution; the lower of the two is
 * reported so a hub ingredient cannot mask a weak relationship.
 */
export function pairwiseCritique(recipe, data) {
  const ingredients = Array.isArray(recipe?.ingredients) ? recipe.ingredients : [];
  const total = ingredients.length;

  const resolvedNames = new Map(); // nodeId → canonical name (first wins)
  const unresolved = [];
  for (const ingredient of ingredients) {
    const name = typeof ingredient?.name === "string" ? ingredient.name : "";
    if (name.trim() === "") {
      unresolved.push(name);
      continue;
    }
    const match = resolveIngredient(name, data);
    if (match) {
      if (!resolvedNames.has(match.nodeId)) resolvedNames.set(match.nodeId, match.name);
    } else {
      unresolved.push(name.trim());
    }
  }
  const nodeIds = [...resolvedNames.keys()];
  if (nodeIds.length < MIN_RESOLVED) return null;
  const pairCount = (nodeIds.length * (nodeIds.length - 1)) / 2;

  const rowIndexes = nodeIds.map((nodeId) => data.rowByNodeId.get(nodeId));
  const totalNodes = data.nodeIds.length;
  // Full affinity rows for the resolved ingredients only (dot with every node).
  const affinityRows = rowIndexes.map((index) => {
    const row = new Float32Array(totalNodes);
    for (let k = 0; k < totalNodes; k += 1) row[k] = dot(data.embeddings, index * DIM, k * DIM);
    return row;
  });

  const pairs = [];
  for (let i = 0; i < nodeIds.length; i += 1) {
    for (let j = i + 1; j < nodeIds.length; j += 1) {
      const affinity = affinityRows[i][rowIndexes[j]];
      const pctA = percentileInRow(affinityRows[i], rowIndexes[i], affinity, totalNodes);
      const pctB = percentileInRow(affinityRows[j], rowIndexes[j], affinity, totalNodes);
      pairs.push({
        a: resolvedNames.get(nodeIds[i]),
        b: resolvedNames.get(nodeIds[j]),
        i,
        j,
        percentile: Math.min(pctA, pctB),
      });
    }
  }

  const coherence = pairs.reduce((sum, pair) => sum + pair.percentile, 0) / pairs.length;
  const weakestPairs = pairs
    .filter((pair) => pair.percentile < FLAG_PERCENTILE)
    .sort((x, y) => x.percentile - y.percentile)
    .slice(0, MAX_FLAGGED_PAIRS)
    .map((pair) => ({ a: pair.a, b: pair.b, percentile: round1(pair.percentile) }));

  let weakestIngredient = null;
  if (nodeIds.length >= MIN_RESOLVED_FOR_WEAKEST) {
    let bestMean = Infinity;
    let bestNode = null;
    for (let n = 0; n < nodeIds.length; n += 1) {
      let sum = 0;
      let count = 0;
      for (const pair of pairs) {
        if (pair.i === n || pair.j === n) {
          sum += pair.percentile;
          count += 1;
        }
      }
      const mean = count > 0 ? sum / count : Infinity;
      if (mean < bestMean) {
        bestMean = mean;
        bestNode = nodeIds[n];
      }
    }
    if (bestNode !== null) {
      weakestIngredient = { name: resolvedNames.get(bestNode), meanPercentile: round1(bestMean) };
    }
  }

  return {
    total,
    resolved: nodeIds.length,
    unresolved,
    pairCount,
    coherencePercentile: round1(coherence),
    weakestPairs,
    weakestIngredient,
  };
}

/** Set diff of ingredient names keyed by trimmed lowercase; names keep the
 *  casing of the recipe they come from. */
export function ingredientDiff(beforeRecipe, afterRecipe) {
  const namesOf = (recipe) => (Array.isArray(recipe?.ingredients) ? recipe.ingredients : [])
    .map((ingredient) => (typeof ingredient?.name === "string" ? ingredient.name.trim() : ""))
    .filter(Boolean);
  const key = (name) => name.trim().toLowerCase();
  const before = namesOf(beforeRecipe);
  const after = namesOf(afterRecipe);
  const beforeKeys = new Set(before.map(key));
  const afterKeys = new Set(after.map(key));
  const added = after.filter((name) => !beforeKeys.has(key(name)));
  const removed = before.filter((name) => !afterKeys.has(key(name)));
  return { added, removed };
}

/** The deterministic turn-2 user message built from a critique object. */
export function critiqueMessage(critique) {
  const lines = [];
  lines.push(
    "A bundled ingredient-embedding model scored every ingredient pairing in your draft above by flavour affinity, ranking each pair within each ingredient's own affinity distribution and taking the lower of the two percentiles.",
  );
  const unmatched = critique.unresolved.length > 0 ? ` (unmatched: ${critique.unresolved.join(", ")})` : "";
  lines.push(`Scored ${critique.resolved} of ${critique.total} ingredients (${critique.pairCount} pairs)${unmatched}.`);
  lines.push(`Overall coherence: ${critique.coherencePercentile}th percentile.`);
  if (critique.weakestPairs.length > 0) {
    lines.push("Weakest pairings:");
    for (const pair of critique.weakestPairs) {
      lines.push(`- ${pair.a} — ${pair.b} (${pair.percentile}th percentile)`);
    }
  } else {
    lines.push("Weakest pairings: none below the flagging threshold.");
  }
  if (critique.weakestIngredient) {
    lines.push(
      `Most weakly paired ingredient: ${critique.weakestIngredient.name} (mean ${critique.weakestIngredient.meanPercentile}th percentile).`,
    );
  }
  lines.push("");
  lines.push(
    "For each weak pairing you agree with, either swap one side for a better-matched ingredient or keep it as an intentional contrast. Do not contort the recipe to maximise scores — a pair that works despite low affinity is a feature, not a flaw. Change only what you agree is weak.",
  );
  lines.push("");
  lines.push(
    "Return the complete revised recipe as a single JSON object in the same schema as your draft above (title, description, prepMinutes, cookMinutes, servings, ingredients with name/quantity/unit/optional, steps with text/chartLabel/timerSeconds/ingredientUses/inputSteps), with an empty sources array. No prose, no Markdown fences.",
  );
  return lines.join("\n");
}
