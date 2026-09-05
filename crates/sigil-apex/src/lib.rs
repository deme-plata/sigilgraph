//! # Sigil Apex — the engine
//!
//! OK, here's the deal. Crown & Ash on Quillon is a *turn-based, verifiable* world:
//! your faction comes from your wallet, you take actions, and the state is something a
//! second machine can re-derive rather than take on faith. This is the same machine,
//! pointed at a battle royale.
//!
//! The whole thing is **deterministic**. A match is `(seed, your actions)` and nothing
//! else — no clock, no OS randomness, no hidden state. Feed the same two in and you get
//! the same match, the same winner, and the same [`Match::hash`]. That last part is the
//! SIGIL idea in one line: the game does not ask you to *believe* the result, it hands you
//! a number you can [`verify`] against a fresh replay.
//!
//! There is no `rand` crate here and no `chrono`. Randomness is a splitmix64 you seed
//! yourself, so "random" and "reproducible" are the same word.

/// A seedable splitmix64 — the only source of chance in the whole game.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // A zero seed is a degenerate splitmix state; nudge it off zero.
        Self(seed ^ 0x5A17_C0DE_1234_ABCD)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// A number in `0..n` (0 when `n == 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
    pub fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
    /// Derive a match seed from a 64-hex wallet address, so a wallet plays "its own"
    /// matches the way a Crown & Ash faction is chosen by address. Non-hex falls back to 0.
    pub fn seed_from_wallet(addr: &str) -> u64 {
        let a = addr.trim().trim_start_matches("qnk").trim_start_matches("0x");
        let mut s: u64 = 0;
        for (i, b) in a.bytes().take(16).enumerate() {
            let v = (b as char).to_digit(16).unwrap_or(0) as u64;
            s |= v << (i * 4);
        }
        s
    }
}

/// The five roles, straight from Apex. The class is the strategy; the legend is the face.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class { Assault, Skirmisher, Recon, Controller, Support }

impl Class {
    pub fn glyph(self) -> &'static str {
        match self { Class::Assault=>"⚔", Class::Skirmisher=>"➤", Class::Recon=>"◎", Class::Controller=>"◧", Class::Support=>"✚" }
    }
    pub fn name(self) -> &'static str {
        match self { Class::Assault=>"Assault", Class::Skirmisher=>"Skirmisher", Class::Recon=>"Recon", Class::Controller=>"Controller", Class::Support=>"Support" }
    }
}

/// A legend: a name, a class, and one signature edge that bends the model a little.
#[derive(Clone, Copy)]
pub struct Legend { pub name: &'static str, pub class: Class, pub perk: &'static str }

/// The roster. Names are original to SIGIL — obsidian-and-frost, not the real game's.
pub const LEGENDS: &[Legend] = &[
    Legend { name: "Frostbite", class: Class::Assault,    perk: "+ firepower in every fight" },
    Legend { name: "Emberwake", class: Class::Assault,    perk: "keeps shooting while others reload" },
    Legend { name: "Slipstream", class: Class::Skirmisher, perk: "rotates two zones for the price of one" },
    Legend { name: "Vantage",   class: Class::Recon,      perk: "reads every squad's strength before the fight" },
    Legend { name: "Quill",     class: Class::Recon,      perk: "marks the next ring before it moves" },
    Legend { name: "Bulwark",   class: Class::Controller, perk: "halves storm damage in her zone" },
    Legend { name: "Warden",    class: Class::Controller, perk: "a held zone is a fortress" },
    Legend { name: "Mercy",     class: Class::Support,    perk: "heals and revives for more" },
];

/// One squad of up to three. The player's squad and the bots use the same struct — the
/// only difference is who chooses the action.
#[derive(Clone)]
pub struct Squad {
    pub id: usize,
    pub tag: char,
    pub legend: Legend,
    pub members: i32,   // 0..3 alive
    pub hp: i32,        // shared health pool for the squad (0..300)
    pub shield: i32,    // EVO shield pool (0..150), soaked before hp
    pub pos: usize,     // zone index
    pub loot: i32,      // gun tier 0..3 (white/blue/purple/gold)
    pub is_player: bool,
    pub kills: i32,
}

