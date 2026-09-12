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
        let mut fallback = vec![0; needle.len()];
        let mut matched = 0;
        for index in 1..needle.len() {
            while matched > 0 && needle[index] != needle[matched] {
                matched = fallback[matched - 1];
            }
            if needle[index] == needle[matched] {
                matched += 1;
            }
            fallback[index] = matched;
        }
        Self { needle, fallback }
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
            for character in valid.chars().flat_map(char::to_lowercase) {
                let mut encoded = [0; 4];
                for &byte in character.encode_utf8(&mut encoded).as_bytes() {
                    while matched > 0 && byte != self.needle[matched] {
                        matched = self.fallback[matched - 1];
                    }
                    if byte == self.needle[matched] {
                        matched += 1;
                    }
                    if matched == self.needle.len() {
                        found = true;
                        matched = self.fallback[matched - 1];
                    }
                }
            }
            buffer.copy_within(end - incomplete..end, 0);
            carry = incomplete;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Matcher;
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
