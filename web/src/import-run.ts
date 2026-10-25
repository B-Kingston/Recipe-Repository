/* From Video import: replay the bounded run feed, then poll for new events.
 * The background job survives navigation and each raw channel is replaced
 * atomically when its backend stage reports, avoiding partial or unsafe HTML.
 * The plain summary words each source like `import_sources` in ai.rs. The
 * technical details remember whether they were left open, and while they
 * are closed a finished import opens its draft by itself. Polling stops once
 * the page has been swapped away. */
namespace Recipes {
  interface ImportEvent {
    kind: string;
    state?: string;
    phase?: string;
    message?: string;
    title?: string;
    description?: string;
    durationSeconds?: number | null;
    transcript?: string;
    text?: string;
    planned?: number;
    draftUrl?: string;
    /** An error event's failure is fixed in Settings (a missing or refused AI login). */
    settings?: boolean;
    cards?: { kept?: boolean }[];
    captures?: ImportCapture[];
  }

  interface ImportCapture {
    image?: string;
    seconds: number;
    text?: string | null;
    raw?: string;
  }

  interface ImportFeed {
    events?: ImportEvent[];
    next?: number;
    done?: boolean;
  }

  type ProgressName = "extract" | "clean" | "generate";

  const READING = "Watching the video…";
  const CLEANING = "Picking out the recipe…";
  const PREPARING = "Writing the draft…";
  const DRAFTING = "Writing the draft…";
  const READY = "Done.";
  const DETAILS_KEY = "recipe-import-details";
  const OPEN_DELAY_MS = 1200;
  const OCR_COPY = "Reading the text on screen…";
  const MAX_FAILURES = 4;
  const RETRY_MS = 3500;
  const PROGRESS_NAMES: ProgressName[] = ["extract", "clean", "generate"];

  onPage((page) => each(within(page, "[data-import-root]"), (root) => {
    if (firstTime(root, "import")) importRun(root);
  }));

