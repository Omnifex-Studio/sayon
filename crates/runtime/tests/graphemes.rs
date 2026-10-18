//! Checks grapheme clusters against the cases in graphemes.txt, whose
//! expected clusters come from an independent implementation of the rules.

use sayon_runtime::graphemes::clusters;

fn decode(cluster: &str) -> String {
    cluster
        .split_whitespace()
        .map(|hex| {
            let code = u32::from_str_radix(hex, 16).expect("hexadecimal code point");
            char::from_u32(code).expect("a code point")
        })
        .collect()
}

#[test]
fn clusters_match_the_generated_cases() {
    let cases = include_str!("graphemes.txt");
    let mut failures = Vec::new();
    let mut checked = 0;
    for line in cases.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let expected: Vec<String> = line.split('|').map(decode).collect();
        let text: String = expected.concat();
        let found: Vec<&str> = clusters(&text);
        if found != expected {
            failures.push(format!("{line}\n  found: {found:?}"));
        }
        checked += 1;
    }
    assert!(checked > 1000, "only {checked} cases");
    assert!(
        failures.is_empty(),
        "{} of {checked} cases differ:\n{}",
        failures.len(),
        failures
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