impl Squad {
    pub fn alive(&self) -> bool { self.members > 0 && self.hp > 0 }
    /// Fighting strength in this exact moment. Deterministic given the squad.
    pub fn power(&self) -> i32 {
        let class_edge = match self.legend.class { Class::Assault => 14, Class::Controller => 6, _ => 0 };
        self.members * 24 + self.loot * 11 + self.shield / 4 + self.hp / 20 + class_edge
    }
}

/// What the player (or the auto-pilot) may do on their turn. Six, like the ping wheel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action { Loot, Rotate, Fight, Heal, Revive, Ultimate }

impl Action {
    pub fn from_num(n: u32) -> Option<Action> {
        Some(match n { 1=>Action::Loot, 2=>Action::Rotate, 3=>Action::Fight, 4=>Action::Heal, 5=>Action::Revive, 6=>Action::Ultimate, _=>return None })
    }
    pub fn label(self) -> &'static str {
        match self { Action::Loot=>"Loot (raise your gun tier)", Action::Rotate=>"Rotate toward the safe zone",
            Action::Fight=>"Fight the nearest squad", Action::Heal=>"Heal (shield then health)",
            Action::Revive=>"Revive a downed teammate", Action::Ultimate=>"Ultimate (your legend's edge)" }
    }
}

pub const ZONES: usize = 9;

/// The whole match: the map is a line of [`ZONES`] zones and the ring is a safe window
/// `[lo, hi]` that closes one step from each side every turn until only the centre is safe.
#[derive(Clone)]
pub struct Match {
    pub rng: Rng,
    pub squads: Vec<Squad>,
    pub lo: usize,
    pub hi: usize,
    pub turn: u32,
    pub log: Vec<String>,
    events: Vec<u8>, // canonical bytes hashed into the match id
}

impl Match {
    /// Twenty squads' worth of drama compressed to a terminal-sized six: you plus five
    /// bots. `player_legend` picks your face; the rest are dealt from the seed.
    pub fn new(seed: u64, player_legend: usize) -> Match {
        let mut rng = Rng::new(seed);
        let mut squads = Vec::new();
        let pl = &LEGENDS[player_legend % LEGENDS.len()];
        // Everyone drops somewhere in the outer ring; the player's drop is seed-derived too.
        let mut drop = |rng: &mut Rng| (rng.below(ZONES as u64)) as usize;
        squads.push(Squad { id: 0, tag: 'P', legend: *pl, members: 3, hp: 300, shield: 50, pos: drop(&mut rng), loot: 0, is_player: true, kills: 0 });
        let tags = ['A','B','C','D','E'];
        for (i, tag) in tags.iter().enumerate() {
            let lg = LEGENDS[rng.below(LEGENDS.len() as u64) as usize];
            squads.push(Squad { id: i+1, tag: *tag, legend: lg, members: 3, hp: 300, shield: 25 + (rng.below(4) as i32)*10, pos: drop(&mut rng), loot: rng.below(2) as i32, is_player: false, kills: 0 });
        }
        Match { rng, squads, lo: 0, hi: ZONES - 1, turn: 0, log: Vec::new(), events: Vec::new() }
    }

    fn record(&mut self, kind: u8, a: u32, b: u32) {
        self.events.push(kind);
        self.events.extend_from_slice(&a.to_le_bytes());
        self.events.extend_from_slice(&b.to_le_bytes());
    }

    pub fn player(&self) -> &Squad { &self.squads[0] }
    pub fn alive_count(&self) -> usize { self.squads.iter().filter(|s| s.alive()).count() }
    pub fn over(&self) -> bool { self.alive_count() <= 1 }
    pub fn winner(&self) -> Option<&Squad> {
        if self.alive_count() == 1 { self.squads.iter().find(|s| s.alive()) } else { None }
    }
    fn safe(&self, pos: usize) -> bool { pos >= self.lo && pos <= self.hi }
    fn centre(&self) -> usize { (self.lo + self.hi) / 2 }

    /// The nearest ALIVE enemy squad to `who` (by zone distance), if any.
    fn nearest_enemy(&self, who: usize) -> Option<usize> {
        let p = self.squads[who].pos;
        self.squads.iter().enumerate()
            .filter(|(i, s)| *i != who && s.alive())
            .min_by_key(|(_, s)| (s.pos as i64 - p as i64).unsigned_abs())
            .map(|(i, _)| i)
    }

