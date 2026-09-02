pub fn needs_quotes(path: &[u8]) -> bool {
    path.contains(&b' ')
}

pub fn is_representable(path: &[u8]) -> bool {
    if !path.is_empty() && path[0] == b'"' {
        return false;
    }
    for &byte in path {
        if byte == b'\t' || byte == b'\n' || byte == b'\r' {
            return false;
        }
    }
    !needs_quotes(path) || !path.contains(&b'"')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn representable_paths_cannot_open_picker_quote_grammar() {
        assert!(!is_representable("\"leading-quote".as_bytes()));
        assert!(is_representable("embedded-\"-quote".as_bytes()));
    }
}
