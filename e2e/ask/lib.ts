/**
 * HTTP client and page readers for the Ask (recipe chat) end-to-end corpus.
 *
 * The client keeps its own cookie jar and never follows redirects on its
 * own, so every case can check the 303, its Location, and the one-time
 * `recipe_flash` notice a handler set. The readers parse the server-rendered
 * pages the way the templates write them (templates/recipe.html,
 * recipe_chat.html, recipe_notes.html, version_tabs.html); they read the
 * hidden edit forms for the recipe itself, since those carry the stored
 * values and block ids.
 *
 * Nothing here logs a request body, a password, or a cookie value.
 */

export interface Reply {
  status: number;
  location: string;
  /** The `recipe_flash` key this response set, or "". */
  flash: string;
  headers: Headers;
  body: string;
}

export interface RequestOptions {
  form?: Record<string, string>;
  multipart?: FormData;
  headers?: Record<string, string>;
  timeoutMs?: number;
}

const DEFAULT_TIMEOUT_MS = 60_000;

/** One browser: its cookie jar and the server it talks to. */
export class Client {
  readonly base: URL;
  readonly label: string;
  readonly jar = new Map<string, string>();

  constructor(base: string, label: string) {
    this.base = new URL(base);
    this.label = label;
  }

  async request(method: string, path: string, options: RequestOptions = {}): Promise<Reply> {
    const url = new URL(path, this.base);
    const headers: Record<string, string> = { Accept: "text/html", ...options.headers };
    const cookie = [...this.jar].map(([name, value]) => `${name}=${value}`).join("; ");
    if (cookie) headers.Cookie = cookie;
    let body: BodyInit | undefined;
    if (options.form) {
      headers["Content-Type"] = "application/x-www-form-urlencoded";
      body = new URLSearchParams(options.form).toString();
    } else if (options.multipart) {
      body = options.multipart;
    }
    let response: Response;
    try {
      response = await fetch(url, {
        method,
        headers,
        body,
        redirect: "manual",
        signal: AbortSignal.timeout(options.timeoutMs ?? DEFAULT_TIMEOUT_MS),
      });
    } catch (error) {
      throw new Error(`${this.label}: ${method} ${url.pathname} failed: ${error instanceof Error ? error.message : String(error)}`);
    }
    let flash = "";
    for (const line of response.headers.getSetCookie()) {
      const [pair, ...attributes] = line.split(";");
      const at = pair.indexOf("=");
      const name = pair.slice(0, at).trim();
      const value = pair.slice(at + 1).trim();
      const expired = attributes.some((attribute) => /^\s*max-age=0\s*$/i.test(attribute)) || value === "";
      if (expired) this.jar.delete(name);
      else this.jar.set(name, value);
      if (name === "recipe_flash" && !expired) flash = value;
    }
    return {
      status: response.status,
      location: response.headers.get("location") ?? "",
      flash,
      headers: response.headers,
      body: await response.text(),
    };
  }

  get(path: string, options: RequestOptions = {}): Promise<Reply> {
    return this.request("GET", path, options);
  }

  post(path: string, form: Record<string, string> = {}, options: RequestOptions = {}): Promise<Reply> {
    return this.request("POST", path, { ...options, form });
  }

  /** A page that must render: 200 and HTML. */
  async page(path: string): Promise<string> {
    const reply = await this.get(path);
    if (reply.status !== 200) throw new Error(`${this.label}: GET ${path} answered ${reply.status}${reply.location ? ` → ${reply.location}` : ""}`);
    return reply.body;
  }

  /** Signs in through the native sign-in form. Returns false when refused. */
  async signIn(username: string, password: string): Promise<boolean> {
    const reply = await this.post("/login", { username, password, next: "/" });
    return reply.status === 303 && this.jar.has("recipe_session") && !reply.location.startsWith("/login");
  }

  async signOut(): Promise<void> {
    if (this.jar.has("recipe_session")) await this.post("/logout");
  }
}

/** Text from HTML: tags dropped, entities decoded, whitespace collapsed. */
export function text(html: string): string {
  return decode(html.replace(/<[^>]*>/g, " ")).replace(/\s+/g, " ").trim();
}