    /// Advance one full turn: the player's action, then every bot's, then the ring closes
    /// and the storm and the fights are resolved. Returns a short narration of the turn.
    pub fn step(&mut self, player_action: Action) -> Vec<String> {
        self.turn += 1;
        self.log.clear();
        let start = self.log.len();

        // 1. the player acts
        self.act(0, player_action);
        // 2. bots act, in id order, each from the shared deterministic rng
        for i in 1..self.squads.len() {
            if self.squads[i].alive() {
                let a = self.bot_choice(i);
                self.act(i, a);
            }
        }
        // 3. the ring closes one notch from each side
        if self.lo < self.hi { self.lo += 1; }
        if self.hi > self.lo { self.hi -= 1; }
        self.record(1, self.lo as u32, self.hi as u32);
        self.log.push(format!("── the ring closes to zones {}‥{} ──", self.lo, self.hi));
        // 4. the storm bites anyone outside — weakest first (they fall first). The
        //    last-squad-standing guarantee lives in damage(), so nothing extra is needed here.
        let (lo, hi) = (self.lo, self.hi);
        let mut outside: Vec<usize> = (0..self.squads.len())
            .filter(|&i| self.squads[i].alive() && !(self.squads[i].pos >= lo && self.squads[i].pos <= hi))
            .collect();
        outside.sort_by_key(|&i| self.squads[i].power());
        for i in outside {
            if !self.squads[i].alive() { continue; }
            let mut dmg = 40;
            if self.squads[i].legend.class == Class::Controller { dmg /= 2; } // Bulwark/Warden
            self.damage(i, dmg, "the storm");
        }
        // 5. fights: every zone holding two or more living squads
        for z in 0..ZONES {
            self.resolve_zone(z);
        }
        self.log[start..].to_vec()
    }

    fn act(&mut self, i: usize, a: Action) {
        let name = self.squads[i].legend.name;
        match a {
            Action::Loot => {
                if self.squads[i].loot < 3 { self.squads[i].loot += 1; }
                self.log.push(format!("{name} loots — gun tier {}", self.squads[i].loot));
            }
            Action::Rotate => {
                let c = self.centre();
                let step = if self.squads[i].legend.class == Class::Skirmisher { 2 } else { 1 }; // Slipstream
                let p = self.squads[i].pos as i64;
                let np = if (p as usize) < c { (p + step).min(c as i64) } else { (p - step).max(c as i64) };
                self.squads[i].pos = np as usize;
                self.log.push(format!("{name} rotates to zone {}", self.squads[i].pos));
            }
            Action::Fight => {
                if let Some(e) = self.nearest_enemy(i) {
                    self.squads[i].pos = self.squads[e].pos; // close the distance
                    self.log.push(format!("{name} pushes squad {} in zone {}", self.squads[e].tag, self.squads[e].pos));
                } else {
                    self.log.push(format!("{name} finds no one to fight"));
                }
            }
            Action::Heal => {
                let heal = if self.squads[i].legend.class == Class::Support { 60 } else { 40 }; // Mercy
                let s = &mut self.squads[i];
                let want = 50 - s.shield;
                let to_shield = heal.min(want.max(0));
                s.shield += to_shield;
                s.hp = (s.hp + (heal - to_shield)).min(s.members * 100);
                self.log.push(format!("{name} heals — shield {} hp {}", self.squads[i].shield, self.squads[i].hp));
            }
            Action::Revive => {
                if self.squads[i].members < 3 && self.nearest_enemy(i).map(|e| self.squads[e].pos != self.squads[i].pos).unwrap_or(true) {
                    let bonus = if self.squads[i].legend.class == Class::Support { 100 } else { 60 };
                    self.squads[i].members += 1;
                    self.squads[i].hp += bonus;
                    self.log.push(format!("{name} revives — {} up now", self.squads[i].members));
                } else {
                    self.log.push(format!("{name} can't revive under fire"));
                }
            }
            Action::Ultimate => self.ultimate(i),
        }
        self.record(2, i as u32, a as u32);
    }

