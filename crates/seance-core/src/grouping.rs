//! Sidebar sections and prefix grouping — pure, and shared so the native and
//! web rails cannot drift apart.
//!
//! Two independent axes:
//!
//! * **Section** — where a circle sits: pinned, active, sleeping, parked. This
//!   is lifecycle, and mostly not something you choose.
//! * **Group** — visual clustering *inside* a section, from the text before the
//!   first `-` in a circle's label. Name three circles `mtg-growth`,
//!   `mtg-ai`, `mtg-carl` and they cluster under `mtg`.
//!
//! Grouping is deliberately a naming convention rather than a stored
//! attribute. You get it by typing, you undo it by typing, and it costs
//! nothing when you don't want it — which is the point when the grouping you
//! want only matters for an afternoon. A prefix carried by just one circle is
//! not a group; it renders as a plain row.
//!
//! Each section groups **independently**: `mtg` circles that are awake cluster
//! under Active, the slept ones cluster under Sleeping, and neither knows
//! about the other.

use std::collections::BTreeSet;

/// Which band a circle renders in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    /// Explicitly pinned. Wins over every other state — a pin is a statement
    /// about where you want to *look*, not about what the circle is doing.
    Pinned,
    /// Subscribed and awake: the working set.
    Active,
    /// Subscribed but slept — no process, wakeable onto its own conversation.
    Sleeping,
    /// Not in this window's active set.
    Parked,
}

impl Section {
    /// Top-to-bottom rail order.
    pub const ALL: [Section; 4] = [
        Section::Pinned,
        Section::Active,
        Section::Sleeping,
        Section::Parked,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Section::Pinned => "pinned",
            Section::Active => "active",
            Section::Sleeping => "sleeping",
            Section::Parked => "parked",
        }
    }

    /// Stable key for persisting collapse state.
    pub fn key(self) -> &'static str {
        self.title()
    }
}

/// One rendered entry in a section: a circle on its own, or a cluster of
/// circles sharing a name prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SectionRow {
    Circle(String),
    Group {
        /// The shared text before the first `-`, as first typed.
        prefix: String,
        members: Vec<String>,
    },
}

/// Split circles into the four bands.
///
/// `ordered` carries the sidebar sort, and each band preserves it. Pinned wins
/// outright; among the rest, being asleep decides Sleeping vs Active, and
/// anything unsubscribed is Parked. A pinned circle that is asleep stays
/// pinned — you asked for it to be at the top, and the daemon dozing it off is
/// not a reason to move it.
pub fn partition_sections(
    ordered: &[String],
    active: &BTreeSet<String>,
    pinned: &BTreeSet<String>,
    asleep: &BTreeSet<String>,
) -> Vec<(Section, Vec<String>)> {
    let mut pin = Vec::new();
    let mut act = Vec::new();
    let mut sleep = Vec::new();
    let mut parked = Vec::new();
    for ws in ordered {
        if pinned.contains(ws) {
            pin.push(ws.clone());
        } else if !active.contains(ws) {
            parked.push(ws.clone());
        } else if asleep.contains(ws) {
            sleep.push(ws.clone());
        } else {
            act.push(ws.clone());
        }
    }
    vec![
        (Section::Pinned, pin),
        (Section::Active, act),
        (Section::Sleeping, sleep),
        (Section::Parked, parked),
    ]
}

/// The grouping key of a label: the text before its first `-`, lowercased.
/// `None` when there is no hyphen — an unhyphenated name opts out, which is
/// how you keep a circle loose without thinking about it.
pub fn prefix_of(label: &str) -> Option<String> {
    let (head, _) = label.split_once('-')?;
    let head = head.trim();
    if head.is_empty() {
        return None;
    }
    Some(head.to_ascii_lowercase())
}

