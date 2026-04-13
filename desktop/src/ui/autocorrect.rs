//! Lightweight autocorrect engine for the message input.
//!
//! Replaces common English typos on word boundaries (space, punctuation, Enter).
//! No external dependencies — uses a hardcoded dictionary of frequent misspellings.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use gtk4::prelude::*;

/// Common English words for edit-distance matching.
/// When a typed word isn't in CORRECTIONS, we generate all edit-distance-1
/// candidates and check if exactly one matches this set.
static WORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    // Top ~1500 most common English words used in messaging
    let w: &[&str] = &[
        "a",
        "about",
        "above",
        "across",
        "actually",
        "after",
        "again",
        "against",
        "ago",
        "ahead",
        "all",
        "almost",
        "along",
        "already",
        "also",
        "always",
        "am",
        "among",
        "an",
        "and",
        "another",
        "any",
        "anyone",
        "anything",
        "anyway",
        "are",
        "area",
        "around",
        "as",
        "ask",
        "asked",
        "at",
        "available",
        "away",
        "back",
        "bad",
        "be",
        "beautiful",
        "because",
        "been",
        "before",
        "began",
        "behind",
        "being",
        "believe",
        "below",
        "best",
        "better",
        "between",
        "big",
        "bit",
        "both",
        "bring",
        "brought",
        "build",
        "business",
        "but",
        "buy",
        "by",
        "call",
        "called",
        "came",
        "can",
        "car",
        "care",
        "case",
        "change",
        "changed",
        "check",
        "children",
        "city",
        "class",
        "close",
        "cold",
        "come",
        "coming",
        "community",
        "company",
        "computer",
        "could",
        "country",
        "course",
        "create",
        "current",
        "cut",
        "day",
        "days",
        "deal",
        "dear",
        "did",
        "different",
        "do",
        "does",
        "doing",
        "done",
        "dont",
        "door",
        "down",
        "during",
        "each",
        "early",
        "easy",
        "eat",
        "end",
        "enough",
        "even",
        "evening",
        "ever",
        "every",
        "everyone",
        "everything",
        "example",
        "experience",
        "eye",
        "eyes",
        "face",
        "fact",
        "family",
        "far",
        "fast",
        "feel",
        "feeling",
        "few",
        "field",
        "file",
        "final",
        "finally",
        "find",
        "fine",
        "first",
        "five",
        "follow",
        "food",
        "for",
        "found",
        "free",
        "friend",
        "friends",
        "from",
        "front",
        "full",
        "fun",
        "game",
        "gave",
        "general",
        "get",
        "getting",
        "give",
        "given",
        "glad",
        "go",
        "going",
        "gone",
        "good",
        "got",
        "government",
        "great",
        "group",
        "grow",
        "guess",
        "guy",
        "had",
        "half",
        "hand",
        "happen",
        "happened",
        "happy",
        "hard",
        "has",
        "have",
        "having",
        "he",
        "head",
        "hear",
        "heard",
        "help",
        "her",
        "here",
        "hey",
        "hi",
        "high",
        "him",
        "his",
        "hit",
        "hold",
        "home",
        "hope",
        "hot",
        "hour",
        "hours",
        "house",
        "how",
        "however",
        "human",
        "i",
        "idea",
        "if",
        "image",
        "important",
        "in",
        "include",
        "information",
        "interest",
        "interested",
        "interesting",
        "into",
        "is",
        "issue",
        "it",
        "its",
        "job",
        "just",
        "keep",
        "kept",
        "kid",
        "kids",
        "kind",
        "knew",
        "know",
        "known",
        "language",
        "large",
        "last",
        "late",
        "later",
        "lead",
        "learn",
        "learned",
        "least",
        "leave",
        "left",
        "less",
        "let",
        "level",
        "life",
        "light",
        "like",
        "likely",
        "line",
        "link",
        "links",
        "list",
        "listen",
        "little",
        "live",
        "long",
        "look",
        "looking",
        "lost",
        "lot",
        "love",
        "low",
        "made",
        "main",
        "major",
        "make",
        "making",
        "man",
        "manage",
        "many",
        "matter",
        "may",
        "maybe",
        "me",
        "mean",
        "means",
        "media",
        "meet",
        "member",
        "members",
        "men",
        "message",
        "might",
        "mind",
        "minute",
        "minutes",
        "miss",
        "moment",
        "money",
        "month",
        "months",
        "more",
        "morning",
        "most",
        "mother",
        "move",
        "much",
        "must",
        "my",
        "myself",
        "name",
        "national",
        "near",
        "need",
        "never",
        "new",
        "news",
        "next",
        "nice",
        "night",
        "no",
        "none",
        "normal",
        "not",
        "note",
        "nothing",
        "now",
        "number",
        "of",
        "off",
        "office",
        "often",
        "oh",
        "ok",
        "okay",
        "old",
        "on",
        "once",
        "one",
        "only",
        "open",
        "or",
        "order",
        "other",
        "others",
        "our",
        "out",
        "outside",
        "over",
        "own",
        "page",
        "paid",
        "part",
        "past",
        "pay",
        "people",
        "perhaps",
        "period",
        "person",
        "personal",
        "phone",
        "pick",
        "picture",
        "place",
        "plan",
        "play",
        "please",
        "point",
        "political",
        "possible",
        "post",
        "power",
        "pretty",
        "probably",
        "problem",
        "program",
        "project",
        "provide",
        "public",
        "pull",
        "put",
        "question",
        "questions",
        "quickly",
        "quite",
        "ran",
        "rather",
        "read",
        "ready",
        "real",
        "really",
        "reason",
        "remember",
        "report",
        "rest",
        "result",
        "right",
        "room",
        "run",
        "running",
        "said",
        "same",
        "sat",
        "save",
        "saw",
        "say",
        "saying",
        "school",
        "second",
        "see",
        "seem",
        "seemed",
        "send",
        "sense",
        "sent",
        "service",
        "set",
        "several",
        "share",
        "she",
        "short",
        "should",
        "show",
        "side",
        "sign",
        "simple",
        "simply",
        "since",
        "sit",
        "situation",
        "six",
        "small",
        "so",
        "social",
        "some",
        "someone",
        "something",
        "sometimes",
        "son",
        "soon",
        "sort",
        "sound",
        "space",
        "speak",
        "special",
        "spend",
        "stand",
        "standard",
        "start",
        "started",
        "state",
        "stay",
        "step",
        "still",
        "stop",
        "story",
        "strong",
        "study",
        "stuff",
        "such",
        "sure",
        "system",
        "take",
        "taken",
        "talk",
        "tell",
        "test",
        "than",
        "thank",
        "thanks",
        "that",
        "the",
        "their",
        "them",
        "then",
        "there",
        "these",
        "they",
        "thing",
        "things",
        "think",
        "thinking",
        "third",
        "this",
        "those",
        "though",
        "thought",
        "three",
        "through",
        "time",
        "to",
        "today",
        "together",
        "told",
        "tomorrow",
        "tonight",
        "too",
        "took",
        "top",
        "total",
        "toward",
        "try",
        "turn",
        "turned",
        "two",
        "type",
        "under",
        "understand",
        "until",
        "up",
        "upon",
        "us",
        "use",
        "used",
        "using",
        "usually",
        "value",
        "very",
        "view",
        "wait",
        "walk",
        "want",
        "wanted",
        "was",
        "watch",
        "water",
        "way",
        "we",
        "week",
        "weeks",
        "well",
        "went",
        "were",
        "what",
        "when",
        "where",
        "whether",
        "which",
        "while",
        "white",
        "who",
        "whole",
        "why",
        "will",
        "win",
        "with",
        "without",
        "woman",
        "women",
        "won",
        "wonder",
        "word",
        "words",
        "work",
        "working",
        "world",
        "would",
        "write",
        "writing",
        "wrong",
        "year",
        "years",
        "yes",
        "yet",
        "you",
        "young",
        "your",
        // Tech/messaging vocabulary
        "account",
        "add",
        "address",
        "admin",
        "already",
        "app",
        "attach",
        "audio",
        "button",
        "cancel",
        "chat",
        "click",
        "close",
        "code",
        "color",
        "comment",
        "connect",
        "contact",
        "content",
        "copy",
        "data",
        "database",
        "debug",
        "default",
        "delete",
        "deploy",
        "design",
        "desktop",
        "detail",
        "details",
        "develop",
        "developer",
        "device",
        "dialog",
        "display",
        "docs",
        "document",
        "documents",
        "download",
        "edit",
        "email",
        "emoji",
        "enable",
        "error",
        "event",
        "feature",
        "feedback",
        "fetch",
        "files",
        "filter",
        "fix",
        "folder",
        "font",
        "format",
        "function",
        "github",
        "google",
        "grid",
        "handle",
        "header",
        "icon",
        "image",
        "images",
        "implement",
        "import",
        "inbox",
        "input",
        "install",
        "interface",
        "item",
        "items",
        "key",
        "label",
        "launch",
        "layout",
        "library",
        "load",
        "loading",
        "local",
        "location",
        "login",
        "logout",
        "manage",
        "menu",
        "modal",
        "mode",
        "model",
        "module",
        "network",
        "next",
        "notification",
        "notifications",
        "online",
        "option",
        "options",
        "output",
        "package",
        "panel",
        "password",
        "paste",
        "path",
        "photo",
        "photos",
        "platform",
        "plugin",
        "popup",
        "preview",
        "previous",
        "process",
        "profile",
        "progress",
        "push",
        "query",
        "queue",
        "react",
        "refresh",
        "release",
        "remote",
        "remove",
        "render",
        "replace",
        "reply",
        "request",
        "reset",
        "resize",
        "response",
        "restart",
        "restore",
        "return",
        "review",
        "role",
        "route",
        "save",
        "screen",
        "scroll",
        "search",
        "section",
        "select",
        "server",
        "session",
        "settings",
        "setup",
        "shared",
        "shortcut",
        "sidebar",
        "signin",
        "signup",
        "size",
        "socket",
        "source",
        "status",
        "storage",
        "style",
        "submit",
        "support",
        "switch",
        "sync",
        "table",
        "tab",
        "tabs",
        "tag",
        "task",
        "tasks",
        "template",
        "text",
        "theme",
        "thread",
        "thumbnail",
        "title",
        "token",
        "toggle",
        "tool",
        "tools",
        "track",
        "trigger",
        "update",
        "upgrade",
        "upload",
        "url",
        "user",
        "users",
        "variable",
        "version",
        "video",
        "videos",
        "visible",
        "warning",
        "web",
        "website",
        "widget",
        "window",
        "wrapper",
    ];
    w.iter().cloned().collect()
});

