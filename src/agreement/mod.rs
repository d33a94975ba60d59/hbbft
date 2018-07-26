//! # Binary Byzantine agreement protocol
//!
//! The Binary Agreement protocol allows each node to input one binary (`bool`) value, and will
//! output a binary value. The output is guaranteed to have been input by at least one correct
//! node, and all correct nodes will have the same output.
//!
//! ## How it works
//!
//! The algorithm proceeds in _epochs_, and the number of epochs it takes until it terminates is
//! unbounded in theory but has a finite expected value. Each node keeps track of an _estimate_
//! value `e`, which is initialized to the node's own input. Let's call a value `v`
//! that has been input by at least one correct node and such that `!v` hasn't been _output_ by any
//! correct node yet, a _viable output_. The estimate will always be a viable output.
//!
//! All messages are annotated with the epoch they belong to, but we omit that here for brevity.
//!
//! * At the beginning of each epoch, we multicast `BVal(e)`. It translates to: "I know that `e` is
//! a viable output."
//!
//! * Once we receive `BVal(v)` with the same value from _f + 1_ different validators, we know that
//!   at least one of them must be correct. So we know that `v` is a viable output. If we haven't
//!   done so already we multicast `BVal(v)`. (Even if we already multicast `BVal(!v)`).
//!
//! * Let's say a node _believes in `v`_ if it received `BVal(v)` from _2 f + 1_ validators.
//! For the _first_ value `v` we believe in, we multicast `Aux(v)`. It translates to:
//! "I know that all correct nodes will eventually know that `v` is a viable output.
//! I'm not sure about `!v` yet."
//!
//!   * Since every node will receive at least _2 f + 1_ `BVal` messages from correct validators,
//!   there is at least one value `v`, such that every node receives _f + 1_ `BVal(v)` messages.
//!   As a consequence, every correct validator will multicast `BVal(v)` itself. Hence we are
//!   guaranteed to receive _2 f + 1_ `BVal(v)` messages.
//!   In short: If _any_ correct node believes in `v`, _every_ correct node will.
//!
//!   * Every correct node will eventually send exactly one `Aux`, so we will receive at least
//!   _2 f + 1_ `Aux` messages with values we believe in. At that point, we define the set `vals`
//!   of _candidate values_: the set of values we believe in _and_ have received in an `Aux`.
//!
//! * Once we have the set of candidate values, we obtain a _coin value_ `s` (see below).
//!
//!   * If there is only a single candidate value `b`, we set our estimate `e = b`. If `s == b`,
//!   we _output_ and send a `Term(b)` message which is interpreted as `BVal(b)` and `Aux(b)` for
//!   all future epochs. If `s != b`, we just proceed to the next epoch.
//!
//!   * If both values are candidates, we set `e = s` and proceed to the next epoch.
//!
//! In epochs that are 0 modulo 3, the value `s` is `true`. In 1 modulo 3, it is `false`. In the
//! case 2 modulo 3, we flip a common coin to determine a pseudorandom `s`.
//!
//! An adversary that knows each coin value, controls a few validators and controls network
//! scheduling can delay the delivery of `Aux` and `BVal` messages to influence which candidate
//! values the nodes will end up with. In some circumstances that allows them to stall the network.
//! This is even true if the coin is flipped too early: the adversary must not learn about the coin
//! value early enough to delay enough `Aux` messages. That's why in the third case, the value `s`
//! is determined as follows:
//!
//! * We multicast a `Conf` message containing our candidate values.
//!
//! * Since every good node believes in all values it puts into its `Conf` message, we will
//! eventually receive _2 f + 1_ `Conf` messages containing only values we believe in. Then we
//! trigger the common coin.
//!
//! * After _f + 1_ nodes have sent us their coin shares, we receive the coin output and assign it
//! to `s`.

pub mod bin_values;

use rand;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::mem::replace;
use std::sync::Arc;

use itertools::Itertools;

use agreement::bin_values::BinValues;
use common_coin::{self, CommonCoin, CommonCoinMessage};
use messaging::{self, DistAlgorithm, NetworkInfo, Target};

