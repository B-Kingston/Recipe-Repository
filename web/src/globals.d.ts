// htmx is loaded by its own <script> before app.js (see templates/base.html).
// When it fails to load, `typeof htmx` is "undefined" and app.js wires the
// page once by itself, so every page still works as plain HTML.
declare const htmx: typeof import("htmx.org");

/** The `detail` htmx puts on its request and swap events (htmx 1.9). */
interface HtmxDetail {
  elt: HTMLElement;
  target: HTMLElement;
  xhr: XMLHttpRequest;
  /** Request path and verb, as in `requestConfig`. */
  requestConfig: { verb: string; path: string; elt: HTMLElement };
  pathInfo: { requestPath: string; finalRequestPath: string; responsePath?: string };
  /** beforeSwap only: whether to swap, and the text to swap in. */
  shouldSwap: boolean;
  isError: boolean;
  serverResponse: string;
  /** configRequest only: the parameters about to be sent. */
  parameters: { [name: string]: string | string[] };
}

type HtmxEvent = CustomEvent<HtmxDetail>;

/** One remembered curl filter per library ticket (see library.ts). */
interface HTMLElement {
  recipeCurl?: Recipes.TicketCurl;
  recipeReady?: { [feature: string]: boolean };
}