/// Common English typos → corrections.
/// Sourced from the most frequent misspellings in messaging contexts.
static CORRECTIONS: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    let entries: &[(&str, &str)] = &[
        // Missing internal vowels (user's specific complaint)
        ("abt", "about"),
        ("bcause", "because"),
        ("becuse", "because"),
        ("becase", "because"),
        ("beleive", "believe"),
        ("belive", "believe"),
        ("btwn", "between"),
        ("definately", "definitely"),
        ("definatly", "definitely"),
        ("defintely", "definitely"),
        ("diffrent", "different"),
        ("differnt", "different"),
        ("enviroment", "environment"),
        ("exprience", "experience"),
        ("exprienced", "experienced"),
        ("genral", "general"),
        ("goverment", "government"),
        ("importnt", "important"),
        ("intresting", "interesting"),
        ("intrest", "interest"),
        ("languge", "language"),
        ("managment", "management"),
        ("messge", "message"),
        ("mesage", "message"),
        ("necesary", "necessary"),
        ("occured", "occurred"),
        ("oppertunity", "opportunity"),
        ("organiztion", "organization"),
        ("probbly", "probably"),
        ("probaly", "probably"),
        ("problm", "problem"),
        ("recieve", "receive"),
        ("recived", "received"),
        ("recomend", "recommend"),
        ("recomended", "recommended"),
        ("refrence", "reference"),
        ("refrences", "references"),
        ("remeber", "remember"),
        ("rember", "remember"),
        ("respnse", "response"),
        ("seperate", "separate"),
        ("similiar", "similar"),
        ("somthing", "something"),
        ("someting", "something"),
        ("specfic", "specific"),
        ("succesful", "successful"),
        ("sucess", "success"),
        ("surprize", "surprise"),
        ("technolgy", "technology"),
        ("togeather", "together"),
        ("togethr", "together"),
        ("tommorow", "tomorrow"),
        ("tommorrow", "tomorrow"),
        ("tomorow", "tomorrow"),
        ("undrstnd", "understand"),
        ("unfortuantely", "unfortunately"),
        ("untill", "until"),
        // Common double/missing letter typos
        ("accomodate", "accommodate"),
        ("adress", "address"),
        ("agressive", "aggressive"),
        ("alot", "a lot"),
        ("anothe", "another"),
        ("apparantly", "apparently"),
        ("arguement", "argument"),
        ("assasination", "assassination"),
        ("basicly", "basically"),
        ("begining", "beginning"),
        ("calender", "calendar"),
        ("catagory", "category"),
        ("changable", "changeable"),
        ("collegue", "colleague"),
        ("comming", "coming"),
        ("commited", "committed"),
        ("completly", "completely"),
        ("concious", "conscious"),
        ("dilemna", "dilemma"),
        ("dissapear", "disappear"),
        ("dissapoint", "disappoint"),
        ("embarass", "embarrass"),
        ("existance", "existence"),
        ("familar", "familiar"),
        ("finaly", "finally"),
        ("foriegn", "foreign"),
        ("freind", "friend"),
        ("gaurntee", "guarantee"),
        ("garantee", "guarantee"),
        ("harrass", "harass"),
        ("humourous", "humorous"),
        ("imediately", "immediately"),
        ("independant", "independent"),
        ("knowlege", "knowledge"),
        ("liason", "liaison"),
        ("maintenace", "maintenance"),
        ("milenium", "millennium"),
        ("mispell", "misspell"),
        ("neccessary", "necessary"),
        ("noticable", "noticeable"),
        ("occassion", "occasion"),
        ("occurence", "occurrence"),
        ("peice", "piece"),
        ("persistant", "persistent"),
        ("posession", "possession"),
        ("privelege", "privilege"),
        ("profesional", "professional"),
        ("publically", "publicly"),
        ("realy", "really"),
        ("relevent", "relevant"),
        ("rythm", "rhythm"),
        ("schedual", "schedule"),
        ("shedule", "schedule"),
        ("sieze", "seize"),
        ("supercede", "supersede"),
        ("thier", "their"),
        ("truely", "truly"),
        ("tyrany", "tyranny"),
        ("wierd", "weird"),
        // Common messaging shortcuts/typos
        ("accross", "across"),
        ("acheive", "achieve"),
        ("aquire", "acquire"),
        ("beutiful", "beautiful"),
        ("cant", "can't"),
        ("dont", "don't"),
        ("doesnt", "doesn't"),
        ("didnt", "didn't"),
        ("couldnt", "couldn't"),
        ("wouldnt", "wouldn't"),
        ("shouldnt", "shouldn't"),
        ("hasnt", "hasn't"),
        ("havent", "haven't"),
        ("hadnt", "hadn't"),
        ("isnt", "isn't"),
        ("wasnt", "wasn't"),
        ("werent", "weren't"),
        ("wont", "won't"),
        ("im", "I'm"),
        ("ive", "I've"),
        ("id", "I'd"),
        ("ill", "I'll"),
        ("youre", "you're"),
        ("youve", "you've"),
        ("youd", "you'd"),
        ("youll", "you'll"),
        ("theyre", "they're"),
        ("theyve", "they've"),
        ("theyd", "they'd"),
        ("theyll", "they'll"),
        ("weve", "we've"),
        ("wed", "we'd"),
        ("were", "we're"), // Note: context-dependent, but commonly intended
        ("hes", "he's"),
        ("shes", "she's"),
        ("its", "it's"), // Note: context-dependent
        ("thats", "that's"),
        ("whats", "what's"),
        ("whos", "who's"),
        ("wheres", "where's"),
        ("heres", "here's"),
        ("theres", "there's"),
        ("lets", "let's"),
        // Fast typing transpositions
        ("teh", "the"),
        ("hte", "the"),
        ("taht", "that"),
        ("waht", "what"),
        ("whne", "when"),
        ("adn", "and"),
        ("ahve", "have"),
        ("jsut", "just"),
        ("woudl", "would"),
        ("coudl", "could"),
        ("shoudl", "should"),
        ("nto", "not"),
        ("yuo", "you"),
        ("knwo", "know"),
        ("liek", "like"),
        ("tihs", "this"),
        ("fro", "for"),
        ("wrok", "work"),
        ("form", "from"), // risky but common in messaging
        ("wiht", "with"),
        ("thsi", "this"),
        ("hwo", "how"),
        ("nad", "and"),
        ("tow", "two"),
        ("cna", "can"),
        // 2-letter abbreviations/typos
        ("nd", "and"),
        ("ot", "to"),
        ("fo", "of"),
        ("si", "is"),
        ("ti", "it"),
        ("hv", "have"),
        ("wt", "what"),
        ("hw", "how"),
        ("bt", "but"),
        ("yr", "your"),
        ("ur", "your"),
        ("r", "are"),
        ("u", "you"),
        ("n", "and"),
        ("b", "be"),
        ("bc", "because"),
        ("ab", "about"),
        ("wd", "would"),
        ("cd", "could"),
        ("sd", "should"),
        ("th", "the"),
        ("whn", "when"),
        ("wht", "what"),
        ("ths", "this"),
        ("tht", "that"),
        ("frm", "from"),
        ("msg", "message"),
        ("msgs", "messages"),
        ("pls", "please"),
        ("plz", "please"),
        ("thx", "thanks"),
        ("rly", "really"),
        ("sry", "sorry"),
        ("tmr", "tomorrow"),
        ("tmrw", "tomorrow"),
        ("yday", "yesterday"),
        ("yr", "year"),
        ("hrs", "hours"),
        ("min", "minutes"),
        ("mins", "minutes"),
        ("sec", "seconds"),
        ("secs", "seconds"),
        ("pic", "picture"),
        ("pics", "pictures"),
        ("info", "information"),
        ("govt", "government"),
        ("diff", "different"),
        ("prob", "problem"),
        ("probs", "problems"),
    ];
    entries.iter().cloned().collect()
});

