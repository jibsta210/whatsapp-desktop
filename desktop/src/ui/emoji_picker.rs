use std::cell::Cell;
use std::rc::Rc;

use gtk4::gdk;
use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, EventControllerKey, FlowBox, Label, Orientation, ScrolledWindow,
    SearchEntry,
};

const MAX_RENDERED_EMOJIS: usize = 64;

struct EmojiEntry {
    glyph: &'static str,
    name: &'static str,
    /// Space-separated keywords, common shortcodes, and aliases.
    aliases: &'static str,
}

macro_rules! emoji {
    ($glyph:literal, $name:literal, $aliases:literal) => {
        EmojiEntry {
            glyph: $glyph,
            name: $name,
            aliases: $aliases,
        }
    };
}

/// Frequently useful emoji first, followed by a broad searchable catalog.
/// This is static data: searching never touches disk or allocates GTK widgets
/// for the complete catalog.
static EMOJIS: &[EmojiEntry] = &[
    emoji!("😀", "grinning face", "grin smile happy cheerful"),
    emoji!(
        "😂",
        "face with tears of joy",
        "joy laugh laughing lol funny tears"
    ),
    emoji!(
        "😍",
        "smiling face with heart eyes",
        "heart eyes love crush adore"
    ),
    emoji!("🥰", "smiling face with hearts", "love affection adored"),
    emoji!("😢", "crying face", "cry sad tear upset"),
    emoji!("😡", "enraged face", "angry mad rage furious"),
    emoji!("👍", "thumbs up", "thumbsup like approve yes good +1"),
    emoji!(
        "👎",
        "thumbs down",
        "thumbsdown dislike disapprove no bad -1"
    ),
    emoji!("❤️", "red heart", "heart love romance favourite favorite"),
    emoji!("🔥", "fire", "flame hot lit trending"),
    emoji!(
        "🎉",
        "party popper",
        "party celebrate celebration tada congrats"
    ),
    emoji!(
        "💯",
        "hundred points",
        "100 perfect score agree keep it real"
    ),
    emoji!(
        "🙏",
        "folded hands",
        "pray please thanks thank you namaste hope"
    ),
    emoji!(
        "😊",
        "smiling face with smiling eyes",
        "smile happy blush pleased"
    ),
    emoji!("🤔", "thinking face", "think hmm question wonder unsure"),
    emoji!(
        "😎",
        "smiling face with sunglasses",
        "cool shades confident"
    ),
    emoji!("👋", "waving hand", "wave hello goodbye hi bye"),
    emoji!("✨", "sparkles", "sparkle shine magic clean new"),
    emoji!("💪", "flexed biceps", "strong muscle strength workout flex"),
    emoji!("🤝", "handshake", "deal agreement partner together meet"),
    emoji!("😭", "loudly crying face", "sob cry tears very sad"),
    emoji!("🥺", "pleading face", "please puppy eyes beg cute"),
    emoji!(
        "😤",
        "face with steam from nose",
        "triumph frustrated angry proud"
    ),
    emoji!("🫡", "saluting face", "salute respect yes sir understood"),
    emoji!("🎊", "confetti ball", "party celebrate congratulations"),
    emoji!("💀", "skull", "dead death dying lol skeleton"),
    emoji!(
        "😱",
        "face screaming in fear",
        "scream scared shocked horror"
    ),
    emoji!("🤩", "star struck", "stars excited wow amazing"),
    emoji!("😘", "face blowing a kiss", "kiss love xoxo affection"),
    emoji!("💕", "two hearts", "love hearts romance affection"),
    emoji!("👀", "eyes", "look watch see attention suspicious"),
    emoji!("🫶", "heart hands", "love support care appreciation"),
    emoji!(
        "🤣",
        "rolling on the floor laughing",
        "rofl laugh lol funny"
    ),
    emoji!("😇", "smiling face with halo", "angel innocent blessed"),
    emoji!("🥳", "partying face", "party birthday celebrate hat"),
    emoji!("🤯", "exploding head", "mind blown shocked amazed wow"),
    emoji!("💔", "broken heart", "heartbreak sad breakup"),
    emoji!("🫠", "melting face", "melt hot embarrassed sarcasm"),
    emoji!("😏", "smirking face", "smirk flirt sly suggestive"),
    emoji!("🙄", "face with rolling eyes", "eyeroll annoyed whatever"),
    emoji!("😒", "unamused face", "unimpressed annoyed meh"),
    emoji!("🤗", "smiling face with open hands", "hug hugs embrace"),
    emoji!(
        "🤭",
        "face with hand over mouth",
        "giggle oops secret laugh"
    ),
    emoji!("🫣", "face with peeking eye", "peek scared shy watching"),
    emoji!("💅", "nail polish", "nails manicure sass unbothered"),
    emoji!("🦋", "butterfly", "insect beautiful transformation nature"),
    emoji!("🌈", "rainbow", "pride colourful colorful weather"),
    emoji!("⭐", "star", "favorite favourite rating night"),
    emoji!(
        "😁",
        "beaming face with smiling eyes",
        "grin smile happy teeth"
    ),
    emoji!("😄", "grinning face with smiling eyes", "smile happy laugh"),
    emoji!(
        "😅",
        "grinning face with sweat",
        "sweat relief nervous laugh"
    ),
    emoji!("😉", "winking face", "wink flirt joke"),
    emoji!("🙂", "slightly smiling face", "smile okay friendly"),
    emoji!("🙃", "upside down face", "sarcasm silly irony"),
    emoji!("😋", "face savoring food", "yum delicious tasty tongue"),
    emoji!("😛", "face with tongue", "tongue playful silly"),
    emoji!("🤪", "zany face", "crazy goofy wild silly"),
    emoji!("🤓", "nerd face", "geek glasses smart study"),
    emoji!("🧐", "face with monocle", "inspect curious fancy"),
    emoji!("😐", "neutral face", "meh blank indifferent"),
    emoji!("😑", "expressionless face", "blank annoyed speechless"),
    emoji!(
        "🫤",
        "face with diagonal mouth",
        "unsure skeptical disappointed"
    ),
    emoji!("😬", "grimacing face", "awkward nervous yikes"),
    emoji!("😴", "sleeping face", "sleep tired snooze zzz"),
    emoji!("🤒", "face with thermometer", "sick ill fever"),
    emoji!("🤢", "nauseated face", "sick gross green vomit"),
    emoji!("🥶", "cold face", "freezing ice frozen"),
    emoji!("🥵", "hot face", "heat sweating spicy"),
    emoji!("🥴", "woozy face", "dizzy drunk confused"),
    emoji!("😵", "face with crossed out eyes", "dizzy unconscious dead"),
    emoji!("🤐", "zipper mouth face", "secret quiet silent zip"),
    emoji!("🤫", "shushing face", "quiet hush secret silence"),
    emoji!(
        "🫢",
        "face with open eyes and hand over mouth",
        "gasp surprise shock oops"
    ),
    emoji!("😮", "face with open mouth", "surprise wow shocked"),
    emoji!("😲", "astonished face", "surprised shocked amazed"),
    emoji!("😳", "flushed face", "embarrassed blush shocked"),
    emoji!(
        "🥹",
        "face holding back tears",
        "grateful touched proud cry"
    ),
    emoji!("😞", "disappointed face", "sad disappointed regret"),
    emoji!("😔", "pensive face", "sad thoughtful sorry"),
    emoji!("😟", "worried face", "worry anxious concerned"),
    emoji!("😩", "weary face", "tired frustrated exhausted"),
    emoji!("😫", "tired face", "exhausted fed up"),
    emoji!(
        "🤬",
        "face with symbols on mouth",
        "swearing angry curse censored"
    ),
    emoji!("😈", "smiling face with horns", "devil evil mischievous"),
    emoji!("👻", "ghost", "halloween spooky boo"),
    emoji!("👽", "alien", "ufo space extraterrestrial"),
    emoji!("🤖", "robot", "bot machine ai android"),
    emoji!("💩", "pile of poo", "poop shit funny"),
    emoji!("👏", "clapping hands", "clap applause bravo well done"),
    emoji!("🙌", "raising hands", "hooray celebrate praise high five"),
    emoji!("👐", "open hands", "hug openness jazz hands"),
    emoji!("🤲", "palms up together", "pray receive dua"),
    emoji!("👌", "ok hand", "okay perfect good approval"),
    emoji!("🤌", "pinched fingers", "italian what gesture"),
    emoji!("✌️", "victory hand", "peace victory two"),
    emoji!("🤞", "crossed fingers", "luck hope fingers crossed"),
    emoji!(
        "🫰",
        "hand with index finger and thumb crossed",
        "finger heart money love"
    ),
    emoji!("🤟", "love you gesture", "ily hand sign"),
    emoji!("🤘", "sign of the horns", "rock metal horns"),
    emoji!("👊", "oncoming fist", "fist bump punch bro"),
    emoji!("✊", "raised fist", "power solidarity resist"),
    emoji!("🫵", "index pointing at viewer", "you point"),
    emoji!(
        "👉",
        "backhand index pointing right",
        "right point direction"
    ),
    emoji!("👈", "backhand index pointing left", "left point direction"),
    emoji!("☝️", "index pointing up", "up one attention"),
    emoji!("👇", "backhand index pointing down", "down point direction"),
    emoji!("✍️", "writing hand", "write signature note"),
    emoji!("💋", "kiss mark", "lips kiss lipstick"),
    emoji!("🫂", "people hugging", "hug comfort support"),
    emoji!("👤", "bust in silhouette", "person profile user account"),
    emoji!("👥", "busts in silhouette", "people group users team"),
    emoji!("🧠", "brain", "mind smart think intelligence"),
    emoji!("🫀", "anatomical heart", "heart organ health"),
    emoji!("🐶", "dog face", "dog puppy pet animal"),
    emoji!("🐱", "cat face", "cat kitten pet animal"),
    emoji!("🐭", "mouse face", "mouse animal rodent"),
    emoji!("🐰", "rabbit face", "rabbit bunny animal"),
    emoji!("🦊", "fox", "fox animal clever"),
    emoji!("🐻", "bear", "bear animal teddy"),
    emoji!("🐼", "panda", "panda bear animal"),
    emoji!("🐸", "frog", "frog toad animal"),
    emoji!("🐵", "monkey face", "monkey animal"),
    emoji!("🙈", "see no evil monkey", "monkey embarrassed hide"),
    emoji!("🙉", "hear no evil monkey", "monkey ignore ears"),
    emoji!("🙊", "speak no evil monkey", "monkey secret mouth"),
    emoji!("🐔", "chicken", "chicken bird animal"),
    emoji!("🐧", "penguin", "penguin bird cold"),
    emoji!("🦄", "unicorn", "unicorn magic horse"),
    emoji!("🐝", "honeybee", "bee insect honey busy"),
    emoji!("🌸", "cherry blossom", "flower spring pink bloom"),
    emoji!("🌹", "rose", "flower love romance red"),
    emoji!("🌻", "sunflower", "flower sun summer yellow"),
    emoji!("🍀", "four leaf clover", "luck lucky ireland plant"),
    emoji!("🌞", "sun with face", "sun sunny summer morning"),
    emoji!("🌙", "crescent moon", "moon night sleep"),
    emoji!("☀️", "sun", "sunny weather hot day"),
    emoji!("☁️", "cloud", "cloudy weather sky"),
    emoji!("⚡", "high voltage", "lightning electric fast power"),
    emoji!("❄️", "snowflake", "snow cold winter frozen"),
    emoji!(
        "☔",
        "umbrella with rain drops",
        "rain weather wet umbrella"
    ),
    emoji!("🍎", "red apple", "apple fruit food teacher"),
    emoji!("🍌", "banana", "banana fruit food"),
    emoji!("🍓", "strawberry", "berry fruit food"),
    emoji!("🍕", "pizza", "pizza food slice"),
    emoji!("🍔", "hamburger", "burger food fast food"),
    emoji!("🍟", "french fries", "fries chips food"),
    emoji!("🌮", "taco", "taco mexican food"),
    emoji!("🍿", "popcorn", "movie cinema snack"),
    emoji!("☕", "hot beverage", "coffee tea drink cafe morning"),
    emoji!("🍺", "beer mug", "beer drink cheers pub"),
    emoji!("🍷", "wine glass", "wine drink cheers"),
    emoji!("🥂", "clinking glasses", "cheers toast celebrate champagne"),
    emoji!("🎂", "birthday cake", "cake birthday dessert celebrate"),
    emoji!("⚽", "soccer ball", "football soccer sport ball"),
    emoji!("🏀", "basketball", "basketball sport ball"),
    emoji!("🏈", "american football", "football nfl sport ball"),
    emoji!("🎮", "video game", "gaming controller play game"),
    emoji!("🎵", "musical note", "music song audio note"),
    emoji!("🎧", "headphone", "headphones music audio listen"),
    emoji!("🎬", "clapper board", "movie film cinema action"),
    emoji!(
        "📸",
        "camera with flash",
        "camera photo picture photography"
    ),
    emoji!("💡", "light bulb", "idea light smart inspiration"),
    emoji!("📌", "pushpin", "pin location important"),
    emoji!("📅", "calendar", "date schedule event"),
    emoji!("✅", "check mark button", "check done yes complete correct"),
    emoji!("❌", "cross mark", "x no wrong cancel delete"),
    emoji!("⚠️", "warning", "alert caution danger"),
    emoji!("❓", "red question mark", "question help unknown"),
    emoji!("‼️", "double exclamation mark", "important surprise bang"),
    emoji!("🚀", "rocket", "launch space fast startup ship"),
    emoji!("✈️", "airplane", "plane flight travel vacation"),
    emoji!("🚗", "automobile", "car vehicle drive travel"),
    emoji!("🏠", "house", "home building"),
    emoji!("🏆", "trophy", "winner award champion prize"),
    emoji!("🥇", "first place medal", "gold winner champion number one"),
    emoji!("🎁", "wrapped gift", "gift present birthday surprise"),
    emoji!("💰", "money bag", "money cash rich dollar"),
    emoji!("💎", "gem stone", "diamond jewel valuable"),
    emoji!("🔒", "locked", "lock private secure security"),
    emoji!("🔑", "key", "key password unlock access"),
    emoji!("📱", "mobile phone", "phone smartphone cell"),
    emoji!("💻", "laptop", "computer pc work code"),
    emoji!("⌚", "watch", "time clock wearable"),
];

