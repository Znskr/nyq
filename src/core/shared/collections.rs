//! Owned string-list helpers shared across the codebase.

/// Frees every owned string in `list` and deinitializes the list itself,
/// releasing its capacity.
pub fn free_string_list(list: &mut Vec<Vec<u8>>) {
    drop(list.drain(..));
    list.shrink_to_fit();
}

#[cfg(test)]
mod tests {
    use super::free_string_list;

    #[test]
    fn free_string_list_frees_and_deinits_an_owned_string_list() {
        let mut list: Vec<Vec<u8>> = Vec::new();
        list.push(b"alpha".to_vec());
        list.push(b"beta".to_vec());

        free_string_list(&mut list);

        assert!(list.is_empty());
    }
}
