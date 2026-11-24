//! Recognize the detach key in terminal input.
//!
//! Programs can switch the user's terminal into richer keyboard protocols
//! (kitty's `CSI u`, xterm's modifyOtherKeys). Ctrl-a then arrives as
//! `ESC [ 97 ; 5 u` instead of the byte 0x01, so matching raw bytes is not
//! enough. Like tmux, we decode input into keys and compare those.

/// A key press, normalized so every encoding of the same key compares equal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    code: u32,
    ctrl: bool,
    alt: bool,
}

/// Never equal to a real key: breaks a partial match.
const OTHER: Key = Key {
    code: u32::MAX,
    ctrl: false,
    alt: false,
};

enum Unit {
    Key(Key),
    /// Key releases and bare modifier presses: they neither advance nor break a match.
    Ignore,
    PasteStart,
    PasteEnd,
}

enum Next {
    Unit(usize, Unit),
    /// `buf` ends inside an escape sequence; wait for more input.
    Incomplete,
}

/// The key a legacy terminal sends as this byte.
fn legacy(b: u8) -> Key {
    let (code, ctrl) = match b {
        0 => (b'@', true),
        1..=26 => (b'a' + b - 1, true),
        0x1b => (b'[', true),
        0x1c => (b'\\', true),
        0x1d => (b']', true),
        0x1e => (b'^', true),
        0x1f => (b'_', true),
        0x7f => (b'?', true),
        _ => (b, false),
    };
    Key {
        code: code.into(),
        ctrl,
        alt: false,
    }
}

/// Key from a kitty `CSI u` / modifyOtherKeys code point and modifier value.
fn encoded(code: u32, mods: u32) -> Option<Key> {
    let bits = mods.saturating_sub(1) & !(64 | 128); // ignore Caps Lock / Num Lock
    if bits & !(1 | 2 | 4) != 0 {
        return None; // Super/Hyper/Meta: never part of a detach key
    }
    let (shift, alt, ctrl) = (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0);
    let mut key = match u8::try_from(code) {
        Ok(b) if b < 0x20 || b == 0x7f => legacy(b),
        _ => Key {
            code,
            ctrl: false,
            alt: false,
        },
    };
    key.alt |= alt;
    if ctrl {
        key.ctrl = true;
        key.code = match char::from_u32(key.code).map(|c| c.to_ascii_lowercase()) {
            Some(' ') => '@'.into(),
            Some('/') => '_'.into(),
            Some(c) => c.into(),
            None => key.code,
        };
    } else if shift && let Some(c) = char::from_u32(key.code).filter(char::is_ascii_lowercase) {
        key.code = c.to_ascii_uppercase().into();
    }
    Some(key)
}

fn params(raw: &[u8]) -> Vec<Vec<u32>> {
    raw.split(|&b| b == b';')
        .map(|f| {
            f.split(|&b| b == b':')
                .map(|n| {
                    std::str::from_utf8(n)
                        .ok()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0)
                })
                .collect()
        })
        .collect()
}

fn csi(params_raw: &[u8], fin: u8) -> Unit {
    if params_raw.first().is_some_and(|b| !b.is_ascii_digit()) {
        return Unit::Key(OTHER); // private (`?`, `>`, ...) sequences are never keys
    }
    let p = params(params_raw);
    let field = |i: usize, j: usize, default: u32| {
        p.get(i)
            .and_then(|f| f.get(j))
            .copied()
            .filter(|&v| v != 0)
            .unwrap_or(default)
    };
    match fin {
        b'u' => {
            let code = field(0, 0, 0);
            // Modifier keys themselves (kitty "report all keys") and releases.
            if (57441..=57454).contains(&code) || field(1, 1, 1) == 3 {
                return Unit::Ignore;
            }
            Unit::Key(encoded(code, field(1, 0, 1)).unwrap_or(OTHER))
        }
        b'~' => match field(0, 0, 0) {
            27 => Unit::Key(encoded(field(2, 0, 0), field(1, 0, 1)).unwrap_or(OTHER)),
            200 => Unit::PasteStart,
            201 => Unit::PasteEnd,
            _ if field(1, 1, 1) == 3 => Unit::Ignore,
            _ => Unit::Key(OTHER),
        },
        _ if field(1, 1, 1) == 3 => Unit::Ignore,
        _ => Unit::Key(OTHER),
    }
}