pub struct EmojiPicker {
    root: Box,
    search: SearchEntry,
}

impl EmojiPicker {
    pub fn new(on_selected: impl Fn(&str) + 'static) -> Self {
        let root = Box::new(Orientation::Vertical, 6);
        let search = SearchEntry::new();
        search.set_placeholder_text(Some("Search emoji by name or keyword"));
        search.set_margin_start(8);
        search.set_margin_end(8);
        search.set_margin_top(8);

        let grid = FlowBox::new();
        grid.set_max_children_per_line(8);
        grid.set_min_children_per_line(8);
        grid.set_selection_mode(gtk4::SelectionMode::None);
        grid.set_homogeneous(true);
        grid.set_column_spacing(2);
        grid.set_row_spacing(2);

        let status = Label::new(None);
        status.add_css_class("dim-label");
        status.add_css_class("caption");
        status.set_halign(Align::Center);
        status.set_margin_bottom(6);

        let scroll = ScrolledWindow::new();
        scroll.set_child(Some(&grid));
        scroll.set_vexpand(true);

        root.append(&search);
        root.append(&scroll);
        root.append(&status);

        let on_selected: Rc<dyn Fn(&str)> = Rc::new(on_selected);
        render_results(&grid, &status, "", &on_selected);

        // A tiny debounce coalesces fast typing and keeps GTK row churn to one
        // bounded rebuild per burst instead of one rebuild per key event.
        let generation = Rc::new(Cell::new(0_u64));
        {
            let grid = grid.clone();
            let status = status.clone();
            let on_selected = on_selected.clone();
            let generation = generation.clone();
            search.connect_search_changed(move |entry| {
                let current = generation.get().wrapping_add(1);
                generation.set(current);
                let query = entry.text().to_string();
                let grid = grid.clone();
                let status = status.clone();
                let on_selected = on_selected.clone();
                let generation = generation.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(45), move || {
                    if generation.get() == current {
                        render_results(&grid, &status, &query, &on_selected);
                    }
                });
            });
        }

