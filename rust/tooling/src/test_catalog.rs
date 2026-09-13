//! Conservative source declarations used only for structural evidence.
//! Runtime collection/results supply expanded identities and execution evidence.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug)]
struct Token {
    text: String,
    literal: bool,
}

fn tokens(source: &str, python: bool, rust: bool) -> Vec<Token> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"//") || (python && bytes[i] == b'#') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i..].starts_with(b"/*") {
            i += 2;
            let mut depth = 1;
            while i < bytes.len() && depth > 0 {
                if bytes[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if bytes[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        // Rust raw strings can contain quotes, comments and fake declarations.
        if bytes[i] == b'r' {
            let mut opening = i + 1;
            while opening < bytes.len() && bytes[opening] == b'#' {
                opening += 1;
            }
            if bytes.get(opening) == Some(&b'"') {
                let suffix = format!("\"{}", "#".repeat(opening - i - 1));
                let start = opening + 1;
                let end = source[start..]
                    .find(&suffix)
                    .map_or(bytes.len(), |offset| start + offset);
                out.push(Token {
                    text: source[start..end].to_owned(),
                    literal: true,
                });
                i = (end + suffix.len()).min(bytes.len());
                continue;
            }
        }
        let quote = bytes[i];
        let lifetime = rust
            && quote == b'\''
            && bytes.get(i + 1).is_some_and(u8::is_ascii_alphabetic)
            && bytes.get(i + 2) != Some(&b'\'');
        if matches!(quote, b'\'' | b'"' | b'`') && !lifetime {
            let triple = python && bytes.get(i..i + 3).is_some_and(|part| part == [quote; 3]);
            let width = if triple { 3 } else { 1 };
            i += width;
            let start = i;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                    continue;
                }
                if bytes
                    .get(i..i + width)
                    .is_some_and(|part| part.iter().all(|byte| *byte == quote))
                {
                    break;
                }
                i += 1;
            }
            out.push(Token {
                text: source[start..i].to_owned(),
                literal: true,
            });
            i = (i + width).min(bytes.len());
        } else if bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'$') {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'$'))
            {
                i += 1;
            }
            out.push(Token {
                text: source[start..i].to_owned(),
                literal: false,
            });
        } else {
            let ch = source[i..].chars().next().expect("valid UTF-8 boundary");
            out.push(Token {
                text: ch.to_string(),
                literal: false,
            });
            i += ch.len_utf8();
        }
    }
    out
}

pub fn declared_tests(path: &str, source: &str) -> Vec<String> {
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if !matches!(
        extension,
        "rs" | "py" | "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs"
    ) {
        return Vec::new();
    }
    let ts = tokens(source, extension == "py", extension == "rs");
    let is = |index: usize, expected: &str| {
        ts.get(index)
            .is_some_and(|t| !t.literal && t.text == expected)
    };
    let mut names = Vec::new();
    let mut rust_test = false;
    for (i, token) in ts.iter().enumerate() {
        if token.literal {
            continue;
        }
        if extension == "rs" {
            if is(i, "#") && is(i + 1, "[") {
                let end = (i + 2..ts.len())
                    .find(|index| is(*index, "]"))
                    .unwrap_or(ts.len());
                rust_test |= ts[i + 2..end]
                    .iter()
                    .take_while(|t| !matches!(t.text.as_str(), "(" | "="))
                    .last()
                    .is_some_and(|t| !t.literal && matches!(t.text.as_str(), "test" | "rstest"));
            }
            if is(i, "fn") {
                if rust_test && let Some(name) = ts.get(i + 1).filter(|t| !t.literal) {
                    names.push(name.text.clone());
                }
                rust_test = false;
            }
            // cfg(test) applies to the module, not to every helper inside it.
            if is(i, "mod") || is(i, "struct") || is(i, "impl") {
                rust_test = false;
            }
        } else if extension == "py" && is(i, "def") {
            if let Some(name) = ts
                .get(i + 1)
                .filter(|t| !t.literal && t.text.starts_with("test_"))
            {
                names.push(name.text.clone());
            }
        } else if matches!(extension, "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs")
            && matches!(token.text.as_str(), "test" | "it" | "specify")
            && (i == 0 || !is(i - 1, "."))
        {
            let mut next = i + 1;
            if is(next, ".")
                && ts.get(next + 1).is_some_and(|t| {
                    !t.literal && matches!(t.text.as_str(), "only" | "skip" | "todo" | "fixme")
                })
            {
                next += 2;
            }
            if is(next, "(")
                && let Some(name) = ts.get(next + 1).filter(|t| t.literal)
            {
                names.push(name.text.clone());
            }
        }
    }
    names
}

pub fn declares_test(path: &str, source: &str, name: &str) -> bool {
    declared_tests(path, source)
        .iter()
        .filter(|candidate| candidate.as_str() == name)
        .count()
        == 1
}

pub fn source_inventory(repo: &Path, files: &[crate::audit_queue::FileEntry]) -> BTreeSet<String> {
    let mut inventory = BTreeSet::new();
    for file in files {
        if let Ok(Some(bytes)) =
            crate::audit_ledger::read_bytes_nofollow(&repo.join(&file.rel_path), Some(repo))
            && let Ok(source) = std::str::from_utf8(&bytes)
        {
            let mut names = BTreeMap::<String, usize>::new();
            for name in declared_tests(&file.rel_path, source) {
                *names.entry(name).or_default() += 1;
            }
            inventory.extend(names.into_iter().filter_map(|(name, count)| {
                (count == 1).then(|| format!("{}#{name}", file.rel_path))
            }));
        }
    }
    inventory
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarations_ignore_comments_strings_and_helpers_but_preserve_named_tests() {
        let js = "// test('fake', () => {});\nconst code = \"test('fake', ()=>{})\";\nit('real', () => expect(1).toBe(1));";
        assert_eq!(declared_tests("tests/a.test.mjs", js), ["real"]);
        let rust = "const CODE: &str = r###\"#[test] fn fake() {}\"###;\n#[cfg(test)] mod tests { fn helper() {} #[test] fn actual() {} #[tokio::test] async fn async_test() {} }";
        assert_eq!(declared_tests("src/lib.rs", rust), ["actual", "async_test"]);
        assert_eq!(
            declared_tests(
                "test_a.py",
                "# def test_fake():\n\"\"\"def test_fake():\"\"\"\ndef test_real():\n  assert True\n"
            ),
            ["test_real"]
        );
    }

    #[test]
    fn duplicate_names_are_ambiguous_and_dynamic_names_need_runtime_collection() {
        assert!(!declares_test(
            "src/lib.rs",
            "#[cfg(test)] fn helper() {}",
            "helper"
        ));
        assert!(!declares_test(
            "a.test.ts",
            "logger.test('message',()=>{});",
            "message"
        ));
        assert!(!declares_test(
            "a.test.ts",
            "test('x',()=>{});test('x',()=>{});",
            "x"
        ));
        assert!(!declares_test("a.test.ts", "test(name,()=>{});", "name"));
    }
}
