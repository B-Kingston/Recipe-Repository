/**
 * The recipe chat's edit tools. They change a working copy of the recipe
 * inside the worker, never the saved recipe: the copy goes back to the host
 * as a proposal, which the cook sees as a list of changes and accepts or
 * denies. Every item remembers where it came from (`from`, its index in the
 * recipe the host sent, or null when the model added it), so the host can
 * keep the stored ingredient and step rows the proposal leaves in place.
 */
import { Type } from "@earendil-works/pi-ai";
import type { Tool, ToolCall, ToolResultMessage } from "@earendil-works/pi-ai";

export interface ProposedIngredient {
  from: number | null;
  name: string;
  quantity: string;
  unit: string;
  optional: boolean;
}

export interface ProposedStep {
  from: number | null;
  text: string;
  /** The ingredient lines the step lists, e.g. "200 g butter". */
  ingredients: string[];
}

/** The recipe as the edits leave it; the shape of the host's snapshot plus
 * each item's origin. */
export interface RecipeProposal {
  title: string;
  description: string;
  servings: number | null;
  prepMinutes: number | null;
  cookMinutes: number | null;
  ingredients: ProposedIngredient[];
  steps: ProposedStep[];
}

/** Characters of a step's text the edit results repeat back. */
const STEP_PREVIEW_CHARS = 90;

const position = (description: string) => Type.Integer({ minimum: 1, description });
const ingredientLines = Type.Array(Type.String(), {
  description: "The ingredient lines this step lists, each as quantity, unit, and name, e.g. \"200 g butter\".",
});

/** The edit tools, offered when the host sends the recipe. show_recipe only
 * reads; every other one changes the working copy. */
export const EDIT_TOOLS: Tool[] = [
  {
    name: "show_recipe",
    description: "Show the recipe as your edits so far have left it: details, numbered ingredients, and numbered steps. Read-only; use it to check numbers or your edits. Never call a change tool just to look.",
    parameters: Type.Object({}),
  },
  {
    name: "change_details",
    description: "Only when the cook asked for a change: change the recipe's title, description, servings, or prep and cook times. Leave out what stays the same.",
    parameters: Type.Object({
      title: Type.Optional(Type.String({ minLength: 1 })),
      description: Type.Optional(Type.String()),
      servings: Type.Optional(Type.Integer({ minimum: 1 })),
      prepMinutes: Type.Optional(Type.Integer({ minimum: 0 })),
      cookMinutes: Type.Optional(Type.Integer({ minimum: 0 })),
    }),
  },
  {
    name: "add_ingredient",
    description: "Only when the cook asked for a change: add an ingredient to the ingredient list.",
    parameters: Type.Object({
      name: Type.String({ minLength: 1, description: "The ingredient, e.g. \"unsalted butter, softened\"." }),
      quantity: Type.Optional(Type.String({ description: "e.g. \"200\" or \"1/2\"; empty for \"to taste\"." })),
      unit: Type.Optional(Type.String({ description: "e.g. \"g\", \"tbsp\"; empty when the quantity is a count." })),
      optional: Type.Optional(Type.Boolean()),
      position: Type.Optional(position("Where it goes in the list, counting from 1; the end when left out.")),
    }),
  },
  {
    name: "change_ingredient",
    description: "Only when the cook asked for a change: change one ingredient's name, quantity, unit, or whether it is optional. Leave out what stays the same. Update the steps that list it too.",
    parameters: Type.Object({
      ingredient: position("The ingredient's number in the current list."),
      name: Type.Optional(Type.String({ minLength: 1 })),
      quantity: Type.Optional(Type.String()),
      unit: Type.Optional(Type.String()),
      optional: Type.Optional(Type.Boolean()),
    }),
  },
  {
    name: "remove_ingredient",
    description: "Only when the cook asked for a change: remove one ingredient from the list. Update the steps that list it too.",
    parameters: Type.Object({ ingredient: position("The ingredient's number in the current list.") }),
  },
  {
    name: "move_ingredient",
    description: "Only when the cook asked for a change: move one ingredient to another place in the list.",
    parameters: Type.Object({
      ingredient: position("The ingredient's number in the current list."),
      to: position("Its new number."),
    }),
  },
  {
    name: "add_step",
    description: "Only when the cook asked for a change: add a step to the method.",
    parameters: Type.Object({
      text: Type.String({ minLength: 1 }),
      ingredients: Type.Optional(ingredientLines),
      position: Type.Optional(position("Its step number; the end when left out.")),
    }),
  },
  {
    name: "change_step",
    description: "Only when the cook asked for a change: rewrite one step, or replace the ingredient lines it lists. Leave out what stays the same.",
    parameters: Type.Object({
      step: position("The step's number in the current method."),
      text: Type.Optional(Type.String({ minLength: 1 })),
      ingredients: Type.Optional(ingredientLines),
    }),
  },
  {
    name: "remove_step",
    description: "Only when the cook asked for a change: remove one step from the method.",
    parameters: Type.Object({ step: position("The step's number in the current method.") }),
  },
  {
    name: "move_step",
    description: "Only when the cook asked for a change: move one step to another place in the method.",
    parameters: Type.Object({
      step: position("The step's number in the current method."),
      to: position("Its new number."),
    }),
  },
];

