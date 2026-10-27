-- The recipe chat now edits a copy of the recipe with tools and proposes
-- the result, which the cook accepts or denies in the chat; it no longer
-- drafts changes through ai_drafts (ai_drafts.chat_message_id stays, unused).
-- proposal_json holds {"before": the recipe snapshot the chat saw, "after":
-- the proposed recipe, each item with `from`, its index in "before" or null
-- when new}. proposal_base is recipes.updated_at when the chat read the
-- recipe: accepting refuses once the recipe changed since. While a proposal
-- awaits the cook, change_state is 'ready'; then 'applied' or 'discarded'.
ALTER TABLE recipe_chat_messages ADD COLUMN proposal_json TEXT NOT NULL DEFAULT '';
ALTER TABLE recipe_chat_messages ADD COLUMN proposal_base TEXT NOT NULL DEFAULT '';
