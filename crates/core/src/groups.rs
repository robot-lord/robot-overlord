//! Groups over session rows: tags are the groups. Shared by the TUI's
//! Groups tab and the web UI's snapshot (computed in Rust, drawn by
//! either).

use serde_json::Value;

/// The "untagged" pseudo-group (tags are never empty).
pub const UNTAGGED: &str = "";

/// A session row's tags.
pub fn tags_of(s: &Value) -> Vec<&str> {
    s["tags"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// Groups over `sessions`: (tag, count). The user's favourites first, in
/// their order (`order`), then the rest, most sessions first, then
/// "untagged" last when there are any. Favourites with no sessions here
/// aren't shown (they keep their place for when they have some).
pub fn group_list(sessions: &[Value], order: &[String]) -> Vec<(String, usize)> {
    let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
    let mut untagged = 0;
    for s in sessions {
        let tags = tags_of(s);
        if tags.is_empty() {
            untagged += 1;
        }
        for t in tags {
            *counts.entry(t).or_default() += 1;
        }
    }
    let mut v: Vec<(String, usize)> = counts
        .into_iter()
        .map(|(t, n)| (t.to_string(), n))
        .collect();
    let rank = |t: &str| order.iter().position(|o| o == t).unwrap_or(usize::MAX);
    v.sort_by(|a, b| {
        rank(&a.0)
            .cmp(&rank(&b.0))
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.0.cmp(&b.0))
    });
    if untagged > 0 {
        v.push((UNTAGGED.to_string(), untagged));
    }
    v
}

/// Whether session row `s` is in `group`.
pub fn in_group(s: &Value, group: &str) -> bool {
    let tags = tags_of(s);
    if group == UNTAGGED {
        tags.is_empty()
    } else {
        tags.contains(&group)
    }
}

/// How a group is shown: `#tag`, or "untagged".
pub fn group_label(g: &str) -> String {
    if g == UNTAGGED {
        "untagged".into()
    } else {
        format!("#{g}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn groups_count_tags_and_untagged() {
        let s = |tags: &[&str]| json!({"tags": tags});
        let all = vec![s(&["web", "wip"]), s(&["web"]), s(&[]), s(&["api"])];
        let g = group_list(&all, &[]);
        assert_eq!(
            g,
            vec![
                ("web".to_string(), 2),
                ("api".to_string(), 1),
                ("wip".to_string(), 1),
                (UNTAGGED.to_string(), 1),
            ]
        );
        // A session with two tags is in both groups.
        assert!(in_group(&all[0], "web") && in_group(&all[0], "wip"));
        assert!(in_group(&all[2], UNTAGGED) && !in_group(&all[1], UNTAGGED));
        assert_eq!(group_label("web"), "#web");
        assert_eq!(group_label(UNTAGGED), "untagged");
    }

    #[test]
    fn favourites_lead_in_their_order() {
        let s = |tags: &[&str]| json!({"tags": tags});
        let all = vec![s(&["web", "wip"]), s(&["web"]), s(&[]), s(&["api"])];
        // "gone" has no sessions: not shown, but keeps its place.
        let order = ["wip".to_string(), "gone".to_string(), "api".to_string()];
        let names: Vec<String> = group_list(&all, &order).into_iter().map(|g| g.0).collect();
        assert_eq!(names, ["wip", "api", "web", UNTAGGED]);
    }
}
