/**
 * The Ask cases. Each one drives the recipe chat over HTTP the way the
 * drawer's forms do, then checks structure and invariants rather than
 * wording: model output varies, so a "hard" check is something the app or
 * the system prompt guarantees, and a "soft" check (a warning) is what a
 * good answer would very likely contain.
 */
import { readFileSync } from "node:fs";
import { Client, answerTo, answers, content, readChat, readNotes, readRecipe, readVersions, sleep, text } from "./lib.ts";
import type { ChatAnswer, ChatView, RecipeView } from "./lib.ts";

export interface Settings {
  answerTimeoutMs: number;
  generateTimeoutMs: number;
  /** A web-search answer must carry a URL (the deployment can search). */
  expectSearch: boolean;
  runId: string;
}

/** What the cases share: the signed-in browsers, the test recipe, and what
 * earlier cases left for later ones. */
export interface Context {
  owner: Client;
  other: Client | null;
  recipeId: string;
  settings: Settings;
  /** Message ids earlier cases produced, by name. */
  saved: Map<string, string>;
  warn(message: string): void;
}

export class CaseFailure extends Error {}
export class CaseSkipped extends Error {}

export function check(condition: unknown, message: string): asserts condition {
  if (!condition) throw new CaseFailure(message);
}

function soft(context: Context, condition: unknown, message: string): void {
  if (!condition) context.warn(message);
}

export interface Case {
  name: string;
  /** What the case proves, for the report and the README. */
  proves: string;
  /** Needs a second account. */
  needsOther?: boolean;
  /** Runs a model turn (costs a request to the AI provider). */
  usesModel: boolean;
  /** Runs only when named in E2E_CASES. */
  optIn?: boolean;
  run(context: Context): Promise<void>;
}

const recipePath = (context: Context) => `/recipes/${context.recipeId}`;

export async function recipe(context: Context): Promise<{ html: string; recipe: RecipeView; chat: ChatView }> {
  const html = await context.owner.page(recipePath(context));
  return { html, recipe: readRecipe(html), chat: readChat(html) };
}

/** Asks a question and waits for its answer to land. */
export async function ask(context: Context, question: string): Promise<{ answer: ChatAnswer; chat: ChatView; html: string }> {
  const reply = await context.owner.post(`${recipePath(context)}/chat`, { question, back: recipePath(context) });
  check(reply.status === 303, `asking answered ${reply.status}`);
  check(reply.location === `${recipePath(context)}#recipe-chat`, `asking went to ${reply.location}`);
  check(reply.flash !== "chat_busy", "the chat was still busy with an earlier answer");
  return waitForAnswer(context, question);
}

export async function waitForAnswer(context: Context, question: string): Promise<{ answer: ChatAnswer; chat: ChatView; html: string }> {
  const deadline = Date.now() + context.settings.answerTimeoutMs;
  for (;;) {
    const { html, chat } = await recipe(context);
    check(chat.open, "the chat drawer is not on the recipe page");
    const answer = answerTo(chat, question);
    check(answer, "the question is not in the thread");
    if (answer.status !== "pending") {
      check(chat.pollState === "", "nothing is running, yet the drawer still polls");
      check(!chat.questionDisabled, "the question box stays disabled after the answer");
      return { answer, chat, html };
    }
    check(chat.pollState !== "", "a pending answer must leave a poller in the drawer");
    check(chat.questionDisabled, "the question box must be disabled while an answer runs");
    if (Date.now() > deadline) throw new CaseFailure(`no answer within ${context.settings.answerTimeoutMs / 1000}s`);
    await sleep(3000);
  }
}

function answered(answer: ChatAnswer): void {
  check(answer.status === "done", `the answer ${answer.status}: ${answer.content}`);
  check(answer.content.length > 0 && answer.content.length <= 6000, "the answer is empty or longer than the 6000-character clip");
  check(answer.id !== "", "a finished answer carries its id in its forms");
  check(answer.canKeep || answer.kept, "a finished answer offers Keep as note");
}

