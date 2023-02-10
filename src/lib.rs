use dashmap::{DashMap, DashSet};
use nostr_types::{PublicKeyHex, RelayUrl, Unixtime};
use thiserror::Error;

/// This is how a person uses a relay: to write (outbox) or to read (inbox)
#[derive(Debug, Copy, Clone)]
pub enum Direction {
    Read,
    Write,
}

/// Errors the RelayPicker functions can return
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum Error {
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
    pub pubkeys: Vec<PublicKeyHex>,
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
pub trait RelayPickerHooks: Send + Sync {
    fn get_all_relays(&self) -> Vec<RelayUrl>;
    fn get_relays_for_pubkey(&self, pubkey: PublicKeyHex, direction: Direction) -> Vec<(RelayUrl, u64)>;
    fn get_max_relays(&self) -> usize;
    fn get_num_relays_per_person(&self) -> usize;
    fn get_followed_pubkeys(&self) -> Vec<PublicKeyHex>;
}

/// The RelayPicker is a structure that helps assign people we follow to relays we watch.
/// It remembers which publickeys are assigned to which relays, which pubkeys need more
/// relays and how many, which relays need a time out, and person-relay scores for making
/// good assignments dynamically.
#[derive(Debug, Default)]
pub struct RelayPicker<H: RelayPickerHooks + Default> {
    /// All of the relays we might use
    pub all_relays: DashSet<RelayUrl>,

    /// Hooks you provide to the Relay Picker
    pub hooks: H,

    /// A ranking of relays per person.
    pub person_relay_scores: DashMap<PublicKeyHex, Vec<(RelayUrl, u64)>>,

    /// All of the relays actually connected
    pub connected_relays: DashSet<RelayUrl>,

    /// All of the relays currently connected, with optional assignments.
    /// (Sometimes a relay is connected for a different kind of subscription.)
    pub relay_assignments: DashMap<RelayUrl, RelayAssignment>,

    /// Relays which recently failed and which require a timeout before
    /// they can be chosen again.  The value is the time when it can be removed
    /// from this list.
    pub excluded_relays: DashMap<RelayUrl, i64>,

    /// For each followed pubkey that still needs assignments, the number of relay
    /// assignments it is seeking.  These start out at settings.num_relays_per_person
    /// (if the person doesn't have that many relays, it will do the best it can)
    pub pubkey_counts: DashMap<PublicKeyHex, usize>,

}

impl<H: RelayPickerHooks + Default> RelayPicker<H> {
    /// Create a new Relay Picker
    pub async fn new(hooks: H) -> Result<RelayPicker<H>, Error> {

        let all_relays: DashSet<RelayUrl> = DashSet::new();
        for relay_url in hooks.get_all_relays()
            .drain(..)
        {
            all_relays.insert(relay_url);
        }

        let rp = RelayPicker {
            all_relays,
            hooks,
            .. Default::default()
        };

        rp.refresh_person_relay_scores_inner(true).await?;

        Ok(rp)
    }

    /// Add a public key
    pub fn add_someone(&self, pubkey: PublicKeyHex) -> Result<(), Error> {
        self.pubkey_counts.insert(pubkey, self.hooks.get_num_relays_per_person());
        Ok(())
    }

    // We could do 'remove_someone' but that's less important. People can just restart.

    /// Refresh the person relay scores from the hook function
    pub async fn refresh_person_relay_scores(&self) -> Result<(), Error> {
        Ok(self.refresh_person_relay_scores_inner(false).await?)
    }

    // Refresh person relay scores.
    async fn refresh_person_relay_scores_inner(&self, initialize_counts: bool) -> Result<(), Error> {
        self.person_relay_scores.clear();

        if initialize_counts {
            self.pubkey_counts.clear();
        }

        // Get all the people we follow
        let pubkeys: Vec<PublicKeyHex> = self
            .hooks
            .get_followed_pubkeys()
            .iter()
            .map(|p| p.to_owned())
            .collect();

        // Compute scores for each person_relay pairing
        for pubkey in &pubkeys {
            let best_relays: Vec<(RelayUrl, u64)> =
                self.hooks.get_relays_for_pubkey(pubkey.to_owned(), Direction::Write);
            self.person_relay_scores.insert(pubkey.clone(), best_relays);

            if initialize_counts {
                self.pubkey_counts
                    .insert(pubkey.clone(), self.hooks.get_num_relays_per_person());
            }
        }

        Ok(())
    }

    /// When a relay disconnects, call this so that whatever assignments it might have
    /// had can be reassigned.  Then call pick_relays() again.
    pub fn relay_disconnected(&self, url: &RelayUrl) {
        // Remove from connected relays list
        if let Some((_key, assignment)) = self.relay_assignments.remove(url) {
            // Exclude the relay for the next 30 seconds
            let hence = Unixtime::now().unwrap().0 + 30;
            self.excluded_relays.insert(url.to_owned(), hence);
            tracing::debug!("{} goes into the penalty box until {}", url, hence,);

            // Put the public keys back into pubkey_counts
            for pubkey in assignment.pubkeys.iter() {
                self.pubkey_counts
                    .entry(pubkey.to_owned())
                    .and_modify(|e| *e += 1)
                    .or_insert(1);
            }
        }
    }

    /// Create the next assignment, and return the RelayUrl that has it.
    /// The caller is responsible for making that assignment actually happen.
    pub async fn pick(&self) -> Result<RelayUrl, Error> {
        // If we are at max relays, only consider relays we are already
        // connected to
        let at_max_relays = self.relay_assignments.len() >= self.hooks.get_max_relays();

        // Maybe include excluded relays
        let now = Unixtime::now().unwrap().0;
        self.excluded_relays.retain(|_, v| *v > now);

        if self.pubkey_counts.is_empty() {
            return Err(Error::NoPeopleLeft);
        }

        // Keep score for each relay
        let scoreboard: DashMap<RelayUrl, u64> = self
            .all_relays
            .iter()
            .map(|x| (x.key().to_owned(), 0))
            .collect();

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
                if at_max_relays && !self.connected_relays.contains(relay) {
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
            let mut pubkeys: Vec<PublicKeyHex> = Vec::new();

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

                // Skip if relay is already assigned this pubkey
                if let Some(assignment) = self.relay_assignments.get(&winning_url) {
                    if assignment.pubkeys.contains(pubkeyhex) {
                        continue;
                    }
                }

                for (relay, _) in relay_scores.iter() {
                    if *relay == winning_url {
                        // Add to pubkeys
                        pubkeys.push(pubkeyhex.to_owned());

                        // Decrement in pubkey counts
                        if let Some(mut count) = self.pubkey_counts.get_mut(pubkeyhex) {
                            if *count > 0 {
                                *count -= 1;
                            }
                        }

                        break;
                    }
                }
            }

            pubkeys
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
}