        // Enter chooses the highest-ranked result. Down moves into the grid;
        // GTK then provides normal arrow/Tab navigation between buttons.
        {
            let on_selected = on_selected.clone();
            search.connect_activate(move |entry| {
                if let Some(result) = ranked_results(entry.text().as_str()).first() {
                    on_selected(result.glyph);
                }
            });
        }
        {
            let controller = EventControllerKey::new();
            let grid = grid.clone();
            let search_weak = search.downgrade();
            controller.connect_key_pressed(move |_, key, _, _| {
                if key == gdk::Key::Escape {
                    if let Some(search) = search_weak.upgrade() {
                        if !search.text().is_empty() {
                            search.set_text("");
                            return glib::Propagation::Stop;
                        }
                    }
                }
                if key == gdk::Key::Down {
                    if let Some(button) = first_grid_button(&grid) {
                        button.grab_focus();
                        return glib::Propagation::Stop;
                    }
                }
                glib::Propagation::Proceed
            });
            search.add_controller(controller);
        }

        Self { root, search }
    }

    pub fn widget(&self) -> &Box {
        &self.root
    }

    pub fn search_entry(&self) -> &SearchEntry {
        &self.search
    }
}

fn render_results(grid: &FlowBox, status: &Label, query: &str, on_selected: &Rc<dyn Fn(&str)>) {
    while let Some(child) = grid.first_child() {
        grid.remove(&child);
    }

    let results = ranked_results(query);

    for entry in &results {
        let button = Button::with_label(entry.glyph);
        button.add_css_class("flat");
        button.set_size_request(46, 42);
        button.set_tooltip_text(Some(entry.name));
        let glyph = entry.glyph;
        let on_selected = on_selected.clone();
        button.connect_clicked(move |_| on_selected(glyph));
        grid.append(&button);
    }

    let normalized = normalize_query(query);
    if results.is_empty() {
        status.set_text(&format!("No emoji found for “{}”", query.trim()));
        status.remove_css_class("dim-label");
    } else if normalized.is_empty() {
        status.set_text("Common emoji · type to search the full catalog");
        status.add_css_class("dim-label");
    } else {
        status.set_text(&format!(
            "{} match{} · Enter selects the first",
            results.len(),
            if results.len() == 1 { "" } else { "es" }
        ));
        status.add_css_class("dim-label");
    }
}