function plainAnswer(answer: ChatAnswer): void {
  answered(answer);
  check(answer.proposal === null, `a question that asks for no change came back with a proposal: ${answer.proposal?.diff}`);
}

function proposed(answer: ChatAnswer): NonNullable<ChatAnswer["proposal"]> {
  answered(answer);
  const proposal = answer.proposal;
  check(proposal, `a change request came back without a proposal: ${answer.content}`);
  check(proposal.state === "ready", `the proposal is ${proposal.state}`);
  check(!proposal.stale, "a fresh proposal shows as stale");
  check(proposal.canAccept && proposal.canDeny, "a ready proposal shows Accept and Deny");
  check(proposal.items > 0 || /detail/.test(proposal.summary), "the proposal lists no change");
  check(proposal.summary !== "", "the proposal has no summary line");
  return proposal;
}

/** Accepts a proposal and checks the page, the thread, and the history. */
async function accept(context: Context, answer: ChatAnswer, before: { recipe: RecipeView; html: string; chat: ChatView }): Promise<RecipeView> {
  const reply = await context.owner.post(`${recipePath(context)}/chat/${answer.id}/accept`, { back: recipePath(context) });
  check(reply.status === 303, `Accept answered ${reply.status} ${text(reply.body).slice(0, 300)}`);
  check(reply.location === `${recipePath(context)}#recipe-chat`, `Accept went to ${reply.location}`);
  check(reply.flash === "change_applied", `Accept set ${reply.flash || "no"} notice`);
  const { html, recipe: after, chat } = await recipe(context);
  check(chat.notice === "Change saved to the recipe.", `the drawer says "${chat.notice}"`);
  const landed = answerTo(chat, answer.question);
  check(landed?.proposal?.state === "applied", "the accepted answer is not marked saved");
  const dividers = (view: ChatView) => view.rows.filter((row) => row.kind === "change").length;
  check(dividers(chat) === dividers(before.chat) + 1, "Accept adds exactly one divider");
  const last = chat.rows.at(-1);
  check(last?.kind === "change" && last.content !== "", "the divider lands at the end of the thread, labelled");
  check(content(after) !== content(before.recipe), "the recipe did not change");
  const versions = readVersions(html);
  const previous = readVersions(before.html);
  check(versions.length === previous.length + 1 || (previous.length === 0 && versions.length === 2), "Accept records one version (two when the recipe had no history)");
  check(versions[0].label.startsWith("Chat: "), `the newest version is "${versions[0]?.label}", not "Chat: …"`);
  check(versions[0].label === `Chat: ${last.content}`, `the version label "${versions[0].label}" does not match the divider "${last.content}"`);
  // The second Accept of one proposal does nothing more.
  const again = await context.owner.post(`${recipePath(context)}/chat/${answer.id}/accept`, { back: recipePath(context) });
  check(again.status === 303 && again.flash === "", "a second Accept is not a quiet no-op");
  const { html: afterAgain } = await recipe(context);
  check(readVersions(afterAgain).length === versions.length, "a second Accept recorded another version");
  return after;
}

function idsKept(before: RecipeView, after: RecipeView, section: "ingredients" | "steps"): number {
  const old = new Set(before[section].map((item) => item.id));
  return after[section].filter((item) => old.has(item.id)).length;
}

/** The recipe the cases start from. Distinctive words let the checks tell
 * what the model read with its tools from what it could guess. */
