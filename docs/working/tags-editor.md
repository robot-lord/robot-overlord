**Status:** shipped

# Tags editor (`#` modal)

PR #74 replaced the one-line `#` prompt with a modal
(`crates/tui/src/tagsform.rs`): the session's tags as a list, suggestions
from known tags (`tag.list {archived: true}`), a `y` confirmation before
creating a tag no session uses, and a discard prompt on esc with changes.
Keys are in `docs/guide.md` ("Editing tags").

## Owed

- **Short terminals:** when the screen is shorter than the fixed rows plus
  suggestions, the bottom rows (hints) clip. The tag list scrolls, but the
  fixed rows don't shrink.
- **Case variants in known tags:** `known` keeps `Web` and `web` as two
  entries if both exist in the DB; merge them (sum counts) when loading.
  `changed()`/`diff()` compare exactly while the rest of the editor
  ignores case.
- **`tag.list` untagged scope:** with `archived: true`, `groups` counts
  archived sessions but `untagged` stays live only. Decide whether the
  option should cover it.
- **Archived DB test:** no test sets up an archived session for
  `Db::tag_counts_in(true)`; the current assertion only checks it equals
  the live count.
- **`choices()` caching:** recomputed several times per key and per draw.
  Cheap today; cache on input change if known-tag lists grow.
- **`group_list` dedupe:** the fallback in `known_tags` (counting tags
  from the loaded list) duplicates `groups::group_list`.
- **Paste:** bracketed paste doesn't reach the editor's input.