const EDIT_TOOL_NAMES = new Set(EDIT_TOOLS.map((tool) => tool.name));

/** Whether a tool call is one of the edit tools. */
export function isEditTool(name: string): boolean {
  return EDIT_TOOL_NAMES.has(name);
}

function text(value: unknown): string {
  return typeof value === "string" ? value : "";
}

/** A whole number the model sent, as a number or as a numeric string
 * ("2"), which some tool-calling servers send. */
function whole(value: unknown): number | null {
  const number = typeof value === "string" && /^\s*\d+\s*$/.test(value) ? Number(value) : value;
  return typeof number === "number" && Number.isInteger(number) ? number : null;
}

function count(value: unknown): number | null {
  const number = whole(value);
  return number !== null && number >= 0 ? number : null;
}

/** The working copy of the recipe snapshot the host sent, or null when it
 * sent none (or one without a title). */
export function recipeDraft(value: unknown): RecipeProposal | null {
  if (!value || typeof value !== "object") return null;
  const raw = value as Record<string, unknown>;
  const title = text(raw.title).trim();
  if (!title) return null;
  const list = (entries: unknown): Record<string, unknown>[] =>
    (Array.isArray(entries) ? entries : []).map((entry) => (entry && typeof entry === "object" ? entry as Record<string, unknown> : {}));
  return {
    title,
    description: text(raw.description),
    servings: count(raw.servings),
    prepMinutes: count(raw.prepMinutes),
    cookMinutes: count(raw.cookMinutes),
    ingredients: list(raw.ingredients).map((ingredient, index) => ({
      from: index,
      name: text(ingredient.name),
      quantity: text(ingredient.quantity),
      unit: text(ingredient.unit),
      optional: ingredient.optional === true,
    })),
    steps: list(raw.steps).map((step, index) => ({
      from: index,
      text: text(step.text),
      ingredients: (Array.isArray(step.ingredients) ? step.ingredients : []).map(text).filter((line) => line.trim()),
    })),
  };
}

/** A deep copy, so a proposal can be compared with the recipe it started from. */
export function copyDraft(draft: RecipeProposal): RecipeProposal {
  return JSON.parse(JSON.stringify(draft)) as RecipeProposal;
}

/** The recipe's content without the origins: what the cook would see. */
function visible(draft: RecipeProposal): string {
  return JSON.stringify({
    ...draft,
    ingredients: draft.ingredients.map(({ from: _from, ...rest }) => rest),
    steps: draft.steps.map(({ from: _from, ...rest }) => rest),
  });
}

