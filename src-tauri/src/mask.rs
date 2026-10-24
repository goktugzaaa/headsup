//! Secret masking for everything that leaves the PC (Telegram, ntfy) and for events.log.
//! Hand-written scanners (no regex crate); they work on chars, so any UTF-8 input is safe,
//! and never index out of bounds. Not used for the PC card: the user must see what they
//! approve.

const DOTS: &str = "••••";

/// Mask credentials in free text: known token formats, long random-looking blobs,
/// `key=value` / `key: value` / `--key value` / `-pVALUE` for password- and token-like keys,
/// URL userinfo, Bearer/Basic credentials and PEM private key blocks. Masking an already
/// masked text changes nothing.
pub fn secrets(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let s = pem_blocks(s);
    let s = url_userinfo(&s);
    let s = key_values(&s);
    tokens(&s)
}

/// `-----BEGIN ... PRIVATE KEY-----` up to its END line (or the end of the text).
fn pem_blocks(s: &str) -> String {
    const BEGIN: &str = "-----BEGIN ";
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find(BEGIN) {
        let after = &rest[start..];
        let header = &after[..after.find('\n').unwrap_or(after.len())];
        if !header.contains("PRIVATE KEY-----") {
            out.push_str(&rest[..start + BEGIN.len()]);
            rest = &rest[start + BEGIN.len()..];
            continue;
        }
        out.push_str(&rest[..start]);
        out.push_str("-----BEGIN PRIVATE KEY----- ");
        out.push_str(DOTS);
        match after.find("-----END ") {
            Some(e) => {
                let tail = &after[e + 9..];
                let close = tail.find("-----").map(|x| e + 9 + x + 5).unwrap_or(after.len());
                out.push_str(" -----END PRIVATE KEY-----");
                rest = &after[close..];
            }
            None => rest = "",
        }
    }
    out.push_str(rest);
    out
}

/// `scheme://user:password@host` -> `scheme://user:••••@host`.
fn url_userinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| matches!(c, '@' | '/' | '?' | '#' | '"' | '\'' | '<' | '>' | '`') || c.is_whitespace())
            .unwrap_or(tail.len());
        if tail[end..].starts_with('@') {
            if let Some(colon) = tail[..end].find(':') {
                if colon + 1 < end {
                    out.push_str(&tail[..colon + 1]);
                    out.push_str(DOTS);
                    rest = &tail[end..];
                    continue;
                }
            }
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
}

/// Does this key name a credential? `password`, `db_pass`, `apiKey`, `X-Api-Key`,
/// `client_secret`, `--token`, `.env` names such as `AWS_SECRET_ACCESS_KEY`.
fn sensitive_key(key: &str) -> bool {
    let raw = key.trim_start_matches('-').trim_end_matches('.');
    if raw.is_empty() {
        return false;
    }
    let norm: String = raw.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect();
    const ENDS: [&str; 13] = [
        "pass", "passwd", "password", "pwd", "secret", "token", "apikey", "auth", "credential", "credentials",
        "privatekey", "accesskey", "secretkey",
    ];
    if ENDS.iter().any(|w| norm.ends_with(w)) || norm.contains("password") || norm.contains("passwd") {
        return true;
    }
    // .env-style UPPER_SNAKE names: any segment that names a key or token.
    if raw.contains('_') && !raw.chars().any(|c| c.is_ascii_lowercase()) {
        return raw.split('_').any(|seg| {
            matches!(
                seg,
                "KEY" | "TOKEN" | "SECRET" | "PASSWORD" | "PASSWD" | "PASS" | "PWD" | "AUTH" | "CREDENTIAL" | "CREDENTIALS" | "APIKEY"
            )
        });
    }
    false
}

fn ends_value(c: char) -> bool {
    c.is_whitespace() || matches!(c, ',' | ';' | '&' | '"' | '\'' | ')' | '}' | ']' | '<' | '>' | '|' | '`')
}

/// The value that starts at `k`: (start, end) of the part to mask. Quoted values keep their
/// quotes; `Bearer x` / `token x` take the following word too.
fn value_span(c: &[char], k: usize) -> Option<(usize, usize)> {
    let n = c.len();
    let q = *c.get(k)?;
    if q == '"' || q == '\'' {
        let start = k + 1;
        let mut e = start;
        while e < n && c[e] != q && c[e] != '\n' {
            e += 1;
        }
        return (e > start).then_some((start, e));
    }
    let mut e = k;
    while e < n && !ends_value(c[e]) {
        e += 1;
    }
    if e == k {
        return None;
    }
    let word: String = c[k..e].iter().collect::<String>().to_ascii_lowercase();
    if matches!(word.as_str(), "bearer" | "basic" | "token") {
        let mut f = e;
        while f < n && (c[f] == ' ' || c[f] == '\t') {
            f += 1;
        }
        let mut g = f;
        while g < n && !ends_value(c[g]) {
            g += 1;
        }
        if g > f {
            e = g;
        }
    }
    Some((k, e))
}

