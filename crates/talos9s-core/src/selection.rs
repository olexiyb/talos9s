//! Selection abstractions for list-based UI components
//!
//! Provides generic, reusable selection logic that eliminates
//! duplication across TUI components.

use serde::{Deserialize, Serialize};

/// A generic selectable list with automatic bounds checking
///
/// This type manages selection state for any list of items,
/// providing consistent navigation behavior across components.
///
/// # Examples
///
/// ```
/// use talos9s_core::selection::SelectableList;
///
/// let mut list = SelectableList::new(vec!["a", "b", "c"]);
///
/// assert_eq!(list.selected(), Some(&"a"));
/// list.select_next();
/// assert_eq!(list.selected(), Some(&"b"));
/// list.select_prev();
/// assert_eq!(list.selected(), Some(&"a"));
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectableList<T> {
    items: Vec<T>,
    selected_index: usize,
}

impl<T> Default for SelectableList<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            selected_index: 0,
        }
    }
}

impl<T> SelectableList<T> {
    /// Create a new selectable list with the given items
    ///
    /// Selection starts at index 0 if items are present.
    pub fn new(items: Vec<T>) -> Self {
        Self {
            items,
            selected_index: 0,
        }
    }

    /// Create an empty selectable list
    pub fn empty() -> Self {
        Self::default()
    }

    /// Get a reference to the items
    pub fn items(&self) -> &[T] {
        &self.items
    }

    /// Get the number of items
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Check if the list is empty
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Get the currently selected index
    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    /// Get the currently selected item
    pub fn selected(&self) -> Option<&T> {
        self.items.get(self.selected_index)
    }

    /// Check if an index is the selected index
    pub fn is_selected(&self, index: usize) -> bool {
        index == self.selected_index
    }

    /// Move selection to the next item (wraps to start)
    pub fn select_next(&mut self) {
        if !self.items.is_empty() {
            self.selected_index = (self.selected_index + 1) % self.items.len();
        }
    }

    /// Move selection to the next item (no wrap, stops at end)
    pub fn select_next_no_wrap(&mut self) {
        if self.selected_index + 1 < self.items.len() {
            self.selected_index += 1;
        }
    }

    /// Move selection to the previous item (wraps to end)
    pub fn select_prev(&mut self) {
        if !self.items.is_empty() {
            self.selected_index = if self.selected_index == 0 {
                self.items.len() - 1
            } else {
                self.selected_index - 1
            };
        }
    }

    /// Move selection to the previous item (no wrap, stops at start)
    pub fn select_prev_no_wrap(&mut self) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
        }
    }

    /// Select a specific index
    ///
    /// Clamps to valid range if index is out of bounds.
    pub fn select(&mut self, index: usize) {
        if self.items.is_empty() {
            self.selected_index = 0;
        } else {
            self.selected_index = index.min(self.items.len() - 1);
        }
    }

    /// Select the first item
    pub fn select_first(&mut self) {
        self.selected_index = 0;
    }

    /// Select the last item
    pub fn select_last(&mut self) {
        if !self.items.is_empty() {
            self.selected_index = self.items.len() - 1;
        }
    }

    /// Replace all items, preserving selection if possible
    pub fn update_items(&mut self, items: Vec<T>) {
        let old_len = self.items.len();
        self.items = items;

        // Clamp selection to new bounds
        if self.items.is_empty() {
            self.selected_index = 0;
        } else if self.selected_index >= self.items.len() {
            // Try to keep similar position
            self.selected_index = self.items.len() - 1;
        }
        // Otherwise keep current selection
        let _ = old_len; // silence unused warning
    }

    /// Push an item to the end
    pub fn push(&mut self, item: T) {
        self.items.push(item);
    }

    /// Clear all items
    pub fn clear(&mut self) {
        self.items.clear();
        self.selected_index = 0;
    }
}

impl<T> From<Vec<T>> for SelectableList<T> {
    fn from(items: Vec<T>) -> Self {
        Self::new(items)
    }
}