/// Check if a word has a known correction.
///
/// Strategy (in order):
/// 1. Known typo dictionary — O(1), high confidence
/// 2. If word is already a valid English word — skip
/// 3. Edit-distance-1 candidates against common word list — catches any single typo
///
/// Preserves original capitalization pattern.
pub fn correct_word(word: &str) -> Option<String> {
    let lower = word.to_lowercase();

    // 1. Known typo dictionary (highest priority) — works for any length including "nd"→"and"
    if let Some(&correction) = CORRECTIONS.get(lower.as_str()) {
        return Some(apply_case(word, correction));
    }

    // Skip very short words for edit-distance (too ambiguous)
    if word.len() <= 2 {
        return None;
    }

    // 2. If word is already valid, don't correct
    if WORDS.contains(lower.as_str()) {
        return None;
    }

    // 3. Generate edit-distance-1 candidates and find matches in WORDS
    let candidates = edits1(&lower);
    let mut matches: Vec<&str> = candidates
        .iter()
        .filter_map(|c| {
            if WORDS.contains(c.as_str()) {
                Some(c.as_str())
            } else {
                None
            }
        })
        .collect();
    matches.sort_unstable();
    matches.dedup();

    // Only correct if there's exactly 1 match (unambiguous) or
    // if there's a match that's the same length (likely a substitution/transposition)
    let correction: Option<String> = if matches.len() == 1 {
        Some(matches[0].to_string())
    } else if matches.len() > 1 {
        // Prefer same-length matches (substitution/transposition more likely than insertion/deletion)
        let same_len: Vec<&str> = matches
            .iter()
            .filter(|m| m.len() == lower.len())
            .copied()
            .collect();
        if same_len.len() == 1 {
            Some(same_len[0].to_string())
        } else {
            None // Ambiguous — don't correct
        }
    } else {
        None
    };

    correction.map(|c| apply_case(word, &c))
}

