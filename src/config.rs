//! Configuration: a tiny `key = value` file plus environment.
//!
//! File: `$XDG_CONFIG_HOME/pear/config` (default `~/.config/pear/config`)
//!
//! ```text
//! # key sequence that detaches the client. Tokens separated by spaces.
//! # C-x = Ctrl+x, M-x = Alt+x, plain characters are literal.
//! detach-key = C-a d
//! ```

use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub detach_key: Vec<u8>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            detach_key: parse_key("C-a d").unwrap(),
        }
    }
}

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("pear").join("config")
}

pub fn load() -> Result<Config, String> {
    let mut cfg = Config::default();
    let path = config_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(cfg);
    };
    for (lineno, raw) in text.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            return Err(format!(
                "{}:{}: expected `key = value`",
                path.display(),
                lineno + 1
            ));
        };
        let v = v.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(v);
        let k = k.trim();
        match k {
            "detach-key" => {
                cfg.detach_key =
                    parse_key(v).map_err(|e| format!("{}:{}: {e}", path.display(), lineno + 1))?
            }
            _ => {
                return Err(format!(
                    "{}:{}: unknown option `{k}`",
                    path.display(),
                    lineno + 1
                ));
            }
        }
    }
    Ok(cfg)
}

/// Remove a `#` comment: a whole line, or ` # ...` after a value.
fn strip_comment(line: &str) -> &str {
    let t = line.trim_start();
    if t.starts_with('#') {
        return "";
    }
    match line.find(" #").or_else(|| line.find("\t#")) {
        Some(i) => &line[..i],
        None => line,
    }
}

/// Directory holding the per-session sockets.
pub fn session_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("PEAR_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d).join("pear");
    }
    PathBuf::from(format!("/tmp/pear-{}", crate::sys::getuid()))
}

/// Parse a key sequence like `C-a d`, `C-\`, `M-d`, `C-a C-d` into the raw bytes
/// a terminal sends for it.
pub fn parse_key(spec: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for tok in spec.split_whitespace() {
        let bytes = parse_token(tok)?;
        out.extend_from_slice(&bytes);
    }
    if out.is_empty() {
        return Err("empty key".into());
    }
    Ok(out)
}

fn parse_token(tok: &str) -> Result<Vec<u8>, String> {
    let lower = tok.to_ascii_lowercase();
    match lower.as_str() {
        "esc" | "escape" => return Ok(vec![0x1b]),
        "enter" | "return" | "cr" => return Ok(vec![b'\r']),
        "tab" => return Ok(vec![b'\t']),
        "space" => return Ok(vec![b' ']),
        "bs" | "backspace" => return Ok(vec![0x7f]),
        _ => {}
    }
    if let Some(rest) = tok
        .strip_prefix("C-")
        .or_else(|| tok.strip_prefix("c-"))
        .or_else(|| tok.strip_prefix('^'))
    {
        let c = single_char(rest)?;
        return ctrl(c)
            .map(|b| vec![b])
            .ok_or_else(|| format!("cannot Ctrl-modify `{rest}`"));
    }
    if let Some(rest) = tok
        .strip_prefix("M-")
        .or_else(|| tok.strip_prefix("m-"))
        .or_else(|| tok.strip_prefix("A-"))
    {
        let c = single_char(rest)?;
        return Ok(vec![0x1b, c as u8]);
    }
    if tok.chars().count() == 1 {
        return Ok(tok.as_bytes().to_vec());
    }
    Err(format!("unknown key token `{tok}`"))
}

fn single_char(s: &str) -> Result<char, String> {
    let mut it = s.chars();
    match (it.next(), it.next()) {
        (Some(c), None) => Ok(c),
        _ => Err(format!("expected a single character, got `{s}`")),
    }
}

fn ctrl(c: char) -> Option<u8> {
    let c = c.to_ascii_lowercase();
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        '@' | ' ' => Some(0),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' | '/' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

/// Human readable form of a key sequence, for messages.
pub fn describe_key(bytes: &[u8]) -> String {
    let mut parts = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let s = match b {
            0x1b if i + 1 < bytes.len() && bytes[i + 1].is_ascii_graphic() => {
                i += 1;
                format!("M-{}", bytes[i] as char)
            }
            0x1b => "Esc".to_string(),
            0 => "C-Space".to_string(),
            b'\r' => "Enter".to_string(),
            b'\t' => "Tab".to_string(),
            1..=26 => format!("C-{}", (b'a' + b - 1) as char),
            0x1c => "C-\\".to_string(),
            0x1d => "C-]".to_string(),
            0x1e => "C-^".to_string(),
            0x1f => "C-_".to_string(),
            0x7f => "Backspace".to_string(),
            b' ' => "Space".to_string(),
            _ => (b as char).to_string(),
        };
        parts.push(s);
        i += 1;
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(parse_key("C-a d").unwrap(), vec![1, b'd']);
        assert_eq!(parse_key("C-\\").unwrap(), vec![0x1c]);
        assert_eq!(parse_key("^q").unwrap(), vec![0x11]);
        assert_eq!(parse_key("M-d").unwrap(), vec![0x1b, b'd']);
        assert_eq!(parse_key("C-a C-d").unwrap(), vec![1, 4]);
        assert!(parse_key("").is_err());
        assert!(parse_key("foo").is_err());
        assert_eq!(describe_key(&[1, b'd']), "C-a d");
        assert_eq!(describe_key(&[0x1c]), "C-\\");
    }

    #[test]
    fn comments() {
        assert_eq!(
            strip_comment("detach-key = C-a d # note"),
            "detach-key = C-a d"
        );
        assert_eq!(strip_comment("  # whole line"), "");
        assert_eq!(strip_comment("detach-key = C-\\"), "detach-key = C-\\");
    }
}
