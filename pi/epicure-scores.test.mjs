import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  FLAG_PERCENTILE,
  MAX_FLAGGED_PAIRS,
  critiqueMessage,
  ingredientDiff,
  loadEpicure,
  normalize,
  pairwiseCritique,
  resolveIngredient,
  stem,
} from "./epicure-scores.mjs";

const DIM = 300;

/** Builds a small in-memory data object shaped like loadEpicure's output.
 *  Five nodes in 300 dims (one-hot style):
 *  apple {0}, basil {0,1}, garlic {0,2}, duck {3}, egg {0,3}
 *  Every node overlaps with every other except duck, which only overlaps
 *  egg — so apple–duck is the unique percentile-0 pair. */
function fixture() {
  const nodeIds = [1, 2, 3, 4, 5];
  const rows = [
    { 0: 1 }, // apple
    { 0: 1, 1: 1 }, // basil
    { 0: 1, 2: 1 }, // garlic
    { 3: 1 }, // duck
    { 0: 1, 3: 1 }, // egg
  ];
  const embeddings = new Float32Array(rows.length * DIM);
  rows.forEach((row, r) => {
    for (const [d, value] of Object.entries(row)) embeddings[r * DIM + Number(d)] = value;
  });
  const names = ["apple", "basil", "garlic", "duck", "egg"];
  const lookup = new Map();
  const nameByNodeId = new Map();
  nodeIds.forEach((nodeId, i) => {
    nameByNodeId.set(nodeId, names[i]);
    lookup.set(names[i], { nodeId, name: names[i] });
  });
  lookup.set("sweet basil", { nodeId: 2, name: "basil" }); // alias vocabulary
  return {
    embeddings,
    nodeIds,
    rowByNodeId: new Map(nodeIds.map((nodeId, i) => [nodeId, i])),
    nameByNodeId,
    nodeIdByExactName: new Map(names.map((name, i) => [name, nodeIds[i]])),
    aliases: new Map([["sweet basil", 2]]),
    lookup,
  };
}

test("normalize lowercases, normalises separators, and trims", () => {
  assert.equal(normalize("  Smoked-Paprika_FLAKES  "), "smoked paprika flakes");
});

test("stem singularises common plurals and leaves other words alone", () => {
  assert.equal(stem("tomatoes"), "tomato");
  assert.equal(stem("potatoes"), "potato");
  assert.equal(stem("olives"), "olive");
  assert.equal(stem("apples"), "apple");
  assert.equal(stem("pears"), "pear");
  assert.equal(stem("cherries"), "cherry");
  assert.equal(stem("dishes"), "dish");
  assert.equal(stem("boxes"), "box");
  assert.equal(stem("fish"), "fish"); // sibilant: no blind s-drop
  assert.equal(stem("class"), "class"); // ends ss: no drop
  assert.equal(stem("paprika"), "paprika");
});

test("resolveIngredient matches exact names first", () => {
  assert.deepEqual(resolveIngredient("apple", fixture()), { nodeId: 1, name: "apple" });
});

test("resolveIngredient matches consolidation aliases", () => {
  assert.deepEqual(resolveIngredient("sweet basil", fixture()), { nodeId: 2, name: "basil" });
});

test("resolveIngredient strips modifiers before re-trying the vocabulary", () => {
  assert.deepEqual(resolveIngredient("smoked duck", fixture()), { nodeId: 4, name: "duck" });
});

test("resolveIngredient falls back to stemming for plurals", () => {
  assert.deepEqual(resolveIngredient("apples", fixture()), { nodeId: 1, name: "apple" });
});

test("resolveIngredient returns null for unknown names", () => {
  assert.equal(resolveIngredient("quantum foam", fixture()), null);
  assert.equal(resolveIngredient("", fixture()), null);
});

test("pairwiseCritique reports coverage, coherence, and the weakest ingredient", () => {
  // n=5 caps the lowest achievable percentile at 1/(n-1) = 25, which is at
  // the flagging threshold (strictly below 20), so this fixture cannot
  // produce flagged pairs — flagging is covered by the 12-node test below.
  const critique = pairwiseCritique(
    { ingredients: ["apple", "basil", "garlic", "duck", "egg"].map((name) => ({ name })) },
    fixture(),
  );
  assert.equal(critique.total, 5);
  assert.equal(critique.resolved, 5);
  assert.deepEqual(critique.unresolved, []);
  assert.equal(critique.pairCount, 10);
  assert.equal(critique.coherencePercentile, 77.5);
  assert.deepEqual(critique.weakestPairs, []);
  assert.deepEqual(critique.weakestIngredient, { name: "duck", meanPercentile: 43.8 });
});

test("pairwiseCritique keeps all pair percentiles within [0, 100]", () => {
  const data = fixture();
  for (let a = 0; a < 5; a += 1) {
    for (let b = a + 1; b < 5; b += 1) {
      const critique = pairwiseCritique(
        { ingredients: [{ name: ["apple", "basil", "garlic", "duck", "egg"][a] }, { name: ["apple", "basil", "garlic", "duck", "egg"][b] }] },
        data,
      );
      assert.equal(critique.pairCount, 1);
      const percentile = critique.coherencePercentile;
      assert.ok(percentile >= 0 && percentile <= 100, `percentile ${percentile} out of range`);
    }
  }
});

test("pairwiseCritique returns null below the minimum resolved count", () => {
  assert.equal(pairwiseCritique({ ingredients: [{ name: "apple" }] }, fixture()), null);
  assert.equal(pairwiseCritique({ ingredients: [] }, fixture()), null);
});