impl<T> FromIterator<T> for SelectableList<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self::new(iter.into_iter().collect())
    }
}

impl<T> IntoIterator for SelectableList<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter()
    }
}

impl<'a, T> IntoIterator for &'a SelectableList<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}

/// Vim-style visual selection over a list: an anchor row set with `v`, and
/// the cursor. Rows between the two, inclusive, are selected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VisualSelection {
    anchor: Option<usize>,
}

impl VisualSelection {
    pub fn active(&self) -> bool {
        self.anchor.is_some()
    }

    /// Start at the cursor, or stop when already selecting
    pub fn toggle(&mut self, cursor: usize) {
        self.anchor = match self.anchor {
            Some(_) => None,
            None => Some(cursor),
        };
    }

    pub fn clear(&mut self) {
        self.anchor = None;
    }

    /// The selected rows, `None` when not selecting
    pub fn range(&self, cursor: usize) -> Option<std::ops::RangeInclusive<usize>> {
        self.anchor
            .map(|anchor| anchor.min(cursor)..=anchor.max(cursor))
    }

    pub fn contains(&self, row: usize, cursor: usize) -> bool {
        self.range(cursor).is_some_and(|r| r.contains(&row))
    }

    /// The rows to copy: the selection, or the cursor row alone
    pub fn rows(&self, cursor: usize) -> std::ops::RangeInclusive<usize> {
        self.range(cursor).unwrap_or(cursor..=cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_selectable_list_navigation() {
        let mut list = SelectableList::new(vec!["a", "b", "c"]);

        assert_eq!(list.selected_index(), 0);
        assert_eq!(list.selected(), Some(&"a"));

        list.select_next();
        assert_eq!(list.selected_index(), 1);
        assert_eq!(list.selected(), Some(&"b"));

        list.select_next();
        list.select_next();
        // Should wrap to start
        assert_eq!(list.selected_index(), 0);

        list.select_prev();
        // Should wrap to end
        assert_eq!(list.selected_index(), 2);
    }

    #[test]
    fn test_selectable_list_no_wrap() {
        let mut list = SelectableList::new(vec!["a", "b", "c"]);

        list.select_prev_no_wrap();
        assert_eq!(list.selected_index(), 0); // Stays at start

        list.select_last();
        list.select_next_no_wrap();
        assert_eq!(list.selected_index(), 2); // Stays at end
    }

    #[test]
    fn test_selectable_list_empty() {
        let mut list: SelectableList<i32> = SelectableList::empty();

        assert!(list.is_empty());
        assert_eq!(list.selected(), None);

        list.select_next();
        assert_eq!(list.selected_index(), 0);

        list.push(42);
        assert_eq!(list.selected(), Some(&42));
    }

    #[test]
    fn test_selectable_list_update_items() {
        let mut list = SelectableList::new(vec!["a", "b", "c", "d", "e"]);
        list.select(3); // Select "d"

        // Shrink list - selection should be clamped
        list.update_items(vec!["x", "y"]);
        assert_eq!(list.selected_index(), 1); // Clamped to last
        assert_eq!(list.selected(), Some(&"y"));

        // Grow list - selection should stay
        list.update_items(vec!["a", "b", "c", "d", "e"]);
        assert_eq!(list.selected_index(), 1); // Preserved
    }

    #[test]
    fn visual_selection_spans_anchor_and_cursor() {
        let mut v = VisualSelection::default();
        assert!(!v.active());
        assert_eq!(v.range(3), None);
        assert_eq!(v.rows(3), 3..=3);
        assert!(!v.contains(3, 3));
        v.toggle(5);
        assert!(v.active());
        // The cursor moved up past the anchor
        assert_eq!(v.range(2), Some(2..=5));
        assert!(v.contains(2, 2) && v.contains(5, 2) && !v.contains(6, 2));
        assert_eq!(v.rows(8), 5..=8);
        v.toggle(8);
        assert!(!v.active());
        v.toggle(1);
        v.clear();
        assert_eq!(v.range(1), None);
    }
}
