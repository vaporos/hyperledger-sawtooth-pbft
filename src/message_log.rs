/*
 * Copyright 2018 Bitwise IO, Inc.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 * -----------------------------------------------------------------------------
 */

//! The message log used by PBFT nodes to save messages

#![allow(unknown_lints)]

use std::collections::{HashSet, VecDeque};
use std::fmt;

use hex;
use itertools::Itertools;
use sawtooth_sdk::consensus::engine::Block;

use config::PbftConfig;
use error::PbftError;
use message_type::{ParsedMessage, PbftHint, PbftMessageType};
use protos::pbft_message::{PbftMessage, PbftMessageInfo};
use state::PbftState;

/// The log keeps track of the last stable checkpoint
#[derive(Clone)]
pub struct PbftStableCheckpoint {
    pub seq_num: u64,
    pub checkpoint_messages: Vec<PbftMessage>,
}

/// Struct for storing messages that a PbftNode receives
pub struct PbftLog {
    /// Generic messages (BlockNew, PrePrepare, Prepare, Commit, Checkpoint)
    messages: HashSet<ParsedMessage>,

    /// Watermarks (minimum/maximum sequence numbers)
    /// Ensure that log does not get too large
    low_water_mark: u64,
    high_water_mark: u64,

    /// Maximum log size, defined from on-chain settings
    max_log_size: u64,

    /// How many cycles through the algorithm we've done (BlockNew messages)
    cycles: u64,

    /// How many cycles in between checkpoints
    checkpoint_period: u64,

    /// Backlog of messages (from peers) with sender's ID
    backlog: VecDeque<ParsedMessage>,

    /// Backlog of blocks (from BlockNews messages)
    block_backlog: VecDeque<Block>,

    /// The most recent checkpoint that contains proof
    pub latest_stable_checkpoint: Option<PbftStableCheckpoint>,
}

impl fmt::Display for PbftLog {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let msg_infos: Vec<PbftMessageInfo> = self
            .messages
            .iter()
            .map(|ref msg| msg.info().clone())
            .collect();
        let string_infos: Vec<String> = msg_infos
            .iter()
            .map(|info: &PbftMessageInfo| -> String {
                format!(
                    "    {{ {}, view: {}, seq: {}, signer: {} }}",
                    info.get_msg_type(),
                    info.get_view(),
                    info.get_seq_num(),
                    hex::encode(info.get_signer_id()),
                )
            })
            .collect();

        write!(
            f,
            "\nPbftLog ({}, {}):\n{}",
            self.low_water_mark,
            self.high_water_mark,
            string_infos.join("\n")
        )
    }
}

impl PbftLog {
    pub fn new(config: &PbftConfig) -> Self {
        PbftLog {
            messages: HashSet::new(),
            low_water_mark: 0,
            cycles: 0,
            checkpoint_period: config.checkpoint_period,
            high_water_mark: config.max_log_size,
            max_log_size: config.max_log_size,
            backlog: VecDeque::new(),
            block_backlog: VecDeque::new(),
            latest_stable_checkpoint: None,
        }
    }

    /// `check_prepared` predicate
    /// `check_prepared` is true for this node if the following messages are present in its log:
    ///  + The original `BlockNew` message
    ///  + A `PrePrepare` message matching the original message (in the current view)
    ///  + `2f + 1` matching `Prepare` messages from different nodes that match
    ///    `PrePrepare` message above (including its own)
    pub fn check_prepared(&self, info: &PbftMessageInfo, f: u64) -> Result<bool, PbftError> {
        // Check if we have both BlockNew and PrePrepare
        let block_new_msg = self.get_one_msg(info, &PbftMessageType::BlockNew);
        let preprep_msg = self.get_one_msg(info, &PbftMessageType::PrePrepare);

        if block_new_msg.is_none() || preprep_msg.is_none() {
            return Ok(false);
        }

        let block_new_msg = block_new_msg.unwrap();
        let preprep_msg = preprep_msg.unwrap();

        // Ensure BlockNew and PrePrepare match
        if block_new_msg.get_block() != preprep_msg.get_block() {
            error!(
                "BlockNew {:?} does not match PrePrepare {:?}",
                block_new_msg, preprep_msg
            );
            return Err(PbftError::BlockMismatch(
                block_new_msg.get_block().clone(),
                preprep_msg.get_block().clone(),
            ));
        }

        // Check if we have 2f + 1 matching Prepares
        Ok(self.log_has_required_msgs(&PbftMessageType::Prepare, &preprep_msg, true, 2 * f + 1))
    }

