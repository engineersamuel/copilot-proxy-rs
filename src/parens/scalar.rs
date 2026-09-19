/// Finds the index of the parenthesis that closes the one at `open_index`.
///
/// This is the reference implementation of the matching-parenthesis contract.
/// Any other backend must return exactly the same value for every input.
///
/// The scan is a plain depth counter over raw bytes:
///
/// - Returns `None` when `open_index` is out of bounds for `input`.
/// - Returns `None` when `input[open_index]` is not `b'('`.
/// - Otherwise the depth starts at 1 and the scan reads the bytes strictly
///   after `open_index`, in increasing index order, one byte at a time.
/// - Each `b'('` increments the depth. Each `b')'` decrements the depth.
/// - The result is `Some(i)` for the first index `i` at which the depth
///   reaches zero. Because the scan runs in increasing index order, this is
///   always the earliest such index.
/// - Returns `None` when the depth never reaches zero, that is, when the
///   opening parenthesis is never closed.
///
/// Every byte other than `b'('` and `b')'` is literal and has no effect on the
/// depth. This function does not parse string literals, escape sequences,
/// comments, or any other syntax. Bytes such as `"`, `'`, `\`, and the `//`
/// sequence carry no meaning here, and parentheses inside them are counted
/// like all others. There are no exceptions to this rule.
pub(crate) fn find_matching_paren_scalar(input: &[u8], open_index: usize) -> Option<usize> {
    if input.get(open_index) != Some(&b'(') {
        return None;
    }

    let mut depth = 1usize;
    for (offset, byte) in input[open_index + 1..].iter().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open_index + 1 + offset);
                }
            }
            _ => {}
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::find_matching_paren_scalar;

    #[test]
    fn empty_input_has_no_match() {
        assert_eq!(find_matching_paren_scalar(b"", 0), None);
    }

    #[test]
    fn open_index_past_the_end_has_no_match() {
        assert_eq!(find_matching_paren_scalar(b"()", 2), None);
        assert_eq!(find_matching_paren_scalar(b"()", usize::MAX), None);
    }

    #[test]
    fn open_index_on_a_non_open_byte_has_no_match() {
        assert_eq!(find_matching_paren_scalar(b"()", 1), None);
        assert_eq!(find_matching_paren_scalar(b"a()", 0), None);
        assert_eq!(find_matching_paren_scalar(b"[]", 0), None);
    }

    #[test]
    fn immediate_match_is_the_next_byte() {
        assert_eq!(find_matching_paren_scalar(b"()", 0), Some(1));
    }

    #[test]
    fn nested_parens_match_the_outer_pair() {
        assert_eq!(find_matching_paren_scalar(b"(a(b)c)", 0), Some(6));
        assert_eq!(find_matching_paren_scalar(b"(a(b)c)", 2), Some(4));
        assert_eq!(find_matching_paren_scalar(b"(((x)))", 0), Some(6));
        assert_eq!(find_matching_paren_scalar(b"(((x)))", 1), Some(5));
        assert_eq!(find_matching_paren_scalar(b"(((x)))", 2), Some(4));
    }

    #[test]
    fn unmatched_opener_has_no_match() {
        assert_eq!(find_matching_paren_scalar(b"(", 0), None);
        assert_eq!(find_matching_paren_scalar(b"(abc", 0), None);
        assert_eq!(find_matching_paren_scalar(b"((abc)", 0), None);
    }

    #[test]
    fn extra_closers_after_the_match_are_ignored() {
        assert_eq!(find_matching_paren_scalar(b"())))", 0), Some(1));
    }

    #[test]
    fn long_sparse_run_matches_far_from_the_opener() {
        let mut input = Vec::with_capacity(4098);
        input.push(b'(');
        input.extend(std::iter::repeat_n(b'x', 4096));
        input.push(b')');
        assert_eq!(find_matching_paren_scalar(&input, 0), Some(4097));
    }

    #[test]
    fn dense_back_to_back_parens_match_pairwise() {
        let input = b"()()()";
        assert_eq!(find_matching_paren_scalar(input, 0), Some(1));
        assert_eq!(find_matching_paren_scalar(input, 2), Some(3));
        assert_eq!(find_matching_paren_scalar(input, 4), Some(5));

        // 512 nested pairs: "((( ... )))".
        let depth = 512;
        let mut nested = vec![b'('; depth];
        nested.extend(std::iter::repeat_n(b')', depth));
        for level in 0..depth {
            assert_eq!(
                find_matching_paren_scalar(&nested, level),
                Some(2 * depth - 1 - level)
            );
        }
    }

    #[test]
    fn quotes_backslashes_and_comment_markers_are_literal_bytes() {
        // The `)` inside the quoted text closes the opener, because string
        // literals are not parsed.
        assert_eq!(find_matching_paren_scalar(br#"("a)b")"#, 0), Some(3));
        // A backslash never escapes the following parenthesis.
        assert_eq!(find_matching_paren_scalar(br"(\)x)", 0), Some(2));
        // `//` starts no comment, so the `(` after it still raises the depth.
        assert_eq!(find_matching_paren_scalar(b"(// (\n)x)", 0), Some(8));
        // Neither does a `/* */` block.
        assert_eq!(find_matching_paren_scalar(b"(/* ) */)", 0), Some(4));
        // Single quotes and non-UTF-8 bytes are literal too.
        assert_eq!(find_matching_paren_scalar(b"('\xff\x00)", 0), Some(4));
    }
}
