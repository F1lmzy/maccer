use crate::Item;

/// Tracks the selected item by stable provider/item identity, not row number.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Selection {
    selected: Option<(String, String)>,
}

impl Selection {
    pub fn selected(&self) -> Option<(&str, &str)> {
        self.selected
            .as_ref()
            .map(|(p, i)| (p.as_str(), i.as_str()))
    }
    pub fn reconcile(&mut self, items: &[Item]) {
        if self.selected.as_ref().is_some_and(|(p, i)| {
            items
                .iter()
                .any(|item| &item.provider.0 == p && &item.id.0 == i)
        }) {
            return;
        }
        self.selected = items
            .first()
            .map(|item| (item.provider.0.clone(), item.id.0.clone()));
    }
    pub fn move_by(&mut self, items: &[Item], delta: isize) {
        if items.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|(p, i)| {
                items
                    .iter()
                    .position(|item| &item.provider.0 == p && &item.id.0 == i)
            })
            .unwrap_or(0);
        let len = items.len() as isize;
        let next = (current as isize + delta).rem_euclid(len) as usize;
        self.selected = Some((items[next].provider.0.clone(), items[next].id.0.clone()));
    }
    pub fn current<'a>(&self, items: &'a [Item]) -> Option<&'a Item> {
        let (provider, id) = self.selected.as_ref()?;
        items
            .iter()
            .find(|item| &item.provider.0 == provider && &item.id.0 == id)
    }
}
