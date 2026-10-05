//! [`DecisionCache`]: a classifier's recent answers, kept in memory so an
//! identical input is routed without calling the classifier again.
//!
//! The cache is per gateway instance; instances warm independently. An entry
//! is keyed by a SHA-256 digest of everything the answer depends on, so no
//! request text is stored.

use std::time::Duration;

use moka::sync::Cache;
use prometheus::Counter;
use sha2::{Digest, Sha256};

use super::{Classification, ClassifierInput, Tier};
use crate::telemetry::metrics::{CLASSIFIER_CACHE_HITS_TOTAL, CLASSIFIER_CACHE_MISSES_TOTAL};

/// Digest of an alias's configuration and one request's input.
pub(super) type Key = [u8; 32];

/// The longest TTL the cache is built with. Anything longer is a cache that
/// never expires within a process's lifetime anyway, and `moka` refuses a TTL
/// beyond 1000 years.
const MAX_TTL: Duration = Duration::from_secs(100 * 365 * 24 * 60 * 60);

/// One classifier's cached answers, shared by every alias that uses it.
pub(super) struct DecisionCache {
    entries: Cache<Key, Classification>,
    hits: Counter,
    misses: Counter,
}

impl DecisionCache {
    /// The cache of classifier `classifier_id`, or `None` when `ttl` or
    /// `max_entries` is zero: the classifier then has no cache at all.
    pub(super) fn new(classifier_id: &str, ttl: Duration, max_entries: u64) -> Option<Self> {
        if ttl.is_zero() || max_entries == 0 {
            return None;
        }
        let entries = Cache::builder()
            .max_capacity(max_entries)
            .time_to_live(ttl.min(MAX_TTL))
            .build();
        // Resolved once, so a lookup costs no label hashing.
        let labels = &[classifier_id];
        Some(Self {
            entries,
            hits: CLASSIFIER_CACHE_HITS_TOTAL.with_label_values(labels),
            misses: CLASSIFIER_CACHE_MISSES_TOTAL.with_label_values(labels),
        })
    }

    /// The answer stored under `key`, if it has not expired. Counts the hit
    /// or the miss.
    pub(super) fn get(&self, key: &Key) -> Option<Classification> {
        let answer = self.entries.get(key);
        match answer {
            Some(_) => self.hits.inc(),
            None => self.misses.inc(),
        }
        answer
    }

    /// Store `answer` under `key`. Nothing was billed for a later hit, so
    /// the stored answer carries no input tokens.
    pub(super) fn insert(&self, key: Key, mut answer: Classification) {
        answer.input_tokens = 0;
        self.entries.insert(key, answer);
    }

    /// Apply pending evictions, then count the entries held.
    #[cfg(test)]
    fn settled_len(&self) -> u64 {
        self.entries.run_pending_tasks();
        self.entries.entry_count()
    }
}

/// Feed one field into `hasher`, prefixed with its length so that adjacent
/// fields cannot run into each other (`"ab", "c"` vs `"a", "bc"`).
fn write(hasher: &mut Sha256, field: &str) {
    write_len(hasher, field.len());
    hasher.update(field.as_bytes());
}

fn write_len(hasher: &mut Sha256, len: usize) {
    hasher.update((len as u64).to_le_bytes());
}

/// The part of a key that depends only on configuration: the requested
/// alias, the classifier and its model, and the alias's tiers. Hashed once at
/// startup; a request continues from a clone of it.
pub(super) fn key_prefix(alias: &str, classifier_id: &str, model: &str, tiers: &[Tier]) -> Sha256 {
    let mut hasher = Sha256::new();
    write(&mut hasher, alias);
    write(&mut hasher, classifier_id);
    write(&mut hasher, model);
    write_len(&mut hasher, tiers.len());
    for tier in tiers {
        write(&mut hasher, &tier.name);
        write(&mut hasher, &tier.when);
    }
    hasher
}