test("pairwiseCritique omits weakestIngredient below the minimum resolved count", () => {
  const critique = pairwiseCritique(
    { ingredients: [{ name: "apple" }, { name: "basil" }, { name: "quantum foam" }] },
    fixture(),
  );
  assert.equal(critique.resolved, 2);
  assert.equal(critique.weakestIngredient, null);
  assert.deepEqual(critique.unresolved, ["quantum foam"]);
});

test("weakestPairs are capped and sorted ascending", () => {
  // Six target pairs (x_i, y_i): x_i carries every pair's dims except its
  // own, y_i carries only its own pair's dim. Each x_i row then has its
  // partner as the unique minimum affinity (percentile 1/11 ~= 9.1 < 20),
  // so exactly six pairs are flagged — overflowing the cap of five. (With
  // n nodes the lowest achievable percentile is 1/(n-1); percentile ties
  // are common, so this needs a strict-minimum construction rather than
  // an all-zeros one.)
  const pairCount = 6;
  const names = [];
  const nodeIds = [];
  const embeddings = new Float32Array(pairCount * 2 * DIM);
  for (let i = 1; i <= pairCount; i += 1) {
    const xi = (i - 1) * 2;
    const yi = xi + 1;
    names.push(`x${i}`, `y${i}`);
    nodeIds.push(xi + 1, yi + 1);
    for (let j = 1; j <= pairCount; j += 1) {
      if (j !== i) embeddings[xi * DIM + 2 * j] = 1;
    }
    embeddings[yi * DIM + 2 * i] = 1;
  }
  const lookup = new Map(names.map((name, i) => [name, { nodeId: nodeIds[i], name }]));
  const data = {
    embeddings,
    nodeIds,
    rowByNodeId: new Map(nodeIds.map((nodeId, i) => [nodeId, i])),
    nameByNodeId: new Map(nodeIds.map((nodeId, i) => [nodeId, names[i]])),
    nodeIdByExactName: new Map(names.map((name, i) => [name, nodeIds[i]])),
    aliases: new Map(),
    lookup,
  };
  const critique = pairwiseCritique({ ingredients: names.map((name) => ({ name })) }, data);
  assert.equal(critique.pairCount, 66);
  assert.equal(critique.weakestPairs.length, MAX_FLAGGED_PAIRS);
  assert.ok(critique.weakestPairs.every((pair) => pair.percentile < FLAG_PERCENTILE));
  for (let i = 1; i < critique.weakestPairs.length; i += 1) {
    assert.ok(critique.weakestPairs[i - 1].percentile <= critique.weakestPairs[i].percentile);
  }
  const message = critiqueMessage(critique);
  assert.ok(message.includes("- x1 — y1 (9.1th percentile)"));
});

test("ingredientDiff reports added and removed names case-insensitively", () => {
  const diff = ingredientDiff(
    { ingredients: [{ name: "chicken" }, { name: "rice" }] },
    { ingredients: [{ name: "Tofu" }, { name: "rice" }] },
  );
  assert.deepEqual(diff, { added: ["Tofu"], removed: ["chicken"] });
  assert.deepEqual(
    ingredientDiff(
      { ingredients: [{ name: "Chicken" }] },
      { ingredients: [{ name: "chicken" }] },
    ),
    { added: [], removed: [] },
  );
});

test("critiqueMessage carries coverage and the weakest ingredient", () => {
  const critique = pairwiseCritique(
    { ingredients: ["apple", "basil", "garlic", "duck", "egg"].map((name) => ({ name })) },
    fixture(),
  );
  const message = critiqueMessage(critique);
  assert.ok(message.includes("Scored 5 of 5 ingredients (10 pairs)."));
  assert.ok(message.includes("Weakest pairings: none below the flagging threshold."));
  assert.ok(message.includes("Most weakly paired ingredient: duck (mean 43.8th percentile)."));
  assert.ok(message.includes("same schema as your draft above"));
});

test("critiqueMessage handles an unmatched ingredient", () => {
  const critique = pairwiseCritique(
    { ingredients: [{ name: "apple" }, { name: "basil" }, { name: "quantum foam" }] },
    fixture(),
  );
  const message = critiqueMessage(critique);
  assert.ok(message.includes("(unmatched: quantum foam)"));
  assert.ok(message.includes("Weakest pairings: none below the flagging threshold."));
});

test("loadEpicure throws on a corrupted bundle so the pass skips loudly", () => {
  // Runs before any successful load: the loader is a lazy singleton, so a
  // cache-warmed bundle would mask a bad directory.
  const dir = mkdtempSync(join(tmpdir(), "epicure-bad-"));
  try {
    writeFileSync(join(dir, "embeddings.csv"), "node_id,dim_0\n1,0.5\n");
    writeFileSync(join(dir, "ingredient_list.csv"), "node_id,name\n1,apple\n");
    writeFileSync(join(dir, "consolidated_nodes.csv"), "new_node_id,final_name,node_ids_consolidated,original_names_consolidated\n");
    assert.throws(() => loadEpicure(dir), /epicure embeddings:/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("loadEpicure reads the bundled data and resolves real ingredients", () => {
  const data = loadEpicure();
  assert.equal(data.nodeIds.length, 1790);
  assert.equal(data.embeddings.length, 1790 * DIM);
  const tomato = resolveIngredient("tomato", data);
  assert.ok(tomato, "tomato must resolve");
  assert.equal(tomato.nodeId, resolveIngredient("tomatoes", data)?.nodeId, "tomatoes must reach tomato via the stem fallback");
  // "smoked paprika" is its own canonical node in the bundle ("smoked_paprika"),
  // so the exact vocab match wins over modifier stripping — same as upstream.
  assert.ok(resolveIngredient("smoked paprika", data), "smoked paprika must resolve");
});
