/* Recipe chat: the thread opens scrolled to its latest message, and Enter
 * sends the question (Shift+Enter starts a new line). The drawer, the
 * question form, and the pending-answer poll are plain htmx attributes in
 * recipe_chat.html, so the chat also works without this script. */
namespace Recipes {
  onPage((root) => {
    each(within(root, "[data-chat-thread]"), (thread) => {
      thread.scrollTop = thread.scrollHeight;
    });
  });

  document.addEventListener("keydown", (event) => {
    if (event.keyCode !== 13 || event.shiftKey || event.altKey || event.ctrlKey || event.metaKey) return;
    const field = closest(event.target, "[data-chat-question]") as HTMLTextAreaElement | null;
    if (!field) return;
    event.preventDefault();
    if (!/\S/.test(field.value)) return;
    const form = closest(field, "form");
    const button = form ? form.querySelector("button") as HTMLButtonElement | null : null;
    // A click submits the way a tap does: through htmx, with the busy label.
    if (button && !button.disabled) button.click();
  });
}
