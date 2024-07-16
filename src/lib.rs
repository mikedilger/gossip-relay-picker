//! The main type here is `RelayPicker`. You will need to implement `RelayPickerHooks`
//! and then create a `RelayPicker::new(hooks)` with those hooks.
//!
//! If you instantiate `RelayPicker` via `Default::default()`, for example in a lazy_static type
//! setup, and then make changes which cause the Hooks to return something different than they
//! did when they were created with `Default::default()` (e.g. global variable changes), then you
//! might need to run `RelayPicker::init()` to reinitialize it with actual data.

use async_trait::async_trait;
use dashmap::DashMap;
pub use nostr_types::{PublicKey, RelayUrl, RelayListUsage, Unixtime};
use thiserror::Error;

/// Errors the RelayPicker functions can return
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// No relays to pick from
    #[error("No relays to pick from")]
    NoRelays,

    /// No people left to assign. A good result.
    #[error("All people accounted for.")]
    NoPeopleLeft,

    /// No progress was made. A stuck result.
    #[error("Unable to make further progress.")]
    NoProgress,

    /// General error
    #[error("Error: {0}")]
    General(String),
}

/// A RelayAssignment is a record of a relay which is serving (or will serve) the general
/// feed for a set of public keys.
#[derive(Debug, Clone)]
pub struct RelayAssignment {
    /// The URL of the relay
    pub relay_url: RelayUrl,

    /// The public keys assigned to the relay
    pub pubkeys: Vec<PublicKey>,
}

impl RelayAssignment {
    pub fn merge_in(&mut self, other: RelayAssignment) -> Result<(), Error> {
        if self.relay_url != other.relay_url {
            return Err(Error::General(
                "Attempted to merge relay assignments on different relays".to_owned(),
            ));
        }
        self.pubkeys.extend(other.pubkeys);
        Ok(())
    }
}

/// These are functions that need to be provided to the Relay Picker
#[async_trait]
pub trait RelayPickerHooks: Send + Sync {
    type Error: std::fmt::Display;

    /// Returns all relays available to be connected to
    fn get_all_relays(&self) -> Vec<RelayUrl>;

    /// Returns the best relays that this public key uses in the given RelayListUsage,
    /// in order of score from highest to lowest, along with the score.
    ///
    /// This is async and returns `Result<Vec<(RelayUrl, u64)>, Self::Error>`
    /// Documentation may show the raw type
    async fn get_relays_for_pubkey(
        &self,
        pubkey: PublicKey,
        usage: RelayListUsage,
    ) -> Result<Vec<(RelayUrl, u64)>, Self::Error>;

    /// Is the relay currently connected?
    fn is_relay_connected(&self, relay: &RelayUrl) -> bool;

    /// Returns the maximum number of relays that should be connected to at one time
    fn get_max_relays(&self) -> usize;

    /// Returns the number of relays each followed person's events should be pulled from
    /// Many people use 2 or 3 for redundancy.
    fn get_num_relays_per_person(&self) -> usize;

    /// Returns the public keys of all the people followed
    fn get_followed_pubkeys(&self) -> Vec<PublicKey>;

    /// Adjusts the score for a given relay, perhaps based on relay-specific metrics
    fn adjust_score(&self, relay: RelayUrl, score: u64) -> u64;
}

/// The RelayPicker is a structure that helps assign people we follow to relays we watch.
/// It remembers which publickeys are assigned to which relays, which pubkeys need more
/// relays and how many, which relays need a time out, and person-relay scores for making
/// good assignments dynamically.
#[derive(Debug, Default)]
pub struct RelayPicker<H: RelayPickerHooks + Default> {
    /// Hooks you provide to the Relay Picker
    hooks: H,

    /// A ranking of relays per person.
    person_relay_scores: DashMap<PublicKey, Vec<(RelayUrl, u64)>>,

    /// All of the relays currently connected, with their assignment.
    relay_assignments: DashMap<RelayUrl, RelayAssignment>,