/// For a sensitive key that ends at `j`: the span of its value, if one follows.
fn value_after(c: &[char], j: usize, key: &str) -> Option<(usize, usize)> {
    if !sensitive_key(key) {
        return None;
    }
    let n = c.len();
    let mut k = j;
    // Closing quote of a quoted key: "password": "x"
    if k < n && (c[k] == '"' || c[k] == '\'') {
        k += 1;
    }
    let gap_start = k;
    while k < n && (c[k] == ' ' || c[k] == '\t') {
        k += 1;
    }
    match c.get(k).copied() {
        Some(sep @ ('=' | ':')) => {
            if sep == '=' && c.get(k + 1) == Some(&'=') {
                return None; // a comparison, not an assignment
            }
            if sep == ':' && c.get(k + 1) == Some(&'/') {
                return None; // a URL scheme
            }
            k += 1;
            if c.get(k) == Some(&'=') {
                k += 1; // :=
            }
            while k < n && (c[k] == ' ' || c[k] == '\t') {
                k += 1;
            }
            value_span(c, k)
        }
        // --password value
        Some(v) if key.starts_with('-') && k > gap_start && v != '-' && v != '\n' => value_span(c, k),
        _ => None,
    }
}

/// `Bearer <token>` / `Basic <token>` starting at `i`: (token start, token end).
fn scheme_credential(c: &[char], i: usize) -> Option<(usize, usize)> {
    let n = c.len();
    let word_len = ["bearer", "basic"].iter().find_map(|w| {
        let len = w.len();
        let got: String = c.get(i..i + len)?.iter().collect::<String>().to_ascii_lowercase();
        (got == *w).then_some(len)
    })?;
    let mut ts = i + word_len;
    if !matches!(c.get(ts), Some(' ') | Some('\t')) {
        return None;
    }
    while ts < n && (c[ts] == ' ' || c[ts] == '\t') {
        ts += 1;
    }
    let mut te = ts;
    while te < n && (c[te].is_ascii_alphanumeric() || matches!(c[te], '.' | '_' | '~' | '+' | '/' | '=' | '-')) {
        te += 1;
    }
    let tok = &c[ts..te];
    let random = tok.iter().any(char::is_ascii_digit)
        || (tok.iter().any(char::is_ascii_uppercase) && tok.iter().any(char::is_ascii_lowercase))
        || tok.iter().any(|ch| matches!(ch, '=' | '+' | '/'));
    (tok.len() >= 8 && random).then_some((ts, te))
}

/// `-pVALUE` (mysql style). Port numbers and plain words (`-path`, `-pthread`) are left alone.
fn short_p_value(c: &[char], i: usize) -> Option<usize> {
    if c.get(i) != Some(&'-') || c.get(i + 1) != Some(&'p') || (i > 0 && !c[i - 1].is_whitespace()) {
        return None;
    }
    let vs = i + 2;
    let mut ve = vs;
    while ve < c.len() && !c[ve].is_whitespace() && c[ve] != '"' && c[ve] != '\'' {
        ve += 1;
    }
    let v = &c[vs..ve];
    let plain_word = v.iter().all(char::is_ascii_lowercase);
    let port_like = v.iter().all(|ch| ch.is_ascii_digit() || matches!(ch, ':' | '.' | ',' | '/' | '-'));
    (v.len() >= 3 && !plain_word && !port_like && v[0] != '-' && v[0] != '=').then_some(ve)
}

fn key_values(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let n = c.len();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < n {
        let word_start = i == 0 || !c[i - 1].is_alphanumeric();
        if word_start {
            if let Some((ts, te)) = scheme_credential(&c, i) {
                out.extend(&c[i..ts]);
                out.push_str(DOTS);
                i = te;
                continue;
            }
        }
        if is_key_char(c[i]) && (i == 0 || !is_key_char(c[i - 1])) {
            if let Some(ve) = short_p_value(&c, i) {
                out.push_str("-p");
                out.push_str(DOTS);
                i = ve;
                continue;
            }
            let mut j = i;
            while j < n && is_key_char(c[j]) {
                j += 1;
            }
            let key: String = c[i..j].iter().collect();
            if let Some((vs, ve)) = value_after(&c, j, &key) {
                out.extend(&c[i..vs]);
                out.push_str(DOTS);
                i = ve;
                continue;
            }
            out.push_str(&key);
            i = j;
            continue;
        }
        out.push(c[i]);
        i += 1;
    }
    out
}