    /// Checks if the node is ready to enter the `Committing` phase based on the `PbftMessage` received
    ///
    /// `check_committable` is true if for this node:
    ///   + `check_prepared` is true
    ///   + This node has accepted `2f + 1` `Commit` messages, including its own, that match the
    ///     corresponding `PrePrepare` message
    pub fn check_committable(&self, info: &PbftMessageInfo, f: u64) -> Result<bool, PbftError> {
        // Check if Prepared predicate is true
        if !self.check_prepared(info, f)? {
            return Ok(false);
        }

        // Check if we have 2f + 1 matching Commits
        let preprep_msg = self
            .get_one_msg(info, &PbftMessageType::PrePrepare)
            .unwrap();
        Ok(self.log_has_required_msgs(&PbftMessageType::Commit, &preprep_msg, true, 2 * f + 1))
    }

    /// Get one message matching the type, view number, and sequence number
    pub fn get_one_msg(
        &self,
        info: &PbftMessageInfo,
        msg_type: &PbftMessageType,
    ) -> Option<&ParsedMessage> {
        let msgs =
            self.get_messages_of_type_seq_view(msg_type, info.get_seq_num(), info.get_view());
        msgs.first().cloned()
    }

    /// Check if the log contains `required` number of messages with type `msg_type` that match the
    /// sequence and view number of the provided `ref_msg`, as well as its block (optional)
    pub fn log_has_required_msgs(
        &self,
        msg_type: &PbftMessageType,
        ref_msg: &ParsedMessage,
        check_block: bool,
        required: u64,
    ) -> bool {
        let msgs = self.get_messages_of_type_seq_view(
            msg_type,
            ref_msg.info().get_seq_num(),
            ref_msg.info().get_view(),
        );

        let msgs = if check_block {
            msgs.iter()
                .filter(|msg| msg.get_block() == ref_msg.get_block())
                .cloned()
                .collect()
        } else {
            msgs
        };

        msgs.len() as u64 >= required
    }

    /// Add a generic PBFT message to the log
    pub fn add_message(&mut self, msg: ParsedMessage, state: &PbftState) {
        if msg.info().get_seq_num() >= self.high_water_mark
            || msg.info().get_seq_num() < self.low_water_mark
        {
            warn!(
                "Not adding message with sequence number {}; outside of log bounds ({}, {})",
                msg.info().get_seq_num(),
                self.low_water_mark,
                self.high_water_mark,
            );
            return;
        }

        // Except for Checkpoints and ViewChanges, the message must be for the current view to be
        // accepted
        let msg_type = PbftMessageType::from(msg.info().get_msg_type());
        if msg_type != PbftMessageType::Checkpoint
            && msg_type != PbftMessageType::ViewChange
            && msg.info().get_view() != state.view
        {
            warn!(
                "Not adding message with view number {}; does not match node's view: {}",
                msg.info().get_view(),
                state.view,
            );
            return;
        }

        // If the message wasn't already in the log, increment cycles
        let msg_type = PbftMessageType::from(msg.info().get_msg_type());
        let inserted = self.messages.insert(msg);
        if msg_type == PbftMessageType::BlockNew && inserted {
            self.cycles += 1;
        }
        trace!("{}", self);
    }