/** Whether the edits changed anything the cook would see. */
export function draftChanged(original: RecipeProposal, draft: RecipeProposal): boolean {
  return visible(original) !== visible(draft);
}

export function ingredientLine(ingredient: { name: string; quantity: string; unit: string; optional: boolean }): string {
  const line = [ingredient.quantity, ingredient.unit, ingredient.name].map((part) => part.trim()).filter(Boolean).join(" ");
  return ingredient.optional ? `${line} (optional)` : line;
}

function stepPreview(step: ProposedStep): string {
  const body = step.text.length > STEP_PREVIEW_CHARS ? `${step.text.slice(0, STEP_PREVIEW_CHARS).trimEnd()}…` : step.text;
  return step.ingredients.length ? `${body} [uses: ${step.ingredients.join("; ")}]` : body;
}

function detailsLine(draft: RecipeProposal): string {
  const minutes = (value: number | null) => (value === null ? "not set" : `${value} min`);
  return [
    `Title: ${draft.title}`,
    `Description: ${draft.description.trim() || "none"}`,
    `Servings: ${draft.servings ?? "not set"} · Prep: ${minutes(draft.prepMinutes)} · Cook: ${minutes(draft.cookMinutes)}`,
  ].join("\n");
}

function ingredientList(draft: RecipeProposal): string {
  if (draft.ingredients.length === 0) return "Ingredients now: none.";
  return `Ingredients now:\n${draft.ingredients.map((ingredient, index) => `${index + 1}. ${ingredientLine(ingredient)}`).join("\n")}`;
}

function stepList(draft: RecipeProposal): string {
  if (draft.steps.length === 0) return "Method now: no steps.";
  return `Method now:\n${draft.steps.map((step, index) => `${index + 1}. ${stepPreview(step)}`).join("\n")}`;
}

/** A 0-based index from a 1-based number the model gave, or null. */
function index(value: unknown, length: number): number | null {
  const number = whole(value);
  return number !== null && number >= 1 && number <= length ? number - 1 : null;
}

/** A 0-based insert position from an optional 1-based number: the end when
 * absent, clamped into the list otherwise. */
function insertAt(value: unknown, length: number): number {
  const number = whole(value);
  if (number === null) return length;
  return Math.min(Math.max(number - 1, 0), length);
}

function lines(value: unknown): string[] | null {
  if (!Array.isArray(value)) return null;
  return value.map(text).map((line) => line.trim()).filter(Boolean);
}

function optionalText(value: unknown): string | undefined {
  return typeof value === "string" ? value.trim() : undefined;
}

function moveItem<T>(items: T[], from: number, to: number): void {
  const [item] = items.splice(from, 1);
  items.splice(to, 0, item);
}

/** Runs one edit tool against the working copy. A bad number or empty text
 * is an error result naming what to fix; the copy is left as it was. */