    /// Relays which recently failed and which require a timeout before
    /// they can be chosen again. The value is the time when it can be removed
    /// from this list.
    excluded_relays: DashMap<RelayUrl, i64>,

    /// For each followed pubkey that still needs assignments, the number of relay
    /// assignments it is seeking.  These start out at get_num_relays_per_person()
    /// (if the person doesn't have that many relays, it will do the best it can)
    pubkey_counts: DashMap<PublicKey, usize>,
}

impl<H: RelayPickerHooks + Default> RelayPicker<H> {
    /// Create a new Relay Picker
    pub async fn new(hooks: H) -> Result<RelayPicker<H>, Error> {
        let rp = RelayPicker {
            hooks,
            ..Default::default()
        };

        rp.refresh_person_relay_scores_inner(true).await?;

        Ok(rp)
    }

    /// Re-initialize an existing Relay Picker
    /// This is useful if you created the RelayPicker from Default (e.g. in lazy static)
    pub async fn init(&self) -> Result<(), Error> {
        self.relay_assignments.clear();
        self.excluded_relays.clear();
        self.pubkey_counts.clear();
        self.person_relay_scores.clear();

        self.refresh_person_relay_scores_inner(true).await?;

        Ok(())
    }

    /// Add a public key
    pub fn add_someone(&self, pubkey: PublicKey) -> Result<(), Error> {
        if self.pubkey_counts.get(&pubkey).is_some() {
            // We already know they need relays
            return Ok(());
        }
        for elem in self.relay_assignments.iter() {
            let assignment = elem.value();
            if assignment.pubkeys.contains(&pubkey) {
                // We already assigned them to some relays
                return Ok(());
            }
        }

        // Add that the need relays
        self.pubkey_counts
            .insert(pubkey, self.hooks.get_num_relays_per_person());

        Ok(())
    }

    /// Remove a public key
    pub fn remove_someone(&self, pubkey: PublicKey) {
        // Remove from pubkey counts
        self.pubkey_counts.remove(&pubkey);

        // Remove from relay assignments
        for mut elem in self.relay_assignments.iter_mut() {
            let assignment = elem.value_mut();
            if let Some(pos) = assignment.pubkeys.iter().position(|x| x == &pubkey) {
                assignment.pubkeys.swap_remove(pos);
                // relay assignment may have zero pubkeys at this point, but
                // garbage collect will figure that out.
            }
        }
    }

    /// Garbage Collect
    /// This removes pubkeys that are no longer in get_followed_pubkeys(), and returns
    /// the relays whose assignments have become empty
    pub async fn garbage_collect(&self) -> Result<Vec<RelayUrl>, Error> {
        let mut idle: Vec<RelayUrl> = Vec::new();

        let mut followed: Vec<PublicKey> = self.hooks.get_followed_pubkeys();

        // Sort so we can use binary search
        followed.sort();

        for mut elem in self.relay_assignments.iter_mut() {
            let assignment = elem.value_mut();

            // Remove all pubkeys we no longer follow
            let mut index = 0;
            while let Some(key) = assignment.pubkeys.get(index) {
                if followed.binary_search(key).is_err() {
                    // that key is not followed.
                    assignment.pubkeys.swap_remove(index);
                    // don't bump index, it is now the next one slid back.
                } else {
                    index += 1;
                }
            }

            // If assignment is now empty, save as an idle relay
            if assignment.pubkeys.is_empty() {
                idle.push(assignment.relay_url.clone());
            }
        }

        Ok(idle)
    }

    /// Refresh the person relay scores from the hook function
    pub async fn refresh_person_relay_scores(&self) -> Result<(), Error> {
        self.refresh_person_relay_scores_inner(false).await
    }

