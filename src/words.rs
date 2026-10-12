//! Word list for verification words.
//!
//! One byte maps to one word, so `n` words carry `8 * n` bits. The list is
//! sorted, has no duplicates, and uses only lowercase ASCII letters.

pub const WORDS: [&str; 256] = [
    "acorn", "amber", "anchor", "apple", "apron", "arrow", "aspen", "atlas", "autumn", "badge",
    "bamboo", "banjo", "barley", "basil", "beacon", "bean", "berry", "birch", "biscuit", "bison",
    "blossom", "bonfire", "branch", "breeze", "brick", "bridge", "brook", "bubble", "bucket",
    "button", "cabin", "cactus", "camel", "canary", "candle", "canoe", "canyon", "carrot",
    "castle", "cedar", "cello", "chalk", "cherry", "chess", "cider", "citrus", "clay", "cliff",
    "clock", "cloud", "clover", "cobalt", "coconut", "comet", "compass", "copper", "coral",
    "cotton", "cradle", "crane", "crayon", "cricket", "crystal", "cupcake", "dahlia", "daisy",
    "dawn", "delta", "denim", "desert", "dolphin", "domino", "dragon", "drum", "dune", "eagle",
    "echo", "ember", "emerald", "engine", "falcon", "feather", "fennel", "fern", "ferret",
    "fiddle", "fig", "flame", "flint", "forest", "fossil", "fountain", "fox", "garden", "garnet",
    "gecko", "ginger", "glacier", "globe", "goose", "granite", "grape", "gravel", "guitar",
    "hammer", "harbor", "harp", "hazel", "heron", "hickory", "hill", "honey", "horizon", "hornet",
    "husky", "igloo", "indigo", "iris", "island", "ivory", "jade", "jaguar", "jasmine", "jelly",
    "jungle", "kayak", "kettle", "kiwi", "koala", "ladder", "lagoon", "lantern", "lava", "lemon",
    "lily", "lime", "linen", "lizard", "llama", "lobster", "lotus", "magnet", "mango", "maple",
    "marble", "meadow", "melon", "meteor", "mint", "mitten", "moose", "moss", "mountain", "nectar",
    "nest", "nickel", "noodle", "nutmeg", "oak", "oasis", "ocean", "olive", "onyx", "orbit",
    "orchid", "otter", "owl", "paddle", "panda", "pansy", "paper", "parsley", "peach", "pearl",
    "pebble", "pelican", "pepper", "piano", "pine", "planet", "plum", "pocket", "pony", "poppy",
    "prism", "pumpkin", "quail", "quartz", "quill", "rabbit", "radish", "rainbow", "raven", "reef",
    "rhubarb", "ribbon", "river", "robin", "rocket", "rose", "ruby", "saddle", "saffron", "sage",
    "salmon", "sandal", "sapphire", "satin", "seal", "sesame", "shell", "sierra", "silver",
    "sketch", "sparrow", "spruce", "squash", "star", "stone", "storm", "sugar", "summit", "sunset",
    "swan", "tablet", "tango", "teapot", "thistle", "thunder", "tiger", "timber", "toast", "topaz",
    "tulip", "tundra", "turnip", "turtle", "umbrella", "valley", "velvet", "violet", "volcano",
    "walnut", "walrus", "wasabi", "whale", "willow", "window", "winter", "wizard", "wren",
    "yarrow", "yogurt", "zebra", "zephyr", "zinc",
];

pub fn word(byte: u8) -> &'static str {
    WORDS[usize::from(byte)]
}

/// Renders bytes as words separated by single spaces.
pub fn render(bytes: &[u8]) -> String {
    bytes.iter().map(|b| word(*b)).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_byte_has_its_own_word() {
        // Two bytes sharing a word would let different verifiers look equal.
        let mut sorted = WORDS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 256);
        assert!(
            WORDS
                .iter()
                .all(|w| w.bytes().all(|b| b.is_ascii_lowercase()))
        );
    }
}