export function runEditTool(draft: RecipeProposal, call: ToolCall): ToolResultMessage {
  const result = (output: string, isError = false): ToolResultMessage => ({
    role: "toolResult",
    toolCallId: call.id,
    toolName: call.name,
    content: [{ type: "text", text: output }],
    isError,
    timestamp: Date.now(),
  });
  const args = call.arguments as Record<string, unknown>;
  const noIngredient = () => result(`There is no ingredient ${String(args.ingredient)}. ${ingredientList(draft)}`, true);
  const noStep = () => result(`There is no step ${String(args.step)}. ${stepList(draft)}`, true);
  switch (call.name) {
    case "show_recipe":
      return result(`${detailsLine(draft)}\n\n${ingredientList(draft)}\n\n${stepList(draft)}`);
    case "change_details": {
      const title = optionalText(args.title);
      if (title === "") return result("The title cannot be empty.", true);
      const changes: string[] = [];
      if (title !== undefined) { draft.title = title; changes.push("title"); }
      const description = optionalText(args.description);
      if (description !== undefined) { draft.description = description; changes.push("description"); }
      for (const field of ["servings", "prepMinutes", "cookMinutes"] as const) {
        const value = count(args[field]);
        if (value !== null && !(field === "servings" && value === 0)) { draft[field] = value; changes.push(field); }
      }
      if (changes.length === 0) {
        return result(`Nothing changed: give the details to change. To look at the recipe, call show_recipe.\n${detailsLine(draft)}`, true);
      }
      return result(`Changed ${changes.join(", ")}.`);
    }
    case "add_ingredient": {
      const name = text(args.name).trim();
      if (!name) return result("The ingredient needs a name.", true);
      const at = insertAt(args.position, draft.ingredients.length);
      draft.ingredients.splice(at, 0, {
        from: null,
        name,
        quantity: text(args.quantity).trim(),
        unit: text(args.unit).trim(),
        optional: args.optional === true,
      });
      return result(`Added as ingredient ${at + 1}. ${ingredientList(draft)}`);
    }
    case "change_ingredient": {
      const at = index(args.ingredient, draft.ingredients.length);
      if (at === null) return noIngredient();
      const ingredient = draft.ingredients[at];
      const name = optionalText(args.name);
      if (name === "") return result("The ingredient name cannot be empty.", true);
      if (name !== undefined) ingredient.name = name;
      const quantity = optionalText(args.quantity);
      if (quantity !== undefined) ingredient.quantity = quantity;
      const unit = optionalText(args.unit);
      if (unit !== undefined) ingredient.unit = unit;
      if (typeof args.optional === "boolean") ingredient.optional = args.optional;
      return result(`Ingredient ${at + 1} is now: ${ingredientLine(ingredient)}.`);
    }
    case "remove_ingredient": {
      const at = index(args.ingredient, draft.ingredients.length);
      if (at === null) return noIngredient();
      const [removed] = draft.ingredients.splice(at, 1);
      return result(`Removed ${ingredientLine(removed)}. ${ingredientList(draft)}`);
    }
    case "move_ingredient": {
      const at = index(args.ingredient, draft.ingredients.length);
      if (at === null) return noIngredient();
      const to = index(args.to, draft.ingredients.length);
      if (to === null) return result(`There is no place ${String(args.to)}: the list has ${draft.ingredients.length}.`, true);
      moveItem(draft.ingredients, at, to);
      return result(`Moved. ${ingredientList(draft)}`);
    }
    case "add_step": {
      const body = text(args.text).trim();
      if (!body) return result("The step needs text.", true);
      const at = insertAt(args.position, draft.steps.length);
      draft.steps.splice(at, 0, { from: null, text: body, ingredients: lines(args.ingredients) ?? [] });
      return result(`Added as step ${at + 1}. ${stepList(draft)}`);
    }
    case "change_step": {
      const at = index(args.step, draft.steps.length);
      if (at === null) return noStep();
      const body = optionalText(args.text);
      if (body === "") return result("The step text cannot be empty.", true);
      const uses = lines(args.ingredients);
      if (body === undefined && uses === null) return result("Nothing to change: give the text or the ingredient lines.", true);
      if (body !== undefined) draft.steps[at].text = body;
      if (uses !== null) draft.steps[at].ingredients = uses;
      return result(`Step ${at + 1} is now: ${stepPreview(draft.steps[at])}`);
    }
    case "remove_step": {
      const at = index(args.step, draft.steps.length);
      if (at === null) return noStep();
      draft.steps.splice(at, 1);
      return result(`Removed step ${at + 1}. ${stepList(draft)}`);
    }
    case "move_step": {
      const at = index(args.step, draft.steps.length);
      if (at === null) return noStep();
      const to = index(args.to, draft.steps.length);
      if (to === null) return result(`There is no step ${String(args.to)}: the method has ${draft.steps.length}.`, true);
      moveItem(draft.steps, at, to);
      return result(`Moved. ${stepList(draft)}`);
    }
    default:
      return result(`Unknown tool ${call.name}.`, true);
  }
}
