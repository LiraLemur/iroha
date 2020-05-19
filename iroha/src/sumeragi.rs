//! This module contains consensus related logic of the Iroha.
//!
//! `Consensus` trait is now implemented only by `Sumeragi` for now.

use crate::{
    block::{PendingBlock, SignedBlock},
    crypto::Hash,
    peer::PeerId,
    prelude::*,
    torii::uri,
};
use async_std::sync::RwLock;
use iroha_derive::*;
use iroha_network::{Network, Request};
use parity_scale_codec::{Decode, Encode};
use rand::{rngs::StdRng, seq::SliceRandom, SeedableRng};
use std::{
    fmt::{self, Debug, Formatter},
    sync::Arc,
};

trait Consensus {
    fn round(&mut self, transactions: Vec<AcceptedTransaction>) -> Option<PendingBlock>;
}

/// Message's variants that are used by peers to communicate in the process of consensus.
#[derive(Io, Decode, Encode, Debug, Clone)]
pub enum Message {
    /// Is sent by leader to all validating peers, when a new block is created.
    Created(SignedBlock),
    /// Is sent by validating peers to proxy tail and observing peers when they have signed this block.
    Signed(SignedBlock),
    /// Is sent by proxy tail to validating peers and to leader, when the block is committed.
    Committed(SignedBlock),
}

impl Message {
    async fn send_to(self, peer: &PeerId) -> Result<(), String> {
        let _response = Network::send_request_to(
            &peer.address,
            Request::new(uri::BLOCKS_URI.to_string(), self.into()),
        )
        .await?;
        Ok(())
    }
}

/// Possible Peer's roles in consensus.
#[derive(Eq, PartialEq, Debug)]
pub enum Role {
    /// Leader.
    Leader,
    /// Validating Peer.
    ValidatingPeer,
    /// Observing Peer.
    ObservingPeer,
    /// Proxy Tail.
    ProxyTail,
}

/// `Sumeragi` is the implementation of the consensus.
pub struct Sumeragi {
    public_key: PublicKey,
    private_key: PrivateKey,
    sorted_peers: Vec<PeerId>,
    max_faults: usize,
    peer_id: PeerId,
    /// PendingBlock in discussion this round
    voting_block: Option<SignedBlock>,
    blocks_sender: Arc<RwLock<ValidBlockSender>>,
    world_state_view: Arc<RwLock<WorldStateView>>,
}

impl Sumeragi {
    /// Default `Sumeragi` constructor.
    pub fn new(
        private_key: PrivateKey,
        peers: &[PeerId],
        peer_id: PeerId,
        max_faults: usize,
        blocks_sender: Arc<RwLock<ValidBlockSender>>,
        world_state_view: Arc<RwLock<WorldStateView>>,
    ) -> Result<Self, String> {
        if !peers.contains(&peer_id) {
            return Err("Peers list should contain this peer.".to_string());
        }
        let min_peers = 3 * max_faults + 1;
        if peers.len() >= min_peers {
            //TODO: get previous block hash from kura
            let mut sorted_peers = peers.to_vec();
            Self::sort_peers(&mut sorted_peers, None);
            Ok(Self {
                public_key: peer_id.public_key,
                private_key,
                sorted_peers,
                max_faults,
                peer_id,
                voting_block: None,
                blocks_sender,
                world_state_view,
            })
        } else {
            Err(format!("Not enough peers to be Byzantine fault tolerant. Expected a least {} peers, got {}", 3 * max_faults + 1, peers.len()))
        }
    }

    /// the leader of each round just uses the transactions they have at hand to create a block
    pub async fn round(&mut self, transactions: Vec<AcceptedTransaction>) -> Result<(), String> {
        if transactions.is_empty() {
            return Ok(());
        }
        if let Role::Leader = self.role() {
            let block = PendingBlock::new(transactions)
                //TODO: actually chain block?
                .chain_first()
                .sign(&self.public_key, &self.private_key)?;
            let minimum_quorum_of_peers = 2;
            if self.sorted_peers.len() < minimum_quorum_of_peers {
                let block = block.validate(&*self.world_state_view.read().await)?;
                self.blocks_sender.write().await.send(block).await;
                Ok(())
            } else {
                self.voting_block = Some(block.clone());
                let mut send_futures = Vec::new();
                for peer in self.validating_peers() {
                    send_futures.push(Message::Created(block.clone()).send_to(peer));
                }
                send_futures.push(Message::Created(block.clone()).send_to(self.proxy_tail()));
                futures::future::join_all(send_futures).await;
                Ok(())
            }
        } else {
            //TODO: send pending transactions to all peers and as leader check what tx have already been committed
            //Sends transactions to leader
            let mut send_futures = Vec::new();
            for transaction in &transactions {
                send_futures.push(Network::send_request_to(
                    &self.leader().address,
                    Request::new(
                        uri::INSTRUCTIONS_URI.to_string(),
                        RequestedTransaction::from(transaction).into(),
                    ),
                ));
            }
            let _results = futures::future::join_all(send_futures).await;
            Ok(())
        }
    }

