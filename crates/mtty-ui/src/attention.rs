//! Pending human actions, deduplicated by pane rather than by notification.

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reason {
    Failed,
    Input,
    Review,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub pane: String,
    pub agent: String,
    pub reason: Reason,
}

#[derive(Default)]
pub struct Inbox {
    items: Vec<Item>,
}

impl Inbox {
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn dismiss(&mut self, pane: &str) {
        self.items.retain(|item| item.pane != pane);
    }

    /// Call only for actual state transitions. Repeated state reports must not
    /// resurrect an item the user already dismissed.
    pub fn transition(&mut self, pane: &str, agent: &str, previous: Option<&str>, now: &str) {
        if previous == Some(now) {
            return;
        }
        let reason = match now {
            "error" => Some(Reason::Failed),
            "awaiting" => Some(Reason::Input),
            "completed" => Some(Reason::Review),
            "idle" if agent != "miao" && previous == Some("processing") => Some(Reason::Review),
            _ => None,
        };
        self.dismiss(pane);
        if let Some(reason) = reason {
            self.items.push(Item {
                pane: pane.to_owned(),
                agent: agent.to_owned(),
                reason,
            });
            // Stable sorting preserves arrival order within each urgency.
            self.items.sort_by_key(|item| item.reason);
        }
    }

    pub fn retain_panes(&mut self, mut exists: impl FnMut(&str) -> bool) {
        self.items.retain(|item| exists(&item.pane));
    }

    /// Cycle in urgency order without acknowledging an unresolved item.
    pub fn next(&self, current: Option<&str>) -> Option<&str> {
        let index = current
            .and_then(|pane| self.items.iter().position(|item| item.pane == pane))
            .map_or(0, |index| (index + 1) % self.items.len());
        self.items.get(index).map(|item| item.pane.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions_replace_resolve_and_do_not_resurrect_dismissals() {
        let mut inbox = Inbox::default();
        inbox.transition("a", "agent", None, "awaiting");
        inbox.transition("a", "agent", Some("awaiting"), "error");
        assert_eq!(inbox.items().len(), 1);
        assert_eq!(inbox.items()[0].reason, Reason::Failed);
        inbox.dismiss("a");
        inbox.transition("a", "agent", Some("error"), "error");
        assert!(inbox.items().is_empty());
        inbox.transition("a", "agent", Some("error"), "awaiting");
        inbox.transition("a", "agent", Some("awaiting"), "processing");
        assert!(inbox.items().is_empty());
    }

    #[test]
    fn native_idle_is_not_completion_and_navigation_skips_closed_panes() {
        let mut inbox = Inbox::default();
        assert_eq!(inbox.next(None), None);
        inbox.transition("native", "miao", Some("processing"), "idle");
        assert!(inbox.items().is_empty());
        inbox.transition("done", "agent", Some("processing"), "idle");
        inbox.transition("input", "agent", None, "awaiting");
        inbox.transition("failed", "agent", None, "error");
        assert_eq!(inbox.next(None), Some("failed"));
        assert_eq!(inbox.next(Some("failed")), Some("input"));
        assert_eq!(inbox.next(Some("done")), Some("failed"));
        inbox.retain_panes(|pane| pane == "done");
        assert_eq!(inbox.next(None), Some("done"));
        inbox.transition("done", "agent", Some("idle"), "waiting");
        assert_eq!(inbox.next(None), None);
    }
}