    /// Adds a message the (back)log, based on the given `PbftHint`
    ///
    /// Past messages are added to the general message log
    /// Future messages are added to the backlog of messages to handle at a later time
    /// Present messages are ignored, as they're generally added immediately after
    /// this method is called by the calling code, except for `PrePrepare` messages
    #[allow(clippy::ptr_arg)]
    pub fn add_message_with_hint(
        &mut self,
        msg: ParsedMessage,
        hint: &PbftHint,
        state: &PbftState,
    ) -> Result<(), PbftError> {
        match hint {
            PbftHint::FutureMessage => {
                self.push_backlog(msg);
                Err(PbftError::NotReadyForMessage)
            }
            PbftHint::PastMessage => {
                self.add_message(msg, state);
                Err(PbftError::NotReadyForMessage)
            }
            PbftHint::PresentMessage => Ok(()),
        }
    }

    /// Obtain all messages from the log that match a given type and sequence_number
    pub fn get_messages_of_type_seq(
        &self,
        msg_type: &PbftMessageType,
        sequence_number: u64,
    ) -> Vec<&ParsedMessage> {
        self.messages
            .iter()
            .filter(|&msg| {
                let info = (*msg).info();
                info.get_msg_type() == String::from(msg_type)
                    && info.get_seq_num() == sequence_number
            })
            .collect()
    }

    /// Obtain messages from the log that match a given type, sequence number, and view
    pub fn get_messages_of_type_seq_view(
        &self,
        msg_type: &PbftMessageType,
        sequence_number: u64,
        view: u64,
    ) -> Vec<&ParsedMessage> {
        self.messages
            .iter()
            .filter(|&msg| {
                let info = (*msg).info();
                info.get_msg_type() == String::from(msg_type)
                    && info.get_seq_num() == sequence_number
                    && info.get_view() == view
            })
            .collect()
    }

    /// Get sufficient messages for the given type and sequence number
    ///
    /// Gets all messages that match the given type and sequence number,
    /// groups them by the view number, filters out view number groups
    /// that don't have enough messages, and then sorts by view number
    /// and returns the highest one found, as an option in case there's
    /// no matching view number groups.
    ///
    /// This is useful in cases where e.g. we have enough messages to
    /// publish for some view number for the current sequence number,
    /// but we've forced a view change before the publishing could happen
    /// and we don't have any/enough messages for the current view num.
    ///
    /// Considers messages from self to not count towards being enough,
    /// as the current usage of this function is building a seal, where
    /// the publishing node's approval is implicit via publishing.
    pub fn get_enough_messages(
        &self,
        msg_type: &PbftMessageType,
        sequence_number: u64,
        minimum: u64,
    ) -> Option<Vec<&ParsedMessage>> {
        self.messages
            .iter()
            .filter_map(|msg| {
                let info = msg.info();
                let same_type = info.get_msg_type() == String::from(msg_type);
                let same_seq = info.get_seq_num() == sequence_number;

                if same_type && same_seq && !msg.from_self {
                    Some((info.get_view(), msg))
                } else {
                    None
                }
            })
            .into_group_map()
            .into_iter()
            .filter(|(_, msgs)| msgs.len() >= minimum as usize)
            .sorted_by_key(|(view, _)| *view)
            .pop()
            .map(|(_, msgs)| msgs)
    }

    /// Get the latest stable checkpoint
    pub fn get_latest_checkpoint(&self) -> u64 {
        if let Some(ref cp) = self.latest_stable_checkpoint {
            cp.seq_num
        } else {
            0
        }
    }

    /// Is this node ready for a checkpoint?
    pub fn at_checkpoint(&self) -> bool {
        self.cycles >= self.checkpoint_period
    }

    /// Garbage collect the log, and create a stable checkpoint
    pub fn garbage_collect(&mut self, stable_checkpoint: u64, view: u64) {
        self.low_water_mark = stable_checkpoint;
        self.high_water_mark = self.low_water_mark + self.max_log_size;
        self.cycles = 0;

        // Update the stable checkpoint
        let cp_msgs: Vec<PbftMessage> = self
            .get_messages_of_type_seq_view(&PbftMessageType::Checkpoint, stable_checkpoint, view)
            .iter()
            .map(|&cp| cp.get_pbft_message().clone())
            .collect();
        let cp = PbftStableCheckpoint {
            seq_num: stable_checkpoint,
            checkpoint_messages: cp_msgs,
        };
        self.latest_stable_checkpoint = Some(cp);

        // Garbage collect logs, filter out all old messages (up to but not including the
        // checkpoint)
        self.messages = self
            .messages
            .iter()
            .filter(|ref msg| {
                let seq_num = msg.info().get_seq_num();
                seq_num >= self.get_latest_checkpoint() && seq_num > 0
            })
            .cloned()
            .collect();
    }