    // Refresh person relay scores.
    async fn refresh_person_relay_scores_inner(
        &self,
        initialize_counts: bool,
    ) -> Result<(), Error> {
        self.person_relay_scores.clear();

        if initialize_counts {
            self.pubkey_counts.clear();
        }

        // Get all the people we follow
        let pubkeys: Vec<PublicKey> = self.hooks.get_followed_pubkeys();

        // Compute scores for each person_relay pairing
        for pubkey in &pubkeys {
            let best_relays: Vec<(RelayUrl, u64)> = self
                .hooks
                .get_relays_for_pubkey(pubkey.to_owned(), RelayListUsage::Outbox)
                .await
                .map_err(|e| Error::General(format!("{e}")))?;
            self.person_relay_scores.insert(*pubkey, best_relays);

            if initialize_counts {
                self.pubkey_counts
                    .insert(*pubkey, self.hooks.get_num_relays_per_person());
            }
        }

        Ok(())
    }

    /// When a relay disconnects, call this so that whatever assignments it might have
    /// had can be reassigned.  Then call `pick_relays()` again.
    pub fn relay_disconnected(&self, url: &RelayUrl, penalty_seconds: i64) {
        if penalty_seconds > 0 {
            // Exclude the relay for a period
            let hence = Unixtime::now().0 + penalty_seconds;
            self.excluded_relays.insert(url.to_owned(), hence);
            tracing::debug!(
                "{} goes into the penalty box for {} seconds until {}",
                url,
                penalty_seconds,
                hence
            );
        }

        // Remove from connected relays list
        if let Some((_key, assignment)) = self.relay_assignments.remove(url) {
            // Put the public keys back into pubkey_counts
            for pubkey in assignment.pubkeys.iter() {
                self.pubkey_counts
                    .entry(pubkey.to_owned())
                    .and_modify(|e| *e += 1)
                    .or_insert(1);
            }
        }
    }