export const TEST_RECIPE = {
  description: "A throwaway recipe for the Ask end-to-end tests. Safe to delete.",
  servings: "4",
  prep: "10",
  cook: "30",
  ingredients: [
    { text: "butter", quantity: "2", unit: "tbsp" },
    { text: "onion, diced", quantity: "1", unit: "" },
    { text: "garlic cloves, crushed", quantity: "2", unit: "" },
    { text: "Aleppo pepper flakes", quantity: "1", unit: "tsp" },
    { text: "chopped tomatoes", quantity: "800", unit: "g" },
    { text: "eggs", quantity: "6", unit: "" },
    { text: "double cream", quantity: "2", unit: "tbsp", optional: true },
    { text: "salt", quantity: "1", unit: "tsp" },
  ],
  steps: [
    { text: "Melt the butter in a wide pan and soften the onion for 8 minutes.", uses: "2 tbsp butter\n1 onion, diced" },
    { text: "Stir in the garlic and pepper flakes and cook for 1 minute.", uses: "2 garlic cloves, crushed\n1 tsp Aleppo pepper flakes" },
    { text: "Add the tomatoes and salt and simmer for 15 minutes until thick.", uses: "800 g chopped tomatoes\n1 tsp salt" },
    { text: "Make 6 wells, crack in the eggs, cover, and cook for 6 to 8 minutes.", uses: "6 eggs" },
    { text: "Swirl through the cream and serve.", uses: "2 tbsp double cream" },
  ],
};

/** A word only the note holds, so an answer naming it read the note. */
const NOTE_WORD = "Kalamansi";

