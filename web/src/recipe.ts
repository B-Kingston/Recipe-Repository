/* Recipe page: the cooking-step timer counts down and announces its end once,
 * the arrow keys step through the chart, an ingredient or step link opens its
 * edit form without a reload, and a refreshed dish-photo panel above the
 * viewport keeps the page from jumping. The chart links and the pending
 * photo and moment polls are plain htmx attributes in recipe.html. */
namespace Recipes {
  let timerId: number | null = null;

  function format(seconds: number): string {
    const minutes = Math.floor(seconds / 60);
    const remainder = seconds % 60;
    return minutes + ":" + (remainder < 10 ? "0" : "") + remainder;
  }

  function stopTimer(): void {
    if (timerId) { window.clearInterval(timerId); timerId = null; }
  }

  function startTimer(detail: HTMLElement): void {
    stopTimer();
    const seconds = parseInt(detail.getAttribute("data-timer-seconds") || "", 10) || 0;
    const output = detail.querySelector("[data-timer-value]");
    if (!seconds || !output) return;
    const deadline = new Date().getTime() + seconds * 1000;
    const tick = () => {
      // The step was swapped out: this timer belongs to a page no longer shown.
      if (!attached(detail)) { stopTimer(); return; }
      const remaining = Math.max(0, Math.ceil((deadline - new Date().getTime()) / 1000));
      output.textContent = format(remaining);
      if (!remaining) {
        stopTimer();
        const note = detail.querySelector("[data-timer-announce]");
        if (note) note.textContent = "Time's up";
      }
    };
    tick();
    timerId = window.setInterval(tick, 250);
  }

  onPage((root) => {
    each(within(root, "[data-chart-detail]"), startTimer);
    each(within(root, ".chart-cell.is-active"), (cell) => {
      if (cell.scrollIntoView) cell.scrollIntoView(false);
    });
  });

  document.addEventListener("click", (event) => {
    const link = closest(event.target, "[data-edit-block]");
    if (!link) return;
    const form = document.getElementById("edit-" + link.getAttribute("data-edit-block")) as HTMLFormElement | null;
    if (!form) return;
    event.preventDefault();
    form.hidden = false;
    if (form.scrollIntoView) form.scrollIntoView(true);
    const field = form.querySelector("input,textarea") as HTMLElement | null;
    if (field) field.focus();
  });

  document.addEventListener("keydown", (event) => {
    if (event.altKey || event.ctrlKey || event.metaKey || isTyping(event.target)) return;
    // The still viewer has the arrow keys while it is open.
    if (document.body.classList.contains("still-viewer-open")) return;
    const selector = event.keyCode === 37 ? "[data-chart-back]" : event.keyCode === 39 ? "[data-chart-next]" : null;
    const link = selector ? document.querySelector(selector) as HTMLElement | null : null;
    if (!link) return;
    event.preventDefault();
    // A click takes the same htmx route as a tap on the link.
    link.click();
  });

  // The dish-photo panel can change height when its poll swaps it. When the
  // panel sits above the viewport, scroll by the difference so the text being
  // read stays put.
  let thumbAbove: { bottom: number; offset: number } | null = null;
  document.addEventListener("htmx:beforeSwap", ((event: HtmxEvent) => {
    const target = event.detail.target;
    thumbAbove = null;
    if (!target || !target.className || !/(^| )thumb-manage( |$)/.test(target.className)) return;
    const box = target.getBoundingClientRect();
    if (box.bottom <= 0) thumbAbove = { bottom: box.bottom, offset: window.pageYOffset || 0 };
  }) as EventListener);
  document.addEventListener("htmx:afterSwap", () => {
    if (!thumbAbove) return;
    const fresh = document.querySelector(".thumb-manage");
    if (fresh && window.scrollTo) window.scrollTo(0, thumbAbove.offset + fresh.getBoundingClientRect().bottom - thumbAbove.bottom);
    thumbAbove = null;
  });
}