/// Generate all strings that are 1 edit away from `word`.
/// Edits: deletion, transposition, replacement, insertion.
fn edits1(word: &str) -> Vec<String> {
    let chars: Vec<char> = word.chars().collect();
    let n = chars.len();
    let mut results = Vec::with_capacity(n * 54 + 26);
    let alphabet = "abcdefghijklmnopqrstuvwxyz";

    // Deletions
    for i in 0..n {
        let mut s = String::with_capacity(n - 1);
        for (j, &c) in chars.iter().enumerate() {
            if j != i {
                s.push(c);
            }
        }
        results.push(s);
    }

    // Transpositions
    for i in 0..n.saturating_sub(1) {
        let mut s: String = chars.iter().collect();
        // Safety: we're swapping adjacent ASCII chars
        unsafe {
            let bytes = s.as_bytes_mut();
            bytes.swap(i, i + 1);
        }
        results.push(s);
    }

    // Replacements
    for i in 0..n {
        for a in alphabet.chars() {
            if a != chars[i] {
                let mut s: String = chars.iter().collect();
                // Replace char at position i
                let byte_pos: usize = chars[..i].iter().map(|c| c.len_utf8()).sum();
                unsafe {
                    s.as_bytes_mut()[byte_pos] = a as u8;
                }
                results.push(s);
            }
        }
    }

    // Insertions
    for i in 0..=n {
        for a in alphabet.chars() {
            let mut s = String::with_capacity(n + 1);
            for (j, &c) in chars.iter().enumerate() {
                if j == i {
                    s.push(a);
                }
                s.push(c);
            }
            if i == n {
                s.push(a);
            }
            results.push(s);
        }
    }

    results
}

/// Apply the capitalization pattern of `original` to `correction`.
fn apply_case(original: &str, correction: &str) -> String {
    if original.chars().all(|c| c.is_uppercase()) {
        correction.to_uppercase()
    } else if original
        .chars()
        .next()
        .map(|c| c.is_uppercase())
        .unwrap_or(false)
    {
        let mut chars = correction.chars();
        let first = chars
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_default();
        format!("{first}{}", chars.as_str())
    } else {
        correction.to_string()
    }
}

/// Channel sender for the AI correction background task.
/// Set once at startup by `start_ai_corrector`.
static AI_TX: std::sync::OnceLock<std::sync::mpsc::Sender<AiCorrectionRequest>> =
    std::sync::OnceLock::new();