export const CASES: Case[] = [
  {
    name: "routes",
    proves: "Signed in, every chat route exists on the deployed build: a GET on each POST-only route answers 405 (an unknown path would be 404), and the drawer toggle answers 303.",
    usesModel: false,
    async run(context) {
      const base = recipePath(context);
      for (const path of [`${base}/chat`, `${base}/chat/clear`, `${base}/chat/drawer`, `${base}/chat/e2e/note`, `${base}/chat/e2e/accept`, `${base}/chat/e2e/deny`, "/settings/web-search", "/settings/web-search/delete"]) {
        const reply = await context.owner.get(path);
        check(reply.status === 405, `GET ${path.replace(base, "/recipes/…")} answered ${reply.status}, so the route is missing`);
      }
      check((await context.owner.get(`${base}/chat/e2e/bogus`)).status === 404, "an unknown chat path does not 404");
      for (const [path, flash] of [[`${base}/chat/e2e/accept`, ""], [`${base}/chat/e2e/deny`, ""]] as const) {
        const reply = await context.owner.post(path, { back: base });
        check(reply.status === 303 && reply.flash === flash, `POST ${path.replace(base, "/recipes/…")} with an unknown message answered ${reply.status} ${reply.flash}`);
      }
      const offPage = await context.owner.post(`${base}/chat`, { question: "x".repeat(1001), back: base });
      check(offPage.status === 400, `a question over 1000 characters answered ${offPage.status}`);
      const emptyQuestion = await context.owner.post(`${base}/chat`, { question: "   ", back: base });
      check(emptyQuestion.status === 400, `an empty question answered ${emptyQuestion.status}`);
      const away = await context.owner.post(`${base}/chat/drawer`, { open: "on", back: "https://example.com/" });
      check(away.status === 303 && away.location === `${base}#recipe-chat`, `a foreign back target was followed: ${away.location}`);
      const settings = await context.owner.page("/settings");
      check(!/name="api_key"[^>]*value="[^"]+"/.test(settings), "Settings prints a search key into the form");
    },
  },
  {
    name: "no-ai-login",
    proves: "On a deployment with no AI login, a question is still stored and its answer fails as needs_settings with a link to Settings, the drawer stops polling, and nothing is proposed. Opt-in: name it in E2E_CASES.",
    usesModel: false,
    optIn: true,
    async run(context) {
      const before = await recipe(context);
      const { answer } = await ask(context, "How thick should the sauce be?");
      check(answer.status === "needs_settings", `the answer is ${answer.status}: ${answer.content}`);
      check(answer.proposal === null && !answer.canKeep, "a failed answer offers Keep or a proposal");
      check(content((await recipe(context)).recipe) === content(before.recipe), "the recipe changed");
    },
  },
  {
    name: "plain-question",
    proves: "A question is stored with a pending answer at once, the drawer polls while it runs (question box disabled), and the answer lands as plain text with Keep as note and no proposal.",
    usesModel: true,
    async run(context) {
      const before = await recipe(context);
      const { answer } = await ask(context, "Roughly how long does this take from start to finish, and can I make the sauce a day ahead?");
      plainAnswer(answer);
      context.saved.set("plain", answer.id);
      context.saved.set("plain-question", answer.question);
      check(content((await recipe(context)).recipe) === content(before.recipe), "a plain question changed the recipe");
      soft(context, /\d/.test(answer.content), "the timing answer names no number");
    },
  },
  {
    name: "no-edit-for-a-question",
    proves: "\"Can I use …?\" is a question, not a request: the system prompt forbids the edit tools for it, so no proposal appears and the recipe stays as it was.",
    usesModel: true,
    async run(context) {
      const before = await recipe(context);
      const { answer } = await ask(context, "Can I use olive oil instead of butter here?");
      plainAnswer(answer);
      check(content((await recipe(context)).recipe) === content(before.recipe), "the recipe changed");
      soft(context, /olive/i.test(answer.content), "the answer does not mention olive oil");
    },
  },
  {
    name: "keep-as-note",
    proves: "Keep as note turns an answer into a whole-recipe note marked From the chat, the answer then shows Kept in notes, and keeping it twice adds nothing.",
    usesModel: false,
    async run(context) {
      const id = context.saved.get("plain");
      const question = context.saved.get("plain-question");
      if (!id || !question) throw new CaseSkipped("needs the plain-question answer");
      const notesBefore = readNotes((await recipe(context)).html).length;
      for (let time = 0; time < 2; time += 1) {
        const reply = await context.owner.post(`${recipePath(context)}/chat/${id}/note`, { back: recipePath(context) });
        check(reply.status === 303 && reply.flash === "note_kept", `Keep as note answered ${reply.status} ${reply.flash}`);
      }
      const { html, chat } = await recipe(context);
      const notes = readNotes(html);
      check(notes.length === notesBefore + 1, `keeping one answer twice made ${notes.length - notesBefore} notes`);
      const answer = answerTo(chat, question);
      check(answer, "the kept answer is missing from the thread");
      check(answer.kept && !answer.canKeep, "the answer does not show Kept in notes");
      const opening = answer.content.replace(/\s+/g, " ").trim().slice(0, 60);
      const kept = notes.filter((note) => note.fromChat && note.body.slice(0, 60) === opening);
      check(kept.length === 1, `${kept.length} notes from the chat hold the answer's text`);
    },
  },
  {
    name: "notes-and-photo",
    proves: "The model reads the cook's notes with recipe_notes (it names a word only the note holds) and, on a model that sees images, the note's photo with note_photo.",
    usesModel: true,
    async run(context) {
      const form = new FormData();
      form.set("back", recipePath(context));
      form.set("about", "");
      form.set("body", `Last time the sauce was too thin. Auntie's ${NOTE_WORD} trick fixed it: a squeeze at the end. The photo shows the colour I want.`);
      const photo = readFileSync(new URL("./fixtures/red-square.jpg", import.meta.url));
      form.set("photo", new Blob([photo], { type: "image/jpeg" }), "sauce.jpg");
      const added = await context.owner.request("POST", `${recipePath(context)}/notes`, { multipart: form });
      check(added.status === 303, `adding the note answered ${added.status}`);
      const notes = readNotes((await recipe(context)).html);
      check(notes.some((note) => note.hasPhoto && note.body.includes(NOTE_WORD)), "the note with its photo is not on the page");
      const { answer } = await ask(context, "What trick did I write in my notes last time, and what colour is the sauce in my note's photo?");
      plainAnswer(answer);
      check(answer.content.toLowerCase().includes(NOTE_WORD.toLowerCase()), `the answer does not name the note's ${NOTE_WORD} trick, so it did not read the notes: ${answer.content}`);
      soft(context, /\bred\b/i.test(answer.content), "the answer does not say the photo is red (the model may not see images)");
    },
  },
  {
    name: "version-lookup",
    proves: "The model opens a saved version with recipe_version: after a hand edit swaps the pepper flakes for smoked paprika, it names the spice the previous version used.",
    usesModel: true,
    async run(context) {
      const { recipe: current } = await recipe(context);
      const pepper = current.ingredients.find((item) => /Aleppo/.test(item.name));
      if (!pepper) throw new CaseSkipped("the Aleppo pepper ingredient is gone");
      const edited = await context.owner.post(`${recipePath(context)}/blocks/${pepper.id}`, { text: "smoked paprika", quantity: pepper.quantity, unit: pepper.unit });
      check(edited.status === 303, `the hand edit answered ${edited.status}`);
      const versions = readVersions((await recipe(context)).html);
      check(versions[0]?.label === "Ingredient edited", `the hand edit recorded "${versions[0]?.label}"`);
      const { answer } = await ask(context, "I changed one spice just now. Look at the saved version before the current one: which spice did it use instead of smoked paprika?");
      plainAnswer(answer);
      check(/aleppo/i.test(answer.content), `the answer does not name Aleppo pepper, so it did not open the saved version: ${answer.content}`);
    },
  },
  {
    name: "web-search",
    proves: "With PI_SEARCH_ENABLED the model searches when the answer depends on facts it should check (hosted search on Codex/Claude/OpenAI/Anthropic, the search API key otherwise) and ends its reply with the URLs it used.",
    usesModel: true,
    async run(context) {
      const { answer } = await ask(context, "Search the web: what minimum internal temperature does the USDA give for egg dishes like this? Put the URL of the page you used at the end.");
      plainAnswer(answer);
      const urls = answer.content.match(/https?:\/\/\S+/g) ?? [];
      if (context.settings.expectSearch) check(urls.length > 0, `the answer has no source URL, so it did not search: ${answer.content}`);
      else soft(context, urls.length > 0, "no source URL (E2E_EXPECT_SEARCH=0)");
      soft(context, /160|71/.test(answer.content), "the answer does not give 160°F / 71°C");
    },
  },
  {
    name: "edit-scale",
    proves: "\"Scale it to 6\" comes back as a proposal (details and amounts, Accept and Deny), a second question meanwhile is refused as busy, and Accept saves servings 6 with changed amounts, one divider, and a Chat: version, keeping the ingredient rows.",
    usesModel: true,
    async run(context) {
      const before = await recipe(context);
      const question = "Scale this recipe to serve 6 people.";
      const reply = await context.owner.post(`${recipePath(context)}/chat`, { question, back: recipePath(context) });
      check(reply.status === 303 && reply.flash === "", `asking answered ${reply.status} ${reply.flash}`);
      const busy = await context.owner.post(`${recipePath(context)}/chat`, { question: "And one more thing?", back: recipePath(context) });
      check(busy.status === 303 && busy.flash === "chat_busy", "a second question while one runs is not refused as busy");
      const { answer } = await waitForAnswer(context, question);
      const proposal = proposed(answer);
      check(/detail/.test(proposal.summary), `the proposal does not change the servings: ${proposal.summary}`);
      const after = await accept(context, answer, before);
      check(after.servings === 6, `servings are ${after.servings}`);
      const changed = after.ingredients.filter((item, at) => before.recipe.ingredients[at] && item.quantity !== before.recipe.ingredients[at].quantity);
      check(changed.length > 0, "no amount changed");
      soft(context, after.ingredients.some((item) => /tomato/.test(item.name) && item.quantity === "1200"), "800 g tomatoes did not become 1200 g");
      check(idsKept(before.recipe, after, "ingredients") >= before.recipe.ingredients.length - 1, "scaling replaced ingredient rows instead of changing them in place");
    },
  },
  {
    name: "edit-vegan",
    proves: "\"Make it vegan\" proposes ingredient and step changes, and Accept saves a recipe without the animal products, keeping untouched rows (and their step moments).",
    usesModel: true,
    async run(context) {
      const before = await recipe(context);
      const { answer } = await ask(context, "Make it vegan.");
      proposed(answer);
      const after = await accept(context, answer, before);
      const animal = after.ingredients.filter((item) => /\b(butter|eggs?|cream|milk|cheese|honey)\b/i.test(item.name) && !/vegan|plant|dairy[- ]free|non-dairy|oat|soy|coconut|tofu|chickpea|flax/i.test(item.name));
      check(animal.length === 0, `still not vegan: ${animal.map((item) => item.name).join(", ")}`);
      soft(context, after.ingredients.some((item) => /tomato/.test(item.name) && before.recipe.ingredients.some((old) => old.id === item.id)), "the tomatoes did not keep their row");
      check(after.steps.length > 0 && after.ingredients.length > 0, "the vegan recipe lost its ingredients or steps");
    },
  },
  {
    name: "edit-remove-step",
    proves: "\"Remove step 2\" proposes one removed step, and Accept leaves one fewer step, without step 2's text, with the other steps on their own rows.",
    usesModel: true,
    async run(context) {
      const before = await recipe(context);
      const second = before.recipe.steps[1];
      check(second, "the recipe has no step 2");
      const { answer } = await ask(context, "Remove step 2.");
      proposed(answer);
      const after = await accept(context, answer, before);
      check(after.steps.length === before.recipe.steps.length - 1, `${before.recipe.steps.length} steps became ${after.steps.length}`);
      check(!after.steps.some((step) => step.id === second.id), "step 2's row is still there");
      check(idsKept(before.recipe, after, "steps") === after.steps.length, "the other steps did not keep their rows");
    },
  },
  {
    name: "edit-add-step-end",
    proves: "\"Add a final step\" proposes one added step at the end, and Accept saves it after the others.",
    usesModel: true,
    async run(context) {
      const before = await recipe(context);
      const { answer } = await ask(context, "Add a final step: let it rest for 2 minutes off the heat before serving.");
      proposed(answer);
      const after = await accept(context, answer, before);
      check(after.steps.length === before.recipe.steps.length + 1, `${before.recipe.steps.length} steps became ${after.steps.length}`);
      soft(context, /rest/i.test(after.steps.at(-1)?.text ?? ""), "the last step is not the rest");
    },
  },
  {
    name: "edit-add-step-start",
    proves: "A step added before the others (add_step with position 1) saves on Accept, the others moving down. Fails today: Accept answers 500 (UNIQUE recipe_id, section, position).",
    usesModel: true,
    async run(context) {
      const before = await recipe(context);
      const { answer } = await ask(context, "Add a new first step, before all the others: warm the serving plates in a low oven.");
      proposed(answer);
      const after = await accept(context, answer, before);
      check(after.steps.length === before.recipe.steps.length + 1, `${before.recipe.steps.length} steps became ${after.steps.length}`);
      check(/plate/i.test(after.steps[0]?.text ?? ""), `the first step is "${after.steps[0]?.text}"`);
      check(idsKept(before.recipe, after, "steps") === before.recipe.steps.length, "the old steps did not keep their rows");
    },
  },
  {
    name: "edit-reorder",
    proves: "A pure move (move_ingredient) proposes a moved item and Accept saves the new order on the same rows. Fails today for the same UNIQUE constraint.",
    usesModel: true,
    async run(context) {
      const before = await recipe(context);
      const last = before.recipe.ingredients.at(-1);
      check(last && before.recipe.ingredients.length > 1, "too few ingredients to move");
      const { answer } = await ask(context, `Move "${last.name}" to the top of the ingredient list. Change nothing else.`);
      proposed(answer);
      const after = await accept(context, answer, before);
      check(after.ingredients[0]?.id === last.id, "the moved ingredient is not first on its own row");
      check(after.ingredients.length === before.recipe.ingredients.length, "the move added or removed ingredients");
    },
  },
  {
    name: "search-then-edit",
    proves: "A turn that searches the web and then edits (hosted search followed by function calls) completes: the follow-up request after a search round is accepted by the provider, and the proposal saves.",
    usesModel: true,
    async run(context) {
      const before = await recipe(context);
      // Asks for a step no earlier case adds (storing leftovers), so the model
      // has nothing to decline, and the request is an explicit edit.
      const { answer } = await ask(context, "Search the web for how many days leftover shakshuka keeps in the fridge, then add a new final step to the method about storing leftovers that gives that number of days.");
      check(answer.status === "done", `the answer ${answer.status}: ${answer.content} (a failure here after a search round points at the replayed search items)`);
      proposed(answer);
      await accept(context, answer, before);
    },
  },
  {
    name: "deny",
    proves: "Deny marks the proposal denied, the drawer says so, and the recipe, its history, and the dividers stay exactly as they were.",
    usesModel: true,
    async run(context) {
      const before = await recipe(context);
      const { answer } = await ask(context, "Make it gluten-free and halve the salt.");
      proposed(answer);
      const reply = await context.owner.post(`${recipePath(context)}/chat/${answer.id}/deny`, { back: recipePath(context) });
      check(reply.status === 303 && reply.flash === "change_declined", `Deny answered ${reply.status} ${reply.flash}`);
      const after = await recipe(context);
      check(after.chat.notice === "Change denied. The recipe is as it was.", `the drawer says "${after.chat.notice}"`);
      check(answerTo(after.chat, answer.question)?.proposal?.state === "discarded", "the answer is not marked denied");
      check(content(after.recipe) === content(before.recipe), "Deny changed the recipe");
      check(readVersions(after.html).length === readVersions(before.html).length, "Deny recorded a version");
      const accepted = await context.owner.post(`${recipePath(context)}/chat/${answer.id}/accept`, { back: recipePath(context) });
      check(accepted.status === 303 && accepted.flash === "", "a denied proposal can still be accepted");
      check(content((await recipe(context)).recipe) === content(before.recipe), "accepting a denied proposal changed the recipe");
    },
  },
  {
    name: "stale",
    proves: "A hand edit between proposal and Accept makes the proposal stale: the drawer hides Accept and offers Dismiss, a forced Accept is refused with change_stale, and the hand edit stands.",
    usesModel: true,
    async run(context) {
      const before = await recipe(context);
      const { answer } = await ask(context, "Double the garlic.");
      proposed(answer);
      const title = `${before.recipe.title} (hand edit)`;
      const edited = await context.owner.post(recipePath(context), {
        title,
        description: before.recipe.description,
        servings: String(before.recipe.servings ?? ""),
        prep_minutes: String(before.recipe.prepMinutes ?? ""),
        cook_minutes: String(before.recipe.cookMinutes ?? ""),
      });
      check(edited.status === 303, `the hand edit answered ${edited.status}`);
      const shown = await recipe(context);
      const stale = answerTo(shown.chat, answer.question)?.proposal;
      check(stale?.stale && !stale.canAccept && stale.canDeny, "the drawer still offers Accept on a stale proposal");
      const reply = await context.owner.post(`${recipePath(context)}/chat/${answer.id}/accept`, { back: recipePath(context) });
      check(reply.status === 303 && reply.flash === "change_stale", `a stale Accept answered ${reply.status} ${reply.flash}`);
      const after = await recipe(context);
      check(after.recipe.title === title, "the hand edit was lost");
      check(content({ ...after.recipe, title: before.recipe.title }) === content(before.recipe), "a stale Accept changed the recipe");
      const dismissed = await context.owner.post(`${recipePath(context)}/chat/${answer.id}/deny`, { back: recipePath(context) });
      check(dismissed.status === 303 && dismissed.flash === "change_declined", "Dismiss did not deny the stale proposal");
    },
  },
  {
    name: "privacy",
    proves: "Another account cannot see the recipe or its thread, and gets 404 for asking, accepting, denying, keeping, clearing, and the drawer, while the owner's proposal stays ready.",
    needsOther: true,
    usesModel: true,
    async run(context) {
      const other = context.other!;
      const { answer } = await ask(context, "Halve the pepper or paprika.");
      proposed(answer);
      const before = await recipe(context);
      const base = recipePath(context);
      check((await other.get(base)).status === 404, "the other account can open the recipe");
      for (const [path, form] of [
        [`${base}/chat`, { question: "Hello?", back: base }],
        [`${base}/chat/${answer.id}/accept`, { back: base }],
        [`${base}/chat/${answer.id}/deny`, { back: base }],
        [`${base}/chat/${answer.id}/note`, { back: base }],
        [`${base}/chat/clear`, { back: base }],
        [`${base}/chat/drawer`, { open: "on", back: base }],
      ] as const) {
        const reply = await other.post(path, form);
        check(reply.status === 404, `the other account got ${reply.status} for POST ${path.replace(base, "/recipes/…")}`);
      }
      const home = await other.page("/");
      check(!home.includes(context.recipeId) && !home.includes(context.settings.runId), "the other account's library lists the recipe");
      const after = await recipe(context);
      check(answerTo(after.chat, answer.question)?.proposal?.state === "ready", "the owner's proposal is no longer ready");
      check(content(after.recipe) === content(before.recipe) && after.chat.rows.length === before.chat.rows.length, "the other account changed the recipe or thread");
      await context.owner.post(`${base}/chat/${answer.id}/deny`, { back: base });
    },
  },
  {
    name: "clear",
    proves: "Clear deletes the whole thread (proposals and dividers included) and the drawer shows its empty state.",
    usesModel: false,
    async run(context) {
      const reply = await context.owner.post(`${recipePath(context)}/chat/clear`, { back: recipePath(context) });
      check(reply.status === 303 && reply.flash === "chat_cleared", `Clear answered ${reply.status} ${reply.flash}`);
      const { chat } = await recipe(context);
      check(chat.notice === "Conversation cleared.", `the drawer says "${chat.notice}"`);
      check(chat.rows.length === 0 && chat.empty, "messages are left after Clear");
    },
  },
];

