/* Small DOM helpers. The output is ES5 for older Kindle browsers, so nothing
 * here leans on NodeList.forEach, Element.closest, or other later APIs; the
 * type checker only sees the ES5 library, and these fill the DOM gaps. */
namespace Recipes {
  type MatchingElement = Element & {
    webkitMatchesSelector?: (selector: string) => boolean;
    msMatchesSelector?: (selector: string) => boolean;
  };

  export function matches(element: Element, selector: string): boolean {
    const candidate = element as MatchingElement;
    const test = candidate.matches || candidate.webkitMatchesSelector || candidate.msMatchesSelector;
    return !!test && test.call(element, selector);
  }

  /** Every element matching `selector` within `root`, `root` included. */
  export function within(root: Element, selector: string): HTMLElement[] {
    const found: HTMLElement[] = [];
    if (root.nodeType === 1 && matches(root, selector)) found.push(root as HTMLElement);
    const nodes = root.querySelectorAll(selector);
    for (let index = 0; index < nodes.length; index += 1) found.push(nodes[index] as HTMLElement);
    return found;
  }

  /** The element itself or its nearest ancestor matching `selector`. */
  export function closest(node: EventTarget | Node | null, selector: string): HTMLElement | null {
    let current = node as Node | null;
    while (current && current.nodeType === 1) {
      if (matches(current as Element, selector)) return current as HTMLElement;
      current = current.parentNode;
    }
    return null;
  }

  export function each<T>(list: ArrayLike<T>, visit: (item: T, index: number) => void): void {
    for (let index = 0; index < list.length; index += 1) visit(list[index], index);
  }

  export function attached(node: Node | null): boolean {
    return !!node && document.body.contains(node);
  }

  /** True the first time `feature` is set up on `element`, false after. */
  export function firstTime(element: HTMLElement, feature: string): boolean {
    const ready = element.recipeReady || (element.recipeReady = {});
    if (ready[feature]) return false;
    ready[feature] = true;
    return true;
  }

  export function el(tag: string, className?: string | null, text?: string | null): HTMLElement {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined && text !== null) node.textContent = text;
    return node;
  }

  export function clear(node: Node | null): void {
    while (node && node.firstChild) node.removeChild(node.firstChild);
  }

  export function isTyping(target: EventTarget | null): boolean {
    const element = target as Element | null;
    return !!element && !!element.tagName && /^(input|textarea|select)$/i.test(element.tagName);
  }

  /**
   * GETs `url` and hands its JSON to `done`, or null when the request failed
   * or the body was not JSON. Returns the request so a caller can abort it.
   */
  export function getJson<T>(url: string, done: (payload: T | null, status: number) => void): XMLHttpRequest {
    const request = new XMLHttpRequest();
    request.open("GET", url, true);
    request.onreadystatechange = () => {
      if (request.readyState !== 4) return;
      let payload: T | null = null;
      if (request.status >= 200 && request.status < 300) {
        try { payload = JSON.parse(request.responseText) as T; } catch (ignored) { payload = null; }
      }
      done(payload, request.status);
    };
    request.send();
    return request;
  }

  /** Loads `url` as a whole page, as a link would. */
  export function go(url: string): void {
    window.location.href = url;
  }
}
