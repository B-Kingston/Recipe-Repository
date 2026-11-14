/* Settings: the model picker follows the chosen provider (only the shown
 * panel's fields are submitted) and fills from live model discovery; the
 * video cleaner's model list follows its own provider choice; the "Add a
 * provider" form shows the server URL and model fields only for the "Other
 * server" choices. Runs again whenever a save swaps the panel in, filling
 * from the lists it already fetched while those are fresh. */
namespace Recipes {
  interface ModelList {
    models: string[];
    selected?: string;
  }

  // The server keeps a model listing for 15 minutes (ai::discover_models), so
  // a list fetched within that time is still the current one: a save that
  // swaps the panel in reuses it rather than asking again.
  const LIST_FRESH_MS = 15 * 60 * 1000;
  const lists: { [source: string]: { at: number; data: ModelList } } = {};

  function now(): number {
    return new Date().getTime();
  }

  /** The model list for `source` ("pi", "claude", or an endpoint id), handed
   * to `done` at once when a fresh one is kept, else fetched. Returns the
   * request, or null when none was needed. */
  function modelList(source: string, done: (data: ModelList | null) => void): XMLHttpRequest | null {
    const kept = lists[source];
    if (kept && now() - kept.at < LIST_FRESH_MS) {
      done(kept.data);
      return null;
    }
    return getJson<ModelList>("/settings/models?provider=" + encodeURIComponent(source), (data) => {
      if (data && data.models) lists[source] = { at: now(), data: data };
      done(data);
    });
  }

  function option(id: string, selected: boolean): HTMLOptionElement {
    const element = document.createElement("option");
    element.value = id;
    element.textContent = id;
    if (selected) element.selected = true;
    return element;
  }

  function fill(panel: HTMLElement, data: ModelList): void {
    const select = panel.querySelector("select");
    if (select) {
      const keep = select.value || data.selected || "";
      const models = data.models.slice();
      if (keep && models.indexOf(keep) === -1) models.unshift(keep);
      select.innerHTML = "";
      each(models, (id) => select.appendChild(option(id, id === keep)));
      return;
    }
    const list = panel.querySelector("datalist");
    if (!list || data.models.length === 0) return;
    list.innerHTML = "";
    each(data.models, (id) => list.appendChild(option(id, false)));
  }

  function providerForm(form: HTMLFormElement): void {
    const radios = form.querySelectorAll('input[name="provider"]');
    const panels: { [provider: string]: HTMLElement } = {};
    const loaded: { [provider: string]: boolean } = {};
    each(form.querySelectorAll("[data-model-panel]"), (panel) => {
      panels[panel.getAttribute("data-model-panel") || ""] = panel as HTMLElement;
    });

    function load(provider: string): void {
      const panel = panels[provider];
      // A subscription that is not connected shows its sign-in link instead
      // of a model field, so there is nothing to fill.
      if (!panel || loaded[provider] || !panel.querySelector("select, datalist")) return;
      loaded[provider] = true;
      modelList(provider, (data) => {
        if (!data || !data.models) { loaded[provider] = false; return; }
        fill(panel, data);
      });
    }

    function sync(): void {
      each(radios, (radio) => {
        const input = radio as HTMLInputElement;
        if (!input.checked) return;
        for (const key in panels) {
          if (!Object.prototype.hasOwnProperty.call(panels, key)) continue;
          const hidden = key !== input.value;
          panels[key].hidden = hidden;
          // Only the shown panel's fields are submitted.
          each(panels[key].querySelectorAll("input, select"), (field) => { (field as HTMLInputElement).disabled = hidden; });
        }
        load(input.value);
      });
    }

    each(radios, (radio) => radio.addEventListener("change", sync));
    sync();
  }

  // An empty cleaner provider means the active AI provider. The blank option
  // ("Provider's model") always comes first.
  function cleanerForm(form: HTMLFormElement): void {
    const provider = form.querySelector("#cleaner_provider") as HTMLSelectElement | null;
    const model = form.querySelector("#cleaner_model") as HTMLSelectElement | null;
    if (!provider || !model) return;
    let request: XMLHttpRequest | null = null;
    // Only the newest load fills the list; an older one answering late (or
    // aborted) is ignored.
    let newest = 0;

    function load(): void {
      if (!provider || !model) return;
      const source = provider.value || model.getAttribute("data-ai-provider") || "";
      const ticket = newest += 1;
      if (request) request.abort();
      request = modelList(source, (data) => {
        if (ticket !== newest) return;
        request = null;
        if (!data || !data.models) return;
        const keep = model.value;
        const models = data.models.slice();
        if (keep && models.indexOf(keep) === -1) models.unshift(keep);
        const blank = option("", keep === "");
        blank.textContent = "Provider's model";
        model.innerHTML = "";
        model.appendChild(blank);
        each(models, (id) => model.appendChild(option(id, id === keep)));
      });
    }

    provider.addEventListener("change", () => {
      // A model from another provider would not run there.
      model.value = "";
      load();
    });
    load();
  }

  function addProviderForm(form: HTMLFormElement): void {
    const picker = form.querySelector("#add_provider") as HTMLSelectElement | null;
    const custom = form.querySelector("[data-custom-fields]") as HTMLElement | null;
    const baseUrl = form.querySelector("#add_base_url") as HTMLInputElement | null;
    if (!picker || !custom) return;
    const syncCustom = () => {
      const chosen = picker.options[picker.selectedIndex];
      const on = !!chosen && chosen.getAttribute("data-custom") === "true";
      custom.hidden = !on;
      if (baseUrl) baseUrl.required = on;
    };
    picker.addEventListener("change", syncCustom);
    syncCustom();
  }

  onPage((root) => {
    each(within(root, "[data-provider-form]"), (form) => {
      if (firstTime(form, "provider")) providerForm(form as HTMLFormElement);
    });
    each(within(root, "[data-cleaner-form]"), (form) => {
      if (firstTime(form, "cleaner")) cleanerForm(form as HTMLFormElement);
    });
    each(within(root, "[data-add-provider]"), (form) => {
      if (firstTime(form, "add-provider")) addProviderForm(form as HTMLFormElement);
    });
  });
}
