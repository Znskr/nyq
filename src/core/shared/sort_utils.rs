use std::cmp::Ordering;

// Share one stable sort across element types to avoid duplicating generic code.
// Rust's slice::sort_by is a stable sort (tim sort), which matches the
// "drop-in replacement for std.mem.sort" contract from the Zig original.

/// Drop-in replacement for `std.mem.sort`: stable, in place.
pub fn sort<T, C, F>(items: &mut [T], context: C, less_than: F)
where
    F: Fn(&C, &T, &T) -> bool,
{
    items.sort_by(|a, b| {
        if less_than(&context, a, b) {
            Ordering::Less
        } else if less_than(&context, b, a) {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct TestElem {
        key: u16,
        seq: u16,
    }

    fn test_elem_key_less(_: &(), lhs: &TestElem, rhs: &TestElem) -> bool {
        lhs.key < rhs.key
    }

    // A stable sort's output is uniquely determined by its input, so comparing
    // against `slice::sort_by` element-for-element is an exact oracle.
    fn expect_matches_std(items: &[TestElem]) {
        let mut expected = items.to_vec();
        let mut actual = items.to_vec();
        expected.sort_by(|a, b| a.key.cmp(&b.key));
        sort(&mut actual, (), test_elem_key_less);
        assert_eq!(expected, actual);
    }

    struct XorShift64(u64);

    impl XorShift64 {
        fn new(seed: u64) -> Self {
            XorShift64(seed)
        }

        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn uint_less_than_u16(&mut self, range: u16) -> u16 {
            (self.next_u64() % range as u64) as u16
        }

        fn next_u8(&mut self) -> u8 {
            self.next_u64() as u8
        }

        fn int_range_at_most_i32(&mut self, min: i32, max: i32) -> i32 {
            let width = (max - min + 1) as u64;
            min + (self.next_u64() % width) as i32
        }
    }

    #[test]
    fn matches_stable_sort_across_sizes_seeds_and_key_densities() {
        let sizes = [0usize, 1, 2, 3, 5, 19, 20, 21, 40, 41, 100, 257, 1000];
        let key_ranges = [1u16, 2, 3, 16, 251, 65535];
        let mut prng = XorShift64::new(0x5eed);
        for &n in &sizes {
            for &range in &key_ranges {
                let mut items = Vec::with_capacity(n);
                for i in 0..n {
                    items.push(TestElem {
                        key: prng.uint_less_than_u16(range),
                        seq: (i % 65535) as u16,
                    });
                }
                expect_matches_std(&items);
            }
        }
    }

    #[test]
    fn matches_stable_sort_on_adversarial_patterns() {
        let n: usize = 337;
        let mut items: Vec<TestElem> = (0..n)
            .map(|i| TestElem { key: i as u16, seq: i as u16 })
            .collect();
        expect_matches_std(&items);

        items = (0..n)
            .map(|i| TestElem { key: (n - 1 - i) as u16, seq: i as u16 })
            .collect();
        expect_matches_std(&items);

        items = (0..n)
            .map(|i| TestElem { key: (i % 7) as u16, seq: i as u16 })
            .collect();
        expect_matches_std(&items);

        items = (0..n)
            .map(|i| TestElem { key: 42, seq: i as u16 })
            .collect();
        expect_matches_std(&items);

        items = (0..n)
            .map(|i| {
                let key: u16 = if i < n / 2 { i as u16 } else { (n - i) as u16 };
                TestElem { key, seq: i as u16 }
            })
            .collect();
        expect_matches_std(&items);
    }

    fn permute_and_check(items: &mut Vec<TestElem>, k: usize) {
        if k <= 1 {
            expect_matches_std(items);
            return;
        }
        for i in 0..k {
            permute_and_check(items, k - 1);
            if k % 2 == 0 {
                items.swap(i, k - 1);
            } else {
                items.swap(0, k - 1);
            }
        }
    }

    #[test]
    fn every_permutation_of_duplicate_heavy_small_inputs_matches() {
        for n in 1..=6usize {
            let mut items: Vec<TestElem> = (0..n)
                .map(|i| TestElem { key: (i % 3) as u16, seq: i as u16 })
                .collect();
            permute_and_check(&mut items, n);
        }
    }

    #[derive(Clone, Copy)]
    struct Pivot {
        value: i32,
    }

    fn distance_less(ctx: &Pivot, lhs: &i32, rhs: &i32) -> bool {
        (lhs - ctx.value).abs() < (rhs - ctx.value).abs()
    }

    #[test]
    fn runtime_context_reaches_the_comparator() {
        let mut prng = XorShift64::new(0xf00);
        let mut expected: Vec<i32> = (0..500)
            .map(|_| prng.int_range_at_most_i32(-1000, 1000))
            .collect();
        let mut actual = expected.clone();
        let pivot = Pivot { value: 37 };
        expected.sort_by(|a, b| {
            if distance_less(&pivot, a, b) {
                Ordering::Less
            } else if distance_less(&pivot, b, a) {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        });
        sort(&mut actual, pivot, distance_less);
        assert_eq!(expected, actual);
    }

    fn lex_less(_: &(), lhs: &[u8; 7], rhs: &[u8; 7]) -> bool {
        lhs < rhs
    }

    #[test]
    fn odd_sized_elements_sort_correctly() {
        let mut prng = XorShift64::new(7);
        let mut expected: Vec<[u8; 7]> = (0..300)
            .map(|_| {
                let mut elem = [0u8; 7];
                for byte in elem.iter_mut() {
                    *byte = prng.next_u8();
                }
                elem[0] %= 3;
                elem
            })
            .collect();
        let mut actual = expected.clone();
        expected.sort_by(|a, b| a.cmp(b));
        sort(&mut actual, (), lex_less);
        assert_eq!(expected, actual);
    }

    fn str_less(_: &(), lhs: &&[u8], rhs: &&[u8]) -> bool {
        *lhs < *rhs
    }

    #[test]
    fn slice_elements_sort_by_content_with_duplicates() {
        let source: [&str; 8] = [
            "delta",
            "alpha",
            "charlie",
            "alpha",
            "bravo",
            "",
            "charlie",
            "alpha",
        ];
        let mut expected: Vec<&[u8]> = source.iter().map(|s| s.as_bytes()).collect();
        let mut actual = expected.clone();
        expected.sort_by(|a, b| a.cmp(b));
        sort(&mut actual, (), str_less);
        assert_eq!(expected, actual);
    }
}
