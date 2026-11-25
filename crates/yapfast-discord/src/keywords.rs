//! The words that alert this account to a message as a mention does, though
//! Discord wouldn't notify it: Settings' Keyword Alerts. Discord's clients
//! have none; Yapfast keeps them for the whole process, as the window sets
//! them, and the gateway's session looks each new message's text up in
//! them ([`matching`]).

use std::sync::RwLock;

/// The keywords, lowercase and trimmed, without repeats.
static KEYWORDS: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// Alerts from now on to messages that hold any of `keywords`: each is a
/// word or a few, matched whole and ignoring case. Empty ones are left out.
pub fn set(keywords: impl IntoIterator<Item = impl AsRef<str>>) {
    let mut kept: Vec<String> = Vec::new();
    for keyword in keywords {
        let keyword = keyword.as_ref().trim().to_lowercase();
        if !keyword.is_empty() && !kept.contains(&keyword) {
            kept.push(keyword);
        }
    }
    *KEYWORDS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = kept;
}

/// The first keyword `text` holds as words of their own, as it was set.
pub fn matching(text: &str) -> Option<String> {
    let keywords = KEYWORDS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if keywords.is_empty() || text.is_empty() {
        return None;
    }
    let text = text.to_lowercase();
    keywords
        .iter()
        .find(|keyword| holds(&text, keyword))
        .cloned()
}

/// Whether lowercase `text` holds lowercase `keyword` with no letter or
/// digit right before or after it, so "set" is found in "the set's
/// tracklist" and not in "settings".
pub fn holds(text: &str, keyword: &str) -> bool {
    !keyword.is_empty()
        && text.match_indices(keyword).any(|(at, found)| {
            let before = text[..at].chars().next_back();
            let after = text[at + found.len()..].chars().next();
            !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_keyword_is_found_as_a_word_of_its_own() {
        assert!(holds("the set's tracklist", "set"));
        assert!(holds("drop the set!", "set"));
        assert!(!holds("settings", "set"));
        assert!(!holds("reset", "set"));
        assert!(holds("ouvi o dj set ontem", "dj set"));
        assert!(holds("ação", "ação"));
        assert!(!holds("reação", "ação"));
    }

    #[test]
    fn keywords_are_kept_lowercase_once() {
        set(["  Set ", "set", "", "DJ"]);
        assert_eq!(matching("Who's on the DJ booth?").as_deref(), Some("dj"));
        assert_eq!(matching("Nice SET").as_deref(), Some("set"));
        assert_eq!(matching("nothing here"), None);
        set(Vec::<String>::new());
        assert_eq!(matching("Nice set"), None);
    }
}