/// Characters of a base64 / hex / url-safe token.
fn is_blob_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-')
}

fn is_tok_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Keep the first 4 characters of a matched secret, hide the rest.
fn shortened(c: &[char]) -> String {
    let mut s: String = c.iter().take(4).collect();
    s.push('…');
    s
}

/// Length of a known token format starting at `i` (a word start), if any.
fn known_token(c: &[char], i: usize) -> Option<usize> {
    let n = c.len();
    let starts = |p: &str| p.chars().enumerate().all(|(k, pc)| c.get(i + k) == Some(&pc));
    let tail_len = |from: usize| c[from.min(n)..].iter().take_while(|&&ch| is_tok_char(ch)).count();
    const PREFIXES: [(&str, usize); 14] = [
        ("github_pat_", 16),
        ("sk-", 16),
        ("ghp_", 16),
        ("gho_", 16),
        ("ghs_", 16),
        ("ghu_", 16),
        ("ghr_", 16),
        ("glpat-", 16),
        ("xoxb-", 16),
        ("xoxp-", 16),
        ("xoxa-", 16),
        ("xoxs-", 16),
        ("xoxr-", 16),
        ("AIza", 30),
    ];
    for (p, min) in PREFIXES {
        if starts(p) {
            let plen = p.chars().count();
            let t = tail_len(i + plen);
            if t >= min {
                return Some(plen + t);
            }
        }
    }
    // AWS access key ids: AKIA/ASIA + 16 upper-case letters or digits.
    if starts("AKIA") || starts("ASIA") {
        let t = c[(i + 4).min(n)..].iter().take_while(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit()).count();
        if t == 16 && !c.get(i + 20).map_or(false, |ch| ch.is_ascii_alphanumeric()) {
            return Some(20);
        }
    }
    // Telegram bot tokens: 6-12 digits ':' 30+ token chars.
    let digits = c[i..].iter().take_while(|ch| ch.is_ascii_digit()).count();
    if (6..=12).contains(&digits) && c.get(i + digits) == Some(&':') {
        let t = tail_len(i + digits + 1);
        if t >= 30 {
            return Some(digits + 1 + t);
        }
    }
    // JWTs: eyJ... with two dots.
    if starts("eyJ") {
        let len = c[i..].iter().take_while(|&&ch| is_tok_char(ch) || ch == '.').count();
        let dots = c[i..i + len].iter().filter(|&&ch| ch == '.').count();
        if dots >= 2 && len >= 30 {
            return Some(len);
        }
    }
    None
}

/// A 32+ character run that looks like a key rather than a word or a path: all hex, or
/// mixed case with digits spread through it and few separators.
fn blob_like(run: &[char]) -> bool {
    let len = run.len();
    if len < 32 {
        return false;
    }
    if run.iter().all(char::is_ascii_hexdigit) {
        return true;
    }
    let digits = run.iter().filter(|ch| ch.is_ascii_digit()).count();
    let slashes = run.iter().filter(|&&ch| ch == '/').count();
    let seps = run.iter().filter(|&&ch| ch == '_' || ch == '-').count();
    run.iter().any(char::is_ascii_uppercase)
        && run.iter().any(char::is_ascii_lowercase)
        && digits >= 2
        && digits * 16 >= len
        && slashes * 16 <= len
        && seps * 8 <= len
}

