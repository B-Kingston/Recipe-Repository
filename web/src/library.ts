/* Library: the live search leaves an empty query out of the address bar.
 * The search itself is htmx attributes in home.html. */
namespace Recipes {
  // "/?q=" with nothing after it is just the library: send "/" instead, so
  // the address bar reads "/" when the field is cleared.
  document.addEventListener("htmx:configRequest", ((event: HtmxEvent) => {
    const params = event.detail.parameters;
    if (!closest(event.detail.elt, "[data-live-search]")) return;
    const query = params.q;
    if (typeof query === "string" && !query.replace(/\s+/g, "")) delete params.q;
  }) as EventListener);
}
