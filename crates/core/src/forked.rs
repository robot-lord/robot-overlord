//! What to tell the user after `session.fork`: the one wording, for
//! `rlord fork` and the TUI's status line alike.

use serde_json::Value;

/// A fork's answer, said: the line that names it, and what to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forked {
    /// "forked api → api-fork", with its id when `with_id`, and the new
    /// worktree it made, if any.
    pub headline: String,
    /// The answer's warnings, one per entry.
    pub warnings: Vec<String>,
}

/// `session.fork`'s answer `v`, said. `with_id`: name the new session's id
/// too (the CLI; a status line has no room for it).
pub fn forked(v: &Value, with_id: bool) -> Forked {
    let s = |k: &str| v[k].as_str().unwrap_or("?");
    let mut headline = format!("forked {} → {}", s("forked_from_name"), s("name"));
    if with_id {
        headline.push_str(&format!(" ({})", s("id")));
    }
    // Only a worktree this fork made (one in its parent's is no news).
    if v["new_worktree"] == true {
        headline.push_str(&format!(
            " in a new worktree {} [{}]",
            s("project_path"),
            s("branch")
        ));
    }
    let warnings = v["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|w| w.as_str().map(str::to_string))
        .collect();
    Forked { headline, warnings }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_fork_is_said_with_its_worktree_and_warnings() {
        let v = json!({"id": "01f", "name": "x", "forked_from_name": "api",
                       "project_path": "/r/.worktrees/x/web", "branch": "x",
                       "new_worktree": true, "warnings": ["it shares nothing", 3]});
        assert_eq!(
            forked(&v, true),
            Forked {
                headline: "forked api → x (01f) in a new worktree /r/.worktrees/x/web [x]".into(),
                warnings: vec!["it shares nothing".into()],
            }
        );
        let v = json!({"id": "01f", "name": "x", "forked_from_name": "api",
                       "new_worktree": false});
        assert_eq!(forked(&v, false).headline, "forked api → x");
        assert!(forked(&v, false).warnings.is_empty());
    }
}