error_chain!{
    links {
        CommonCoin(common_coin::Error, common_coin::ErrorKind);
    }

    errors {
        UnknownProposer {
            description("unknown proposer")
        }
        InputNotAccepted {
            description("input not accepted")
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum AgreementContent {
    /// `BVal` message.
    BVal(bool),
    /// `Aux` message.
    Aux(bool),
    /// `Conf` message.
    Conf(BinValues),
    /// `Term` message.
    Term(bool),
    /// Common Coin message,
    Coin(Box<CommonCoinMessage>),
}

impl AgreementContent {
    /// Creates an message with a given epoch number.
    pub fn with_epoch(self, epoch: u32) -> AgreementMessage {
        AgreementMessage {
            epoch,
            content: self,
        }
    }
}

/// Messages sent during the binary Byzantine agreement stage.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Rand)]
pub struct AgreementMessage {
    pub epoch: u32,
    pub content: AgreementContent,
}

// NOTE: Extending rand_derive to correctly generate random values from boxes would make this
// implementation obsolete; however at the time of this writing, `rand::Rand` is already deprecated
// with no replacement in sight.
impl rand::Rand for AgreementContent {
    fn rand<R: rand::Rng>(rng: &mut R) -> Self {
        let message_type = *rng
            .choose(&["bval", "aux", "conf", "term", "coin"])
            .unwrap();

        match message_type {
            "bval" => AgreementContent::BVal(rand::random()),
            "aux" => AgreementContent::Aux(rand::random()),
            "conf" => AgreementContent::Conf(rand::random()),
            "term" => AgreementContent::Term(rand::random()),
            "coin" => AgreementContent::Coin(Box::new(rand::random())),
            _ => unreachable!(),
        }
    }
}

/// Possible values of the common coin schedule defining the method to derive the common coin in a
/// given epoch: as a constant value or a distributed computation.
#[derive(Debug)]
enum CoinSchedule {
    False,
    True,
    Random,
}

/// Binary Agreement instance
#[derive(Debug)]
pub struct Agreement<NodeUid> {
    /// Shared network information.
    netinfo: Arc<NetworkInfo<NodeUid>>,
    /// Session ID, e.g, the Honey Badger algorithm epoch.
    session_id: u64,
    /// The ID of the proposer of the value for this agreement instance.
    proposer_id: NodeUid,
    /// Agreement algorithm epoch.
    epoch: u32,
    /// Bin values. Reset on every epoch update.
    bin_values: BinValues,
    /// Values received in `BVal` messages. Reset on every epoch update.
    received_bval: BTreeMap<NodeUid, BTreeSet<bool>>,
    /// Sent `BVal` values. Reset on every epoch update.
    sent_bval: BTreeSet<bool>,
    /// Values received in `Aux` messages. Reset on every epoch update.
    received_aux: BTreeMap<NodeUid, bool>,
    /// Received `Conf` messages. Reset on every epoch update.
    received_conf: BTreeMap<NodeUid, BinValues>,
    /// Received `Term` messages. Kept throughout epoch updates.
    received_term: BTreeMap<NodeUid, bool>,
    /// The estimate of the decision value in the current epoch.
    estimated: Option<bool>,
    /// A permanent, latching copy of the output value. This copy is required because `output` can
    /// be consumed using `DistAlgorithm::next_output` immediately after the instance finishing to
    /// handle a message, in which case it would otherwise be unknown whether the output value was
    /// ever there at all. While the output value will still be required in a later epoch to decide
    /// the termination state.
    decision: Option<bool>,
    /// A cache for messages for future epochs that cannot be handled yet.
    // TODO: Find a better solution for this; defend against spam.
    incoming_queue: Vec<(NodeUid, AgreementMessage)>,
    /// Termination flag. Once the instance determines that all the remote nodes have reached
    /// agreement or have the necessary information to reach agreement, it sets the `terminated`
    /// flag and accepts no more incoming messages.
    terminated: bool,
    /// Whether the `Conf` message round has started in the current epoch.
    conf_round: bool,
    /// A common coin instance. It is reset on epoch update.
    common_coin: CommonCoin<NodeUid, Nonce>,
    /// Common coin schedule computed at the start of each epoch.
    coin_schedule: CoinSchedule,
}

pub type Step<NodeUid> = messaging::Step<Agreement<NodeUid>>;

impl<NodeUid: Clone + Debug + Ord> DistAlgorithm for Agreement<NodeUid> {
    type NodeUid = NodeUid;
    type Input = bool;
    type Output = bool;
    type Message = AgreementMessage;
    type Error = Error;

    fn input(&mut self, input: Self::Input) -> Result<Step<NodeUid>> {
        self.set_input(input)
    }

    /// Receive input from a remote node.
    fn handle_message(
        &mut self,
        sender_id: &Self::NodeUid,
        message: Self::Message,
    ) -> Result<Step<NodeUid>> {
        if self.terminated || message.epoch < self.epoch {
            // Message is obsolete: We are already in a later epoch or terminated.
            Ok(Step::default())
        } else if message.epoch > self.epoch {
            // Message is for a later epoch. We can't handle that yet.
            self.incoming_queue.push((sender_id.clone(), message));
            Ok(Step::default())
        } else {
            match message.content {
                AgreementContent::BVal(b) => self.handle_bval(sender_id, b),
                AgreementContent::Aux(b) => self.handle_aux(sender_id, b),
                AgreementContent::Conf(v) => self.handle_conf(sender_id, v),
                AgreementContent::Term(v) => Ok(self.handle_term(sender_id, v)),
                AgreementContent::Coin(msg) => self.handle_coin(sender_id, *msg),
            }
        }
    }

    /// Whether the algorithm has terminated.
    fn terminated(&self) -> bool {
        self.terminated
    }

    fn our_id(&self) -> &Self::NodeUid {
        self.netinfo.our_uid()
    }
}

impl<NodeUid: Clone + Debug + Ord> Agreement<NodeUid> {
    pub fn new(
        netinfo: Arc<NetworkInfo<NodeUid>>,
        session_id: u64,
        proposer_id: NodeUid,
    ) -> Result<Self> {
        let invocation_id = netinfo.invocation_id();
        if let Some(proposer_i) = netinfo.node_index(&proposer_id) {
            Ok(Agreement {
                netinfo: netinfo.clone(),
                session_id,
                proposer_id,
                epoch: 0,
                bin_values: BinValues::new(),
                received_bval: BTreeMap::new(),
                sent_bval: BTreeSet::new(),
                received_aux: BTreeMap::new(),
                received_conf: BTreeMap::new(),
                received_term: BTreeMap::new(),
                estimated: None,
                decision: None,
                incoming_queue: Vec::new(),
                terminated: false,
                conf_round: false,
                common_coin: CommonCoin::new(
                    netinfo,
                    Nonce::new(invocation_id.as_ref(), session_id, proposer_i, 0),
                ),
                coin_schedule: CoinSchedule::True,
            })
        } else {
            Err(ErrorKind::UnknownProposer.into())
        }
    }

    /// Sets the input value for agreement.
    fn set_input(&mut self, input: bool) -> Result<Step<NodeUid>> {
        if self.epoch != 0 || self.estimated.is_some() {
            return Err(ErrorKind::InputNotAccepted.into());
        }
        if self.netinfo.num_nodes() == 1 {
            let mut step = self.send_bval(input)?;
            step.extend(self.send_aux(input)?);
            step.extend(self.decide(input));
            Ok(step)
        } else {
            // Set the initial estimated value to the input value.
            self.estimated = Some(input);
            // Record the input value as sent.
            self.send_bval(input)
        }
    }

    /// Acceptance check to be performed before setting the input value.
    pub fn accepts_input(&self) -> bool {
        self.epoch == 0 && self.estimated.is_none()
    }

    fn handle_bval(&mut self, sender_id: &NodeUid, b: bool) -> Result<Step<NodeUid>> {
        self.received_bval
            .entry(sender_id.clone())
            .or_insert_with(BTreeSet::new)
            .insert(b);
        let count_bval = self
            .received_bval
            .values()
            .filter(|values| values.contains(&b))
            .count();

        let mut step = Step::default();

        // upon receiving `BVal(b)` messages from 2f + 1 nodes,
        // bin_values := bin_values ∪ {b}
        if count_bval == 2 * self.netinfo.num_faulty() + 1 {
            let previous_bin_values = self.bin_values;
            let bin_values_changed = self.bin_values.insert(b);

            // wait until bin_values != 0, then multicast `Aux(w)`
            // where w ∈ bin_values
            if previous_bin_values == BinValues::None {
                // Send an `Aux` message at most once per epoch.
                step.extend(self.send_aux(b)?);
            }
            if bin_values_changed {
                step.extend(self.on_bin_values_changed()?);
            }
        }

        if count_bval == self.netinfo.num_faulty() + 1 && !self.sent_bval.contains(&b) {
            // upon receiving `BVal(b)` messages from f + 1 nodes, if
            // `BVal(b)` has not been sent, multicast `BVal(b)`
            step.extend(self.send_bval(b)?);
        }
        Ok(step)
    }

    /// Called when `bin_values` changes as a result of receiving a `BVal` message. Tries to update
    /// the epoch.
    fn on_bin_values_changed(&mut self) -> Result<Step<NodeUid>> {
        match self.coin_schedule {
            CoinSchedule::False => {
                let (aux_count, aux_vals) = self.count_aux();
                if aux_count >= self.netinfo.num_nodes() - self.netinfo.num_faulty() {
                    self.on_coin(false, aux_vals.definite())
                } else {
                    Ok(Step::default())
                }
            }
            CoinSchedule::True => {
                let (aux_count, aux_vals) = self.count_aux();
                if aux_count >= self.netinfo.num_nodes() - self.netinfo.num_faulty() {
                    self.on_coin(true, aux_vals.definite())
                } else {
                    Ok(Step::default())
                }
            }
            CoinSchedule::Random => {
                // If the `Conf` round has already started, a change in `bin_values` can lead to its
                // end. Try if it has indeed finished.
                self.try_finish_conf_round()
            }
        }
    }

    fn send_bval(&mut self, b: bool) -> Result<Step<NodeUid>> {
        if !self.netinfo.is_validator() {
            return Ok(Step::default());
        }
        // Record the value `b` as sent.
        self.sent_bval.insert(b);
        // Multicast `BVal`.
        let msg = AgreementContent::BVal(b).with_epoch(self.epoch);
        let mut step: Step<NodeUid> = Target::All.message(msg).into();
        // Receive the `BVal` message locally.
        let our_uid = &self.netinfo.our_uid().clone();
        step.extend(self.handle_bval(our_uid, b)?);
        Ok(step)
    }

    fn send_conf(&mut self) -> Result<Step<NodeUid>> {
        if self.conf_round {
            // Only one `Conf` message is allowed in an epoch.
            return Ok(Step::default());
        }

        // Trigger the start of the `Conf` round.
        self.conf_round = true;

        if !self.netinfo.is_validator() {
            return Ok(Step::default());
        }

        let v = self.bin_values;
        // Multicast `Conf`.
        let msg = AgreementContent::Conf(v).with_epoch(self.epoch);
        let mut step: Step<NodeUid> = Target::All.message(msg).into();
        // Receive the `Conf` message locally.
        let our_uid = &self.netinfo.our_uid().clone();
        step.extend(self.handle_conf(our_uid, v)?);
        Ok(step)
    }

    /// Waits until at least (N − f) `Aux` messages have been received, such that
    /// the set of values carried by these messages, vals, are a subset of
    /// bin_values (note that bin_values_r may continue to change as `BVal`
    /// messages are received, thus this condition may be triggered upon arrival
    /// of either an `Aux` or a `BVal` message).
    fn handle_aux(&mut self, sender_id: &NodeUid, b: bool) -> Result<Step<NodeUid>> {
        // Perform the `Aux` message round only if a `Conf` round hasn't started yet.
        if self.conf_round {
            return Ok(Step::default());
        }
        self.received_aux.insert(sender_id.clone(), b);
        if self.bin_values == BinValues::None {
            return Ok(Step::default());
        }
        let (aux_count, aux_vals) = self.count_aux();
        if aux_count < self.netinfo.num_nodes() - self.netinfo.num_faulty() {
            // Continue waiting for the (N - f) `Aux` messages.
            return Ok(Step::default());
        }

        // Execute the Common Coin schedule `false, true, get_coin(), false, true, get_coin(), ...`
        match self.coin_schedule {
            CoinSchedule::False => self.on_coin(false, aux_vals.definite()),
            CoinSchedule::True => self.on_coin(true, aux_vals.definite()),
            CoinSchedule::Random => self.send_conf(), // Start the `Conf` message round.
        }
    }

    fn handle_conf(&mut self, sender_id: &NodeUid, v: BinValues) -> Result<Step<NodeUid>> {
        self.received_conf.insert(sender_id.clone(), v);
        self.try_finish_conf_round()
    }

    /// Receives a `Term(v)` message. If we haven't yet decided on a value and there are more than
    /// `num_faulty` such messages with the same value from different nodes, performs expedite
    /// termination: decides on `v`, broadcasts `Term(v)` and terminates the instance.
    fn handle_term(&mut self, sender_id: &NodeUid, b: bool) -> Step<NodeUid> {
        self.received_term.insert(sender_id.clone(), b);
        // Check for the expedite termination condition.
        if self.decision.is_none()
            && self.received_term.iter().filter(|(_, &c)| b == c).count()
                > self.netinfo.num_faulty()
        {
            self.decide(b)
        } else {
            Step::default()
        }
    }

    /// Handles a Common Coin message. If there is output from Common Coin, starts the next
    /// epoch. The function may output a decision value.
    fn handle_coin(
        &mut self,
        sender_id: &NodeUid,
        msg: CommonCoinMessage,
    ) -> Result<Step<NodeUid>> {
        let coin_step = self.common_coin.handle_message(sender_id, msg)?;
        self.on_coin_step(coin_step)
    }

    fn on_coin_step(
        &mut self,
        coin_step: common_coin::Step<NodeUid, Nonce>,
    ) -> Result<Step<NodeUid>> {
        let mut step = Step::default();
        let epoch = self.epoch;
        let coin_output = step.extend_with(coin_step, |c_msg| {
            AgreementContent::Coin(Box::new(c_msg)).with_epoch(epoch)
        });
        if let Some(coin) = coin_output.into_iter().next() {
            let def_bin_value = self.count_conf().1.definite();
            step.extend(self.on_coin(coin, def_bin_value)?);
        }
        Ok(step)
    }

    /// When the common coin has been computed, tries to decide on an output value, updates the
    /// `Agreement` epoch and handles queued messages for the new epoch.
    fn on_coin(&mut self, coin: bool, def_bin_value: Option<bool>) -> Result<Step<NodeUid>> {
        if self.terminated {
            // Avoid an infinite regression without making an Agreement step.
            return Ok(Step::default());
        }

        let mut step = Step::default();

        let b = if let Some(b) = def_bin_value {
            // Outputting a value is allowed only once.
            if self.decision.is_none() && b == coin {
                step.extend(self.decide(b));
            }
            b
        } else {
            coin
        };

        self.update_epoch();

        self.estimated = Some(b);
        step.extend(self.send_bval(b)?);
        let queued_msgs = replace(&mut self.incoming_queue, Vec::new());
        for (sender_id, msg) in queued_msgs {
            step.extend(self.handle_message(&sender_id, msg)?);
            if self.terminated {
                break;
            }
        }
        Ok(step)
    }

    /// Computes the coin schedule for the current `Agreement` epoch.
    fn coin_schedule(&self) -> CoinSchedule {
        match self.epoch % 3 {
            0 => CoinSchedule::True,
            1 => CoinSchedule::False,
            _ => CoinSchedule::Random,
        }
    }

    /// Decides on a value and broadcasts a `Term` message with that value.
    fn decide(&mut self, b: bool) -> Step<NodeUid> {
        if self.terminated {
            return Step::default();
        }
        // Output the agreement value.
        let mut step = Step::default();
        step.output.push_back(b);
        // Latch the decided state.
        self.decision = Some(b);
        debug!(
            "{:?}/{:?} (is_validator: {}) decision: {}",
            self.netinfo.our_uid(),
            self.proposer_id,
            self.netinfo.is_validator(),
            b
        );
        if self.netinfo.is_validator() {
            let msg = AgreementContent::Term(b).with_epoch(self.epoch);
            step.messages.push_back(Target::All.message(msg));
            self.received_term.insert(self.netinfo.our_uid().clone(), b);
        }
        self.terminated = true;
        step
    }

    fn try_finish_conf_round(&mut self) -> Result<Step<NodeUid>> {
        if self.conf_round
            && self.count_conf().0 >= self.netinfo.num_nodes() - self.netinfo.num_faulty()
        {
            // Invoke the common coin.
            let coin_step = self.common_coin.input(())?;
            self.on_coin_step(coin_step)
        } else {
            // Continue waiting for (N - f) `Conf` messages
            Ok(Step::default())
        }
    }

    fn send_aux(&mut self, b: bool) -> Result<Step<NodeUid>> {
        if !self.netinfo.is_validator() {
            return Ok(Step::default());
        }
        // Multicast `Aux`.
        let mut step: Step<NodeUid> = Target::All
            .message(AgreementContent::Aux(b).with_epoch(self.epoch))
            .into();
        // Receive the `Aux` message locally.
        let our_uid = &self.netinfo.our_uid().clone();
        step.extend(self.handle_aux(our_uid, b)?);
        Ok(step)
    }

    /// The count of `Aux` messages such that the set of values carried by those messages is a
    /// subset of bin_values_r. The count of matching `Term` messages from terminated nodes is also
    /// added to the count of `Aux` messages as witnesses of the terminated nodes' decision.
    ///
    /// In general, we can't expect every good node to send the same `Aux` value, so waiting for N -
    /// f agreeing messages would not always terminate. We can, however, expect every good node to
    /// send an `Aux` value that will eventually end up in our `bin_values`.
    fn count_aux(&self) -> (usize, BinValues) {
        let mut aux: BTreeMap<_, _> = self
            .received_aux
            .iter()
            .filter(|(_, &b)| self.bin_values.contains(b))
            .collect();

        let term: BTreeMap<_, _> = self
            .received_term
            .iter()
            .filter(|(_, &b)| self.bin_values.contains(b))
            .collect();

        // Ensure that nodes are not counted twice.
        aux.extend(term);
        let bin: BinValues = aux.values().map(|&&v| BinValues::from_bool(v)).collect();
        (aux.len(), bin)
    }

    /// Counts the number of received `Conf` messages.
    fn count_conf(&self) -> (usize, BinValues) {
        let (vals_cnt, vals) = self
            .received_conf
            .values()
            .filter(|&conf| conf.is_subset(self.bin_values))
            .tee();

        (vals_cnt.count(), vals.cloned().collect())
    }

    fn update_epoch(&mut self) {
        self.bin_values.clear();
        self.received_bval.clear();
        self.sent_bval.clear();
        self.received_aux.clear();
        self.received_conf.clear();
        self.conf_round = false;
        self.epoch += 1;
        let nonce = Nonce::new(
            self.netinfo.invocation_id().as_ref(),
            self.session_id,
            self.netinfo.node_index(&self.proposer_id).unwrap(),
            self.epoch,
        );
        // TODO: Don't spend time creating a `CommonCoin` instance in epochs where the common coin
        // is known.
        self.common_coin = CommonCoin::new(self.netinfo.clone(), nonce);
        self.coin_schedule = self.coin_schedule();
        debug!(
            "{:?} Agreement instance {:?} started epoch {}",
            self.netinfo.our_uid(),
            self.proposer_id,
            self.epoch
        );
    }
}

#[derive(Clone, Debug)]
struct Nonce(Vec<u8>);

impl Nonce {
    pub fn new(
        invocation_id: &[u8],
        session_id: u64,
        proposer_id: usize,
        agreement_epoch: u32,
    ) -> Self {
        Nonce(Vec::from(format!(
            "Nonce for Honey Badger {:?}@{}:{}:{}",
            invocation_id, session_id, agreement_epoch, proposer_id
        )))
    }
}

impl AsRef<[u8]> for Nonce {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}