fn tokens(s: &str) -> String {
    // Known formats first, at every word start (also right after a '/' or ':'), then the
    // generic blob rule over whole runs.
    let c: Vec<char> = s.chars().collect();
    let n = c.len();
    let mut known = String::with_capacity(s.len());
    let mut i = 0;
    while i < n {
        if c[i].is_ascii_alphanumeric() && (i == 0 || !is_tok_char(c[i - 1])) {
            if let Some(len) = known_token(&c, i) {
                known.push_str(&shortened(&c[i..i + len]));
                i += len;
                continue;
            }
        }
        known.push(c[i]);
        i += 1;
    }
    let c: Vec<char> = known.chars().collect();
    let n = c.len();
    let mut out = String::with_capacity(known.len());
    let mut i = 0;
    while i < n {
        if !is_blob_char(c[i]) {
            out.push(c[i]);
            i += 1;
            continue;
        }
        let mut j = i;
        while j < n && is_blob_char(c[j]) {
            j += 1;
        }
        if blob_like(&c[i..j]) {
            out.push_str(&shortened(&c[i..j]));
        } else {
            out.extend(&c[i..j]);
        }
        i = j;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::secrets;

    fn masked(input: &str, secret: &str) {
        let out = secrets(input);
        assert!(!out.contains(secret), "{secret:?} leaked: {input:?} -> {out:?}");
        assert_eq!(secrets(&out), out, "not stable: {out:?}");
    }

    fn kept(input: &str) {
        assert_eq!(secrets(input), input);
    }

    /// The fake credentials below are split with `concat!` so that secret scanners (GitHub's
    /// among them) do not report test fixtures as leaked keys. None of them is real.
    #[test]
    fn known_prefixes() {
        masked(concat!("export X=sk-", "ant-api03-AbCdEfGhIjKlMnOpQrStUv"), "AbCdEfGhIjKlMnOp");
        masked(concat!("key sk-", "proj-1234567890abcdefXYZ done"), "1234567890abcdef");
        masked(concat!("git clone https://gh", "p_abcdefghijklmnopqrstuvwxyz0123456789@github.com/x/y"), "abcdefghijklmnop");
        masked(concat!("github_", "pat_11ABCDEFG0123456789_abcdefghijklmnop"), "11ABCDEFG012345");
        masked(concat!("gl", "pat-abcdefghijklmnopqrst"), "abcdefghijklmnop");
        masked(concat!("xo", "xb-1234567890-abcdefghijkl"), "1234567890-abcd");
        masked(concat!("aws AK", "IAIOSFODNN7EXAMPLE x"), "IOSFODNN7EXAMPLE");
        masked(concat!("AI", "zaSyA-abcdefghijklmnopqrstuvwxyz12345"), "abcdefghijklmnop");
        masked(concat!("bot 123456789:", "AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw-x"), "AAHdqTcvCH1vGWJx");
        masked(concat!("jwt eyJhbGciOiJIUzI1NiJ9.", "eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U"), "eyJzdWIiOiIxMjM0");
    }

    #[test]
    fn key_value_forms() {
        masked("password=hunter2!", "hunter2");
        masked("DB_PASSWORD = \"p@ss w0rd\"", "p@ss w0rd");
        masked("{\"api_key\": \"abc123xyz\", \"user\": \"bob\"}", "abc123xyz");
        masked("apiKey: zzTop99", "zzTop99");
        masked("mysql -u root -pS3cr3t! db", "S3cr3t");
        masked("tool --token abcdef123 --verbose", "abcdef123");
        masked("--api-key=qwerty12345", "qwerty12345");
        masked("AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG", "wJalrXUtnFEMI");
        masked("$env:GITHUB_TOKEN=\"tok_value_1\"", "tok_value_1");
        masked("curl -H 'Authorization: Bearer abcDEF123456789' x", "abcDEF123456789");
        masked("auth: Bearer abcDEF123456789", "abcDEF123456789");
        masked("Basic dXNlcjpwYXNzd29yZA==", "dXNlcjpwYXNzd29yZA");
        masked("https://user:hunter2@example.com/path", "hunter2");
        masked("x\n-----BEGIN RSA PRIVATE KEY-----\nMIIEow\nabc\n-----END RSA PRIVATE KEY-----\ny", "MIIEow");
        masked("pwd=12345678", "12345678");
    }

    #[test]
    fn blobs() {
        masked("sha 0123456789abcdef0123456789abcdef01234567", "0123456789abcdef0123");
        masked("key q8Fz3LmP0aXv7RtY2bNc5WdE9hJk4GsU1oIi", "q8Fz3LmP0aXv7RtY2bNc");
    }

    #[test]
    fn leaves_ordinary_text() {
        kept("npm install && npm run build");
        kept("mkdir -p src/components && cc -pthread x.c");
        kept("docker run -p8080:80 nginx");
        kept("if token == expected { return }");
        kept("max_tokens: 1000, author: Jane");
        kept("C:/Users/dev/Desktop/headsup-app/src-tauri/src/events.rs");
        kept("session 550e8400-e29b-41d4-a716-446655440000");
        kept("Basic usage and bearer tokens");
        kept("Türkçe metin: şifre yok, ğüşiöç İı");
        kept("https://example.com/a/b?x=1");
        kept("");
    }

    #[test]
    fn never_panics_on_odd_input() {
        for s in [
            "-", "-p", "--", "=", ":", "\"", "password", "password=", "password=\"", "token:", "sk-", "AKIA", "eyJ..",
            "-----BEGIN ", "-----BEGIN PRIVATE KEY-----", "://", "a://b:@", "Bearer", "Bearer ", "🔐 token=🔐x",
            "ı:İ=ş", "\u{0}\u{1}=\u{2}",
        ] {
            let _ = secrets(s);
        }
    }
}
