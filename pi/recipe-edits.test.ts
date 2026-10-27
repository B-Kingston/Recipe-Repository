import test from "node:test";
import assert from "node:assert/strict";
import { EDIT_TOOLS, copyDraft, draftChanged, isEditTool, recipeDraft, runEditTool } from "./recipe-edits.ts";
import type { RecipeProposal } from "./recipe-edits.ts";
import type { ToolCall, ToolResultMessage } from "@earendil-works/pi-ai";

function snapshot(): RecipeProposal {
  return recipeDraft({
    schemaVersion: 1,
    title: "Shortbread",
    description: "Buttery.",
    servings: 12,
    prepMinutes: 15,
    cookMinutes: null,
    ingredients: [
      { name: "butter", quantity: "200", unit: "g", optional: false },
      { name: "sugar", quantity: "100", unit: "g", optional: false },
      { name: "flour", quantity: "300", unit: "g", optional: false },
    ],
    steps: [
      { text: "Cream the butter and sugar.", ingredients: ["200 g butter", "100 g sugar"] },
      { text: "Work in the flour.", ingredients: ["300 g flour"] },
      { text: "Bake at 160C for 25 minutes.", ingredients: [] },
    ],
  })!;
}

function call(name: string, args: Record<string, unknown>): ToolCall {
  return { type: "toolCall", id: `call-${name}`, name, arguments: args as ToolCall["arguments"] };
}

function output(message: ToolResultMessage): string {
  return message.content.map((part) => (part as { text: string }).text).join("\n");
}

test("recipeDraft reads the host's snapshot and marks where each item came from", () => {
  const draft = snapshot();
  assert.equal(draft.cookMinutes, null);
  assert.deepEqual(draft.ingredients.map((ingredient) => ingredient.from), [0, 1, 2]);
  assert.deepEqual(draft.steps.map((step) => step.from), [0, 1, 2]);
  assert.equal(recipeDraft({ title: " " }), null);
  assert.equal(recipeDraft(undefined), null);
  assert.ok(EDIT_TOOLS.every((tool) => isEditTool(tool.name)));
  assert.equal(isEditTool("recipe_notes"), false);
});

test("the ingredient tools add, change, move, and remove on the working copy", () => {
  const original = snapshot();
  const draft = copyDraft(original);
  const added = runEditTool(draft, call("add_ingredient", { name: "salt", quantity: "1", unit: "pinch", position: 2 }));
  assert.equal(added.isError, false);
  assert.match(output(added), /Added as ingredient 2\.[\s\S]*2\. 1 pinch salt/);
  assert.deepEqual(draft.ingredients.map((ingredient) => [ingredient.from, ingredient.name]), [[0, "butter"], [null, "salt"], [1, "sugar"], [2, "flour"]]);

  runEditTool(draft, call("change_ingredient", { ingredient: 1, quantity: "250", optional: true }));
  assert.deepEqual(draft.ingredients[0], { from: 0, name: "butter", quantity: "250", unit: "g", optional: true });
  runEditTool(draft, call("move_ingredient", { ingredient: 4, to: 1 }));
  assert.equal(draft.ingredients[0].name, "flour");
  runEditTool(draft, call("remove_ingredient", { ingredient: 4 }));
  assert.deepEqual(draft.ingredients.map((ingredient) => ingredient.name), ["flour", "butter", "salt"]);
  assert.equal(original.ingredients.length, 3, "the original stays as the host sent it");
});

test("the step tools add, rewrite, move, and remove steps", () => {
  const draft = snapshot();
  runEditTool(draft, call("add_step", { text: "Chill for 30 minutes.", ingredients: [" ", "1 egg"], position: 3 }));
  assert.deepEqual(draft.steps[2], { from: null, text: "Chill for 30 minutes.", ingredients: ["1 egg"] });
  const changed = runEditTool(draft, call("change_step", { step: 1, ingredients: ["250 g butter", "100 g sugar"] }));
  assert.match(output(changed), /Step 1 is now: Cream the butter and sugar\. \[uses: 250 g butter; 100 g sugar\]/);
  assert.equal(draft.steps[0].text, "Cream the butter and sugar.");
  runEditTool(draft, call("move_step", { step: 4, to: 1 }));
  assert.equal(draft.steps[0].text, "Bake at 160C for 25 minutes.");
  runEditTool(draft, call("remove_step", { step: 1 }));
  assert.deepEqual(draft.steps.map((step) => step.from), [0, 1, null]);
});

test("details change only what the call names", () => {
  const draft = snapshot();
  const result = runEditTool(draft, call("change_details", { servings: 24, title: "Big shortbread" }));
  assert.equal(output(result), "Changed title, servings.");
  assert.equal(draft.servings, 24);
  assert.equal(draft.prepMinutes, 15);
  assert.equal(runEditTool(draft, call("change_details", {})).isError, true);
  assert.equal(runEditTool(draft, call("change_details", { title: "  " })).isError, true);
  assert.equal(draft.title, "Big shortbread");
});

