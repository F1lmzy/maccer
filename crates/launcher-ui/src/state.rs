use launcher_core::{Action, Item, Selection};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Screen {
    Search,
    Actions,
    Output,
}

#[derive(Default)]
pub(crate) struct LauncherState {
    pub screen: Option<Screen>,
    pub selection: Selection,
    pub action_index: usize,
    pub query: String,
    pub items: Vec<Item>,
    pub generation: u64,
    pub searching: bool,
    selection_explicit: bool,
}

pub(crate) const MAX_VISIBLE_RESULTS: usize = 6;

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
        // Keep the last suggestions visible while their replacements are loading.
        self.searching = true;
        self.selection_explicit = false;
        self.screen = Some(Screen::Search);
    }

    pub fn apply_results(&mut self, generation: u64, items: Vec<Item>) -> bool {
        if generation != self.generation {
            return false;
        }
        if self.searching || !self.selection_explicit {
            self.selection = Selection::default();
        }
        self.searching = false;
        self.items = items;
        self.selection.reconcile(&self.items);
        true
    }

    pub fn apply_search_update(
        &mut self,
        generation: u64,
        items: Vec<Item>,
        complete: bool,
    ) -> bool {
        // A fast provider with no matches must not blank slower providers' suggestions.
        if items.is_empty() && !complete {
            return false;
        }
        self.apply_results(generation, items)
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.searching || self.items.is_empty() {
            return;
        }
        self.selection_explicit = true;
        self.selection.move_by(&self.items, delta);
    }

    pub fn select_item(&mut self, item: &Item) {
        if self.searching {
            return;
        }
        self.selection_explicit = true;
        self.selection.reconcile(std::slice::from_ref(item));
    }

    pub fn enter_actions(&mut self, actions: &[Action]) -> bool {
        if self.searching || self.selection.current(&self.items).is_none() || actions.is_empty() {
            return false;
        }
        self.selection_explicit = true;
        self.action_index = 0;
        self.screen = Some(Screen::Actions);
        true
    }

    pub fn escape(&mut self) -> EscapeResult {
        match self.screen {
            Some(Screen::Actions | Screen::Output) => {
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
    fn automatic_selection_tracks_the_top_streamed_result() {
        let mut state = LauncherState {
            generation: 7,
            ..Default::default()
        };
        state.replace_query("aerospace".into());
        let mut google = item("google");
        google.provider = ProviderId("web".into());
        assert!(state.apply_search_update(7, vec![google.clone()], false));
        assert_eq!(
            state.selection.current(&state.items).unwrap().id.0,
            "google"
        );
        assert!(state.apply_search_update(7, vec![item("AeroSpace"), google], true));
        assert_eq!(
            selected_result_index(&state.items, &state.selection),
            Some(0)
        );
        assert_eq!(
            state.selection.current(&state.items).unwrap().id.0,
            "AeroSpace"
        );
    }

    #[test]
    fn selection_stays_with_item_when_streamed_results_reorder() {
        let mut state = LauncherState {
            generation: 7,
            ..Default::default()
        };
        assert!(state.apply_results(7, vec![item("a"), item("b")]));
        state.move_selection(1);
        assert!(state.apply_results(7, vec![item("new-top"), item("a"), item("b")]));
        assert_eq!(state.selection.current(&state.items).unwrap().id.0, "b");
    }

    #[test]
    fn clicked_selection_is_preserved_until_the_query_changes() {
        let mut state = LauncherState {
            generation: 7,
            ..Default::default()
        };
        state.apply_search_update(7, vec![item("google")], false);
        state.select_item(&item("google"));
        state.apply_search_update(7, vec![item("AeroSpace"), item("google")], true);
        assert_eq!(
            state.selection.current(&state.items).unwrap().id.0,
            "google"
        );
        state.replace_query("safari".into());
        state.generation = 8;
        state.apply_search_update(8, vec![item("google")], false);
        state.apply_search_update(8, vec![item("Safari"), item("google")], true);
        assert_eq!(
            state.selection.current(&state.items).unwrap().id.0,
            "Safari"
        );
    }

    #[test]
    fn query_edits_keep_suggestions_until_replacement_results_arrive() {
        let mut state = LauncherState {
            generation: 1,
            ..Default::default()
        };
        state.apply_results(1, vec![item("old")]);
        state.replace_query("new".into());
        assert_eq!(state.items.len(), 1, "typing must not blank the list");
        assert_eq!(state.items[0].id.0, "old");
        state.generation = 2;
        state.apply_results(2, vec![item("new")]);
        assert_eq!(state.items[0].id.0, "new");
        state.apply_results(2, vec![]);
        assert!(
            state.items.is_empty(),
            "a finished empty search must clear old results"
        );
    }

    #[test]
    fn empty_partial_batches_do_not_blank_pending_suggestions() {
        let mut state = LauncherState {
            generation: 1,
            ..Default::default()
        };
        state.apply_results(1, vec![item("old")]);
        state.replace_query("new".into());
        state.generation = 2;
        assert!(!state.apply_search_update(2, vec![], false));
        assert_eq!(state.items[0].id.0, "old");
        assert!(state.searching);
        assert!(!state.apply_search_update(1, vec![item("stale")], true));
        assert!(state.searching);
        assert!(state.apply_search_update(2, vec![], true));
        assert!(state.items.is_empty());
        assert!(!state.searching);
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
        assert_eq!(visible_result_count(items.len()), MAX_VISIBLE_RESULTS);

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
