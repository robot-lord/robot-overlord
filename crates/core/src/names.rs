//! Robot-themed tmux session names: `rlord-<adjective>-<part>`
//! (`rlord-rusty-sprocket`), in the spirit of Docker's adjective_surname.
//! Short and friendly in status bars and terminal titles; unique across
//! every session rlord knows and every tmux session on its server (the
//! caller supplies the collision check).
//!
//! Sizing: 85 × 85 = 7225 names. With ~100 live sessions a fresh pick
//! collides ~1.4% of the time, and a collision only costs a retry.

/// Machinery-flavoured adjectives: materials, sounds, finishes, tech.
const ADJECTIVES: &[&str] = &[
    "alloy",
    "analog",
    "atomic",
    "beeping",
    "binary",
    "blinking",
    "blocky",
    "boxy",
    "brass",
    "bronze",
    "buzzing",
    "calibrated",
    "chrome",
    "clanky",
    "clicking",
    "clockwork",
    "clunky",
    "cobalt",
    "copper",
    "cordless",
    "creaky",
    "dented",
    "diesel",
    "digital",
    "electric",
    "forged",
    "fused",
    "galvanic",
    "geared",
    "gilded",
    "gleaming",
    "glowing",
    "golden",
    "greasy",
    "grimy",
    "humming",
    "hydraulic",
    "ionic",
    "iron",
    "laser",
    "looping",
    "magnetic",
    "metallic",
    "micro",
    "modular",
    "nano",
    "neon",
    "nickel",
    "oily",
    "patched",
    "pixel",
    "plasma",
    "pneumatic",
    "polished",
    "pulsing",
    "quantum",
    "retro",
    "riveted",
    "rugged",
    "rusty",
    "salvaged",
    "scrappy",
    "shiny",
    "silver",
    "smoky",
    "solar",
    "sonic",
    "sooty",
    "sparky",
    "squeaky",
    "stamped",
    "static",
    "steamy",
    "steel",
    "sturdy",
    "tinny",
    "titanium",
    "tuned",
    "tungsten",
    "turbo",
    "vintage",
    "welded",
    "whirring",
    "wired",
    "zinc",
];

/// Machine parts. Never "robot" or its synonyms: everything already says
/// robot overlord (see `BANNED`).
const NOUNS: &[&str] = &[
    "actuator",
    "antenna",
    "anvil",
    "axle",
    "battery",
    "beacon",
    "bearing",
    "bellows",
    "boiler",
    "bolt",
    "bracket",
    "bumper",
    "button",
    "cable",
    "caliper",
    "camshaft",
    "capacitor",
    "caster",
    "chassis",
    "chip",
    "circuit",
    "claw",
    "cog",
    "coil",
    "crank",
    "cylinder",
    "diode",
    "dial",
    "drill",
    "dynamo",
    "flange",
    "flywheel",
    "fuse",
    "gadget",
    "gasket",
    "gauge",
    "gear",
    "girder",
    "gimbal",
    "gizmo",
    "gripper",
    "gyro",
    "hinge",
    "ingot",
    "joint",
    "knob",
    "lathe",
    "lens",
    "lever",
    "magnet",
    "motor",
    "nozzle",
    "nut",
    "piston",
    "pincer",
    "plug",
    "pulley",
    "pylon",
    "radar",
    "ratchet",
    "relay",
    "resistor",
    "rivet",
    "rotor",
    "sensor",
    "servo",
    "socket",
    "solder",
    "spanner",
    "spring",
    "sprocket",
    "stator",
    "strut",
    "switch",
    "thruster",
    "toggle",
    "tread",
    "turbine",
    "valve",
    "visor",
    "washer",
    "wheel",
    "widget",
    "winch",
    "wrench",
];

/// Words the lists must never contain.
#[cfg(test)]
const BANNED: &[&str] = &[
    "robot",
    "bot",
    "droid",
    "android",
    "automaton",
    "cyborg",
    "mech",
    "golem",
    "overlord",
];

/// Random index source without a `rand` dependency.
fn random_u64() -> u64 {
    let b = uuid::Uuid::new_v4().into_bytes();
    u64::from_le_bytes(b[..8].try_into().expect("8 bytes"))
}

/// A fresh `rlord-adjective-noun` name for which `taken` is false. After a
/// run of collisions a numeric suffix keeps it unique however full the space.
pub fn tmux_name(taken: impl Fn(&str) -> bool) -> String {
    let pick = || {
        let r = random_u64();
        let a = ADJECTIVES[(r % ADJECTIVES.len() as u64) as usize];
        let n = NOUNS[((r >> 32) % NOUNS.len() as u64) as usize];
        format!("rlord-{a}-{n}")
    };
    for _ in 0..64 {
        let name = pick();
        if !taken(&name) {
            return name;
        }
    }
    let base = pick();
    // Bounded: if `taken` says yes to everything, give up on uniqueness
    // rather than spin (the id suffix keeps it distinct in practice).
    (2..10_000)
        .map(|i| format!("{base}-{i}"))
        .find(|n| !taken(n))
        .unwrap_or_else(|| format!("{base}-{}", random_u64() % 1_000_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_kebab_and_avoid_collisions() {
        let n = tmux_name(|_| false);
        let parts: Vec<&str> = n.split('-').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "rlord");
        assert!(ADJECTIVES.contains(&parts[1]) && NOUNS.contains(&parts[2]));
        // Everything plain is taken: falls back to a numbered name.
        let n = tmux_name(|n| n.split('-').count() == 3);
        assert!(n.ends_with("-2"), "{n}");
    }

    #[test]
    fn no_banned_words_and_big_enough() {
        for w in ADJECTIVES.iter().chain(NOUNS) {
            assert!(!BANNED.contains(w), "{w}");
            assert!(w.chars().all(|c| c.is_ascii_lowercase()), "{w}");
        }
        assert!(ADJECTIVES.len() * NOUNS.len() >= 6000);
    }

    #[test]
    fn word_lists_have_no_duplicates() {
        for list in [ADJECTIVES, NOUNS] {
            let mut v = list.to_vec();
            v.sort();
            v.dedup();
            assert_eq!(v.len(), list.len());
        }
    }
}
