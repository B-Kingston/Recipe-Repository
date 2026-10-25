/* The page chrome: the account menu, the Add recipe mode switch, and the
 * full-page viewer for step moment stills. All delegated, so they keep
 * working after htmx swaps a page in. */
namespace Recipes {
  /* Account menu: a click outside the open panel, or Escape, closes it. */
  function accountMenu(): HTMLElement | null {
    return document.querySelector("[data-account-menu]");
  }

  document.addEventListener("click", (event) => {
    const menu = accountMenu();
    if (menu && menu.hasAttribute("open") && !menu.contains(event.target as Node)) menu.removeAttribute("open");
  });

  document.addEventListener("keydown", (event) => {
    const menu = accountMenu();
    if (!menu || !menu.hasAttribute("open") || (event.key !== "Escape" && event.key !== "Esc")) return;
    menu.removeAttribute("open");
    const summary = menu.querySelector("summary");
    if (summary) summary.focus();
  });

  /* Add recipe: one entry point with a progressive-enhancement mode switch.
   * The tabs are links, so without JavaScript each mode is its own page. */
  document.addEventListener("click", (event) => {
    const tab = closest(event.target, "[data-add-mode]") as HTMLAnchorElement | null;
    const root = tab && closest(tab, "[data-add-recipe]");
    if (!tab || !root) return;
    event.preventDefault();
    const mode = tab.getAttribute("data-add-mode");
    each(root.querySelectorAll("[data-add-mode]"), (node) => {
      const active = node.getAttribute("data-add-mode") === mode;
      node.className = "add-mode-tab" + (active ? " is-active" : "");
      node.setAttribute("aria-selected", active ? "true" : "false");
    });
    each(root.querySelectorAll("[data-add-panel]"), (panel) => {
      (panel as HTMLElement).hidden = panel.getAttribute("data-add-panel") !== mode;
    });
    if (window.history && window.history.replaceState) window.history.replaceState(window.history.state, "", tab.href);
  });

  /* Step moment stills: one click opens a still in a full-page viewer, with
   * the other stills of the same step one tap or arrow key away. Escape, the
   * close button, or a click outside the image closes it. Without JavaScript
   * the still link opens the image itself. */
  interface Viewer {
    root: HTMLElement;
    image: HTMLImageElement;
    previous: HTMLButtonElement;
    next: HTMLButtonElement;
    close: HTMLButtonElement;
  }
  let viewer: Viewer | null = null;
  let links: HTMLAnchorElement[] = [];
  let index = 0;
  let opener: HTMLElement | null = null;

  function viewerButton(root: HTMLElement, className: string, label: string, text: string, handler: () => void): HTMLButtonElement {
    const button = document.createElement("button");
    button.type = "button";
    button.className = className;
    button.setAttribute("aria-label", label);
    button.textContent = text;
    button.addEventListener("click", (event) => {
      event.stopPropagation();
      handler();
    });
    root.appendChild(button);
    return button;
  }

  // Built on first use, and again after a page swap took the old one away.
  function build(): Viewer {
    if (viewer && attached(viewer.root)) return viewer;
    const root = el("div", "still-viewer");
    root.hidden = true;
    root.setAttribute("role", "dialog");
    root.setAttribute("aria-modal", "true");
    root.setAttribute("aria-label", "Video frame");
    const image = document.createElement("img");
    image.addEventListener("click", (event) => event.stopPropagation());
    root.appendChild(image);
    const built: Viewer = {
      root,
      image,
      previous: viewerButton(root, "still-viewer-prev", "Previous frame", "‹", () => show(index - 1)),
      next: viewerButton(root, "still-viewer-next", "Next frame", "›", () => show(index + 1)),
      close: viewerButton(root, "still-viewer-close", "Close", "×", closeViewer),
    };
    root.addEventListener("click", closeViewer);
    document.body.appendChild(root);
    viewer = built;
    return built;
  }

  function show(next: number): void {
    if (!viewer || next < 0 || next >= links.length) return;
    index = next;
    const still = links[index].getElementsByTagName("img")[0];
    viewer.image.src = links[index].getAttribute("href") || "";
    viewer.image.alt = still ? still.alt : "";
    viewer.previous.hidden = index === 0;
    viewer.next.hidden = index === links.length - 1;
  }

  function openViewer(link: HTMLAnchorElement): void {
    const built = build();
    links = [];
    each(link.parentNode ? (link.parentNode as Element).querySelectorAll("[data-still-viewer]") : [], (node) => links.push(node as HTMLAnchorElement));
    opener = link;
    let position = 0;
    each(links, (candidate, at) => { if (candidate === link) position = at; });
    show(position);
    built.root.hidden = false;
    document.body.classList.add("still-viewer-open");
    built.close.focus();
  }

  function closeViewer(): void {
    // The body's class outlives a page swap, so it is cleared even when the
    // viewer itself has gone.
    document.body.classList.remove("still-viewer-open");
    if (!viewer || viewer.root.hidden) return;
    viewer.root.hidden = true;
    viewer.image.removeAttribute("src");
    if (opener && attached(opener)) opener.focus();
    opener = null;
  }

  document.addEventListener("click", (event) => {
    if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    const link = closest(event.target, "[data-still-viewer]") as HTMLAnchorElement | null;
    if (!link) return;
    event.preventDefault();
    openViewer(link);
  });

  document.addEventListener("keydown", (event) => {
    if (!viewer || viewer.root.hidden) return;
    const key = event.key || "";
    if (key === "Escape" || key === "Esc" || event.keyCode === 27) { event.preventDefault(); closeViewer(); }
    else if (key === "ArrowLeft" || event.keyCode === 37) { event.preventDefault(); show(index - 1); }
    else if (key === "ArrowRight" || event.keyCode === 39) { event.preventDefault(); show(index + 1); }
  });

  // A whole-page swap closes the viewer; a poll refreshing one panel does not.
  document.addEventListener("htmx:beforeSwap", ((event: HtmxEvent) => {
    if (event.detail.target === document.body) closeViewer();
  }) as EventListener);
}
