/* Page wiring and the htmx glue every page shares.
 *
 * htmx swaps whole pages (and some regions) in place, so a feature cannot set
 * itself up once at load: each registers with `onPage`, and runs on the
 * first page and on every piece of content htmx swaps in afterwards. Delegated
 * listeners on `document` survive swaps and are registered once. */
namespace Recipes {
  type PageInit = (root: HTMLElement) => void;
  const inits: PageInit[] = [];

  export function onPage(init: PageInit): void {
    inits.push(init);
  }

  function wire(root: Element): void {
    each(inits, (init) => {
      try { init(root as HTMLElement); } catch (error) { if (window.console) window.console.error(error); }
    });
  }

  /** Runs every page feature: through htmx when it loaded, else once now. */
  export function start(): void {
    if (typeof htmx === "undefined") {
      if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", () => wire(document.body));
      else wire(document.body);
      return;
    }
    htmx.onLoad(wire);
  }

  function on(name: string, handler: (event: HtmxEvent) => void): void {
    document.addEventListener(name, handler as EventListener);
  }

  function isHtml(xhr: XMLHttpRequest): boolean {
    return (xhr.getResponseHeader("Content-Type") || "").indexOf("text/html") === 0;
  }

  /** An inert copy of a fetched page, for looking inside without showing it. */
  function parsePage(markup: string): HTMLElement {
    const holder = document.implementation.createHTMLDocument("").createElement("div");
    holder.innerHTML = markup;
    return holder;
  }

  // Error pages are swapped in like any other page. The server retargets them
  // to the main column (see src/htmx.rs), whatever region was asked for.
  on("htmx:beforeSwap", (event) => {
    const detail = event.detail;
    if (detail.xhr.status >= 400 && isHtml(detail.xhr)) {
      detail.shouldSwap = true;
      detail.isError = false;
    }
  });

  // A poller marked data-swap-when-gone swaps its response in only once the
  // named marker has left the page (a background job has finished).
  on("htmx:beforeSwap", (event) => {
    const marker = event.detail.elt.getAttribute("data-swap-when-gone");
    if (!marker || !event.detail.shouldSwap || event.detail.xhr.status >= 300) return;
    if (parsePage(event.detail.serverResponse).querySelector(marker)) event.detail.shouldSwap = false;
  });

  // Pollers wait while the cook is editing, so a refresh never eats an open
  // form or the text being typed.
  on("htmx:beforeRequest", (event) => {
    if (event.detail.elt.hasAttribute("data-poll-unless-editing") && editingNow()) event.preventDefault();
  });

  export function editingNow(): boolean {
    if (isTyping(document.activeElement)) return true;
    const forms = document.querySelectorAll(".edit-form");
    for (let index = 0; index < forms.length; index += 1) {
      if (!(forms[index] as HTMLElement).hidden) return true;
    }
    return !!document.querySelector(".recipe-details-form");
  }

  // When the server cannot be reached, a link falls back to a normal page
  // load, and a form gets its button back and says so. Polls and the live
  // search just try again next time.
  on("htmx:sendError", (event) => {
    const elt = event.detail.elt;
    if (elt.tagName === "A") go(event.detail.requestConfig.path);
    else if (elt.tagName === "FORM") {
      resetBusy();
      window.alert("The server could not be reached. Check the connection and try again.");
    }
  });
  // A form's request is over: whatever it swapped in, its button is usable
  // again. A poll finishing in the meantime must not re-enable it.
  on("htmx:afterRequest", (event) => {
    if (event.detail.elt.tagName === "FORM") resetBusy();
  });

  /* Destructive controls ask first, and a submitting form's primary button
   * shows its busy label and is disabled so a double tap cannot repeat the
   * request. Both listen in the capture phase, ahead of htmx's own listeners
   * on the form, so a cancelled confirm stops the request outright. Pages
   * restored from the back-forward cache get their buttons back. */
  const busy: { button: HTMLButtonElement; text: string }[] = [];

  function markBusy(form: HTMLFormElement, label: string): void {
    // An empty form submitted from elsewhere (a row button with form="id")
    // marks that button instead.
    const button = (form.querySelector("button.primary") || form.querySelector("button")
      || (form.id ? document.querySelector('button[form="' + form.id + '"]') : null)) as HTMLButtonElement | null;
    if (!button) return;
    busy.push({ button, text: button.textContent || "" });
    button.textContent = label;
    button.disabled = true;
  }

  function resetBusy(): void {
    while (busy.length) {
      const entry = busy.pop()!;
      entry.button.disabled = false;
      entry.button.textContent = entry.text;
    }
  }

  function refuse(event: Event): void {
    event.preventDefault();
    event.stopPropagation();
  }

  document.addEventListener("click", (event) => {
    const node = closest(event.target, "[data-confirm]");
    if (!node || node.nodeName === "FORM") return;
    if (!window.confirm(node.getAttribute("data-confirm") || "")) refuse(event);
  }, true);

  document.addEventListener("submit", (event) => {
    const form = event.target as HTMLFormElement | null;
    if (!form || form.nodeName !== "FORM") return;
    const message = form.getAttribute("data-confirm");
    if (message && !window.confirm(message)) {
      refuse(event);
      return;
    }
    const label = form.getAttribute("data-busy-label");
    if (label) markBusy(form, label);
  }, true);

  window.addEventListener("pageshow", resetBusy);
}
