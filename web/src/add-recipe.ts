/* Add recipe: one card for ideas, recipe links, and reels. Every link typed
 * on the card is pinned under it as a slip, sorted the way the server sorts
 * them (ai::route_prompt): a Facebook or Instagram video is read from the
 * video, and its slip names the parts ticked under Video options; any other
 * link is a page for the recipe model. The slips only mirror the textarea,
 * which stays the one field posted, so "remove" edits the text. The video
 * options and the button's import wording show only while the card links a
 * video. Without JavaScript no slips appear and the video options always
 * show. */
namespace Recipes {
  interface CardLink {
    /** The link as typed, without the punctuation around it. */
    text: string;
    host: string;
    path: string;
    video: boolean;
  }

  const FACEBOOK = /^(www\.|m\.)?facebook\.com$/;
  const FB_WATCH = /^(www\.)?fb\.watch$/;
  const INSTAGRAM = /^(www\.|m\.)?instagram\.com$/;

  /** Whether the server's `canonical_social_url` takes the link. */
  function isVideo(scheme: string, authority: string, host: string, path: string): boolean {
    if (scheme !== "https" || /[@:]/.test(authority)) return false;
    const lower = path.toLowerCase();
    const has = (part: string) => lower.indexOf(part) >= 0;
    if (FACEBOOK.test(host)) return has("/reel/") || has("/reels/") || has("/watch") || has("/videos/") || has("/share/");
    if (FB_WATCH.test(host)) return lower.replace(/\//g, "") !== "";
    if (INSTAGRAM.test(host)) return has("/p/") || has("/reel/") || has("/reels/") || has("/tv/");
    return false;
  }

  /** The word without the punctuation around it, as `ai::prompt_link` reads it. */
  function stripLink(word: string): string {
    return word.replace(/^[(<"']+/, "").replace(/[,.);>"'!?]+$/, "");
  }

  /** The `?v=` id the server keeps on a Facebook /watch link, or "". */
  function watchId(host: string, path: string, query: string): string {
    if (!FACEBOOK.test(host) || path.toLowerCase().indexOf("/watch") < 0) return "";
    const pairs = query.replace(/^\?/, "").split("&");
    for (let index = 0; index < pairs.length; index += 1) {
      const pair = pairs[index].split("=");
      if (pair[0] === "v" && pair.length === 2 && /^[A-Za-z0-9_-]{1,100}$/.test(pair[1])) return "?v=" + pair[1];
    }
    return "";
  }

  /** The links in `text`, in order, each once. */
  function cardLinks(text: string): CardLink[] {
    const links: CardLink[] = [];
    const seen: { [key: string]: boolean } = {};
    each(text.split(/\s+/), (word) => {
      const link = stripLink(word);
      const parts = /^(https?):\/\/([^\/?#]*)([^?#]*)(\?[^#]*)?/.exec(link);
      if (!parts) return;
      const host = parts[2].toLowerCase().replace(/^.*@/, "").replace(/:\d*$/, "").replace(/\.$/, "");
      const video = isVideo(parts[1], parts[2], host, parts[3]);
      // Two video links are one video when `canonical_social_url` makes them
      // the same: the same host and path, plus the id of a Facebook /watch
      // link. Anything else would be refused as two videos.
      const key = video ? host + parts[3] + watchId(host, parts[3], parts[4] || "") : link;
      if (seen[key]) return;
      seen[key] = true;
      links.push({ text: link, host: host, path: parts[3], video: video });
    });
    return links;
  }

  function site(host: string): string {
    return INSTAGRAM.test(host) ? "Instagram" : "Facebook";
  }

  function shorten(text: string): string {
    return text.length > 60 ? text.slice(0, 59) + "…" : text;
  }

  function slipButton(label: string, attribute: string, value: string): HTMLButtonElement {
    const button = el("button", null, label) as HTMLButtonElement;
    button.type = "button";
    button.setAttribute(attribute, value);
    return button;
  }

  /** The ticked Video options, by name. */
  function channels(card: HTMLElement): string[] {
    const ticked: string[] = [];
    each(card.querySelectorAll("[data-channel]"), (box) => {
      if ((box as HTMLInputElement).checked) ticked.push(box.getAttribute("data-channel") || "");
    });
    return ticked;
  }

  function slip(link: CardLink, what: string, refused: boolean): HTMLElement {
    const node = el("div", "link-slip" + (link.video ? " is-video" : " is-page") + (refused ? " is-refused" : ""));
    const path = link.path.replace(/\/+$/, "");
    if (link.video) {
      const kind = /\/reels?\//i.test(link.path) ? "Reel" : "Video";
      node.appendChild(el("span", "link-slip-kind", kind + " · " + site(link.host)));
      node.appendChild(el("span", "link-slip-url", shorten(path || link.host)));
    } else {
      node.appendChild(el("span", "link-slip-kind", "Page · " + link.host.replace(/^www\./, "")));
      node.appendChild(el("span", "link-slip-url", shorten(path || "/")));
    }
    node.appendChild(el("span", "link-slip-what", what));
    const actions = el("span", "link-slip-actions");
    if (link.video) actions.appendChild(slipButton("change", "data-slip-options", ""));
    const remove = slipButton("remove", "data-slip-remove", link.text);
    remove.setAttribute("aria-label", "Remove " + link.text + " from the card");
    actions.appendChild(remove);
    node.appendChild(actions);
    return node;
  }

  function update(card: HTMLElement): void {
    const input = card.querySelector("textarea[name=prompt]") as HTMLTextAreaElement | null;
    const slips = card.querySelector("[data-link-slips]") as HTMLElement | null;
    const options = card.querySelector("[data-video-options]") as HTMLElement | null;
    const form = card.querySelector("form");
    const submit = card.querySelector("[data-add-submit]") as HTMLElement | null;
    if (!input || !slips || !options || !form || !submit) return;
    const links = cardLinks(input.value);
    const ticked = channels(card);
    let videos = 0;
    each(links, (link) => { if (link.video) videos += 1; });
    // Rebuild only when something shown changed, so the live region stays
    // quiet while the cook types prose.
    const shown = ticked.join(",") + "\n" + links.map((link) => link.text).join("\n");
    if (slips.getAttribute("data-shown") !== shown) {
      slips.setAttribute("data-shown", shown);
      clear(slips);
      const pageNote = slips.getAttribute("data-page-note") || "";
      each(links, (link) => {
        if (!link.video) slips.appendChild(slip(link, pageNote, false));
        else if (videos > 1) slips.appendChild(slip(link, "one video per recipe: keep one", true));
        else if (!ticked.length) slips.appendChild(slip(link, "nothing ticked to read", true));
        else slips.appendChild(slip(link, ticked.join(" · "), false));
      });
      slips.hidden = links.length === 0;
    }
    options.hidden = videos === 0;
    // The button and busy wording follow what the card starts.
    if (!submit.getAttribute("data-research-label")) submit.setAttribute("data-research-label", submit.textContent || "");
    if (!form.getAttribute("data-research-busy-label")) form.setAttribute("data-research-busy-label", form.getAttribute("data-busy-label") || "");
    const mode = videos ? "data-video-" : "data-research-";
    submit.textContent = submit.getAttribute(mode + "label");
    form.setAttribute("data-busy-label", form.getAttribute(mode + "busy-label") || "");
  }

  /** Drops every word that is the link `text`, and nothing else: a longer
   * link that starts with the same text stays whole. */
  function removeLink(card: HTMLElement, text: string): void {
    const input = card.querySelector("textarea[name=prompt]") as HTMLTextAreaElement | null;
    if (!input || !text) return;
    // Split keeping the whitespace, so lines and spacing elsewhere survive.
    const words = input.value.split(/(\s+)/);
    each(words, (word, index) => { if (stripLink(word) === text) words[index] = ""; });
    input.value = words.join("").replace(/[ \t]{2,}/g, " ").replace(/^[ \t]+|[ \t]+$/g, "");
    update(card);
    input.focus();
  }

  function openOptions(card: HTMLElement): void {
    const details = card.querySelector("[data-video-options] details") as HTMLElement | null;
    if (!details) return;
    details.setAttribute("open", "");
    const summary = details.querySelector("summary");
    if (summary) {
      summary.focus();
      if (summary.scrollIntoView) summary.scrollIntoView(false);
    }
  }

  onPage((root) => {
    each(within(root, "[data-add-recipe]"), (card) => {
      if (!firstTime(card, "add-recipe")) return;
      const input = card.querySelector("textarea[name=prompt]");
      const refresh = () => update(card);
      if (input) {
        input.addEventListener("input", refresh);
        // Older Kindle browsers send no input event.
        input.addEventListener("keyup", refresh);
      }
      card.addEventListener("change", refresh);
      card.addEventListener("click", (event) => {
        const remove = closest(event.target, "[data-slip-remove]");
        if (remove) removeLink(card, remove.getAttribute("data-slip-remove") || "");
        else if (closest(event.target, "[data-slip-options]")) openOptions(card);
      });
      update(card);
    });
  });
}