/// The key of `input` for the alias `prefix` was built for.
pub(super) fn key(prefix: &Sha256, input: &ClassifierInput) -> Key {
    let mut hasher = prefix.clone();
    write_len(&mut hasher, input.turns.len());
    for turn in &input.turns {
        write(&mut hasher, turn.role.as_str());
        write(&mut hasher, &turn.text);
    }
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classifier::input::{Role, Turn};
    use crate::classifier::test_support::answer;

    fn tiers(tiers: &[(&str, &str)]) -> Vec<Tier> {
        tiers
            .iter()
            .map(|(name, when)| Tier {
                name: name.to_string(),
                when: when.to_string(),
            })
            .collect()
    }

    fn input(turns: &[(Role, &str)]) -> ClassifierInput {
        ClassifierInput {
            turns: turns
                .iter()
                .map(|(role, text)| Turn {
                    role: *role,
                    text: text.to_string(),
                })
                .collect(),
        }
    }

    const TIERS: &[(&str, &str)] = &[("simple", "Short chat"), ("complex", "Reasoning")];
    const TURNS: &[(Role, &str)] = &[(Role::User, "hi"), (Role::Assistant, "hello")];

    fn key_of(
        alias: &str,
        classifier_id: &str,
        model: &str,
        tier_list: &[(&str, &str)],
        turns: &[(Role, &str)],
    ) -> Key {
        let prefix = key_prefix(alias, classifier_id, model, &tiers(tier_list));
        key(&prefix, &input(turns))
    }

    fn base() -> Key {
        key_of("auto", "jev", "jev-latest", TIERS, TURNS)
    }

    #[test]
    fn equal_inputs_have_equal_keys() {
        assert_eq!(base(), base());
    }

    #[test]
    fn every_part_of_the_key_changes_it() {
        let cases = [
            ("alias", key_of("auto-2", "jev", "jev-latest", TIERS, TURNS)),
            (
                "classifier id",
                key_of("auto", "jev-2", "jev-latest", TIERS, TURNS),
            ),
            ("model", key_of("auto", "jev", "jev-2", TIERS, TURNS)),
            (
                "tier name",
                key_of(
                    "auto",
                    "jev",
                    "jev-latest",
                    &[("easy", "Short chat"), ("complex", "Reasoning")],
                    TURNS,
                ),
            ),
            (
                "tier when",
                key_of(
                    "auto",
                    "jev",
                    "jev-latest",
                    &[("simple", "Short chat."), ("complex", "Reasoning")],
                    TURNS,
                ),
            ),
            (
                "tier removed",
                key_of("auto", "jev", "jev-latest", &TIERS[..1], TURNS),
            ),
            (
                "turn text",
                key_of(
                    "auto",
                    "jev",
                    "jev-latest",
                    TIERS,
                    &[(Role::User, "hi!"), (Role::Assistant, "hello")],
                ),
            ),
            (
                "turn role",
                key_of(
                    "auto",
                    "jev",
                    "jev-latest",
                    TIERS,
                    &[(Role::User, "hi"), (Role::User, "hello")],
                ),
            ),
            (
                "turn removed",
                key_of("auto", "jev", "jev-latest", TIERS, &TURNS[..1]),
            ),
        ];
        for (part, changed) in cases {
            assert_ne!(changed, base(), "{part}");
        }
    }

    /// Text moved across a field boundary is a different key.
    #[test]
    fn adjacent_fields_cannot_be_confused() {
        let pairs = [
            (
                key_of("ab", "c", "m", TIERS, TURNS),
                key_of("a", "bc", "m", TIERS, TURNS),
            ),
            (
                key_of("a", "bc", "d", TIERS, TURNS),
                key_of("a", "b", "cd", TIERS, TURNS),
            ),
            (
                key_of("a", "b", "m", &[("ab", "c")], TURNS),
                key_of("a", "b", "m", &[("a", "bc")], TURNS),
            ),
            (
                key_of(
                    "a",
                    "b",
                    "m",
                    TIERS,
                    &[(Role::User, "ab"), (Role::User, "c")],
                ),
                key_of(
                    "a",
                    "b",
                    "m",
                    TIERS,
                    &[(Role::User, "a"), (Role::User, "bc")],
                ),
            ),
            // One turn is not two turns with the same text between them.
            (
                key_of("a", "b", "m", TIERS, &[(Role::User, "ab")]),
                key_of(
                    "a",
                    "b",
                    "m",
                    TIERS,
                    &[(Role::User, "a"), (Role::User, "b")],
                ),
            ),
        ];
        for (i, (left, right)) in pairs.into_iter().enumerate() {
            assert_ne!(left, right, "pair {i}");
        }
    }

    #[test]
    fn a_zero_ttl_or_a_zero_size_is_no_cache() {
        assert!(DecisionCache::new("cache-off", Duration::ZERO, 10).is_none());
        assert!(DecisionCache::new("cache-off", Duration::from_secs(1), 0).is_none());
        assert!(DecisionCache::new("cache-off", Duration::from_secs(1), 1).is_some());
        // Longer than `moka` accepts: clamped, not a panic at startup.
        assert!(DecisionCache::new("cache-off", Duration::from_secs(u64::MAX), 1).is_some());
    }

    #[test]
    fn a_stored_answer_is_returned_without_its_billed_tokens() {
        let cache = DecisionCache::new("cache-roundtrip", Duration::from_secs(60), 10).unwrap();
        let stored = answer("simple", Some(0.9));
        assert_ne!(stored.input_tokens, 0);

        assert_eq!(cache.get(&base()), None);
        cache.insert(base(), stored.clone());

        let hit = cache.get(&base()).unwrap();
        assert_eq!(hit.input_tokens, 0);
        assert_eq!(
            hit,
            Classification {
                input_tokens: 0,
                ..stored
            }
        );
    }

    #[test]
    fn lookups_are_counted_per_classifier() {
        let id = "cache-counted";
        let cache = DecisionCache::new(id, Duration::from_secs(60), 10).unwrap();
        let hits = || CLASSIFIER_CACHE_HITS_TOTAL.with_label_values(&[id]).get();
        let misses = || CLASSIFIER_CACHE_MISSES_TOTAL.with_label_values(&[id]).get();

        assert_eq!(cache.get(&base()), None);
        cache.insert(base(), answer("simple", None));
        assert!(cache.get(&base()).is_some());
        assert!(cache.get(&base()).is_some());

        assert_eq!((hits(), misses()), (2.0, 1.0));
    }

    #[test]
    fn an_entry_is_not_returned_after_its_ttl() {
        let cache = DecisionCache::new("cache-ttl", Duration::from_millis(40), 10).unwrap();
        cache.insert(base(), answer("simple", Some(0.9)));
        assert!(cache.get(&base()).is_some());

        std::thread::sleep(Duration::from_millis(80));

        assert_eq!(cache.get(&base()), None);
    }

    #[test]
    fn the_cache_never_holds_more_than_its_limit() {
        let cache = DecisionCache::new("cache-bound", Duration::from_secs(60), 5).unwrap();
        for i in 0..200 {
            let text = format!("input {i}");
            let key = key_of("auto", "jev", "jev-latest", TIERS, &[(Role::User, &text)]);
            cache.insert(key, answer("simple", Some(0.9)));
        }

        let held = cache.settled_len();
        assert!((1..=5).contains(&held), "{held} entries held");
    }
}