/// True when an AI correction request is in-flight.
/// Used by the send button to delay sending until correction completes.
static AI_IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Global generation counter — bumped on chat switch to invalidate all
/// pending AI corrections from the previous chat.
static GLOBAL_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Call when switching chats to cancel any pending AI autocorrect.
/// Prevents corrections from one chat leaking into another.
pub fn cancel_pending() {
    GLOBAL_GENERATION.fetch_add(1000, std::sync::atomic::Ordering::Relaxed);
    AI_IN_FLIGHT.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// Check if AI autocorrect is currently processing.
pub fn is_correcting() -> bool {
    AI_IN_FLIGHT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Block (with polling) until AI correction finishes or timeout.
/// Returns true if correction completed, false if timed out.
/// Call this from a GTK idle handler, NOT from the main thread directly.
pub fn wait_for_correction_done(timeout_ms: u64) -> bool {
    let start = std::time::Instant::now();
    while AI_IN_FLIGHT.load(std::sync::atomic::Ordering::Relaxed) {
        if start.elapsed().as_millis() as u64 > timeout_ms {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    true
}

/// Submit text directly to the AI corrector (bypasses the 400ms debounce)
/// and call `callback` on the GTK main thread with the corrected text.
/// If AI is unavailable or times out (~4s), the original text is returned.
pub fn correct_for_send(text: String, callback: impl FnOnce(String) + 'static) {
    // If AI isn't initialised or text is too short, return original immediately
    let Some(tx) = AI_TX.get() else {
        callback(text);
        return;
    };
    if text.trim().len() < 3 {
        callback(text);
        return;
    }

    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    if tx
        .send(AiCorrectionRequest {
            full_text: text.clone(),
            reply_tx,
            always_reply: true, // correct_for_send MUST get a response
        })
        .is_err()
    {
        callback(text);
        return;
    }

    // Poll for the AI response on the GTK main thread (every 50ms, max ~4s)
    let original = text;
    let cb = std::cell::Cell::new(Some(callback));
    let mut attempts = 0u32;
    gtk4::glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
        attempts += 1;
        match reply_rx.try_recv() {
            Ok(corrected) => {
                if let Some(f) = cb.take() {
                    f(corrected);
                }
                gtk4::glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                if attempts > 80 {
                    // ~4s timeout — send original
                    log::warn!("correct_for_send: AI timed out, sending original");
                    if let Some(f) = cb.take() {
                        f(original.clone());
                    }
                    gtk4::glib::ControlFlow::Break
                } else {
                    gtk4::glib::ControlFlow::Continue
                }
            }
            Err(_) => {
                // Channel closed
                if let Some(f) = cb.take() {
                    f(original.clone());
                }
                gtk4::glib::ControlFlow::Break
            }
        }
    });
}

struct AiCorrectionRequest {
    /// The full buffer text at the time of the request
    full_text: String,
    /// Callback: (corrected_full_text) → applied on GTK main thread
    reply_tx: std::sync::mpsc::Sender<String>,
    /// When true, ALWAYS send a reply (even if text unchanged or on error).
    /// Used by correct_for_send() so the caller never has to wait for a timeout.
    always_reply: bool,
}

/// Start the background AI correction thread.
/// Reads API key from `GEMINI_API_KEY` env var or `~/.config/whatsapp-desktop/gemini_key`.
/// If no key is found, AI correction is silently disabled.
pub fn start_ai_corrector() {
    // Load settings and pre-fill key from env/files if needed
    let mut settings = crate::ui::settings::AppSettings::load();
    settings.prefill_ai_key();

    let key = if !settings.ai_api_key.is_empty() {
        settings.ai_api_key.clone()
    } else {
        log::info!(
            "AI autocorrect disabled: no API key found. Set it in Settings → AI Autocorrect."
        );
        return;
    };

    if settings.ai_model == "none" {
        log::info!("AI autocorrect disabled by user (model set to 'none')");
        return;
    }

    let (tx, rx) = std::sync::mpsc::channel::<AiCorrectionRequest>();
    let _ = AI_TX.set(tx);

    log::info!("AI autocorrect enabled (key={}...)", &key[..8.min(key.len())]);
    std::thread::Builder::new()
        .name("ai-autocorrect".into())
        .spawn(move || {
            ai_corrector_loop(&key, rx);
        })
        .ok();
    log::info!("AI autocorrect enabled");
}

/// Clean any leaked reasoning, tags, or meta-commentary from AI response.
fn clean_ai_response(raw: &str) -> String {
    let mut text = raw.trim().to_string();

    // Strip any residual delimiter tags (in case model echoes them)
    text = text.replace("AC_START|", "");
    text = text.replace("|AC_END", "");
    text = text.replace("AC_START", "");
    text = text.replace("AC_END", "");

    // If response has "Result:" prefix, extract just the result
    for prefix in &["Result:", "result:", "Corrected:", "corrected:", "Output:"] {
        if let Some(pos) = text.find(prefix) {
            text = text[pos + prefix.len()..].trim().to_string();
        }
    }

    // Strip lines that are clearly reasoning, not corrected text
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() > 1 {
        let clean: Vec<&str> = lines
            .iter()
            .filter(|l| {
                let t = l.trim();
                !t.contains("\" -> \"")
                    && !t.contains("\" → \"")
                    && !t.starts_with("check ")
                    && !t.starts_with("Check ")
                    && !t.starts_with("Change:")
                    && !t.starts_with("Fix:")
                    && !t.starts_with("Correction:")
                    && !t.starts_with("Note:")
            })
            .copied()
            .collect();
        if !clean.is_empty() {
            text = clean.join("\n").trim().to_string();
        }
    }

    // Strip surrounding quotes if model wrapped the response
    if (text.starts_with('"') && text.ends_with('"'))
        || (text.starts_with('`') && text.ends_with('`'))
    {
        text = text[1..text.len() - 1].to_string();
    }

    text
}

fn ai_corrector_loop(api_key: &str, rx: std::sync::mpsc::Receiver<AiCorrectionRequest>) {
    log::info!("AI corrector thread started, waiting for requests...");
    let client = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(5))
        .timeout_read(std::time::Duration::from_secs(8))
        .timeout_write(std::time::Duration::from_secs(5))
        .build();
    // Use gemini-2.5-flash-lite for speed — no thinking overhead, ~1s responses.
    // gemini-3-flash-preview wastes 100-300 "thinking" tokens per request.
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash-lite:generateContent?key={api_key}"
    );

    while let Ok(req) = rx.recv() {
        let text = &req.full_text;
        if text.len() < 3 {
            if req.always_reply {
                let _ = req.reply_tx.send(req.full_text.clone());
            }
            continue;
        }

        log::info!("AI autocorrect: sending '{text}'");

        let body = serde_json::json!({
            "systemInstruction": {
                "parts": [{
                    "text": "You are an autocorrect engine. Return ONLY the corrected text. No explanations. No reasoning. No change descriptions. No markup. No quotes. Fix spelling, typos, missing/swapped letters, split words, missing apostrophes, and capitalization. Preserve meaning, tone, and slang."
                }]
            },
            "contents": [{
                "parts": [{
                    "text": text
                }]
            }],
            "generationConfig": {
                "temperature": 0.0,
                "maxOutputTokens": 2048,
            }
        });

        match client
            .post(&url)
            .set("content-type", "application/json")
            .send_json(&body)
        {
            Ok(resp) => match resp.into_json::<serde_json::Value>() {
                Ok(json) => {
                    if let Some(raw) =
                        json["candidates"][0]["content"]["parts"][0]["text"].as_str()
                    {
                        let corrected = clean_ai_response(raw);
                        log::info!("AI autocorrect: got '{corrected}'");
                        if corrected != req.full_text
                            && !corrected.is_empty()
                            && (corrected.len() as f64) < (req.full_text.len() as f64 * 1.5 + 20.0)
                            && corrected.len() as f64 >= req.full_text.len() as f64 * 0.5
                        {
                            let _ = req.reply_tx.send(corrected);
                        } else if req.always_reply {
                            // Text unchanged or safety-guarded — return original
                            let _ = req.reply_tx.send(req.full_text.clone());
                        }
                    } else {
                        log::warn!("AI autocorrect: unexpected response: {json}");
                        if req.always_reply {
                            let _ = req.reply_tx.send(req.full_text.clone());
                        }
                    }
                }
                Err(e) => {
                    log::warn!("AI autocorrect: JSON parse error: {e}");
                    if req.always_reply {
                        let _ = req.reply_tx.send(req.full_text.clone());
                    }
                }
            },
            Err(e) => {
                log::warn!("AI autocorrect: request failed: {e}");
                if req.always_reply {
                    let _ = req.reply_tx.send(req.full_text.clone());
                }
            }
        }
    }
}

/// Word-level diff: returns (index_pairs) of words that changed between old and new.
/// Each pair is (word_start_byte_offset_in_new, word_end_byte_offset_in_new).
fn diff_words(old: &str, new: &str) -> Vec<(usize, usize)> {
    let old_words: Vec<&str> = old.split_whitespace().collect();
    let new_words: Vec<&str> = new.split_whitespace().collect();
    let mut changed = Vec::new();
    let mut byte_offset = 0usize;

    // Walk through new text to find byte positions of each word
    let mut new_iter = new.char_indices().peekable();
    for (i, new_word) in new_words.iter().enumerate() {
        // Skip whitespace to find word start
        while let Some(&(pos, ch)) = new_iter.peek() {
            if ch.is_whitespace() {
                new_iter.next();
            } else {
                byte_offset = pos;
                break;
            }
        }
        let word_start = byte_offset;
        // Advance past the word
        for _ in new_word.chars() {
            new_iter.next();
        }
        let word_end = word_start + new_word.len();

        // Check if this word differs from the old text
        let old_word = old_words.get(i).copied().unwrap_or("");
        if old_word != *new_word {
            changed.push((word_start, word_end));
        }
    }
    changed
}