  function importRun(root: HTMLElement): void {
    const finished = root.getAttribute("data-import-finished") === "1";
    const eventsUrl = root.getAttribute("data-events-url") || "";
    const framesBase = root.getAttribute("data-frames-base") || "";
    const status = root.querySelector("[data-import-status]") as HTMLElement | null;
    const errorBox = root.querySelector("[data-import-error]") as HTMLElement | null;
    const warnings = root.querySelector("[data-import-warnings]") as HTMLElement | null;
    const ready = root.querySelector("[data-import-ready]") as HTMLElement | null;
    const draftLink = root.querySelector("[data-import-draft]") as HTMLAnchorElement | null;
    const draftCopy = root.querySelector("[data-import-draft-copy]");
    const heading = root.querySelector("[data-import-heading]");
    const retryBox = root.querySelector("[data-import-retry]") as HTMLElement | null;
    const settingsLink = root.querySelector("[data-import-settings]") as HTMLElement | null;
    const readyCopy = root.querySelector("[data-import-ready-copy]");
    const details = root.querySelector("[data-import-details]") as HTMLDetailsElement | null;
    const notesCount = root.querySelector("[data-import-notes-count]");
    const progressBar = root.querySelector("[data-import-progressbar]");
    const progressCopy = root.querySelector("[data-import-progress-copy]");
    const progress: { [name: string]: number } = { extract: 0, clean: 0, generate: 0 };
    // Audio transcription and on-screen text run side by side, so the copy
    // names whichever of the two is still going.
    const lanesDone = { audio: false, ocr: false };
    const parts: { [name: string]: { segment: HTMLElement | null; fill: HTMLElement | null; badge: HTMLElement | null } } = {};
    const stages: { [name: string]: { enabled: boolean; badge: HTMLElement | null } } = {};
    let since = 0;
    let failures = 0;
    let stopped = false;
    let replayed = false;

    each(PROGRESS_NAMES, (name) => {
      parts[name] = {
        segment: root.querySelector('[data-progress-segment="' + name + '"]'),
        fill: root.querySelector('[data-progress-fill="' + name + '"]'),
        badge: root.querySelector('[data-progress-status="' + name + '"]'),
      };
    });

    function clamp(value: number): number { return Math.max(0, Math.min(1, value)); }
    function progressState(value: number): string { return value >= 1 ? "done" : value > 0 ? "working" : "waiting"; }
    function setCopy(copy: string): void { if (progressCopy) progressCopy.textContent = copy; }

    function drawProgress(): void {
      let total = 0;
      each(PROGRESS_NAMES, (name) => {
        const value = clamp(progress[name]);
        const part = parts[name];
        if (part.fill) part.fill.style.width = Math.round(value * 100) + "%";
        if (part.segment) part.segment.className = "import-progress-segment" + (value >= 1 ? " is-complete" : value > 0 ? " is-active" : "");
        if (part.badge) part.badge.textContent = progressState(value);
        total += value;
      });
      total = Math.round(total / PROGRESS_NAMES.length * 100);
      if (progressBar) {
        progressBar.setAttribute("aria-valuenow", String(total));
        progressBar.setAttribute("aria-valuetext", total + "% complete");
      }
    }

    function advanceProgress(name: ProgressName, value: number, copy?: string): void {
      progress[name] = Math.max(progress[name], clamp(value));
      if (copy) setCopy(copy);
      drawProgress();
    }

    function initialiseProgress(): void {
      const state = (root.getAttribute("data-import-progress-state") || "").toLowerCase();
      if (root.getAttribute("data-import-progress-complete") === "1") {
        progress.extract = 1;
        progress.clean = 1;
        progress.generate = 1;
        setCopy(READY);
      } else if (state.indexOf("preparing") !== -1 || state.indexOf("draft") !== -1) {
        progress.extract = 1;
        progress.clean = 1;
        progress.generate = .08;
        setCopy(DRAFTING);
      } else if (state.indexOf("clean") !== -1) {
        progress.extract = 1;
        progress.clean = .08;
        setCopy(CLEANING);
      } else {
        progress.extract = .03;
        setCopy(READING);
      }
      drawProgress();
    }

    function cleaning(): void {
      advanceProgress("extract", 1, CLEANING);
      advanceProgress("clean", .08, CLEANING);
    }

    function drafting(): void {
      advanceProgress("extract", 1, DRAFTING);
      advanceProgress("clean", 1, DRAFTING);
      advanceProgress("generate", .08, DRAFTING);
    }

    function applyProgress(event: ImportEvent): void {
      const state = (event.state || "").toLowerCase();
      if (event.kind === "status") {
        if (state.indexOf("clean") !== -1) cleaning();
        else if (state.indexOf("preparing") !== -1 || state.indexOf("draft") !== -1) drafting();
        else if (state.indexOf("extract") !== -1 || state.indexOf("wait") !== -1) advanceProgress("extract", .06, READING);
      } else if (event.kind === "phase") {
        if (event.phase === "worker") advanceProgress("extract", state.indexOf("wait") !== -1 ? .03 : .06, READING);
        else if (event.phase === "description") advanceProgress("extract", state === "done" ? .25 : .12, READING);
        else if (event.phase === "download") advanceProgress("extract", state === "done" ? .43 : .34, "Downloading the video…");
        else if (event.phase === "audio") {
          if (state === "done") lanesDone.audio = true;
          advanceProgress("extract", state === "done" ? .68 : .54, lanesCopy());
        } else if (event.phase === "ocr") {
          if (state === "done") lanesDone.ocr = true;
          advanceProgress("extract", state === "done" ? .96 : state.indexOf("sampling") !== -1 ? .71 : .79, lanesCopy());
        }
        // Dish photos and video moments are computed in the background while
        // the cleaner and recipe draft run, so they never move the bar or
        // replace the copy of the stage that is actually running.
        else if (event.phase === "cleaner") cleaning();
        else if (event.phase === "draft") drafting();
      } else if (event.kind === "ocr-plan") {
        advanceProgress("extract", .82, lanesCopy());
      } else if (event.kind === "ocr-captures") {
        advanceProgress("extract", .97, lanesCopy());
      } else if (event.kind === "cleaned") {
        advanceProgress("extract", 1, PREPARING);
        advanceProgress("clean", 1, PREPARING);
      } else if (event.kind === "result") {
        advanceProgress("extract", 1, READY);
        advanceProgress("clean", 1, READY);
        advanceProgress("generate", 1, READY);
      } else if (event.kind === "error") {
        setCopy("Import stopped.");
      }
    }

    function lanesCopy(): string {
      const audioPending = stage("audio").enabled && !lanesDone.audio;
      const ocrPending = stage("ocr").enabled && !lanesDone.ocr;
      if (audioPending && ocrPending) return "Listening and reading the text on screen…";
      if (audioPending) return "Listening to the video…";
      if (ocrPending) return OCR_COPY;
      return READING;
    }

    function count(n: number, one: string, many: string): string { return n + " " + (n === 1 ? one : many); }

    function words(text?: string): number {
      const trimmed = (text || "").replace(/^\s+|\s+$/g, "");
      return trimmed ? trimmed.split(/\s+/).length : 0;
    }

    function setFound(key: string, state: string, detail: string): void {
      const item = root.querySelector('[data-found="' + key + '"]');
      if (!item || item.className === "is-skipped") return;
      item.className = "is-" + state;
      const holder = item.querySelector("[data-found-detail]");
      if (holder) holder.textContent = detail;
    }

    function foundDescription(event: ImportEvent): void {
      const n = words(event.description);
      if (n) setFound("description", "found", count(n, "word", "words"));
      else if (event.title) setFound("description", "found", "Title only");
      else setFound("description", "none", "No caption");
    }

    function foundAudio(event: ImportEvent): void {
      const n = words(event.transcript);
      if (n) setFound("audio", "found", count(n, "word", "words"));
      else setFound("audio", "none", "No speech");
    }

    function foundOcr(event: ImportEvent): void {
      let n = 0;
      each(event.cards || [], (card) => { if (card.kept) n += 1; });
      if (!n) each(event.captures || [], (capture) => { if (capture.text !== undefined && capture.text !== null) n += 1; });
      if (n) setFound("ocr", "found", count(n, "card", "cards"));
      else setFound("ocr", "none", "No text found");
    }

    function unreadFound(): void {
      each(root.querySelectorAll("[data-found]"), (item) => {
        if (item.className === "is-waiting") setFound(item.getAttribute("data-found") || "", "none", "Not read");
      });
    }

    function countNotes(): void {
      if (!notesCount || !warnings) return;
      const n = warnings.getElementsByTagName("li").length;
      notesCount.textContent = n ? "· " + count(n, "note", "notes") : "";
    }

    function rememberDetails(): void {
      if (!details) return;
      try { window.localStorage.setItem(DETAILS_KEY, details.open ? "1" : "0"); } catch (ignored) { /* private mode */ }
    }

    function restoreDetails(): void {
      if (!details) return;
      try { if (window.localStorage.getItem(DETAILS_KEY) === "1") details.open = true; } catch (ignored) { /* private mode */ }
      const summary = details.getElementsByTagName("summary")[0];
      if (summary) summary.addEventListener("click", () => { window.setTimeout(rememberDetails, 0); });
    }

    function openDraft(url?: string): void {
      if (!url || (details && details.open)) {
        if (readyCopy) readyCopy.textContent = "";
        return;
      }
      if (readyCopy) readyCopy.textContent = "Opening it now…";
      window.setTimeout(() => {
        if (!attached(root)) return;
        if (draftLink && draftLink.href && (!details || !details.open)) draftLink.click();
        else if (!details || !details.open) go(url);
        else if (readyCopy) readyCopy.textContent = "";
      }, OPEN_DELAY_MS);
    }

    function stage(name: string): { enabled: boolean; badge: HTMLElement | null } {
      if (!stages[name]) {
        const box = root.querySelector('[data-import-stage="' + name + '"]');
        stages[name] = {
          enabled: !box || box.getAttribute("data-stage-enabled") !== "0",
          badge: box ? box.querySelector("[data-stage-status]") : null,
        };
      }
      return stages[name];
    }

    function stageStatus(name: string, value: string): void {
      const entry = stage(name);
      if (entry.badge && entry.enabled) entry.badge.textContent = value;
    }

    function setRaw(holder: Element | null, text: string, emptyText: string): void {
      if (!holder) return;
      clear(holder);
      holder.appendChild(text ? el("div", "debug-mono", text) : el("p", "muted", emptyText));
    }

    function setDescription(event: ImportEvent): void {
      if (!stage("description").enabled) return;
      const holder = root.querySelector("[data-import-description]");
      if (!holder) return;
      clear(holder);
      if (event.durationSeconds !== undefined && event.durationSeconds !== null) holder.appendChild(el("p", "muted", "Duration: " + event.durationSeconds + "s"));
      if (event.title) holder.appendChild(el("p", null, "")).appendChild(el("strong", null, event.title));
      if (event.description) holder.appendChild(el("div", "debug-mono", event.description));
      if (!event.title && !event.description) holder.appendChild(el("p", "muted", "No caption or description was found."));
      stageStatus("description", "done");
    }

    function setAudio(event: ImportEvent): void {
      if (!stage("audio").enabled) return;
      setRaw(root.querySelector("[data-import-audio]"), event.transcript || "", "No spoken transcript was found.");
      stageStatus("audio", "done");
    }

    function setOcr(event: ImportEvent): void {
      if (!stage("ocr").enabled) return;
      const holder = root.querySelector("[data-import-ocr]");
      if (!holder) return;
      const captures = event.captures || [];
      clear(holder);
      if (!captures.length) {
        holder.appendChild(el("p", "muted", "No readable on-screen text was found."));
        stageStatus("ocr", "done");
        return;
      }
      const table = el("table", "debug-captures") as HTMLTableElement;
      const head = table.createTHead().insertRow(-1);
      each(["Frame", "t", "Recognised text", "Raw engine reading"], (label) => {
        head.appendChild(document.createElement("th")).appendChild(document.createTextNode(label));
      });
      const body = table.appendChild(document.createElement("tbody"));
      each(captures, (capture) => {
        const row = body.insertRow(-1);
        const thumb = row.insertCell(-1);
        thumb.className = "debug-thumb";
        if (capture.image) {
          const link = el("a") as HTMLAnchorElement;
          const image = el("img") as HTMLImageElement;
          link.href = framesBase + "/0/" + capture.image;
          link.target = "_blank";
          image.src = link.href;
          image.alt = "frame at " + capture.seconds + "s";
          image.setAttribute("loading", "lazy");
          link.appendChild(image);
          thumb.appendChild(link);
        } else {
          thumb.textContent = "—";
        }
        row.insertCell(-1).textContent = capture.seconds + "s";
        const clean = row.insertCell(-1);
        if (capture.text !== undefined && capture.text !== null) clean.textContent = capture.text;
        else clean.appendChild(el("span", "debug-dropped", "not kept"));
        const raw = row.insertCell(-1);
        raw.className = "debug-mono debug-raw";
        raw.textContent = capture.raw || "";
      });
      const scroll = el("div", "table-scroll");
      scroll.appendChild(table);
      holder.appendChild(scroll);
      stageStatus("ocr", "done");
    }

    function markStatus(text: string, modifier: string): void {
      if (!status) return;
      status.textContent = text;
      status.className = "codex-status codex-status--" + modifier + " import-overall-status";
    }

    function stoppedHeading(message: string): void {
      if (errorBox) {
        errorBox.textContent = message;
        errorBox.hidden = false;
      }
      if (heading) heading.textContent = "Import stopped";
      if (retryBox) retryBox.hidden = false;
    }

    function apply(event: ImportEvent): void {
      applyProgress(event);
      if (event.kind === "status") {
        if (status) status.textContent = event.state || "working";
      } else if (event.kind === "phase") {
        stageStatus(event.phase || "", event.state || "working");
      } else if (event.kind === "description") {
        if (stage("description").enabled) foundDescription(event);
        setDescription(event);
      } else if (event.kind === "audio") {
        if (stage("audio").enabled) foundAudio(event);
        setAudio(event);
      } else if (event.kind === "ocr-plan") {
        stageStatus("ocr", "reading " + event.planned + " frames");
      } else if (event.kind === "ocr-captures") {
        if (stage("ocr").enabled) foundOcr(event);
        setOcr(event);
      } else if (event.kind === "cleaned") {
        setRaw(root.querySelector("[data-import-cleaner]"), event.text || "", "The cleaner returned no recipe facts.");
        stageStatus("cleaner", "done");
      } else if (event.kind === "warning" && event.message && warnings) {
        warnings.appendChild(el("li", null, event.message));
        countNotes();
      } else if (event.kind === "result") {
        markStatus("ready", "connected");
        stageStatus("draft", "done");
        if (heading) heading.textContent = "Your recipe is ready";
        if (event.draftUrl && draftLink) draftLink.href = event.draftUrl;
        if (ready) ready.hidden = false;
        if (draftCopy) draftCopy.textContent = "Ready for your review.";
        openDraft(event.draftUrl);
      } else if (event.kind === "error") {
        markStatus("failed", "missing");
        stoppedHeading(event.message || "The video import failed.");
        if (settingsLink) settingsLink.hidden = !event.settings;
        unreadFound();
      }
    }

    function giveUp(): void {
      stopped = true;
      setCopy("Import stopped.");
      markStatus("unavailable", "missing");
      stoppedHeading("We lost touch with this import, so it may have expired.");
    }

    function retry(): void {
      failures += 1;
      if (failures >= MAX_FAILURES) { giveUp(); return; }
      window.setTimeout(poll, RETRY_MS);
    }

    function poll(): void {
      if (stopped || !attached(root)) return;
      getJson<ImportFeed>(eventsUrl + "?since=" + since, (payload) => {
        if (!attached(root)) return;
        if (!payload) { retry(); return; }
        failures = 0;
        if (!replayed) { clear(warnings); countNotes(); replayed = true; }
        try { each(payload.events || [], apply); } catch (ignored) { /* keep polling */ }
        since = payload.next || since;
        if (payload.done) { stopped = true; return; }
        window.setTimeout(poll, 900);
      });
    }

    restoreDetails();
    initialiseProgress();
    if (!finished) poll();
  }
}
