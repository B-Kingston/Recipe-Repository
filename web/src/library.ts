/* Library: a ticket's thumbnail pins its full dish photo open, the live search
 * leaves an empty query out of the address bar, and tickets on the rail curl
 * up when hovered. The search itself is htmx attributes in home.html. */
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

  /* Hovering a ticket on the rail curls up the bottom corner nearest the
   * pointer, as if grabbed. The bend is nothing at the top, then ramps up
   * steeply towards the bottom, so it can't be a CSS transform: each ticket
   * gets an SVG displacement filter (set through --curl) whose strength
   * animates. Listeners are delegated, so tickets swapped in by a search or a
   * page change curl too. */
  export interface TicketCurl {
    id: string;
    filter: SVGElement;
    dispImage: SVGElement;
    displace: SVGElement;
    shadeImage: SVGElement;
    shadeAlpha: SVGElement;
    amount: number;
    target: number;
    frame: number;
  }

  const SVG_NS = "http://www.w3.org/2000/svg";
  const LIFT = 36;       // displacement scale: the bottom corner rises up to LIFT / 2 px
  const FLAT = 0.4;      // the top 40% of the ticket stays flat
  const DURATION = 260;
  const SPREAD = 0.1;    // the maps reach 10% past each side for paper that bends outwards
  const maps: { [side: string]: { disp: string; shade: string } } = {};
  let curlCount = 0;
  let defs: SVGElement | null = null;

  // Two images per side (1 lifts the right corner, -1 the left): a
  // displacement map (red moves pixels sideways, green moves them up) and the
  // faint shading on the curled paper. Both are stretched over the ticket and
  // SPREAD past each side, since curled paper widens beyond its own edges.
  function curlMaps(side: number): { disp: string; shade: string } {
    if (maps[side]) return maps[side];
    const w = 160, h = 320;
    const disp = document.createElement("canvas"), shade = document.createElement("canvas");
    disp.width = shade.width = w;
    disp.height = shade.height = h;
    const dispContext = disp.getContext("2d")!, shadeContext = shade.getContext("2d")!;
    const d = dispContext.createImageData(w, h), s = shadeContext.createImageData(w, h);
    for (let y = 0; y < h; y++) {
      const v = (y + 0.5) / h;
      const ramp = v < FLAT ? 0 : Math.pow((v - FLAT) / (1 - FLAT), 2.6);
      for (let x = 0; x < w; x++) {
        const u = -SPREAD + (x + 0.5) / w * (1 + 2 * SPREAD);
        const across = Math.min(1, Math.max(0, u));
        const lift = ramp * (0.3 + 0.7 * (side > 0 ? across : 1 - across));
        const i = (y * w + x) * 4;
        // Rising paper is foreshortened (read from lower down) and, being
        // nearer the viewer, a little wider (read from nearer the middle).
        d.data[i] = Math.round(255 * (0.5 - (u - 0.5) * 0.4 * lift));
        d.data[i + 1] = Math.round(255 * (0.5 + 0.5 * lift));
        d.data[i + 2] = 128;
        d.data[i + 3] = 255;
        s.data[i] = 17;
        s.data[i + 1] = 26;
        s.data[i + 2] = 34;
        s.data[i + 3] = Math.round(255 * 0.12 * lift);
      }
    }
    dispContext.putImageData(d, 0, 0);
    shadeContext.putImageData(s, 0, 0);
    maps[side] = { disp: disp.toDataURL(), shade: shade.toDataURL() };
    return maps[side];
  }

  function svg(name: string, attrs: { [key: string]: string }, parent?: Element | null): SVGElement {
    const node = document.createElementNS(SVG_NS, name) as SVGElement;
    for (const key in attrs) {
      if (Object.prototype.hasOwnProperty.call(attrs, key)) node.setAttribute(key, attrs[key]);
    }
    if (parent) parent.appendChild(node);
    return node;
  }

  function curlFor(ticket: HTMLElement): TicketCurl {
    if (ticket.recipeCurl && attached(ticket.recipeCurl.filter)) return ticket.recipeCurl;
    // A page swap replaces the body, and the filters with it.
    if (!defs || !attached(defs)) {
      const holder = svg("svg", { width: "0", height: "0", "aria-hidden": "true", style: "position:absolute" });
      document.body.appendChild(holder);
      defs = svg("defs", {}, holder);
    }
    const id = "ticket-curl-" + (++curlCount);
    const filter = svg("filter", { id, filterUnits: "userSpaceOnUse", primitiveUnits: "userSpaceOnUse", y: "0", "color-interpolation-filters": "sRGB" }, defs);
    const dispImage = svg("feImage", { y: "0", preserveAspectRatio: "none", result: "map" }, filter);
    const displace = svg("feDisplacementMap", { "in": "SourceGraphic", in2: "map", scale: "0", xChannelSelector: "R", yChannelSelector: "G", result: "bent" }, filter);
    const shadeImage = svg("feImage", { y: "0", preserveAspectRatio: "none", result: "shadeMap" }, filter);
    const fade = svg("feComponentTransfer", { "in": "shadeMap", result: "shade" }, filter);
    const shadeAlpha = svg("feFuncA", { type: "linear", slope: "0" }, fade);
    svg("feComposite", { "in": "shade", in2: "bent", operator: "atop" }, filter);
    const curl: TicketCurl = { id, filter, dispImage, displace, shadeImage, shadeAlpha, amount: 0, target: 0, frame: 0 };
    ticket.recipeCurl = curl;
    return curl;
  }

  function setHref(image: SVGElement, href: string): void {
    image.setAttribute("href", href);
    image.setAttributeNS("http://www.w3.org/1999/xlink", "xlink:href", href);
  }

  function draw(ticket: HTMLElement, curl: TicketCurl): void {
    const eased = curl.amount < 0.5 ? 2 * curl.amount * curl.amount : 1 - Math.pow(-2 * curl.amount + 2, 2) / 2;
    curl.displace.setAttribute("scale", String(LIFT * eased));
    curl.shadeAlpha.setAttribute("slope", String(eased));
    if (curl.amount === 0 && curl.target === 0) ticket.style.removeProperty("--curl");
  }

  function animate(ticket: HTMLElement, curl: TicketCurl, reduced: MediaQueryList): void {
    if (curl.frame) return;
    let last: number | null = null;
    const step = (now: number) => {
      if (last === null) last = now;
      const delta = reduced.matches ? 1 : (now - last) / DURATION;
      last = now;
      curl.amount = curl.target > curl.amount ? Math.min(curl.target, curl.amount + delta) : Math.max(curl.target, curl.amount - delta);
      draw(ticket, curl);
      curl.frame = curl.amount === curl.target ? 0 : window.requestAnimationFrame(step);
    };
    curl.frame = window.requestAnimationFrame(step);
  }

  // Only tickets hanging from the rail curl; pinned ones show their pin.
  function onRail(ticket: HTMLElement): boolean {
    return window.getComputedStyle(ticket, "::before").display === "none";
  }

  function isTicket(target: EventTarget | null): target is HTMLElement {
    const element = target as HTMLElement | null;
    return !!element && !!element.classList && element.classList.contains("ticket");
  }

  if (window.matchMedia && window.matchMedia("(hover: hover)").matches && typeof window.requestAnimationFrame === "function") {
    const reduced = window.matchMedia("(prefers-reduced-motion: reduce)");

    document.addEventListener("mouseenter", (event) => {
      const ticket = event.target;
      if (!isTicket(ticket) || !onRail(ticket)) return;
      const curl = curlFor(ticket);
      const box = ticket.getBoundingClientRect();
      const width = ticket.offsetWidth, height = ticket.offsetHeight;
      if (curl.amount === 0) {
        const images = curlMaps(event.clientX > box.left + box.width / 2 ? 1 : -1);
        setHref(curl.dispImage, images.disp);
        setHref(curl.shadeImage, images.shade);
      }
      each([curl.filter, curl.dispImage, curl.shadeImage], (node) => {
        node.setAttribute("x", String(-width * SPREAD));
        node.setAttribute("width", String(width * (1 + 2 * SPREAD)));
        node.setAttribute("height", String(height));
      });
      ticket.style.setProperty("--curl", "url(#" + curl.id + ")");
      curl.target = 1;
      animate(ticket, curl, reduced);
    }, true);

    document.addEventListener("mouseleave", (event) => {
      const ticket = event.target;
      if (!isTicket(ticket) || !ticket.recipeCurl) return;
      ticket.recipeCurl.target = 0;
      animate(ticket, ticket.recipeCurl, reduced);
    }, true);
  }
}
