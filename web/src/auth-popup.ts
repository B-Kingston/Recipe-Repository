/* The Settings sign-in popup. Reconnect / Connect load a sign-in page's
 * #auth-flow into it with htmx (the links' hx-get), so signing in never leaves
 * Settings; the flow's own forms and links swap #auth-flow, so the same page
 * also works on its own. A finished step answers `popup-finished` (see
 * src/htmx.rs): the popup closes and the Settings panel refreshes with the
 * step's notice. Closing the popup empties it, which also stops the Codex
 * flow's polling. Without htmx the links open the sign-in page itself. */
namespace Recipes {
  let opener: HTMLElement | null = null;

  function popup(): HTMLElement | null {
    return document.querySelector("[data-auth-popup]") as HTMLElement | null;
  }

  function isOpen(): boolean {
    const element = popup();
    return !!element && !element.hidden;
  }

  function openPopup(trigger: HTMLElement | null): void {
    const element = popup();
    if (!element) return;
    opener = trigger;
    element.hidden = false;
    document.body.classList.add("auth-popup-open");
    const close = element.querySelector(".auth-popup-close") as HTMLElement | null;
    if (close) close.focus();
  }

  function closePopup(): void {
    // The body's class outlives a swap that took the popup away.
    document.body.classList.remove("auth-popup-open");
    const element = popup();
    if (!element || element.hidden) return;
    element.hidden = true;
    clear(element.querySelector("#auth-popup-body"));
    if (opener && attached(opener)) opener.focus();
    opener = null;
  }

  /** A sign-in finished (or gave up): back to an up-to-date Settings. */
  export function finishSignIn(): void {
    if (!isOpen() || typeof htmx === "undefined") {
      go("/settings");
      return;
    }
    closePopup();
    htmx.ajax("GET", "/settings", { target: "#settings", select: "#settings", swap: "outerHTML show:none" });
  }

  document.addEventListener("htmx:afterSwap", ((event: HtmxEvent) => {
    const target = event.detail.target;
    if (target && target.id === "auth-popup-body") {
      openPopup(event.detail.requestConfig ? event.detail.requestConfig.elt : null);
    } else if (!popup()) {
      document.body.classList.remove("auth-popup-open");
    }
  }) as EventListener);

  document.addEventListener("popup-finished", finishSignIn);

  // Close, Cancel, and "Back to Settings" close the popup in place; outside
  // it they are ordinary links. A click on the backdrop closes it too. This
  // listens in the capture phase, ahead of htmx's boost listener on the link.
  document.addEventListener("click", (event) => {
    if (!isOpen()) return;
    const element = popup();
    if (event.target === element) {
      closePopup();
      return;
    }
    const close = closest(event.target, "[data-auth-close]");
    if (!close || !element || !element.contains(close)) return;
    event.preventDefault();
    event.stopPropagation();
    closePopup();
  }, true);

  document.addEventListener("keydown", (event) => {
    if (isOpen() && (event.key === "Escape" || event.key === "Esc")) closePopup();
  });
}
