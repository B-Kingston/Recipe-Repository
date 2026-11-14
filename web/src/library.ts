/* Library: a ticket's thumbnail pins its full dish photo open, the live search
 * leaves an empty query out of the address bar, and an open folder comes back
 * on Back or refresh. The search itself is htmx attributes in home.html. */
namespace Recipes {
  const OPEN_FOLDER_KEY = "recipe_open_folder";
  let firstPage = true;

  function rememberedFolder(): string | null {
    try { return window.sessionStorage.getItem(OPEN_FOLDER_KEY); } catch (error) { return null; }
  }

  function rememberFolder(slug: string | null): void {
    try {
      if (slug) window.sessionStorage.setItem(OPEN_FOLDER_KEY, slug);
      else window.sessionStorage.removeItem(OPEN_FOLDER_KEY);
    } catch (error) { /* Private windows may refuse storage; the folder is just not remembered. */ }
  }

  function folderUrl(slug: string): string {
    return "/?folder=" + encodeURIComponent(slug);
  }

  /** This document came from a refresh or Back, not from following a link. */
  function returnedToPage(): boolean {
    const timing = window.performance && window.performance.navigation;
    return !!timing && (timing.type === 1 || timing.type === 2);
  }

  // Opening a folder leaves the address bar at "/", so the folder is kept here
  // for the tab. Any library page without one, such as the Recipes link, forgets it.
  onPage((root) => {
    const returned = firstPage && returnedToPage();
    firstPage = false;
    const results = within(root, "#search-results")[0];
    if (!results) return;
    const open = results.getAttribute("data-open-folder");
    const remembered = rememberedFolder();
    if (!open && returned && remembered && !window.location.search && typeof htmx !== "undefined") {
      htmx.ajax("GET", folderUrl(remembered), { target: "#all-recipes", select: "#all-recipes", swap: "outerHTML show:none" });
      return;
    }
    rememberFolder(open);
  });

  // Back to "/" through htmx: fetch the folder in place of the overview, so the
  // overview never shows. htmx opens the request just after this event.
  document.addEventListener("htmx:historyCacheMiss", ((event: HtmxEvent) => {
    const slug = rememberedFolder();
    if (!slug || event.detail.path !== "/") return;
    const xhr = event.detail.xhr;
    const open = xhr.open as (method: string, url: string, async: boolean) => void;
    xhr.open = ((method: string, _url: string, async: boolean) => open.call(xhr, method, folderUrl(slug), async)) as typeof xhr.open;
  }) as EventListener);

  document.addEventListener("click", (event) => {
    const toggle = closest(event.target, "[data-ticket-photo]");
    const ticket = toggle && closest(toggle, ".ticket");
    if (!ticket) return;
    const open = !ticket.classList.contains("is-photo-open");
    if (open) ticket.classList.add("is-photo-open");
    else ticket.classList.remove("is-photo-open");
    const button = ticket.querySelector(".ticket-thumb-toggle");
    if (!button) return;
    button.setAttribute("aria-expanded", open ? "true" : "false");
    button.setAttribute("aria-label", open ? "Hide dish photo" : "Show dish photo");
  });

  // "/?q=" with nothing after it is just the library: send "/" instead, so
  // the address bar reads "/" when the field is cleared.
  document.addEventListener("htmx:configRequest", ((event: HtmxEvent) => {
    const params = event.detail.parameters;
    if (!closest(event.detail.elt, "[data-live-search]")) return;
    const query = params.q;
    if (typeof query === "string" && !query.replace(/\s+/g, "")) delete params.q;
  }) as EventListener);
}
