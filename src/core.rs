//! Pure logic: which marker a pane shows, and its border title.
//! Nothing here touches herdr, the filesystem or the clock.

/// herdr's own state icons (`ui.status_indicators = "symbols"`, herdr
/// v0.9.5 src/client/shell.rs). The "dots" style draws one glyph for all
/// three and tells them apart by color, which a border title can't have.
pub const BLOCKED_ICON: &str = "×";
pub const DONE_ICON: &str = "✓";
pub const WORKING_ICON: &str = "◐";

pub const MAX_TASK_TEXT_CHARS: usize = 60;

/// What a pane shows. `Done` is this plugin's own notion: the agent
/// finished and nobody has focused the pane since (see [`finished_after`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marker {
    Blocked,
    Done,
    Working,
}

impl Marker {
    pub fn icon(self) -> &'static str {
        match self {
            Marker::Blocked => BLOCKED_ICON,
            Marker::Done => DONE_ICON,
            Marker::Working => WORKING_ICON,
        }
    }
}

/// The marker for a pane with this herdr agent status, if any.
pub fn marker(status: &str, finished: bool) -> Option<Marker> {
    match status {
        "blocked" => Some(Marker::Blocked),
        "working" => Some(Marker::Working),
        _ if finished => Some(Marker::Done),
        _ => None,
    }
}

/// Whether a pane counts as finished after its status goes `old` -> `new`.
///
/// herdr's own `done` can't say which pane finished: switching to a tab
/// marks every pane in it seen, and herdr reports `idle` instead of `done`
/// for a pane that finishes in the active tab while the terminal has focus.
/// Only the focused pane was really in view.
pub fn finished_after(was_finished: bool, old: &str, new: &str, focused: bool) -> bool {
    if new != "idle" && new != "done" {
        return false;
    }
    if old == "working" || old == "blocked" {
        return new == "done" || !focused;
    }
    if old == "done" && new == "idle" && focused {
        // The terminal got focus back while this pane was focused.
        return false;
    }
    was_finished
}

fn truncate(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}…", kept.trim_end())
}

/// The pane-border title: icon, the pane's own label (herdr shows this
/// title in place of it), then the task text.
pub fn border_title(marker: Marker, label: Option<&str>, task: Option<&str>) -> String {
    let label = label.map(str::trim).unwrap_or("");
    let task = task
        .map(|t| truncate(t, MAX_TASK_TEXT_CHARS))
        .unwrap_or_default();
    let detail: Vec<&str> = [label, task.as_str()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect();
    if detail.is_empty() {
        marker.icon().to_string()
    } else {
        format!("{} {}", marker.icon(), detail.join(" · "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_follow_status_and_the_finished_flag() {
        assert_eq!(marker("blocked", true), Some(Marker::Blocked));
        assert_eq!(marker("working", true), Some(Marker::Working));
        assert_eq!(marker("idle", true), Some(Marker::Done));
        assert_eq!(marker("done", true), Some(Marker::Done));
        // Seen, or no agent there: the pane shows its own label.
        assert_eq!(marker("idle", false), None);
        assert_eq!(marker("done", false), None);
        assert_eq!(marker("unknown", false), None);
    }

    #[test]
    fn a_pane_that_finishes_out_of_focus_is_finished() {
        // In the active tab herdr skips "done" and reports idle.
        assert!(finished_after(false, "working", "idle", false));
        assert!(finished_after(false, "blocked", "done", false));
    }

    #[test]
    fn the_focused_pane_finishing_in_view_is_not() {
        assert!(!finished_after(false, "working", "idle", true));
    }

    #[test]
    fn the_focused_pane_finishing_while_the_terminal_is_unfocused_is() {
        assert!(finished_after(false, "working", "done", true));
        // Then the terminal gets focus back while that pane is focused.
        assert!(!finished_after(true, "done", "idle", true));
    }

    #[test]
    fn switching_to_the_tab_keeps_unfocused_panes_finished() {
        assert!(finished_after(true, "done", "idle", false));
    }

    #[test]
    fn new_work_or_a_gone_agent_ends_it() {
        assert!(!finished_after(true, "idle", "working", false));
        assert!(!finished_after(true, "idle", "blocked", false));
        assert!(!finished_after(true, "idle", "unknown", false));
    }

    #[test]
    fn unrelated_changes_keep_the_flag() {
        assert!(finished_after(true, "unknown", "idle", false));
        assert!(!finished_after(false, "unknown", "idle", false));
    }

    #[test]
    fn border_title_is_icon_label_and_task() {
        assert_eq!(
            border_title(
                Marker::Done,
                Some("reviewer"),
                Some("Fix the login redirect")
            ),
            "✓ reviewer · Fix the login redirect"
        );
        assert_eq!(
            border_title(Marker::Blocked, Some(" reviewer "), None),
            "× reviewer"
        );
        assert_eq!(
            border_title(Marker::Working, None, Some("  fix login  ")),
            "◐ fix login"
        );
        assert_eq!(border_title(Marker::Working, Some("  "), Some("")), "◐");
    }

    #[test]
    fn long_task_text_is_cut_with_an_ellipsis() {
        let title = border_title(Marker::Working, None, Some(&"x".repeat(100)));
        assert_eq!(title.chars().count(), 2 + MAX_TASK_TEXT_CHARS);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn task_text_is_cut_by_characters_not_bytes() {
        let title = border_title(Marker::Working, None, Some(&"漢".repeat(80)));
        assert_eq!(title.chars().count(), 2 + MAX_TASK_TEXT_CHARS);
    }
}