fn first_grid_button(grid: &FlowBox) -> Option<Button> {
    grid.first_child()?
        .downcast::<gtk4::FlowBoxChild>()
        .ok()?
        .child()?
        .downcast::<Button>()
        .ok()
}

fn ranked_results(query: &str) -> Vec<&'static EmojiEntry> {
    let query = normalize_query(query);
    let mut matches: Vec<_> = EMOJIS
        .iter()
        .enumerate()
        .filter_map(|(position, entry)| {
            match_score(entry, &query).map(|score| (score, position, entry))
        })
        .collect();
    matches.sort_by_key(|(score, position, _)| (*score, *position));
    matches
        .into_iter()
        .take(MAX_RENDERED_EMOJIS)
        .map(|(_, _, entry)| entry)
        .collect()
}

fn normalize_query(query: &str) -> String {
    query
        .trim()
        .trim_matches(':')
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn match_score(entry: &EmojiEntry, query: &str) -> Option<u8> {
    if query.is_empty() {
        return Some(0);
    }
    let name = entry.name;
    let aliases = entry.aliases;
    if query == entry.glyph || query == name || aliases.split_whitespace().any(|word| word == query)
    {
        return Some(0);
    }
    if name.starts_with(query) {
        return Some(1);
    }

    let terms: Vec<_> = query.split_whitespace().collect();
    if terms.iter().all(|term| {
        name.split_whitespace()
            .chain(aliases.split_whitespace())
            .any(|word| word.starts_with(term))
    }) {
        return Some(2);
    }
    terms
        .iter()
        .all(|term| name.contains(term) || aliases.contains(term))
        .then_some(3)
}

#[cfg(test)]
mod tests {
    use super::{MAX_RENDERED_EMOJIS, ranked_results};

    #[test]
    fn ranks_names_keywords_and_shortcode_aliases() {
        assert_eq!(ranked_results("dog")[0].glyph, "🐶");
        assert_eq!(ranked_results(":thumbsup:")[0].glyph, "👍");
        assert_eq!(ranked_results("tears joy")[0].glyph, "😂");
        assert_eq!(ranked_results("smartphone")[0].glyph, "📱");
    }

    #[test]
    fn unknown_query_has_a_real_empty_state() {
        assert!(ranked_results("definitely-not-an-emoji").is_empty());
    }

    #[test]
    fn rendered_result_set_is_bounded() {
        assert!(ranked_results("").len() <= MAX_RENDERED_EMOJIS);
        assert!(ranked_results("face").len() <= MAX_RENDERED_EMOJIS);
    }
}
