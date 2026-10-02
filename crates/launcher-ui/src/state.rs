use launcher_core::{Action, Item, Selection};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Screen {
    Search,
    Actions,
}

#[derive(Default)]
pub(crate) struct LauncherState {
    pub screen: Option<Screen>,
    pub selection: Selection,
    pub action_index: usize,
    pub query: String,
    pub items: Vec<Item>,
    pub generation: u64,
}

pub(crate) const MAX_VISIBLE_RESULTS: usize = 8;

pub(crate) fn result_limit(max_results: usize) -> usize {
    max_results.max(1)
}

pub(crate) fn visible_result_count(total_results: usize) -> usize {
    total_results.min(MAX_VISIBLE_RESULTS)
}

pub(crate) fn selected_result_index(items: &[Item], selection: &Selection) -> Option<usize> {
    let (provider, id) = selection.selected()?;
    items
        .iter()
        .position(|item| item.provider.0 == provider && item.id.0 == id)
}

pub(crate) fn should_apply_activation(completed: u64, current: u64) -> bool {
    completed == current
}

pub(crate) fn should_hide_after_deactivation(
    was_active: bool,
    is_open: bool,
    active: bool,
) -> bool {
    was_active && is_open && !active
}

impl LauncherState {
    pub fn replace_query(&mut self, query: String) {
        self.query = query;
        self.items.clear();
        self.selection.reconcile(&self.items);
        self.screen = Some(Screen::Search);
    }

    pub fn apply_results(&mut self, generation: u64, items: Vec<Item>) -> bool {
        if generation != self.generation {
            return false;
        }
        self.items = items;
        self.selection.reconcile(&self.items);
        true
    }

    pub fn enter_actions(&mut self, actions: &[Action]) -> bool {
        if self.selection.current(&self.items).is_none() || actions.is_empty() {
            return false;
        }
        self.action_index = 0;
        self.screen = Some(Screen::Actions);
        true
    }

    pub fn escape(&mut self) -> EscapeResult {
        match self.screen {
            Some(Screen::Actions) => {
                self.screen = Some(Screen::Search);
                EscapeResult::Back
            }
            Some(Screen::Search) if !self.query.is_empty() => {
                self.replace_query(String::new());
                EscapeResult::Cleared
            }
            _ => EscapeResult::Hide,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EscapeResult {
    Back,
    Cleared,
    Hide,
}

#[cfg(test)]
mod tests {
    use super::*;
    use launcher_core::{ItemId, ProviderId};

    fn item(id: &str) -> Item {
        Item {
            id: ItemId(id.into()),
            provider: ProviderId("apps".into()),
            title: id.into(),
            subtitle: None,
            keywords: vec![],
            icon: None,
            score: 0.0,
            payload: serde_json::json!({}),
        }
    }

    #[test]
    fn selection_stays_with_item_when_streamed_results_reorder() {
        let mut state = LauncherState {
            generation: 7,
            ..Default::default()
        };
        assert!(state.apply_results(7, vec![item("a"), item("b")]));
        state.selection.move_by(&state.items, 1);
        assert!(state.apply_results(7, vec![item("b"), item("a")]));
        assert_eq!(state.selection.current(&state.items).unwrap().id.0, "b");
    }

    #[test]
    fn stale_search_updates_are_ignored() {
        let mut state = LauncherState {
            generation: 2,
            ..Default::default()
        };
        assert!(!state.apply_results(1, vec![item("old")]));
        assert!(state.items.is_empty());
    }

    #[test]
    fn deactivation_hides_only_after_a_logical_show_was_activated() {
        assert!(!should_hide_after_deactivation(false, true, false));
        assert!(!should_hide_after_deactivation(true, false, false));
        assert!(!should_hide_after_deactivation(true, true, true));
        assert!(should_hide_after_deactivation(true, true, false));
    }

    #[test]
    fn results_above_viewport_remain_selectable_and_scroll_to_the_selected_row() {
        let items: Vec<_> = (0..12).map(|i| item(&i.to_string())).collect();
        assert_eq!(result_limit(50), 50);
        assert_eq!(visible_result_count(items.len()), 8);

        let mut selection = Selection::default();
        selection.reconcile(&items);
        for _ in 0..11 {
            selection.move_by(&items, 1);
        }
        assert_eq!(selected_result_index(&items, &selection), Some(11));
        assert!(
            selected_result_index(&items, &selection).unwrap() >= visible_result_count(items.len())
        );
    }

    #[test]
    fn a_late_activation_cannot_apply_after_query_state_changes() {
        let mut state = LauncherState {
            screen: Some(Screen::Actions),
            query: "safari".into(),
            ..Default::default()
        };
        let activation_serial = 9;
        state.replace_query("notes".into());
        assert_eq!(state.screen, Some(Screen::Search));
        assert!(!should_apply_activation(activation_serial, 10));
    }

    #[test]
    fn actions_and_escape_follow_back_clear_hide_order() {
        let mut state = LauncherState {
            generation: 1,
            query: "safari".into(),
            screen: Some(Screen::Search),
            ..Default::default()
        };
        state.apply_results(1, vec![item("safari")]);
        let actions = vec![Action {
            id: launcher_core::ActionId("open".into()),
            title: "Open".into(),
        }];
        assert!(state.enter_actions(&actions));
        assert_eq!(state.escape(), EscapeResult::Back);
        assert_eq!(state.escape(), EscapeResult::Cleared);
        assert_eq!(state.escape(), EscapeResult::Hide);
    }
}