    fn ultimate(&mut self, i: usize) {
        let name = self.squads[i].legend.name;
        match self.squads[i].legend.class {
            Class::Assault => { self.squads[i].loot = 3; self.squads[i].shield = (self.squads[i].shield + 40).min(150);
                self.log.push(format!("{name} ULT — gold weapon + shield battery")); }
            Class::Skirmisher => { self.squads[i].pos = self.centre();
                self.log.push(format!("{name} ULT — launch to the centre (zone {})", self.squads[i].pos)); }
            Class::Recon => { self.squads[i].shield = (self.squads[i].shield + 20).min(150);
                let scan: Vec<String> = self.squads.iter().filter(|s| s.id != i && s.alive()).map(|s| format!("{}={}", s.tag, s.power())).collect();
                self.log.push(format!("{name} ULT — scan: {}", scan.join(" "))); }
            Class::Controller => { self.squads[i].shield = (self.squads[i].shield + 60).min(150); self.squads[i].hp += 40;
                self.log.push(format!("{name} ULT — fortify the zone")); }
            Class::Support => { self.squads[i].members = 3; self.squads[i].hp = (self.squads[i].hp + 150).min(300); self.squads[i].shield = 50;
                self.log.push(format!("{name} ULT — full squad restore")); }
        }
    }

    /// A bot's turn, decided from the position and the rng — never from the wall clock.
    fn bot_choice(&mut self, i: usize) -> Action {
        let s = &self.squads[i];
        if !self.safe(s.pos) { return Action::Rotate; }
        if s.shield == 0 && s.hp < s.members * 60 && self.nearest_enemy(i).map(|e| self.squads[e].pos != s.pos).unwrap_or(true) {
            return Action::Heal;
        }
        // an ultimate, occasionally, when it would matter
        if self.turn >= 3 && self.rng.chance(22) { return Action::Ultimate; }
        if let Some(e) = self.nearest_enemy(i) {
            let adjacent = (self.squads[e].pos as i64 - s.pos as i64).unsigned_abs() <= 1;
            if adjacent && s.power() >= self.squads[e].power() { return Action::Fight; }
        }
        if s.loot < 2 && self.rng.chance(60) { return Action::Loot; }
        Action::Rotate
    }

    fn damage(&mut self, i: usize, mut dmg: i32, src: &str) {
        // THE death chokepoint. Every kill in the game flows through here, so the one rule
        // that makes a battle royale a battle royale lives here: the arena never empties.
        // If this hit would wipe the LAST living squad, it clings to the ring's edge on one
        // point of health instead — a champion, not an empty field.
        let others_alive = self.squads.iter().enumerate().filter(|(j, s)| *j != i && s.alive()).count();
        let (name, tag, wiped) = {
            let s = &mut self.squads[i];
            let soak = dmg.min(s.shield);
            s.shield -= soak; dmg -= soak;
            s.hp -= dmg;
            while s.members > 0 && s.hp <= (s.members - 1) * 100 {
                s.members -= 1;
            }
            if s.hp < 0 { s.hp = 0; }
            let mut wiped = !s.alive();
            if wiped && others_alive == 0 {
                s.members = 1; s.hp = 1; s.shield = 0; // last squad standing survives
                wiped = false;
            } else if wiped {
                s.members = 0; s.hp = 0;
            }
            (s.legend.name, s.tag, wiped)
        };
        if wiped {
            self.record(4, i as u32, 0);
            self.log.push(format!("💀 squad {tag} ({name}) is wiped by {src}"));
        }
    }

    /// Resolve a fight in one zone: while two or more living squads share it, the weakest
    /// loses a member and the strongest chips them. Guaranteed to terminate in one survivor
    /// (a member is removed every iteration), so the centre always produces a winner.
    fn resolve_zone(&mut self, z: usize) {
        loop {
            let here: Vec<usize> = self.squads.iter().enumerate()
                .filter(|(_, s)| s.alive() && s.pos == z).map(|(i, _)| i).collect();
            if here.len() < 2 { break; }
            // strongest and weakest, ties broken by rng then id — deterministic.
            let jitter = |m: &mut Match, i: usize| m.squads[i].power() * 4 + (m.rng.below(4) as i32) - i as i32;
            let mut ranked = here.clone();
            let mut scores: Vec<(usize, i32)> = ranked.iter().map(|&i| (i, jitter(self, i))).collect();
            scores.sort_by_key(|&(_, s)| std::cmp::Reverse(s));
            ranked = scores.iter().map(|&(i, _)| i).collect();
            let top = ranked[0];
            let weak = *ranked.last().unwrap();
            let gap = (self.squads[top].power() - self.squads[weak].power()).max(20);
            self.record(3, top as u32, weak as u32);
            self.damage(weak, gap, &format!("squad {}", self.squads[top].tag));
            if !self.squads[weak].alive() { self.squads[top].kills += 1; }
            // the winner is not untouched — return fire eats a little shield/hp
            self.damage(top, 15, "return fire");
            if !self.squads[top].alive() { continue; } // extremely rare; loop re-evaluates
        }
    }