    /// This method is used to sort list of peers.
    pub fn sort_peers(peers: &mut Vec<PeerId>, block_hash: Option<Hash>) {
        peers.sort_by(|p1, p2| p1.address.cmp(&p2.address));
        if let Some(block_hash) = block_hash {
            let mut rng = StdRng::from_seed(block_hash);
            peers.shuffle(&mut rng);
        }
    }

    /// This method is used to handle messages from other peers.
    #[log]
    pub async fn handle_message(&mut self, message: Message) -> Result<(), String> {
        //TODO: check that the messages come from the right peers (check roles, keys)
        match message {
            Message::Created(block) => match self.role() {
                Role::ValidatingPeer => {
                    let _result = Message::Signed(block.sign(&self.public_key, &self.private_key)?)
                        .send_to(self.proxy_tail())
                        .await;
                    //TODO: send to set b so they can observe
                }
                Role::ProxyTail => {
                    if self.voting_block.is_none() {
                        self.voting_block = Some(block)
                    }
                }
                _ => (),
            },
            Message::Signed(block) => {
                if let Role::ProxyTail = self.role() {
                    match self.voting_block.as_mut() {
                        Some(voting_block) => {
                            // TODO: verify signatures
                            for signature in block.signatures {
                                if !voting_block.signatures.contains(&signature) {
                                    voting_block.signatures.push(signature)
                                }
                            }
                        }
                        None => self.voting_block = Some(block),
                    };
                    if let Some(block) = self.voting_block.clone() {
                        if block.signatures.len() >= 2 * self.max_faults {
                            let block = block.sign(&self.public_key, &self.private_key)?;
                            let mut send_futures = Vec::new();
                            for peer in self.validating_peers() {
                                send_futures.push(Message::Committed(block.clone()).send_to(peer));
                            }
                            for peer in self.peers_set_b() {
                                send_futures.push(Message::Committed(block.clone()).send_to(peer));
                            }
                            send_futures
                                .push(Message::Committed(block.clone()).send_to(self.leader()));
                            let _results = futures::future::join_all(send_futures).await;
                            let block = block.validate(&*self.world_state_view.read().await)?;
                            let hash = block.hash();
                            self.blocks_sender.write().await.send(block).await;
                            self.next_round(hash);
                        }
                    }
                }
            }
            Message::Committed(block) => {
                //TODO: check if the block is the same as pending
                let block = block.validate(&*self.world_state_view.read().await)?;
                let hash = block.hash();
                self.blocks_sender.write().await.send(block).await;
                self.next_round(hash);
            }
        }
        Ok(())
    }

    fn next_round(&mut self, prev_block_hash: Hash) {
        Self::sort_peers(&mut self.sorted_peers, Some(prev_block_hash));
        self.voting_block = None;
    }

    fn peers_set_a(&self) -> &[PeerId] {
        let n_a_peers = 2 * self.max_faults + 1;
        &self.sorted_peers[..n_a_peers]
    }

    fn peers_set_b(&self) -> &[PeerId] {
        &self.sorted_peers[(2 * self.max_faults + 1)..]
    }

    fn leader(&self) -> &PeerId {
        self.peers_set_a()
            .first()
            .expect("Failed to get first peer.")
    }

    fn proxy_tail(&self) -> &PeerId {
        self.peers_set_a().last().expect("Failed to get last peer.")
    }

    fn validating_peers(&self) -> &[PeerId] {
        let a_set = self.peers_set_a();
        if a_set.len() > 1 {
            &a_set[1..(a_set.len() - 1)]
        } else {
            &[]
        }
    }

    fn role(&self) -> Role {
        if *self.leader() == self.peer_id {
            Role::Leader
        } else if *self.proxy_tail() == self.peer_id {
            Role::ProxyTail
        } else if self.validating_peers().contains(&self.peer_id) {
            Role::ValidatingPeer
        } else {
            Role::ObservingPeer
        }
    }
}

impl Debug for Sumeragi {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sumeragi")
            .field("public_key", &self.public_key)
            .field("sorted_peers", &self.sorted_peers)
            .field("max_faults", &self.max_faults)
            .field("peer_id", &self.peer_id)
            .field("voting_block", &self.voting_block)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{account, crypto, torii::Torii};
    use async_std::{prelude::*, sync, task};
    use std::time::Duration;