/** Signed-out checks: they need no account and change nothing. */
export async function signedOutChecks(base: string): Promise<{ name: string; ok: boolean; detail: string }[]> {
  const anonymous = new Client(base, "signed-out");
  const results: { name: string; ok: boolean; detail: string }[] = [];
  const record = (name: string, ok: boolean, detail: string) => results.push({ name, ok, detail });
  const health = await anonymous.get("/healthz");
  record("healthz", health.status === 200 && health.body.trim() === "ok", `${health.status} ${health.body.trim().slice(0, 40)}`);
  const page = await anonymous.get("/recipes/e2e-missing?view=chart");
  const next = new URL(page.location || "/", base).searchParams.get("next");
  record("GET a recipe → sign-in with next", page.status === 303 && page.location.startsWith("/login?") && next === "/recipes/e2e-missing?view=chart", `${page.status} → ${page.location}`);
  const htmx = await anonymous.get("/recipes/e2e-missing", { headers: { "HX-Request": "true" } });
  record("htmx GET → HX-Redirect to sign-in", (htmx.headers.get("hx-redirect") ?? "").startsWith("/login"), `${htmx.status} HX-Redirect: ${htmx.headers.get("hx-redirect")}`);
  const routes = [
    "/recipes/e2e-missing/chat",
    "/recipes/e2e-missing/chat/drawer",
    "/recipes/e2e-missing/chat/clear",
    "/recipes/e2e-missing/chat/e2e-message/note",
    "/recipes/e2e-missing/chat/e2e-message/accept",
    "/recipes/e2e-missing/chat/e2e-message/deny",
    "/settings/web-search",
    "/settings/web-search/delete",
    "/settings/recipe-chat",
  ];
  for (const route of routes) {
    const reply = await anonymous.post(route, { back: "/recipes/e2e-missing" });
    record(`POST ${route} → sign-in`, reply.status === 303 && reply.location === "/login", `${reply.status} → ${reply.location}`);
  }
  const alter = await anonymous.get("/recipes/e2e-missing/ai/alter");
  record("GET the old alter page → sign-in", alter.status === 303 && alter.location.startsWith("/login"), `${alter.status} → ${alter.location}`);
  const login = await anonymous.get("/login");
  record("sign-in page renders", login.status === 200 && /name="password"/.test(login.body), String(login.status));
  return results;
}