    /// Create the next assignment, and return the `RelayUrl` that has it.
    /// You should probably immediately call `get_relay_assignment()` with that `RelayUrl`
    /// to get the newly created assignment. The caller is responsible for making that
    /// assignment actually happen.
    pub async fn pick(&self) -> Result<RelayUrl, Error> {
        // If we are at max relays, only consider relays we are already
        // connected to
        let at_max_relays = self.relay_assignments.len() >= self.hooks.get_max_relays();

        // Maybe include excluded relays
        let now = Unixtime::now().0;
        self.excluded_relays.retain(|_, v| *v > now);

        if self.pubkey_counts.is_empty() {
            return Err(Error::NoPeopleLeft);
        }

        let all_relays = self.hooks.get_all_relays();

        if all_relays.is_empty() {
            return Err(Error::NoRelays);
        }

        // Keep score for each relay, start at 0
        let scoreboard: DashMap<RelayUrl, u64> =
            all_relays.iter().map(|x| (x.to_owned(), 0)).collect();

        // Assign scores to relays from each pubkey
        for elem in self.person_relay_scores.iter() {
            let pubkeyhex = elem.key();
            let relay_scores = elem.value();

            // Skip if this pubkey doesn't need any more assignments
            if let Some(pkc) = self.pubkey_counts.get(pubkeyhex) {
                if *pkc == 0 {
                    // person doesn't need anymore
                    continue;
                }
            } else {
                continue; // person doesn't need any
            }

            // Add scores of their relays
            for (relay, score) in relay_scores.iter() {
                // Skip relays that are excluded
                if self.excluded_relays.contains_key(relay) {
                    continue;
                }

                // If at max, skip relays not already connected
                if at_max_relays && !self.hooks.is_relay_connected(relay) {
                    continue;
                }

                // Skip if relay is already assigned this pubkey
                if let Some(assignment) = self.relay_assignments.get(relay) {
                    if assignment.pubkeys.contains(pubkeyhex) {
                        continue;
                    }
                }

                // Add the score
                if let Some(mut entry) = scoreboard.get_mut(relay) {
                    *entry += score;
                }
            }
        }

        // Adjust all scores based on relay rank and relay success rate
        // (gossip code elided due to complex data tracking required)
        // TBD to add this kind of feature back.
        for mut score_entry in scoreboard.iter_mut() {
            let url = score_entry.key().to_owned();
            let score = score_entry.value_mut();
            *score = self.hooks.adjust_score(url, *score);
        }

        let winner = scoreboard
            .iter()
            .max_by(|x, y| x.value().cmp(y.value()))
            .unwrap();
        let winning_url: RelayUrl = winner.key().to_owned();
        let winning_score: u64 = *winner.value();

        if winning_score == 0 {
            return Err(Error::NoProgress);
        }

        // Now sort out which public keys go with that relay (we did this already
        // above when assigning scores, but in a way which would require a lot of
        // storage to keep, so we just do it again)
        let covered_public_keys = {
            let pubkeys_seeking_relays: Vec<PublicKey> = self
                .pubkey_counts
                .iter()
                .filter(|e| *e.value() > 0)
                .map(|e| e.key().to_owned())
                .collect();

            let mut covered_pubkeys: Vec<PublicKey> = Vec::new();

            for pubkey in pubkeys_seeking_relays.iter() {
                // Skip if relay is already assigned this pubkey
                if let Some(assignment) = self.relay_assignments.get(&winning_url) {
                    if assignment.pubkeys.contains(pubkey) {
                        continue;
                    }
                }

                if let Some(elem) = self.person_relay_scores.get(pubkey) {
                    let relay_scores = elem.value();

                    for (i, (relay, score)) in relay_scores.iter().enumerate() {
                        if *relay == winning_url {
                            // Do not assign to this relay if it's not one of their top
                            // three relays and its score has dropped to 5 or lower.
                            if *score <= 5 && i >= 3 {
                                // in this case we can skip the rest which have lower
                                // scores and are further down the list
                                break;
                            }

                            covered_pubkeys.push(pubkey.to_owned());

                            if let Some(mut count) = self.pubkey_counts.get_mut(pubkey) {
                                if *count > 0 {
                                    *count -= 1;
                                }
                            }
                        }
                    }
                }
            }

            covered_pubkeys
        };

        if covered_public_keys.is_empty() {
            return Err(Error::NoProgress);
        }

        // Only keep pubkey_counts that are still > 0
        self.pubkey_counts.retain(|_, count| *count > 0);

        let assignment = RelayAssignment {
            relay_url: winning_url.clone(),
            pubkeys: covered_public_keys,
        };

        // Put assignment into relay_assignments
        if let Some(mut maybe_elem) = self.relay_assignments.get_mut(&winning_url) {
            // FIXME this could cause a panic, but it would mean we have bad code.
            maybe_elem.value_mut().merge_in(assignment).unwrap();
        } else {
            self.relay_assignments
                .insert(winning_url.clone(), assignment);
        }

        Ok(winning_url)
    }

    /// Get the `RelayAssignment` for a given `RelayUrl`
    pub fn get_relay_assignment(&self, relay_url: &RelayUrl) -> Option<RelayAssignment> {
        self.relay_assignments
            .get(relay_url)
            .map(|elem| elem.value().to_owned())
    }

    /// Get just the count of people assigned to a given `RelayUrl`
    pub fn get_relay_following_count(&self, relay_url: &RelayUrl) -> usize {
        self.relay_assignments
            .get(relay_url)
            .map(|assignment| assignment.pubkeys.len())
            .unwrap_or(0)
    }

    /// Iterate over all `RelayAssignment`s
    pub fn relay_assignments_iter(&self) -> dashmap::iter::Iter<'_, RelayUrl, RelayAssignment> {
        self.relay_assignments.iter()
    }

    /// Get an iterator over all `RelayUrl`s that are excluded, and the `Unixtime` when they
    /// will be candidates again
    pub fn excluded_relays_iter(&self) -> dashmap::iter::Iter<'_, RelayUrl, i64> {
        self.excluded_relays.iter()
    }

    /// Get an iterator over all the `PublicKey`s that are not fully assigned, as well as
    /// the number of relays they still need.
    pub fn pubkey_counts_iter(&self) -> dashmap::iter::Iter<'_, PublicKey, usize> {
        self.pubkey_counts.iter()
    }
}