test("bad numbers and empty text are errors that leave the copy alone", () => {
  const draft = snapshot();
  const before = JSON.stringify(draft);
  const missing = runEditTool(draft, call("change_ingredient", { ingredient: 9, quantity: "1" }));
  assert.equal(missing.isError, true);
  assert.match(output(missing), /There is no ingredient 9\. Ingredients now:\n1\. 200 g butter/);
  for (const bad of [
    call("remove_step", { step: 0 }),
    call("move_step", { step: 1, to: 7 }),
    call("add_step", { text: " " }),
    call("change_step", { step: 1 }),
    call("add_ingredient", { name: "" }),
    call("change_ingredient", { ingredient: 1, name: "" }),
    call("rename_recipe", {}),
  ]) {
    assert.equal(runEditTool(draft, bad).isError, true, bad.name);
  }
  assert.equal(JSON.stringify(draft), before);
});

test("draftChanged ignores where items came from and sees what the cook would", () => {
  const original = snapshot();
  const draft = copyDraft(original);
  assert.equal(draftChanged(original, draft), false);
  // Removing butter and adding it back as new is no visible change.
  runEditTool(draft, call("remove_ingredient", { ingredient: 1 }));
  runEditTool(draft, call("add_ingredient", { name: "butter", quantity: "200", unit: "g", position: 1 }));
  assert.equal(draftChanged(original, draft), false);
  runEditTool(draft, call("change_ingredient", { ingredient: 1, unit: "grams" }));
  assert.equal(draftChanged(original, draft), true);
});

/** A small seeded random generator, so the sequence below is the same every run. */
function seeded(seed: number): () => number {
  let state = seed;
  return () => {
    state = (state * 1103515245 + 12345) % 2147483648;
    return state / 2147483648;
  };
}

test("any sequence of edits keeps every origin real and distinct, as the host requires", () => {
  const random = seeded(7);
  const pick = <T>(items: readonly T[]): T => items[Math.floor(random() * items.length)];
  const number = () => Math.floor(random() * 8) - 1;
  const original = snapshot();
  const draft = copyDraft(original);
  for (let at = 0; at < 500; at += 1) {
    const name = pick(EDIT_TOOLS.map((tool) => tool.name));
    runEditTool(draft, call(name, {
      ingredient: number(), step: number(), to: number(), position: number(),
      name: pick(["", " ", "crème fraîche", "½ tsp salt", "<b>x</b>"]),
      text: pick(["", "Fold gently — don't overmix.", "Bake 🍪"]),
      quantity: pick(["1", "", "½"]), unit: pick(["g", ""]), optional: random() > 0.5,
      ingredients: pick([undefined, [], ["1 egg", "1 egg"], [" "]]),
      servings: pick([0, 3, -1, 2.5]), title: pick([undefined, "", "New"]),
    }));
    for (const items of [draft.ingredients, draft.steps]) {
      const origins = items.map((item) => item.from).filter((from): from is number => from !== null);
      assert.equal(new Set(origins).size, origins.length, `after ${at} edits: origins are distinct`);
      assert.ok(origins.every((from) => Number.isInteger(from) && from >= 0 && from < 3), `after ${at} edits: origins name the recipe sent`);
    }
    assert.ok(draft.title.trim() && draft.ingredients.every((item) => item.name.trim()) && draft.steps.every((step) => step.text.trim()));
    assert.ok(draft.servings === null || draft.servings >= 1);
  }
  assert.equal(original.ingredients.length, 3, "the recipe the host sent is untouched");
});

test("edits keep unicode and repeated ingredient lines as given", () => {
  const draft = snapshot();
  runEditTool(draft, call("change_ingredient", { ingredient: 1, name: "beurre demi-sel 🧈", quantity: "½" }));
  runEditTool(draft, call("change_step", { step: 1, ingredients: ["½ beurre", "½ beurre"] }));
  assert.equal(draft.ingredients[0].name, "beurre demi-sel 🧈");
  assert.deepEqual(draft.steps[0].ingredients, ["½ beurre", "½ beurre"]);
});

test("numbers sent as numeric strings still find the item", () => {
  const draft = snapshot();
  const result = runEditTool(draft, call("remove_ingredient", { ingredient: "2" }));
  assert.equal(result.isError, false, output(result));
});

test("show_recipe reads the working copy without changing it", () => {
  const draft = snapshot();
  runEditTool(draft, call("change_details", { servings: 24 }));
  const before = JSON.stringify(draft);
  const shown = runEditTool(draft, call("show_recipe", {}));
  assert.equal(shown.isError, false);
  assert.match(output(shown), /^Title: Shortbread\nDescription: Buttery\.\nServings: 24 · Prep: 15 min · Cook: not set\n\nIngredients now:\n1\. 200 g butter/);
  assert.match(output(shown), /Method now:\n1\. Cream the butter and sugar\./);
  assert.equal(JSON.stringify(draft), before);
  const empty = runEditTool(draft, call("change_details", {}));
  assert.match(output(empty), /call show_recipe\.\nTitle: Shortbread/);
});
