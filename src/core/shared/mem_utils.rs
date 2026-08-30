//! Centralize generic free paths that Zig would otherwise duplicate at each
//! error exit, reducing generated code to one call per cleanup site.

/// Drop-in replacement for freeing an owned slice.
pub fn free<T>(memory: Vec<T>) {
    drop(memory);
}

/// Drop-in replacement for deinitializing an owned array list: releases the
/// allocated capacity and invalidates the list.
pub fn deinit_list<T>(list: &mut Vec<T>) {
    free(std::mem::take(list));
}

#[cfg(test)]
mod tests {
    use super::{deinit_list, free};

    #[test]
    fn free_releases_an_allocated_slice() {
        let buf: Vec<u64> = vec![0; 16];
        free(buf);
    }

    #[test]
    fn free_accepts_an_empty_slice() {
        let buf: Vec<u8> = Vec::new();
        free(buf);
    }

    #[test]
    fn deinit_list_releases_list_capacity() {
        let mut list: Vec<usize> = vec![7, 11];
        deinit_list(&mut list);
        assert!(list.is_empty());
    }

    #[test]
    fn deinit_list_on_a_never_grown_list_is_a_no_op() {
        let mut list: Vec<u8> = Vec::new();
        deinit_list(&mut list);
        assert!(list.is_empty());
    }
}