/// Cluster one section's circles by name prefix, preserving the incoming sort.
///
/// A group lands at the position of its **first** member, so a cluster floats
/// exactly as high as its most-deserving circle would have on its own — the
/// rail keeps meaning what it meant. Members hold their relative order inside.
/// A prefix with only one circle behind it is not a group.
pub fn group_by_prefix<F>(circles: &[String], label_of: F) -> Vec<SectionRow>
where
    F: Fn(&str) -> String,
{
    let labels: Vec<(String, String)> = circles
        .iter()
        .map(|ws| (ws.clone(), label_of(ws)))
        .collect();

    // Count first: a prefix only earns a header once a second circle shares it.
    let mut counts: Vec<(String, usize)> = Vec::new();
    for (_, label) in &labels {
        if let Some(p) = prefix_of(label) {
            match counts.iter_mut().find(|(k, _)| *k == p) {
                Some((_, n)) => *n += 1,
                None => counts.push((p, 1)),
            }
        }
    }
    let grouped = |p: &str| counts.iter().any(|(k, n)| k == p && *n > 1);

    let mut out: Vec<SectionRow> = Vec::new();
    for (ws, label) in &labels {
        match prefix_of(label).filter(|p| grouped(p)) {
            None => out.push(SectionRow::Circle(ws.clone())),
            Some(key) => {
                // Join the existing cluster, or open one here — which is what
                // pins the group to its first member's position.
                let existing = out.iter_mut().find_map(|row| match row {
                    SectionRow::Group { prefix, members } if prefix.to_ascii_lowercase() == key => {
                        Some(members)
                    }
                    _ => None,
                });
                match existing {
                    Some(members) => members.push(ws.clone()),
                    None => out.push(SectionRow::Group {
                        // Display the prefix as the human typed it here.
                        prefix: label
                            .split_once('-')
                            .map(|(h, _)| h.trim())
                            .unwrap_or("")
                            .into(),
                        members: vec![ws.clone()],
                    }),
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }
    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }
    /// Identity labels — most tests don't need a rename.
    fn same(ws: &str) -> String {
        ws.to_string()
    }

    #[test]
    fn sections_split_by_lifecycle_and_pinned_wins() {
        let ordered = v(&["a", "b", "c", "d", "e"]);
        let bands = partition_sections(
            &ordered,
            &set(&["a", "b", "c", "d"]),
            &set(&["a"]),
            &set(&["b", "a", "e"]),
        );
        let by = |s: Section| bands.iter().find(|(k, _)| *k == s).unwrap().1.clone();
        // `a` is pinned AND asleep — a pin is about where you look.
        assert_eq!(by(Section::Pinned), v(&["a"]));
        assert_eq!(by(Section::Active), v(&["c", "d"]));
        assert_eq!(by(Section::Sleeping), v(&["b"]));
        // `e` is unsubscribed; parked outranks its sleep state.
        assert_eq!(by(Section::Parked), v(&["e"]));
    }

    #[test]
    fn a_prefix_needs_two_circles_to_become_a_group() {
        let rows = group_by_prefix(&v(&["mtg-growth", "solo-thing", "mtg-ai"]), same);
        assert_eq!(
            rows,
            vec![
                SectionRow::Group {
                    prefix: "mtg".into(),
                    members: v(&["mtg-growth", "mtg-ai"]),
                },
                SectionRow::Circle("solo-thing".into()),
            ]
        );
    }

    #[test]
    fn a_group_sits_where_its_first_member_would_have() {
        // Incoming order is the sidebar sort; the cluster must not jump the
        // queue, so it lands exactly where `mtg-growth` was.
        let rows = group_by_prefix(&v(&["urgent", "mtg-growth", "other", "mtg-ai"]), same);
        assert_eq!(
            rows,
            vec![
                SectionRow::Circle("urgent".into()),
                SectionRow::Group {
                    prefix: "mtg".into(),
                    members: v(&["mtg-growth", "mtg-ai"]),
                },
                SectionRow::Circle("other".into()),
            ]
        );
    }

    #[test]
    fn an_unhyphenated_name_opts_out() {
        assert_eq!(prefix_of("mtg"), None);
        assert_eq!(prefix_of("-leading"), None);
        assert_eq!(prefix_of("mtg-growth").as_deref(), Some("mtg"));
        // Only the FIRST hyphen splits, so deeper dashes stay in the tail.
        assert_eq!(prefix_of("stack-misc-a-6553").as_deref(), Some("stack"));
        // A circle named exactly `mtg` does not join the `mtg-` cluster.
        let rows = group_by_prefix(&v(&["mtg", "mtg-a", "mtg-b"]), same);
        assert_eq!(rows[0], SectionRow::Circle("mtg".into()));
        assert!(matches!(&rows[1], SectionRow::Group { members, .. } if members.len() == 2));
    }

    #[test]
    fn grouping_reads_the_label_not_the_slug() {
        // The slug is frozen at creation; the label is what you retype to
        // regroup, so grouping has to follow the label.
        let rows = group_by_prefix(&v(&["growth", "atlas"]), |ws| match ws {
            "growth" => "mtg-growth".into(),
            "atlas" => "mtg-atlas".into(),
            other => other.into(),
        });
        assert_eq!(
            rows,
            vec![SectionRow::Group {
                prefix: "mtg".into(),
                members: v(&["growth", "atlas"]),
            }]
        );
    }

    #[test]
    fn prefix_matching_is_case_insensitive_and_displays_as_typed() {
        let rows = group_by_prefix(&v(&["a", "b"]), |ws| match ws {
            "a" => "MTG-growth".into(),
            _ => "mtg-ai".into(),
        });
        match &rows[0] {
            SectionRow::Group { prefix, members } => {
                assert_eq!(prefix, "MTG", "shown as first typed");
                assert_eq!(members.len(), 2, "matched case-insensitively");
            }
            other => panic!("expected a group, got {other:?}"),
        }
    }

    #[test]
    fn empty_section_yields_no_rows() {
        assert!(group_by_prefix(&[], same).is_empty());
    }
}