fn utf8_len(b: u8) -> usize {
    match b {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

/// Decode one character starting at `buf[0]` (non-empty, not ESC).
fn char_unit(buf: &[u8]) -> (usize, Key) {
    if buf[0] < 0x80 {
        return (1, legacy(buf[0]));
    }
    let n = utf8_len(buf[0]);
    match buf
        .get(..n)
        .and_then(|s| std::str::from_utf8(s).ok())
        .and_then(|s| s.chars().next())
    {
        Some(c) => (
            n,
            Key {
                code: c.into(),
                ctrl: false,
                alt: false,
            },
        ),
        None => (1, OTHER),
    }
}

/// Split one input unit off the front of `buf`. `wait_esc`: a lone trailing
/// ESC may be the start of an Alt key, so wait for more input.
fn next_unit(buf: &[u8], wait_esc: bool) -> Next {
    if buf[0] != 0x1b {
        let (n, key) = char_unit(buf);
        return Next::Unit(n, Unit::Key(key));
    }
    match buf.get(1) {
        None if wait_esc => Next::Incomplete,
        None | Some(0x1b) => Next::Unit(1, Unit::Key(legacy(0x1b))),
        Some(b'[') => {
            let end = buf[2..].iter().position(|b| !(0x20..=0x3f).contains(b));
            match end {
                None => Next::Incomplete,
                Some(i) => {
                    let fin = buf[2 + i];
                    Next::Unit(
                        3 + i,
                        if (0x40..=0x7e).contains(&fin) {
                            csi(&buf[2..2 + i], fin)
                        } else {
                            Unit::Key(OTHER)
                        },
                    )
                }
            }
        }
        Some(b'O') if buf.len() < 3 => Next::Incomplete,
        Some(b'O') => Next::Unit(3, Unit::Key(OTHER)),
        Some(_) => {
            let (n, mut key) = char_unit(&buf[1..]);
            key.alt = true;
            Next::Unit(1 + n, Unit::Key(key))
        }
    }
}

/// Parse a raw byte sequence (as produced by `config::parse_key`) into keys.
pub fn keys_of(bytes: &[u8]) -> Vec<Key> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match next_unit(&bytes[i..], false) {
            Next::Unit(n, Unit::Key(k)) => {
                out.push(k);
                i += n;
            }
            Next::Unit(n, _) => i += n,
            Next::Incomplete => break,
        }
    }
    out
}

struct Held {
    key: Option<Key>,
    raw: Vec<u8>,
}

/// Matches the detach key incrementally across reads, in any keyboard encoding.
pub struct KeyMatcher {
    spec: Vec<Key>,
    wait_esc: bool,
    /// Units that may be the start of the detach key, held back from the program.
    held: Vec<Held>,
    /// An escape sequence split across reads.
    partial: Vec<u8>,
    in_paste: bool,
}

impl KeyMatcher {
    pub fn new(spec: &[u8]) -> Self {
        let spec = keys_of(spec);
        assert!(!spec.is_empty(), "empty detach key");
        let wait_esc = spec.iter().any(|k| k.alt);
        KeyMatcher {
            spec,
            wait_esc,
            held: Vec::new(),
            partial: Vec::new(),
            in_paste: false,
        }
    }

    /// Feed input. Returns (bytes to forward to the program, detach triggered).
    pub fn feed(&mut self, input: &[u8]) -> (Vec<u8>, bool) {
        let mut data = std::mem::take(&mut self.partial);
        data.extend_from_slice(input);
        let mut out = Vec::with_capacity(data.len());
        let mut i = 0;
        while i < data.len() {
            let (n, unit) = match next_unit(&data[i..], self.wait_esc) {
                Next::Unit(n, unit) => (n, unit),
                Next::Incomplete => {
                    self.partial = data[i..].to_vec();
                    break;
                }
            };
            let raw = &data[i..i + n];
            i += n;
            match unit {
                Unit::PasteStart | Unit::PasteEnd => {
                    self.in_paste = matches!(unit, Unit::PasteStart);
                    self.release_held(&mut out);
                    out.extend_from_slice(raw);
                }
                _ if self.in_paste => out.extend_from_slice(raw),
                Unit::Ignore if self.held.is_empty() => out.extend_from_slice(raw),
                Unit::Ignore => self.held.push(Held {
                    key: None,
                    raw: raw.to_vec(),
                }),
                Unit::Key(key) => {
                    self.held.push(Held {
                        key: Some(key),
                        raw: raw.to_vec(),
                    });
                    if self.settle(&mut out) {
                        self.partial.clear();
                        return (out, true);
                    }
                }
            }
        }
        (out, false)
    }

    /// Drop held units from the front until what remains is a prefix of the
    /// detach key (so "a b a c" is found in "a b a b a c"). True on a full match.
    fn settle(&mut self, out: &mut Vec<u8>) -> bool {
        loop {
            let keys: Vec<Key> = self.held.iter().filter_map(|h| h.key).collect();
            if keys.is_empty() {
                self.release_held(out);
                return false;
            }
            if self.spec.starts_with(&keys) {
                if keys.len() < self.spec.len() {
                    return false;
                }
                self.held.clear();
                return true;
            }
            let first_key = self.held.iter().position(|h| h.key.is_some()).unwrap_or(0);
            for h in self.held.drain(..=first_key) {
                out.extend_from_slice(&h.raw);
            }
        }
    }

    fn release_held(&mut self, out: &mut Vec<u8>) {
        for h in self.held.drain(..) {
            out.extend_from_slice(&h.raw);
        }
    }

    /// Give up on a pending partial match (after a timeout) and return the held bytes.
    /// Also ends a paste whose end marker never arrived.
    pub fn flush(&mut self) -> Vec<u8> {
        self.in_paste = false;
        let mut out = Vec::new();
        self.release_held(&mut out);
        out.append(&mut self.partial);
        out
    }

