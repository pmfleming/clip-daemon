//! Full-text search without retaining complete clipboard values in memory.
use std::io::{self, Read};

pub(super) fn fold(value: &str) -> String {
    value.chars().flat_map(char::to_lowercase).collect()
}

pub(super) struct Matcher {
    needle: Vec<u8>,
    fallback: Vec<usize>,
}

impl Matcher {
    pub fn new(needle: &str) -> Self {
        let needle = fold(needle).into_bytes();
        let mut matcher = Self {
            fallback: vec![0; needle.len()],
            needle,
        };
        for index in 1..matcher.needle.len() {
            matcher.fallback[index] =
                matcher.advance(matcher.fallback[index - 1], matcher.needle[index]);
        }
        matcher
    }

    // Both prefix construction and streaming search use the same KMP transition.
    fn advance(&self, mut matched: usize, byte: u8) -> usize {
        while matched > 0 && byte != self.needle[matched] {
            matched = self.fallback[matched - 1];
        }
        matched + usize::from(byte == self.needle[matched])
    }

    fn feed(&self, text: &str, matched: &mut usize, found: &mut bool) {
        for character in text.chars().flat_map(char::to_lowercase) {
            let mut encoded = [0; 4];
            for &byte in character.encode_utf8(&mut encoded).as_bytes() {
                *matched = self.advance(*matched, byte);
                if *matched == self.needle.len() {
                    *found = true;
                    *matched = self.fallback[*matched - 1];
                }
            }
        }
    }

    pub fn contains(&self, mut source: impl Read) -> io::Result<bool> {
        if self.needle.is_empty() {
            return Ok(true);
        }
        let mut buffer = [0; 16 * 1024 + 3];
        let mut carry = 0;
        let mut matched = 0;
        let mut found = false;
        loop {
            let length = match source.read(&mut buffer[carry..16 * 1024]) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if length == 0 {
                return Ok(found && carry == 0);
            }
            let end = carry + length;
            let (valid, incomplete) = match std::str::from_utf8(&buffer[..end]) {
                Ok(text) => (text, 0),
                Err(error) if error.error_len().is_none() => (
                    std::str::from_utf8(&buffer[..error.valid_up_to()])
                        .map_err(io::Error::other)?,
                    end - error.valid_up_to(),
                ),
                Err(_) => return Ok(false),
            };
            // A match is not success until the entire input is valid UTF-8.
            self.feed(valid, &mut matched, &mut found);
            buffer.copy_within(end - incomplete..end, 0);
            carry = incomplete;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Matcher, fold};
    use std::io::{self, Read};

    struct Chunks<'a> {
        bytes: &'a [u8],
        size: usize,
        interrupted: bool,
    }
    impl Read for Chunks<'_> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if std::mem::take(&mut self.interrupted) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let length = output.len().min(self.size);
            self.bytes.read(&mut output[..length])
        }
    }

    #[test]
    fn short_reads_match_whole_string_search_and_reject_invalid_tails() {
        for text in ["", "ababababac", "ÉCOLE İSTANBUL 😀", "€éa"] {
            for needle in ["", "ababac", "école i̇stanbul", "😀", "€é", "absent"] {
                for size in 1..=7 {
                    let input = Chunks {
                        bytes: text.as_bytes(),
                        size,
                        interrupted: true,
                    };
                    assert_eq!(
                        Matcher::new(needle).contains(input).unwrap(),
                        fold(text).contains(&fold(needle))
                    );
                }
            }
        }
        for bytes in [&b"match\xff"[..], &b"match\xf0\x9f"[..]] {
            let input = Chunks {
                bytes,
                size: 1,
                interrupted: true,
            };
            assert!(!Matcher::new("match").contains(input).unwrap());
        }
        let failure = &b"match"[..];
        // An I/O failure after a match must still propagate.
        struct Failure;
        impl Read for Failure {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::Other.into())
            }
        }
        assert!(
            Matcher::new("match")
                .contains(failure.chain(Failure))
                .is_err()
        );
    }

    #[test]
    fn complete_unicode_text_is_searched_across_chunks() {
        let text = format!(
            "{}ÉCOLE\nİstanbul{}late-needle",
            "x".repeat(16 * 1024 - 1),
            "y".repeat(90_000)
        );
        for needle in ["école\ni̇stanbul", "late-needle", "XXÉCOLE"] {
            assert!(
                Matcher::new(needle).contains(text.as_bytes()).unwrap(),
                "{needle}"
            );
        }
        assert!(!Matcher::new("absent").contains(text.as_bytes()).unwrap());
        assert!(!Matcher::new("match").contains(&b"match\xff"[..]).unwrap());
        assert!(!Matcher::new("match").contains(&b"match\xc3"[..]).unwrap());
        assert!(Matcher::new("ababac").contains(&b"ababababac"[..]).unwrap());
    }
}
