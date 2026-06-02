//! Memorable container names: `n8-<adjective>-<animal>`, e.g. `n8-fun-swan`.
//!
//! The control-plane work started naming containers `nemesis8-it-a3f9c2b1`
//! (uuid-suffixed) so they'd be addressable. But discovery keys off the
//! `nemesis8.agent` Docker label, not the name (see `docker::list_containers`),
//! so the uuid bought nothing and the names were impossible to remember or
//! type. These cute names are just as unique-enough for `n8 ps` / `n8 attach`
//! / `n8 agents kill <name>`, and a human can actually read them back.
//!
//! Randomness comes from a v4 UUID (already a dependency) so we don't pull in
//! `rand`. ~48 × ~48 ≈ 2.3k combinations — collisions among the handful of
//! containers alive at once are unlikely; if Docker ever rejects a duplicate
//! name the user simply re-runs.

const ADJECTIVES: &[&str] = &[
    "fun", "brave", "calm", "clever", "bold", "swift", "quiet", "lucky",
    "merry", "nimble", "proud", "sly", "spry", "wise", "zesty", "jolly",
    "keen", "lush", "mellow", "noble", "perky", "quirky", "rapid", "snug",
    "sunny", "tidy", "vivid", "witty", "zany", "amber", "azure", "coral",
    "crisp", "dusky", "fuzzy", "glossy", "hazy", "ivory", "jade", "minty",
    "olive", "rosy", "ruby", "teal", "velvet", "wily", "breezy", "cosmic",
];

const ANIMALS: &[&str] = &[
    "swan", "otter", "lynx", "fox", "owl", "wren", "crow", "hare",
    "newt", "moth", "seal", "toad", "wolf", "yak", "ibis", "kiwi",
    "lark", "mole", "puma", "quail", "raven", "shrew", "stoat", "tapir",
    "vole", "wasp", "bison", "crane", "dingo", "eel", "finch", "gecko",
    "heron", "koala", "lemur", "manta", "narwhal", "panda", "robin", "skink",
    "tern", "urchin", "viper", "weasel", "zebra", "badger", "cobra", "dove",
];

/// Generate a fresh `n8-<adjective>-<animal>` container name.
pub fn fun_name() -> String {
    let id = uuid::Uuid::new_v4();
    let b = id.as_bytes();
    // Combine two bytes per slot for a flatter distribution across the lists.
    let adj = ADJECTIVES[((b[0] as usize) << 8 | b[1] as usize) % ADJECTIVES.len()];
    let animal = ANIMALS[((b[2] as usize) << 8 | b[3] as usize) % ANIMALS.len()];
    format!("n8-{adj}-{animal}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_is_n8_adj_animal() {
        let name = fun_name();
        let parts: Vec<&str> = name.split('-').collect();
        assert_eq!(parts.len(), 3, "expected n8-<adj>-<animal>, got {name}");
        assert_eq!(parts[0], "n8");
        assert!(ADJECTIVES.contains(&parts[1]), "{} not an adjective", parts[1]);
        assert!(ANIMALS.contains(&parts[2]), "{} not an animal", parts[2]);
    }

    #[test]
    fn names_vary() {
        // Astronomically unlikely for 20 draws to all collide.
        let set: std::collections::HashSet<_> = (0..20).map(|_| fun_name()).collect();
        assert!(set.len() > 1, "generator produced no variation");
    }
}
