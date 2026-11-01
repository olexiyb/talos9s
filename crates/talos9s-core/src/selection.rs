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

    /// Get a mutable reference to the items
    pub fn items_mut(&mut self) -> &mut Vec<T> {
        &mut self.items
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

    /// Get a mutable reference to the currently selected item
    pub fn selected_mut(&mut self) -> Option<&mut T> {
        self.items.get_mut(self.selected_index)
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

    /// Move selection up by a page (configurable page size)
    pub fn page_up(&mut self, page_size: usize) {
        self.selected_index = self.selected_index.saturating_sub(page_size);
    }

    /// Move selection down by a page (configurable page size)
    pub fn page_down(&mut self, page_size: usize) {
        if !self.items.is_empty() {
            self.selected_index = (self.selected_index + page_size).min(self.items.len() - 1);
        }
    }

    /// Replace all items, resetting selection to 0
    pub fn set_items(&mut self, items: Vec<T>) {
        self.items = items;
        self.selected_index = 0;
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

    /// Iterate over items with their selection state
    pub fn iter_with_selection(&self) -> impl Iterator<Item = (usize, &T, bool)> {
        self.items
            .iter()
            .enumerate()
            .map(|(i, item)| (i, item, i == self.selected_index))
    }

    /// Remove the selected item and return it
    pub fn remove_selected(&mut self) -> Option<T> {
        if self.items.is_empty() {
            return None;
        }

        let item = self.items.remove(self.selected_index);

        // Adjust selection
        if !self.items.is_empty() && self.selected_index >= self.items.len() {
            self.selected_index = self.items.len() - 1;
        }

        Some(item)
    }

    /// Find and select an item matching a predicate
    ///
    /// Returns true if a matching item was found and selected.
    pub fn select_where<F>(&mut self, predicate: F) -> bool
    where
        F: Fn(&T) -> bool,
    {
        for (i, item) in self.items.iter().enumerate() {
            if predicate(item) {
                self.selected_index = i;
                return true;
            }
        }
        false
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

/// A multi-selection list that tracks multiple selected items
///
/// Useful for batch operations where multiple items can be selected.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MultiSelectList<T> {
    items: Vec<T>,
    /// Set of selected indices
    selected: std::collections::HashSet<usize>,
    /// Currently focused index (cursor position)
    focused: usize,
}

impl<T> MultiSelectList<T> {
    /// Create a new multi-select list
    pub fn new(items: Vec<T>) -> Self {
        Self {
            items,
            selected: std::collections::HashSet::new(),
            focused: 0,
        }
    }

    /// Get the items
    pub fn items(&self) -> &[T] {
        &self.items
    }

    /// Get the number of items
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Get the focused index
    pub fn focused_index(&self) -> usize {
        self.focused
    }

    /// Get the focused item
    pub fn focused(&self) -> Option<&T> {
        self.items.get(self.focused)
    }

    /// Move focus to next item
    pub fn focus_next(&mut self) {
        if !self.items.is_empty() {
            self.focused = (self.focused + 1) % self.items.len();
        }
    }

    /// Move focus to previous item
    pub fn focus_prev(&mut self) {
        if !self.items.is_empty() {
            self.focused = if self.focused == 0 {
                self.items.len() - 1
            } else {
                self.focused - 1
            };
        }
    }

    /// Toggle selection of the focused item
    pub fn toggle_focused(&mut self) {
        if self.selected.contains(&self.focused) {
            self.selected.remove(&self.focused);
        } else {
            self.selected.insert(self.focused);
        }
    }

    /// Check if an index is selected
    pub fn is_selected(&self, index: usize) -> bool {
        self.selected.contains(&index)
    }

    /// Check if an index is focused
    pub fn is_focused(&self, index: usize) -> bool {
        index == self.focused
    }

    /// Get count of selected items
    pub fn selected_count(&self) -> usize {
        self.selected.len()
    }

    /// Get indices of selected items
    pub fn selected_indices(&self) -> impl Iterator<Item = &usize> {
        self.selected.iter()
    }

    /// Get selected items
    pub fn selected_items(&self) -> Vec<&T> {
        self.selected
            .iter()
            .filter_map(|&i| self.items.get(i))
            .collect()
    }

    /// Select all items
    pub fn select_all(&mut self) {
        self.selected = (0..self.items.len()).collect();
    }

    /// Deselect all items
    pub fn deselect_all(&mut self) {
        self.selected.clear();
    }

    /// Set selection state for an index
    pub fn set_selected(&mut self, index: usize, selected: bool) {
        if selected {
            self.selected.insert(index);
        } else {
            self.selected.remove(&index);
        }
    }

    /// Replace items, clearing selection
    pub fn set_items(&mut self, items: Vec<T>) {
        self.items = items;
        self.selected.clear();
        self.focused = 0;
    }

    /// Iterate with selection and focus state
    pub fn iter_with_state(&self) -> impl Iterator<Item = (usize, &T, bool, bool)> {
        self.items
            .iter()
            .enumerate()
            .map(|(i, item)| (i, item, self.is_selected(i), self.is_focused(i)))
    }
}

impl<T> From<Vec<T>> for MultiSelectList<T> {
    fn from(items: Vec<T>) -> Self {
        Self::new(items)
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
    fn test_selectable_list_page_navigation() {
        let mut list = SelectableList::new((0..20).collect::<Vec<_>>());

        list.page_down(5);
        assert_eq!(list.selected_index(), 5);

        list.page_down(100);
        assert_eq!(list.selected_index(), 19); // Clamped to end

        list.page_up(5);
        assert_eq!(list.selected_index(), 14);

        list.page_up(100);
        assert_eq!(list.selected_index(), 0); // Clamped to start
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
    fn test_selectable_list_select_where() {
        let mut list = SelectableList::new(vec!["apple", "banana", "cherry"]);

        assert!(list.select_where(|s| s.starts_with("b")));
        assert_eq!(list.selected(), Some(&"banana"));

        assert!(!list.select_where(|s| s.starts_with("z")));
        // Selection unchanged
        assert_eq!(list.selected(), Some(&"banana"));
    }

    #[test]
    fn test_multi_select_list() {
        let mut list = MultiSelectList::new(vec!["a", "b", "c"]);

        assert_eq!(list.selected_count(), 0);

        list.toggle_focused();
        assert!(list.is_selected(0));
        assert_eq!(list.selected_count(), 1);

        list.focus_next();
        list.toggle_focused();
        assert_eq!(list.selected_count(), 2);

        list.toggle_focused();
        assert_eq!(list.selected_count(), 1);
        assert!(!list.is_selected(1));

        list.select_all();
        assert_eq!(list.selected_count(), 3);

        list.deselect_all();
        assert_eq!(list.selected_count(), 0);
    }
}
