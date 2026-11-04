//! The plugin's texts in English, French, Spanish and Turkish (`lang/<code>.lang`, `key=text`
//! lines; `{0}`, `{1}`, ... are filled in). Any other language reads English; the console reads
//! English.

const FILES: [(&str, &str); 4] = [
    ("en", include_str!("../lang/en.lang")),
    ("fr", include_str!("../lang/fr.lang")),
    ("es", include_str!("../lang/es.lang")),
    ("tr", include_str!("../lang/tr.lang")),
];

/// The language of a locale written as Minecraft writes it (`fr_fr`, `tr-tr`).
#[must_use]
pub fn language(locale: &str) -> &'static str {
    let code = locale.get(..2).unwrap_or("").to_ascii_lowercase();
    FILES
        .iter()
        .map(|(c, _)| *c)
        .find(|c| *c == code)
        .unwrap_or("en")
}

fn lookup<'a>(file: &'a str, key: &str) -> Option<&'a str> {
    file.lines()
        .filter_map(|line| line.split_once('='))
        .find(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim())
}

/// The text of `key` in `language` with `args` in place of `{0}`, `{1}`, ...; English when the
/// language lacks the key, the key itself when English lacks it too.
#[must_use]
pub fn text(language: &str, key: &str, args: &[&str]) -> String {
    let file = |code: &str| FILES.iter().find(|(c, _)| *c == code).map(|(_, f)| *f);
    let raw = file(language)
        .and_then(|f| lookup(f, key))
        .or_else(|| file("en").and_then(|f| lookup(f, key)))
        .unwrap_or(key);
    let mut out = raw.to_owned();
    for (i, arg) in args.iter().enumerate().rev() {
        out = out.replace(&format!("{{{i}}}"), arg);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(file: &str) -> Vec<&str> {
        file.lines()
            .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
            .filter_map(|l| l.split_once('=').map(|(k, _)| k.trim()))
            .collect()
    }

    fn slots(text: &str) -> Vec<usize> {
        (0..10)
            .filter(|i| text.contains(&format!("{{{i}}}")))
            .collect()
    }

    #[test]
    fn every_language_has_exactly_the_english_keys_with_the_same_placeholders() {
        let english = keys(FILES[0].1);
        let mut unique = english.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), english.len(), "a key is twice in en.lang");
        for (code, file) in FILES {
            let mut theirs = keys(file);
            theirs.sort_unstable();
            assert_eq!(theirs, unique, "{code} has other keys than English");
            for key in &english {
                let en = lookup(FILES[0].1, key).unwrap();
                let tr = lookup(file, key).unwrap();
                assert!(!tr.is_empty(), "{code} {key} is empty");
                assert_eq!(slots(tr), slots(en), "{code} {key}");
            }
        }
    }

    #[test]
    fn the_source_uses_only_keys_that_exist() {
        let english = keys(FILES[0].1);
        let mut found = 0;
        for source in [
            include_str!("commands.rs"),
            include_str!("state.rs"),
            include_str!("events.rs"),
        ] {
            for call in ["say(", "bar(", "fail(", "clone(), "] {
                let mut rest = source;
                while let Some(i) = rest.find(call) {
                    rest = &rest[i + call.len()..];
                    // The first quoted text before the call's closing parenthesis.
                    let inside = &rest[..rest.find(')').unwrap_or(rest.len())];
                    let Some(q) = inside.find('"') else { continue };
                    let after = &inside[q + 1..];
                    let key = &after[..after.find('"').unwrap_or(0)];
                    if !key.is_empty()
                        && key
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
                    {
                        assert!(english.contains(&key), "unknown key {key}");
                        found += 1;
                    }
                }
            }
        }
        assert!(found > 40, "only {found} keys found in the source");
    }

    #[test]
    fn locales_pick_their_language() {
        assert_eq!(language("fr_fr"), "fr");
        assert_eq!(language("es_mx"), "es");
        assert_eq!(language("tr_tr"), "tr");
        assert_eq!(language("de_de"), "en");
        assert_eq!(language(""), "en");
        assert_eq!(text("en", "no_such_key", &[]), "no_such_key");
        assert!(text("en", "size", &["1", "2", "3", "6"]).contains('6'));
    }
}