    pub fn push_backlog(&mut self, msg: ParsedMessage) {
        self.backlog.push_back(msg);
    }

    pub fn pop_backlog(&mut self) -> Option<ParsedMessage> {
        self.backlog.pop_front()
    }

    pub fn push_block_backlog(&mut self, msg: Block) {
        self.block_backlog.push_back(msg);
    }

    pub fn pop_block_backlog(&mut self) -> Option<Block> {
        self.block_backlog.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config;
    use hash::hash_sha256;
    use protos::pbft_message::PbftBlock;
    use sawtooth_sdk::consensus::engine::PeerId;

    /// Create a PbftMessage, given its type, view, sequence number, and who it's from
    fn make_msg(
        msg_type: &PbftMessageType,
        view: u64,
        seq_num: u64,
        signer_id: PeerId,
        block_signer_id: PeerId,
    ) -> ParsedMessage {
        let mut info = PbftMessageInfo::new();
        info.set_msg_type(String::from(msg_type));
        info.set_view(view);
        info.set_seq_num(seq_num);
        info.set_signer_id(Vec::<u8>::from(signer_id.clone()));

        let mut pbft_block = PbftBlock::new();
        pbft_block.set_block_id(hash_sha256(
            format!("I'm a block with block num {}", seq_num).as_bytes(),
        ));
        pbft_block.set_signer_id(Vec::<u8>::from(block_signer_id));
        pbft_block.set_block_num(seq_num);

        let mut msg = PbftMessage::new();
        msg.set_info(info);
        msg.set_block(pbft_block);

        ParsedMessage::from_pbft_message(msg)
    }

    /// Obtain the PeerId for node `which`
    fn get_peer_id(cfg: &PbftConfig, which: u64) -> PeerId {
        cfg.peers[which as usize].clone()
    }

    /// Test that adding one message works as expected
    #[test]
    fn one_message() {
        let cfg = config::mock_config(4);
        let mut log = PbftLog::new(&cfg);
        let state = PbftState::new(vec![], &cfg);

        let msg = make_msg(
            &PbftMessageType::PrePrepare,
            0,
            1,
            get_peer_id(&cfg, 0),
            get_peer_id(&cfg, 0),
        );

        log.add_message(msg.clone(), &state);

        let gotten_msgs = log.get_messages_of_type_seq_view(&PbftMessageType::PrePrepare, 1, 0);

        assert_eq!(gotten_msgs.len(), 1);
        assert_eq!(&msg, gotten_msgs[0]);
    }

    /// Test that `check_prepared` and `check_committable` predicates work properly
    #[test]
    fn prepared_committed() {
        let cfg = config::mock_config(4);
        let mut log = PbftLog::new(&cfg);
        let state = PbftState::new(vec![], &cfg);

        let msg = make_msg(
            &PbftMessageType::BlockNew,
            0,
            1,
            get_peer_id(&cfg, 0),
            get_peer_id(&cfg, 0),
        );
        log.add_message(msg.clone(), &state);

        assert_eq!(log.cycles, 1);
        assert!(!log.check_prepared(&msg.info(), 1 as u64).unwrap());
        assert!(!log.check_committable(&msg.info(), 1 as u64).unwrap());

        let msg = make_msg(
            &PbftMessageType::PrePrepare,
            0,
            1,
            get_peer_id(&cfg, 0),
            get_peer_id(&cfg, 0),
        );
        log.add_message(msg.clone(), &state);
        assert!(!log.check_prepared(&msg.info(), 1 as u64).unwrap());
        assert!(!log.check_committable(&msg.info(), 1 as u64).unwrap());

        for peer in 0..4 {
            let msg = make_msg(
                &PbftMessageType::Prepare,
                0,
                1,
                get_peer_id(&cfg, peer),
                get_peer_id(&cfg, 0),
            );

            log.add_message(msg.clone(), &state);
            if peer < 2 {
                assert!(!log.check_prepared(&msg.info(), 1 as u64).unwrap());
                assert!(!log.check_committable(&msg.info(), 1 as u64).unwrap());
            } else {
                assert!(log.check_prepared(&msg.info(), 1 as u64).unwrap());
                assert!(!log.check_committable(&msg.info(), 1 as u64).unwrap());
            }
        }

        for peer in 0..4 {
            let msg = make_msg(
                &PbftMessageType::Commit,
                0,
                1,
                get_peer_id(&cfg, peer),
                get_peer_id(&cfg, 0),
            );

            log.add_message(msg.clone(), &state);
            if peer < 2 {
                assert!(!log.check_committable(&msg.info(), 1 as u64).unwrap());
            } else {
                assert!(log.check_committable(&msg.info(), 1 as u64).unwrap());
            }
        }
    }

    /// Make sure that the log doesn't start out checkpointing
    #[test]
    fn checkpoint_basics() {
        let cfg = config::mock_config(4);
        let log = PbftLog::new(&cfg);

        assert_eq!(log.get_latest_checkpoint(), 0);
        assert!(!log.at_checkpoint());
    }

    /// Make sure that log garbage collection works as expected
    /// (All messages up to, but not including, the checkpoint are deleted)
    #[test]
    fn garbage_collection() {
        let cfg = config::mock_config(4);
        let mut log = PbftLog::new(&cfg);
        let state = PbftState::new(vec![], &cfg);

        for seq in 1..5 {
            let msg = make_msg(
                &PbftMessageType::BlockNew,
                0,
                seq,
                get_peer_id(&cfg, 0),
                get_peer_id(&cfg, 0),
            );
            log.add_message(msg.clone(), &state);

            let msg = make_msg(
                &PbftMessageType::PrePrepare,
                0,
                seq,
                get_peer_id(&cfg, 0),
                get_peer_id(&cfg, 0),
            );
            log.add_message(msg.clone(), &state);

            for peer in 0..4 {
                let msg = make_msg(
                    &PbftMessageType::Prepare,
                    0,
                    seq,
                    get_peer_id(&cfg, peer),
                    get_peer_id(&cfg, 0),
                );

                log.add_message(msg.clone(), &state);
            }

            for peer in 0..4 {
                let msg = make_msg(
                    &PbftMessageType::Commit,
                    0,
                    seq,
                    get_peer_id(&cfg, peer),
                    get_peer_id(&cfg, 0),
                );

                log.add_message(msg.clone(), &state);
            }
        }

        for peer in 0..4 {
            let msg = make_msg(
                &PbftMessageType::Checkpoint,
                0,
                4,
                get_peer_id(&cfg, peer),
                get_peer_id(&cfg, 0),
            );

            log.add_message(msg.clone(), &state);
        }

        log.garbage_collect(4, 0);

        for old in 1..3 {
            for msg_type in &[
                PbftMessageType::BlockNew,
                PbftMessageType::PrePrepare,
                PbftMessageType::Prepare,
                PbftMessageType::Commit,
            ] {
                assert_eq!(
                    log.get_messages_of_type_seq_view(&msg_type, old, 0).len(),
                    0
                );
            }
        }

        for msg_type in &[PbftMessageType::BlockNew, PbftMessageType::PrePrepare] {
            assert_eq!(log.get_messages_of_type_seq_view(&msg_type, 4, 0).len(), 1);
        }

        for msg_type in &[PbftMessageType::Prepare, PbftMessageType::Commit] {
            assert_eq!(log.get_messages_of_type_seq_view(&msg_type, 4, 0).len(), 4);
        }
    }
}