    pub fn pending(&self) -> bool {
        self.in_paste || !self.held.is_empty() || !self.partial.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::KeyMatcher;

    const CA_D: &[u8] = &[1, b'd'];

    #[test]
    fn two_key_sequence() {
        let mut m = KeyMatcher::new(CA_D);
        assert_eq!(m.feed(b"hello"), (b"hello".to_vec(), false));
        assert_eq!(m.feed(&[1]), (vec![], false)); // held
        assert_eq!(m.feed(b"x"), (vec![1, b'x'], false)); // flushed prefix + byte
        assert_eq!(m.feed(&[1, b'd']), (vec![], true));
        // Split across reads
        assert_eq!(m.feed(&[1]), (vec![], false));
        assert_eq!(m.feed(b"d"), (vec![], true));
        // C-a C-a d: first C-a forwarded, second begins the sequence
        assert_eq!(m.feed(&[1, 1, b'd']), (vec![1], true));
    }

    #[test]
    fn single_key() {
        let mut m = KeyMatcher::new(&[0x1c]);
        assert_eq!(m.feed(b"ab\x1c"), (b"ab".to_vec(), true));
    }

    #[test]
    fn flush_pending() {
        let mut m = KeyMatcher::new(CA_D);
        m.feed(&[1]);
        assert!(m.pending());
        assert_eq!(m.flush(), vec![1]);
        assert!(!m.pending());
    }

    #[test]
    fn kitty_keyboard_protocol() {
        // Disambiguate mode: Ctrl-a as CSI u, d as a plain byte.
        let mut m = KeyMatcher::new(CA_D);
        assert_eq!(m.feed(b"\x1b[97;5ud"), (vec![], true));
        // Report-all-keys + event types: modifier presses, releases, and
        // the d press as CSI u.
        let mut m = KeyMatcher::new(CA_D);
        let (out, detach) = m.feed(b"\x1b[57442;5u\x1b[97;5u\x1b[97;5:3u\x1b[57442;1:3u\x1b[100u");
        assert!(detach);
        assert_eq!(out, b"\x1b[57442;5u"); // ctrl press was forwarded before the match began
        // Alternate keys and Caps Lock don't matter.
        let mut m = KeyMatcher::new(CA_D);
        assert_eq!(m.feed(b"\x1b[97:65;69u\x1b[100;65u"), (vec![], true));
        // Ctrl-a followed by something else is passed through intact.
        let mut m = KeyMatcher::new(CA_D);
        assert_eq!(m.feed(b"\x1b[97;5ux"), (b"\x1b[97;5ux".to_vec(), false));
        // Ctrl-\ as CSI u.
        let mut m = KeyMatcher::new(&[0x1c]);
        assert_eq!(m.feed(b"\x1b[92;5u"), (vec![], true));
    }

    #[test]
    fn modify_other_keys() {
        let mut m = KeyMatcher::new(CA_D);
        assert_eq!(m.feed(b"\x1b[27;5;97~d"), (vec![], true));
    }

    #[test]
    fn split_escape_sequence() {
        let mut m = KeyMatcher::new(CA_D);
        assert_eq!(m.feed(b"\x1b[97"), (vec![], false));
        assert!(m.pending());
        assert_eq!(m.feed(b";5u"), (vec![], false));
        assert_eq!(m.feed(b"d"), (vec![], true));
    }

    #[test]
    fn other_sequences_pass_through() {
        let mut m = KeyMatcher::new(CA_D);
        let input = b"\x1b[A\x1bOB\x1b[1;5C\xc3\xa9\x1b";
        assert_eq!(m.feed(input), (input.to_vec(), false));
        assert!(!m.pending()); // a lone ESC is not delayed
    }

    #[test]
    fn alt_key() {
        let mut m = KeyMatcher::new(&[0x1b, b'd']);
        assert_eq!(m.feed(b"\x1b"), (vec![], false)); // may be Alt-d
        assert_eq!(m.feed(b"d"), (vec![], true));
        assert_eq!(m.feed(b"\x1b[100;3u"), (vec![], true));
        assert_eq!(m.feed(b"\x1bx"), (b"\x1bx".to_vec(), false));
    }

    #[test]
    fn backtracking() {
        let mut m = KeyMatcher::new(b"abac");
        assert_eq!(m.feed(b"ababac"), (b"ab".to_vec(), true));
    }

    #[test]
    fn bracketed_paste_is_not_a_key() {
        let mut m = KeyMatcher::new(CA_D);
        let paste = b"\x1b[200~x\x01dy\x1b[201~";
        assert_eq!(m.feed(paste), (paste.to_vec(), false));
        assert_eq!(m.feed(&[1, b'd']), (vec![], true));
        // A paste that never ends doesn't disable the key for good.
        assert_eq!(m.feed(b"\x1b[200~x"), (b"\x1b[200~x".to_vec(), false));
        assert!(m.pending());
        assert_eq!(m.flush(), vec![]);
        assert_eq!(m.feed(&[1, b'd']), (vec![], true));
    }
}