    #[test]
    #[should_panic]
    fn not_enough_peers() {
        let (blocks_sender, _blocks_reciever) = sync::channel(100);
        let (public_key, private_key) =
            crypto::generate_key_pair().expect("Failed to generate key pair.");
        let listen_address = "127.0.0.1".to_string();
        let this_peer = PeerId {
            address: listen_address.clone(),
            public_key,
        };
        Sumeragi::new(
            private_key,
            &[this_peer.clone()],
            this_peer.clone(),
            3,
            Arc::new(RwLock::new(blocks_sender)),
            Arc::new(RwLock::new(WorldStateView::new(Peer::new(
                listen_address.clone(),
                &vec![this_peer],
            )))),
        )
        .expect("Failed to create Sumeragi.");
    }

    #[test]
    fn different_order() {
        let mut peers1 = vec![
            PeerId {
                address: "127.0.0.1:7878".to_string(),
                public_key: [1u8; 32],
            },
            PeerId {
                address: "127.0.0.1:7879".to_string(),
                public_key: [2u8; 32],
            },
            PeerId {
                address: "127.0.0.1:7880".to_string(),
                public_key: [3u8; 32],
            },
        ];
        Sumeragi::sort_peers(&mut peers1, Some([1u8; 32]));
        let mut peers2 = peers1.clone();
        Sumeragi::sort_peers(&mut peers2, Some([2u8; 32]));
        assert_ne!(peers1, peers2);
    }

    #[test]
    fn same_order() {
        let mut peers1 = vec![
            PeerId {
                address: "127.0.0.1:7878".to_string(),
                public_key: [1u8; 32],
            },
            PeerId {
                address: "127.0.0.1:7879".to_string(),
                public_key: [2u8; 32],
            },
            PeerId {
                address: "127.0.0.1:7880".to_string(),
                public_key: [3u8; 32],
            },
        ];
        Sumeragi::sort_peers(&mut peers1, Some([1u8; 32]));
        let mut peers2 = peers1.clone();
        Sumeragi::sort_peers(&mut peers2, Some([1u8; 32]));
        assert_eq!(peers1, peers2);
    }

    #[cfg(feature = "network-mock")]
    #[async_std::test]
    async fn all_peers_commit_block() {
        let n_peers = 10;
        let max_faults = 1;
        let mut keys = Vec::new();
        let mut ids = Vec::new();
        let mut block_counters = Vec::new();
        for i in 0..n_peers {
            let (public_key, private_key) =
                crypto::generate_key_pair().expect("Failed to generate key pair.");
            keys.push((public_key, private_key));
            let peer_id = PeerId {
                address: format!("127.0.0.1:{}", 7878 + i),
                public_key,
            };
            ids.push(peer_id);
            block_counters.push(Arc::new(RwLock::new(0)));
        }
        let mut peers = Vec::new();
        for i in 0..n_peers {
            let (block_sender, mut block_receiver) = sync::channel(100);
            let (tx, _rx) = sync::channel(100);
            let (message_sender, mut message_receiver) = sync::channel(100);
            let wsv = Arc::new(RwLock::new(WorldStateView::new(Peer::new(
                "127.0.0.1:7878".to_string(),
                &ids,
            ))));
            let mut torii = Torii::new(ids[i].address.as_str(), wsv.clone(), tx, message_sender);
            task::spawn(async move {
                torii.start().await.expect("Torii failed.");
            });
            let sumeragi = Arc::new(RwLock::new(
                Sumeragi::new(
                    keys[i].1,
                    &ids,
                    ids[i].clone(),
                    max_faults,
                    Arc::new(RwLock::new(block_sender)),
                    wsv,
                )
                .expect("Failed to create Sumeragi."),
            ));
            peers.push(sumeragi.clone());
            task::spawn(async move {
                while let Some(message) = message_receiver.next().await {
                    let _result = sumeragi.write().await.handle_message(message).await;
                }
            });
            let block_counter = block_counters[i].clone();
            task::spawn(async move {
                while let Some(_block) = block_receiver.next().await {
                    *block_counter.write().await += 1;
                }
            });
        }
        peers
            .first()
            .expect("Failed to get first peer.")
            .write()
            .await
            .round(vec![RequestedTransaction::new(
                vec![],
                account::Id::new("entity", "domain"),
            )
            .accept()
            .expect("Failed to accpet tx.")])
            .await
            .expect("Round failed.");
        async_std::task::sleep(Duration::from_millis(1000)).await;
        for block_counter in block_counters {
            assert_eq!(*block_counter.write().await, 1);
        }
    }
}