/** Decodes the entities Askama writes (numeric) and the common named ones. */
export function decode(value: string): string {
  return value
    .replace(/&#x([0-9a-f]+);/gi, (_, hex: string) => String.fromCodePoint(parseInt(hex, 16)))
    .replace(/&#(\d+);/g, (_, number: string) => String.fromCodePoint(Number(number)))
    .replace(/&quot;/g, "\"").replace(/&#39;|&apos;/g, "'").replace(/&lt;/g, "<").replace(/&gt;/g, ">").replace(/&nbsp;/g, " ")
    .replace(/&amp;/g, "&");
}

function attribute(tag: string, name: string): string | null {
  const match = tag.match(new RegExp(`\\s${name}="([^"]*)"`));
  return match ? decode(match[1]) : null;
}

export interface Ingredient {
  id: string;
  name: string;
  quantity: string;
  unit: string;
  optional: boolean;
}

export interface Step {
  id: string;
  text: string;
  ingredients: string[];
}

/** The recipe as stored, read from the card page's edit forms. */
export interface RecipeView {
  title: string;
  description: string;
  servings: number | null;
  prepMinutes: number | null;
  cookMinutes: number | null;
  ingredients: Ingredient[];
  steps: Step[];
}

function fact(html: string, name: string): number | null {
  const match = html.match(new RegExp(`<dt>${name}</dt><dd>(\\d+)`));
  return match ? Number(match[1]) : null;
}

export function readRecipe(html: string): RecipeView {
  const header = html.match(/<header class="page-header">([\s\S]*?)<\/header>/)?.[1] ?? "";
  const ingredients: Ingredient[] = [];
  const steps: Step[] = [];
  for (const match of html.matchAll(/<form id="edit-([^"]+)" class="edit-form"[^>]*>([\s\S]*?)<\/form>/g)) {
    const [, id, form] = match;
    const stepText = form.match(/<textarea name="text"[^>]*>([\s\S]*?)<\/textarea>/);
    if (stepText) {
      const uses = form.match(/name="step_ingredients"[^>]*>([\s\S]*?)<\/textarea>/)?.[1] ?? "";
      steps.push({
        id,
        text: decode(stepText[1]).trim(),
        ingredients: decode(uses).split("\n").map((line) => line.trim()).filter(Boolean),
      });
      continue;
    }
    const input = (name: string) => attribute(form.match(new RegExp(`<input name="${name}"[^>]*>`))?.[0] ?? "", "value") ?? "";
    ingredients.push({
      id,
      name: input("text"),
      quantity: input("quantity"),
      unit: input("unit"),
      optional: /name="optional"\s+checked/.test(form),
    });
  }
  return {
    title: text(header.match(/<h1>([\s\S]*?)<\/h1>/)?.[1] ?? ""),
    description: text(header.match(/<p class="deck">([\s\S]*?)<\/p>/)?.[1] ?? ""),
    servings: fact(html, "Serves"),
    prepMinutes: fact(html, "Prep"),
    cookMinutes: fact(html, "Cook"),
    ingredients,
    steps,
  };
}

/** The recipe without block ids, for comparing content. */
export function content(recipe: RecipeView): string {
  return JSON.stringify({
    ...recipe,
    ingredients: recipe.ingredients.map(({ id: _id, ...rest }) => rest),
    steps: recipe.steps.map(({ id: _id, ...rest }) => rest),
  });
}

export interface VersionTab {
  version: number;
  label: string;
}

/** The version tabs above the card, newest first. */
export function readVersions(html: string): VersionTab[] {
  const nav = html.match(/<nav class="version-tabs"[\s\S]*?<\/nav>/)?.[0] ?? "";
  return [...nav.matchAll(/>v(\d+) · ([^<]*)</g)].map((match) => ({ version: Number(match[1]), label: decode(match[2]).trim() }));
}

export interface ChatAnswer {
  kind: "answer";
  /** The message id (from its forms); "" while pending or failed, and once
   * the answer is kept with no open proposal, since its forms are gone. */
  id: string;
  /** The question it answers, which finds it again in a later page. */
  question: string;
  status: "pending" | "done" | "failed" | "needs_settings";
  content: string;
  kept: boolean;
  canKeep: boolean;
  proposal: Proposal | null;
}

export interface Proposal {
  /** "ready", "applied", "discarded", "unreadable", or "failed". */
  state: string;
  label: string;
  summary: string;
  items: number;
  canAccept: boolean;
  canDeny: boolean;
  stale: boolean;
  /** The diff as text, with + / − marks, for reports. */
  diff: string;
}

export type ChatRow =
  | { kind: "question"; content: string }
  | { kind: "change"; content: string }
  | ChatAnswer;

export interface ChatView {
  /** The drawer is on the page (the chat is on and the drawer open). */
  open: boolean;
  notice: string;
  /** The poll token; "" when nothing is running. */
  pollState: string;
  empty: boolean;
  rows: ChatRow[];
  questionDisabled: boolean;
}

function readProposal(li: string): Proposal | null {
  const block = li.match(/<div class="chat-proposal">([\s\S]*)<\/div>\s*$/)?.[1] ?? li.match(/<div class="chat-proposal">([\s\S]*)/)?.[1];
  if (block === undefined) return null;
  const canAccept = /\/chat\/[^/"]+\/accept"/.test(block);
  const canDeny = /\/chat\/[^/"]+\/deny"/.test(block);
  const state = block.includes("Saved to the recipe.")
    ? "applied"
    : block.includes("Denied. The recipe was left as it was.")
      ? "discarded"
      : block.includes("This change can no longer be shown.")
        ? "unreadable"
        : canDeny
          ? "ready"
          : "failed";
  const diff = [...block.matchAll(/<li class="chat-diff-item[^"]*">([\s\S]*?)<\/li>/g)].map((item) => text(item[1]));
  return {
    state,
    label: text(block.match(/<p class="chat-proposal-label">([\s\S]*?)<\/p>/)?.[1] ?? ""),
    summary: text(block.match(/<p class="muted chat-proposal-summary">([\s\S]*?)<\/p>/)?.[1] ?? ""),
    items: diff.length,
    canAccept,
    canDeny,
    stale: block.includes("can no longer be saved"),
    diff: diff.join("\n"),
  };
}

export function readChat(html: string): ChatView {
  const section = html.match(/<section id="recipe-chat"[\s\S]*?<\/section>/)?.[0];
  if (!section) return { open: false, notice: "", pollState: "", empty: true, rows: [], questionDisabled: false };
  const rows: ChatRow[] = [];
  const thread = section.match(/<ol class="chat-thread"[^>]*>([\s\S]*)<\/ol>/)?.[1] ?? "";
  const starts = [...thread.matchAll(/<li class="(chat-turn chat-question|chat-change|chat-turn chat-answer)">/g)];
  let question = "";
  starts.forEach((start, at) => {
    const li = thread.slice(start.index, at + 1 < starts.length ? starts[at + 1].index : thread.length);
    if (start[1] === "chat-turn chat-question") {
      question = text(li.match(/<p class="chat-bubble">([\s\S]*?)<\/p>/)?.[1] ?? "");
      rows.push({ kind: "question", content: question });
    } else if (start[1] === "chat-change") {
      rows.push({ kind: "change", content: text(li.match(/<span class="chat-change-label"[^>]*>([\s\S]*?)<\/span>\s*<span class="chat-change-line"/)?.[1] ?? "").replace(/^Recipe changed here:\s*/, "") });
    } else {
      const id = li.match(/\/chat\/([^/"]+)\/(?:note|accept|deny)"/)?.[1] ?? "";
      const failed = li.match(/<p class="chat-bubble is-failed"[^>]*>([\s\S]*?)<\/p>/);
      const status = li.includes("chat-bubble is-pending")
        ? "pending"
        : failed
          ? (failed[1].includes("/settings#settings-ai") ? "needs_settings" : "failed")
          : "done";
      const bubble = failed?.[1] ?? li.match(/<p class="chat-bubble">([\s\S]*?)<\/p>/)?.[1] ?? "";
      rows.push({
        kind: "answer",
        id,
        question,
        status,
        content: decode(bubble.replace(/<a [\s\S]*?<\/a>/g, "")).trim(),
        kept: li.includes("Kept in notes"),
        canKeep: /\/chat\/[^/"]+\/note"/.test(li),
        proposal: readProposal(li),
      });
    }
  });
  return {
    open: true,
    notice: text(section.match(/<p class="notice" role="status">([\s\S]*?)<\/p>/)?.[1] ?? ""),
    pollState: attribute(section.match(/<span hidden data-chat-state="[^"]*"/)?.[0] ?? "", "data-chat-state") ?? "",
    empty: section.includes("chat-empty"),
    rows,
    questionDisabled: /data-chat-question disabled/.test(section),
  };
}

export interface Note {
  id: string;
  body: string;
  hasPhoto: boolean;
  fromChat: boolean;
}

export function readNotes(html: string): Note[] {
  const section = html.match(/<section class="recipe-notes"[\s\S]*?<\/section>/)?.[0] ?? "";
  return [...section.matchAll(/<li class="note" id="note-([^"]+)">([\s\S]*?)<details class="note-edit">/g)].map((match) => ({
    id: match[1],
    body: text(match[2].match(/<p class="note-body">([\s\S]*?)<\/p>/)?.[1] ?? ""),
    hasPhoto: match[2].includes("class=\"note-photo\""),
    fromChat: match[2].includes("From the chat"),
  }));
}

/** The answers in a thread, oldest first. */
export function answers(chat: ChatView): ChatAnswer[] {
  return chat.rows.filter((row): row is ChatAnswer => row.kind === "answer");
}

/** The answer that follows the last time `question` was asked. */
export function answerTo(chat: ChatView, question: string): ChatAnswer | null {
  let asked = -1;
  chat.rows.forEach((row, at) => {
    if (row.kind === "question" && row.content === question.trim()) asked = at;
  });
  if (asked === -1) return null;
  const next = chat.rows.slice(asked + 1).find((row) => row.kind === "answer");
  return next && next.kind === "answer" ? next : null;
}

export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
