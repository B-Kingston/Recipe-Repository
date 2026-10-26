/* Authorise Codex: poll the device-code flow until ChatGPT confirms the
 * sign-in, then return to Settings (closing the sign-in popup when it runs
 * there). Stops once the flow is swapped away or the popup closes. */
namespace Recipes {
  interface FlowState {
    status: string;
    message?: string;
  }

  function codexFlow(root: HTMLElement): void {
    const flowId = root.getAttribute("data-codex-flow") || "";
    const interval = Math.max(2000, (parseInt(root.getAttribute("data-codex-interval") || "", 10) || 0) * 1000);
    const waiting = root.querySelector("#codex-flow-waiting") as HTMLElement | null;
    let failures = 0;

    function showFailure(message: string): void {
      const failed = root.querySelector("#codex-flow-failed") as HTMLElement | null;
      if (failed) failed.hidden = false;
      if (waiting) waiting.hidden = true;
      const text = root.querySelector("#codex-flow-failed-message");
      if (text && message) text.textContent = message;
    }

    function retry(): void {
      failures += 1;
      if (failures >= 4) {
        showFailure("We could not check the sign-in status. Start a new authorisation to try again.");
        return;
      }
      window.setTimeout(poll, interval);
    }

    function poll(): void {
      if (!attached(root)) return;
      getJson<FlowState>("/settings/authorise-codex/status/" + encodeURIComponent(flowId), (state) => {
        if (!attached(root)) return;
        if (!state) { retry(); return; }
        failures = 0;
        if (state.status === "complete") {
          const done = root.querySelector("#codex-flow-done") as HTMLElement | null;
          if (done) done.hidden = false;
          if (waiting) waiting.hidden = true;
          window.setTimeout(() => { if (attached(root)) finishSignIn(); }, 1500);
          return;
        }
        if (state.status === "pending" || state.status === "slow_down") {
          window.setTimeout(poll, interval);
          return;
        }
        if (state.status === "expired") {
          showFailure("This sign-in code expired. Start a new authorisation to try again.");
          return;
        }
        showFailure(state.message || "Authorisation could not be completed.");
      });
    }

    window.setTimeout(poll, interval);
  }

  onPage((root) => each(within(root, "[data-codex-flow]"), (flow) => {
    if (firstTime(flow, "codex")) codexFlow(flow);
  }));
}
