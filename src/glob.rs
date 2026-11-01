/// 仅支持 `*` 与 `?` 的最小 glob（04-config §3.1：刻意不做 `[]`/`**`）。
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    m(&p, &t)
}

fn m(p: &[char], t: &[char]) -> bool {
    if p.is_empty() {
        return t.is_empty();
    }
    match p[0] {
        '*' => {
            for i in 0..=t.len() {
                if m(&p[1..], &t[i..]) {
                    return true;
                }
            }
            false
        }
        '?' => !t.is_empty() && m(&p[1..], &t[1..]),
        c => !t.is_empty() && t[0] == c && m(&p[1..], &t[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::glob_match as g;

    #[test]
    fn star_crosses_words() {
        assert!(g("make run server*", "make run server 127.0.0.1"));
        assert!(g("*migrate*", "make db-migrate up"));
        assert!(!g("make run server*", "make db-migrate"));
        assert!(g("*", ""));
    }

    #[test]
    fn question_single_char() {
        assert!(g("npm run de?", "npm run dev"));
        assert!(!g("npm run de?", "npm run devx"));
    }

    #[test]
    fn exact() {
        assert!(g("npm run dev", "npm run dev"));
        assert!(!g("npm run dev", "npm run dev --verbose"));
    }
}
