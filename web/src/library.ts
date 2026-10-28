/* Library: a ticket's thumbnail pins its full dish photo open, and the live
 * search leaves an empty query out of the address bar. The search itself is
 * htmx attributes in home.html. */
namespace Recipes {
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