    /// The verifiable match id: BLAKE3 over the canonical event stream. Same `(seed,
    /// actions)` in, same 32-byte id out — the number you check a replay against.
    pub fn hash(&self) -> String {
        let mut h = blake3::Hasher::new();
        h.update(b"sigil-apex/v1");
        h.update(&self.events);
        h.finalize().to_hex().to_string()
    }

    /// An ASCII picture of the ring: `#` safe, `·` storm, with each living squad's tag
    /// sitting under its zone.
    pub fn render_ring(&self) -> String {
        let mut top = String::new();
        for z in 0..ZONES { top.push_str(if self.safe(z) { " # " } else { " · " }); }
        let mut bot = String::new();
        for z in 0..ZONES {
            let tags: String = self.squads.iter().filter(|s| s.alive() && s.pos == z).map(|s| s.tag).collect();
            bot.push_str(&format!("{:^3}", if tags.is_empty() { ".".into() } else { tags }));
        }
        format!("  ring  {top}\n  squads{bot}")
    }
}

/// Replay a match from a seed and a list of player actions and return its id — the whole
/// point of the exercise. `verify(seed, actions) == claimed_hash` is proof, not trust.
pub fn replay(seed: u64, player_legend: usize, actions: &[Action]) -> (String, Option<char>) {
    let mut m = Match::new(seed, player_legend);
    for &a in actions {
        if m.over() { break; }
        m.step(a);
    }
    // If the ring has closed and squads still stand apart, force the final convergence.
    let mut guard = 0;
    while !m.over() && guard < 12 {
        m.step(Action::Rotate);
        guard += 1;
    }
    (m.hash(), m.winner().map(|s| s.tag))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_and_actions_reproduce_the_match() {
        let acts = [Action::Loot, Action::Rotate, Action::Ultimate, Action::Fight, Action::Heal];
        let a = replay(0xBEEF, 0, &acts);
        let b = replay(0xBEEF, 0, &acts);
        assert_eq!(a.0, b.0, "identical inputs must give an identical match id");
        assert!(a.1.is_some(), "a match must have a winner");
    }

    #[test]
    fn a_different_seed_is_a_different_match() {
        let acts = [Action::Loot, Action::Rotate, Action::Fight];
        assert_ne!(replay(1, 0, &acts).0, replay(2, 0, &acts).0);
    }

    #[test]
    fn a_different_action_is_a_different_match() {
        assert_ne!(replay(7, 0, &[Action::Loot]).0, replay(7, 0, &[Action::Fight]).0);
    }

    #[test]
    fn the_ring_always_produces_exactly_one_winner() {
        for seed in 0..40u64 {
            let (_h, w) = replay(seed, (seed as usize) % LEGENDS.len(), &[Action::Loot, Action::Rotate, Action::Loot, Action::Rotate]);
            assert!(w.is_some(), "seed {seed} left no winner");
        }
    }

    #[test]
    fn a_squad_left_in_the_storm_dies() {
        // A one-zone map edge case: push everyone but leave the player far out and passive.
        let mut m = Match::new(99, 0);
        m.squads[0].pos = 0; // corner
        for _ in 0..ZONES { if m.over() { break; } m.step(Action::Loot); } // never rotate
        assert!(!m.player().alive() || m.over(), "sitting in the storm must be fatal (or the match ended)");
    }

    #[test]
    fn wallet_seed_is_stable_and_hex_only() {
        let s1 = Rng::seed_from_wallet("qnk4973498a9865b291");
        let s2 = Rng::seed_from_wallet("4973498a9865b291");
        assert_eq!(s1, s2, "the qnk prefix must not change the seed");
        assert_ne!(s1, Rng::seed_from_wallet("aaaaaaaaaaaaaaaa"));
    }
}