use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Install autocorrect on a GTK4 TextView.
///
/// Two-layer correction:
/// 1. **Local** (instant): Dictionary + edit-distance-1 on each space/punctuation
/// 2. **AI** (async): Full-message Gemini API call after a typing pause (400ms debounce)
///
/// Corrected words are highlighted with a subtle background tag.
/// Backspace at the end of a highlighted word reverts the correction.
pub fn install_on_textview(view: &gtk4::TextView) {
    let buf = view.buffer();
    let buf_ai = view.buffer();

    // Create a text tag for highlighting corrected words.
    // Inverted colors for readability on dark theme: white bg + dark text.
    let correction_tag = buf.create_tag(
        Some("autocorrect-highlight"),
        &[
            ("background", &"#e0e0e0"),
            ("background-set", &true),
            ("foreground", &"#1a1a2e"),
            ("foreground-set", &true),
        ],
    );
    // Store original words so backspace can revert individual corrections
    // Key: byte offset of the corrected word start → original word
    let revert_map: Rc<RefCell<HashMap<i32, String>>> = Rc::new(RefCell::new(HashMap::new()));
    let revert_ref = revert_map.clone();
    let revert_ref2 = revert_map.clone();

    let generation: Rc<Cell<u64>> = Rc::new(Cell::new(0));
    let gen_ref = generation.clone();

    let skip_next: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let skip_ref = skip_next.clone();
    let skip_ref2 = skip_next.clone();

    // Guard flag: prevents re-entrant buffer modifications when backspace
    // revert fires delete+insert (which triggers change signals synchronously).
    let modifying: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let modifying_local = modifying.clone();
    let modifying_ai = modifying.clone();

    // Words the user has reverted — skip autocorrect for these in this session.
    let ignored_words: Rc<RefCell<HashSet<String>>> = Rc::new(RefCell::new(HashSet::new()));
    let ignored_local = ignored_words.clone();
    let ignored_ai = ignored_words.clone();

    // ── Backspace revert: intercept key-press BEFORE GTK deletes a char ──
    // CAPTURE phase ensures we run before the TextView's own key handler.
    let bs_ctrl = gtk4::EventControllerKey::new();
    bs_ctrl.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let buf_bs = view.buffer();
    let tag_bs = correction_tag.clone();
    let revert_bs = revert_map.clone();
    let skip_bs = skip_next.clone();
    let modifying_bs = modifying.clone();
    let ignored_bs = ignored_words.clone();
    bs_ctrl.connect_key_pressed(move |_, key, _, modifier| {
        if key == gtk4::gdk::Key::BackSpace {
            if modifying_bs.get() {
                return gtk4::glib::Propagation::Stop; // swallow while deferred revert runs
            }
            let Some(tag_ref) = tag_bs.as_ref() else {
                return gtk4::glib::Propagation::Proceed;
            };
            let cursor = buf_bs.cursor_position();
            let iter = buf_bs.iter_at_offset(cursor);
            // Check if cursor is at or inside a tagged (corrected) word
            let in_tag = iter.has_tag(tag_ref) || {
                let mut prev = iter.clone();
                prev.backward_char();
                prev.has_tag(tag_ref)
            };
            if in_tag {
                // Find the tagged range
                let mut start = iter.clone();
                let mut end = iter.clone();
                if !start.starts_tag(Some(tag_ref)) {
                    start.backward_to_tag_toggle(Some(tag_ref));
                }
                if !end.ends_tag(Some(tag_ref)) {
                    end.forward_to_tag_toggle(Some(tag_ref));
                }
                let tag_start_offset = start.offset();
                let tag_end_offset = end.offset();
                if let Some(original) = revert_bs.borrow().get(&tag_start_offset).cloned() {
                    // Set flags now, do buffer work in idle callback to avoid
                    // re-entrant GTK signal handler crashes.
                    skip_bs.set(true);
                    modifying_bs.set(true);
                    ignored_bs.borrow_mut().insert(original.to_lowercase());
                    revert_bs.borrow_mut().remove(&tag_start_offset);

                    // Defer the actual buffer modification to an idle callback —
                    // this runs AFTER the key event is fully processed by GTK,
                    // so no signal re-entrancy can occur.
                    let buf_def = buf_bs.clone();
                    let mod_def = modifying_bs.clone();
                    gtk4::glib::idle_add_local_once(move || {
                        // Snapshot & rebuild: replace entire buffer to avoid
                        // delete+insert signal cascade.
                        let full = buf_def
                            .text(&buf_def.start_iter(), &buf_def.end_iter(), false)
                            .to_string();
                        // The tagged region is [tag_start_offset..tag_end_offset] in chars
                        let chars: Vec<char> = full.chars().collect();
                        let so = tag_start_offset as usize;
                        let eo = tag_end_offset as usize;
                        if so <= chars.len() && eo <= chars.len() && so <= eo {
                            let prefix: String = chars[..so].iter().collect();
                            let suffix: String = chars[eo..].iter().collect();
                            let new_text = format!("{prefix}{original}{suffix}");
                            let new_cursor = so + original.chars().count();
                            buf_def.set_text(&new_text);
                            let ci = buf_def.iter_at_offset(new_cursor as i32);
                            buf_def.place_cursor(&ci);
                        }
                        // MUST clear this flag — if it gets stuck, all autocorrect dies.
                        mod_def.set(false);
                    });
                    return gtk4::glib::Propagation::Stop;
                }
            }
        }
        // ── Ctrl+Z: skip next AI pass ──
        if key == gtk4::gdk::Key::z && modifier.contains(gtk4::gdk::ModifierType::CONTROL_MASK) {
            skip_bs.set(true);
        }
        gtk4::glib::Propagation::Proceed
    });
    view.add_controller(bs_ctrl);

    let key_ctrl = gtk4::EventControllerKey::new();
    key_ctrl.connect_key_released(move |_, key, _, modifier| {

        // Guard: don't run corrections while backspace revert is modifying the buffer
        if modifying_local.get() {
            return;
        }

        // ── Layer 1: Local instant correction on word boundaries ──
        // ONLY runs when AI is NOT available. When Gemini is active it handles
        // everything — running both causes race conditions where local edits
        // invalidate the AI's snapshot and lead to text deletion.
        let ai_active = AI_TX.get().is_some();
        let is_trigger = !ai_active && matches!(
            key,
            gtk4::gdk::Key::space
                | gtk4::gdk::Key::period
                | gtk4::gdk::Key::comma
                | gtk4::gdk::Key::exclam
                | gtk4::gdk::Key::question
                | gtk4::gdk::Key::semicolon
                | gtk4::gdk::Key::colon
        );

        if is_trigger {
            let cursor_offset = buf.cursor_position();
            if cursor_offset >= 2 {
                let trigger_iter = buf.iter_at_offset(cursor_offset - 1);
                let mut word_start = trigger_iter.clone();
                loop {
                    if !word_start.backward_char() {
                        break;
                    }
                    if word_start.char().is_whitespace() || word_start.char() == '\n' {
                        word_start.forward_char();
                        break;
                    }
                }
                let word = buf.text(&word_start, &trigger_iter, false).to_string();
                log::debug!("autocorrect: local trigger word='{word}'");
                if !word.is_empty() && !ignored_local.borrow().contains(&word.to_lowercase()) {
                    if let Some(corrected) = correct_word(&word) {
                        log::info!("autocorrect: local correction '{word}' → '{corrected}'");
                        let offset_start = word_start.offset();
                        let offset_end = trigger_iter.offset();
                        let buf_c = buf.clone();
                        let tag = correction_tag.clone();
                        let rv = revert_ref2.clone();
                        let orig = word.clone();
                        gtk4::glib::idle_add_local_once(move || {
                            let mut start = buf_c.iter_at_offset(offset_start);
                            let mut end = buf_c.iter_at_offset(offset_end);
                            buf_c.delete(&mut start, &mut end);
                            buf_c.insert(&mut start, &corrected);
                            // Highlight the corrected word
                            let tag_start = buf_c.iter_at_offset(offset_start);
                            let tag_end =
                                buf_c.iter_at_offset(offset_start + corrected.len() as i32);
                            if let Some(t) = &tag {
                                buf_c.apply_tag(t, &tag_start, &tag_end);
                                rv.borrow_mut().insert(offset_start, orig);
                                // Auto-remove highlight after 3 seconds
                                let buf_fade = buf_c.clone();
                                let tag_fade = t.clone();
                                let os = offset_start;
                                let oe = offset_start + corrected.len() as i32;
                                gtk4::glib::timeout_add_local_once(
                                    std::time::Duration::from_secs(3),
                                    move || {
                                        let s = buf_fade.iter_at_offset(os);
                                        let e = buf_fade
                                            .iter_at_offset(oe.min(buf_fade.end_iter().offset()));
                                        buf_fade.remove_tag(&tag_fade, &s, &e);
                                    },
                                );
                            }
                        });
                    }
                }
            }
        }

        // ── Layer 2: AI full-context correction after typing pause ──
        if AI_TX.get().is_some() && !modifying_ai.get() {
            let generation_id = gen_ref.get() + 1;
            gen_ref.set(generation_id);

            let buf_ref = buf_ai.clone();
            let gen_check = gen_ref.clone();
            let skip_check = skip_ref2.clone();
            let tag_for_ai = correction_tag.clone();
            let rv_for_ai = revert_ref2.clone();
            let ignored_for_ai = ignored_ai.clone();
            gtk4::glib::timeout_add_local_once(std::time::Duration::from_millis(400), move || {
                if gen_check.get() != generation_id {
                    return;
                }
                if skip_check.get() {
                    log::info!("AI autocorrect: skipping (user reverted/undid)");
                    skip_check.set(false);
                    return;
                }

                let full_text = buf_ref
                    .text(&buf_ref.start_iter(), &buf_ref.end_iter(), false)
                    .to_string();
                if full_text.trim().len() < 3 {
                    return;
                }

                if let Some(tx) = AI_TX.get() {
                    AI_IN_FLIGHT.store(true, std::sync::atomic::Ordering::Relaxed);
                    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
                    let _ = tx.send(AiCorrectionRequest {
                        full_text: full_text.clone(),
                        reply_tx,
                        always_reply: false, // live AC: silence is fine when unchanged
                    });

                    let buf_apply = buf_ref.clone();
                    let original = full_text;
                    let tag_apply = tag_for_ai.clone();
                    let rv_apply = rv_for_ai.clone();
                    let ignored_apply = ignored_for_ai.clone();
                    // Capture generation + cancellation counter at request time.
                    let request_gen = generation_id;
                    let gen_at_apply = gen_check.clone();
                    let cancel_gen_at_request =
                        GLOBAL_GENERATION.load(std::sync::atomic::Ordering::Relaxed);
                    gtk4::glib::timeout_add_local(
                        std::time::Duration::from_millis(50),
                        move || {
                            match reply_rx.try_recv() {
                                Ok(corrected) => {
                                    // Chat-switch cancellation: if cancel_pending() was called
                                    // since this request was sent, discard immediately.
                                    let current_cancel =
                                        GLOBAL_GENERATION.load(std::sync::atomic::Ordering::Relaxed);
                                    if current_cancel != cancel_gen_at_request {
                                        log::info!("AI autocorrect: discarding response (chat switched)");
                                        AI_IN_FLIGHT.store(false, std::sync::atomic::Ordering::Relaxed);
                                        return gtk4::glib::ControlFlow::Break;
                                    }
                                    // Stale response check: newer request sent since this one.
                                    if gen_at_apply.get() != request_gen {
                                        log::info!("AI autocorrect: discarding stale response (gen {} vs current {})", request_gen, gen_at_apply.get());
                                        AI_IN_FLIGHT.store(false, std::sync::atomic::Ordering::Relaxed);
                                        return gtk4::glib::ControlFlow::Break;
                                    }
                                    let current = buf_apply
                                        .text(&buf_apply.start_iter(), &buf_apply.end_iter(), false)
                                        .to_string();

                                    // Smart apply: if user typed more text after our snapshot,
                                    // apply correction to the prefix and keep the suffix.
                                    let (apply_text, suffix) = if current == original {
                                        (corrected.clone(), String::new())
                                    } else if current.starts_with(&original) {
                                        // User appended text — keep the new part
                                        let suffix = current[original.len()..].to_string();
                                        (format!("{corrected}{suffix}"), String::new())
                                    } else if original.starts_with(&current) {
                                        // User deleted text — skip
                                        AI_IN_FLIGHT
                                            .store(false, std::sync::atomic::Ordering::Relaxed);
                                        return gtk4::glib::ControlFlow::Break;
                                    } else {
                                        // Buffer changed significantly — skip
                                        AI_IN_FLIGHT
                                            .store(false, std::sync::atomic::Ordering::Relaxed);
                                        return gtk4::glib::ControlFlow::Break;
                                    };

                                    // Filter out corrections of ignored words
                                    let apply_text = {
                                        let ignored = ignored_apply.borrow();
                                        if ignored.is_empty() {
                                            apply_text
                                        } else {
                                            let old_words: Vec<&str> = current.split_whitespace().collect();
                                            let new_words: Vec<&str> = apply_text.split_whitespace().collect();
                                            if old_words.len() == new_words.len() {
                                                let filtered: Vec<&str> = old_words.iter().zip(new_words.iter())
                                                    .map(|(o, n)| {
                                                        if o != n && ignored.contains(&o.to_lowercase()) {
                                                            *o // keep original — user reverted this word
                                                        } else {
                                                            *n
                                                        }
                                                    })
                                                    .collect();
                                                filtered.join(" ")
                                            } else {
                                                apply_text
                                            }
                                        }
                                    };

                                    if apply_text == current {
                                        AI_IN_FLIGHT
                                            .store(false, std::sync::atomic::Ordering::Relaxed);
                                        return gtk4::glib::ControlFlow::Break;
                                    }

                                    // Diff to find which words changed
                                    let changed = diff_words(&current, &apply_text);

                                    let cursor = buf_apply.cursor_position();
                                    let old_len = current.len() as i32;
                                    buf_apply.set_text(&apply_text);
                                    let new_len = apply_text.len() as i32;
                                    let new_cursor =
                                        (cursor + (new_len - old_len)).max(0).min(new_len);
                                    let iter = buf_apply.iter_at_offset(new_cursor);
                                    buf_apply.place_cursor(&iter);

                                    // Highlight changed words and store originals for revert
                                    let old_words: Vec<&str> = current.split_whitespace().collect();
                                    if let Some(tag) = &tag_apply {
                                        let mut rv = rv_apply.borrow_mut();
                                        for (ws, we) in &changed {
                                            let s = buf_apply.iter_at_offset(*ws as i32);
                                            let e = buf_apply.iter_at_offset(*we as i32);
                                            buf_apply.apply_tag(tag, &s, &e);
                                            // Find original word at this position
                                            let new_words: Vec<&str> =
                                                apply_text.split_whitespace().collect();
                                            for (i, nw) in new_words.iter().enumerate() {
                                                let nw_start = apply_text.find(nw).unwrap_or(0);
                                                if nw_start == *ws {
                                                    if let Some(ow) = old_words.get(i) {
                                                        rv.insert(*ws as i32, ow.to_string());
                                                    }
                                                    break;
                                                }
                                            }
                                        }
                                        // Auto-fade highlights after 4 seconds
                                        let buf_fade = buf_apply.clone();
                                        let tag_fade = tag.clone();
                                        gtk4::glib::timeout_add_local_once(
                                            std::time::Duration::from_secs(4),
                                            move || {
                                                buf_fade.remove_tag(
                                                    &tag_fade,
                                                    &buf_fade.start_iter(),
                                                    &buf_fade.end_iter(),
                                                );
                                            },
                                        );
                                    }

                                    AI_IN_FLIGHT.store(false, std::sync::atomic::Ordering::Relaxed);
                                    gtk4::glib::ControlFlow::Break
                                }
                                Err(std::sync::mpsc::TryRecvError::Empty) => {
                                    gtk4::glib::ControlFlow::Continue
                                }
                                Err(_) => {
                                    AI_IN_FLIGHT.store(false, std::sync::atomic::Ordering::Relaxed);
                                    gtk4::glib::ControlFlow::Break
                                }
                            }
                        },
                    );
                }
            });
        }
    });
    view.add_controller(key_ctrl);

    // ── Click-to-revert: click on a highlighted correction to toggle it ──
    // First click: revert to original. Second click: re-apply correction.
    // The corrected form is stored so it can be toggled back.
    let click_ctrl = gtk4::GestureClick::new();
    click_ctrl.set_button(1); // left click
    let buf_click = view.buffer();
    let tag_click = buf_click.tag_table().lookup("autocorrect-highlight");
    let revert_click = revert_map.clone();
    // Store corrected→original pairs for re-apply: offset → corrected word
    let corrected_map: Rc<RefCell<HashMap<i32, String>>> = Rc::new(RefCell::new(HashMap::new()));
    let corrected_ref = corrected_map.clone();

    click_ctrl.connect_pressed(move |gesture, _n, x, y| {
        let Some(tag) = &tag_click else { return };
        let Some(widget) = gesture.widget() else {
            return;
        };
        let tv = widget.downcast_ref::<gtk4::TextView>().unwrap();

        // Convert widget coords to buffer coords
        let (bx, by) = tv.window_to_buffer_coords(gtk4::TextWindowType::Widget, x as i32, y as i32);
        let Some((iter, _)) = tv.iter_at_position(bx, by) else {
            return;
        };

        // Check if click landed on a highlighted word
        if !iter.has_tag(tag) {
            return;
        }

        // Find the tagged range
        let mut start = iter.clone();
        let mut end = iter.clone();
        if !start.starts_tag(Some(tag)) {
            start.backward_to_tag_toggle(Some(tag));
        }
        if !end.ends_tag(Some(tag)) {
            end.forward_to_tag_toggle(Some(tag));
        }
        let offset = start.offset();
        let current_word = buf_click.text(&start, &end, false).to_string();

        let mut reverts = revert_click.borrow_mut();
        let mut correcteds = corrected_ref.borrow_mut();

        if let Some(original) = reverts.remove(&offset) {
            // Currently showing CORRECTED word → revert to original
            correcteds.insert(offset, current_word);
            let mut s = buf_click.iter_at_offset(start.offset());
            let mut e = buf_click.iter_at_offset(end.offset());
            buf_click.delete(&mut s, &mut e);
            buf_click.insert(&mut s, &original);
            // Re-apply highlight to the reverted word so user can click again
            let new_end = buf_click.iter_at_offset(offset + original.len() as i32);
            let new_start = buf_click.iter_at_offset(offset);
            buf_click.apply_tag(tag, &new_start, &new_end);
        } else if let Some(corrected) = correcteds.remove(&offset) {
            // Currently showing ORIGINAL word → re-apply correction
            reverts.insert(offset, current_word);
            let mut s = buf_click.iter_at_offset(start.offset());
            let mut e = buf_click.iter_at_offset(end.offset());
            buf_click.delete(&mut s, &mut e);
            buf_click.insert(&mut s, &corrected);
            // Re-apply highlight
            let new_end = buf_click.iter_at_offset(offset + corrected.len() as i32);
            let new_start = buf_click.iter_at_offset(offset);
            buf_click.apply_tag(tag, &new_start, &new_end);
        }
    });
    view.add_controller(click_ctrl);
}
