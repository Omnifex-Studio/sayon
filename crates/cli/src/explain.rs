//! `sayon explain`: the entries of the error-code reference, `docs/errors.md`,
//! which is built into the command so explanations work offline.

use crate::status;

const REFERENCE: &str = include_str!("../../../docs/errors.md");

/// Every code with an entry, in the order of the reference.
fn codes() -> impl Iterator<Item = &'static str> {
    REFERENCE
        .lines()
        .filter_map(|line| line.strip_prefix("### "))
}

/// The text of a code's entry.
fn entry(code: &str) -> Option<&'static str> {
    let heading = format!("\n### {code}\n");
    let start = REFERENCE.find(&heading)? + heading.len();
    let rest = &REFERENCE[start..];
    let end = ["\n### ", "\n## "]
        .iter()
        .filter_map(|marker| rest.find(marker))
        .min()
        .unwrap_or(rest.len());
    Some(rest[..end].trim())
}

/// The entry as terminal text: code blocks are indented instead of fenced.
fn render(text: &str) -> String {
    let mut out = String::new();
    let mut in_code = false;
    for line in text.lines() {
        if line.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            out.push_str("    ");
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Finds a code written in any letter case.
fn lookup(written: &str) -> Option<&'static str> {
    codes().find(|code| code.eq_ignore_ascii_case(written))
}

pub fn explain(args: &[String]) -> u8 {
    let [written] = args else {
        if args.is_empty() {
            println!("Usage: sayon explain <code>, for example `sayon explain E_MOVED_VALUE`\n");
            println!("Codes:");
            for code in codes() {
                println!("    {code}");
            }
            return status::SUCCESS;
        }
        return crate::usage_error("`sayon explain` takes one code");
    };
    match lookup(written).and_then(|code| entry(code).map(|text| (code, text))) {
        Some((code, text)) => {
            println!("{code}\n");
            print!("{}", render(text));
            status::SUCCESS
        }
        None => {
            let upper = written.to_ascii_uppercase();
            let similar: Vec<&str> = codes()
                .filter(|code| {
                    let code = code.to_ascii_uppercase();
                    code.contains(&upper) || upper.contains(&code)
                })
                .collect();
            eprintln!("error: no error code `{written}`");
            if !similar.is_empty() {
                eprintln!("similar codes: {}", similar.join(", "));
            }
            eprintln!("run `sayon explain` to list every code");
            status::USAGE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    /// Diagnostic codes (`E_...`, `W_...`) and runtime error codes
    /// (`...Error`) written as string literals in the compiler and runtime.
    fn codes_in_sources() -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut pending = vec![crates];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).expect("directory") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    pending.push(path);
                } else if path.extension().is_some_and(|e| e == "rs")
                    && !path.ends_with("explain.rs")
                {
                    let text = std::fs::read_to_string(&path).expect("source");
                    // Each quote followed by a word and another quote.
                    for (start, _) in text.match_indices('"') {
                        let rest = &text[start + 1..];
                        let length = rest
                            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                            .unwrap_or(rest.len());
                        if !rest[length..].starts_with('"') {
                            continue;
                        }
                        let word = &rest[..length];
                        let diagnostic = (word.starts_with("E_") || word.starts_with("W_"))
                            && word.len() > 2
                            && word
                                .chars()
                                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
                        let runtime = word.ends_with("Error")
                            && word.len() > 5
                            && word.starts_with(|c: char| c.is_ascii_uppercase())
                            && !word.contains('_');
                        if diagnostic || runtime {
                            found.insert(word.to_string());
                        }
                    }
                }
            }
        }
        found
    }

    #[test]
    fn every_code_is_explained() {
        let documented: BTreeSet<String> = codes().map(str::to_string).collect();
        let used = codes_in_sources();
        let missing: Vec<&String> = used.difference(&documented).collect();
        assert!(
            missing.is_empty(),
            "codes without an entry in docs/errors.md: {missing:?}"
        );
        let stale: Vec<&String> = documented.difference(&used).collect();
        assert!(
            stale.is_empty(),
            "entries for codes nothing uses: {stale:?}"
        );
    }

    #[test]
    fn entries_are_found_in_any_case() {
        let text = entry(lookup("e_moved_value").expect("code")).expect("entry");
        assert!(text.contains("transferred"));
        assert!(!render(text).contains("```"));
        assert!(entry("E_NO_SUCH_CODE").is_none());
    }
}
