// This file is almost identical to this
// https://github.com/webb-tools/anonima/blob/main/network/src/discovery.rs
// appropriate affiliation needs to be added here original header :
//
// Copyright 2020 ChainSafe Systems SPDX-License-Identifier: Apache-2.0, MIT

use std::collections::{HashSet, VecDeque};
use std::fmt::Display;
use std::task::{Context, Poll};
use std::time::Duration;
use std::{cmp, io};

use async_std::stream::{self, Interval};
use futures::StreamExt;
use libp2p::core::connection::{ConnectionId, ListenerId};
use libp2p::core::ConnectedPoint;
use libp2p::kad::handler::KademliaHandlerProto;
use libp2p::kad::store::MemoryStore;
use libp2p::kad::{Kademlia, KademliaConfig, KademliaEvent, QueryId};
use libp2p::mdns::{Mdns, MdnsConfig, MdnsEvent};
use libp2p::multiaddr::Protocol;
use libp2p::swarm::toggle::{Toggle, ToggleIntoProtoHandler};
use libp2p::swarm::{
    IntoProtocolsHandler, NetworkBehaviour, NetworkBehaviourAction,
    PollParameters, ProtocolsHandler,
};
use libp2p::{Multiaddr, PeerId};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    // TODO, it seems that NoKnownPeer is not exposed, could not find it
    #[error("Failed to bootstrap kademlia {0}")]
    FailedBootstrap(String),
    #[error("Failed to initialize mdns {0}")]
    FailedMdns(std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Event generated by the `DiscoveryBehaviour`.
#[derive(Debug)]
pub enum DiscoveryEvent {
    /// Event that notifies that we connected to the node with the given peer
    /// id.
    Connected(PeerId),

    /// Event that notifies that we disconnected with the node with the given
    /// peer id.
    Disconnected(PeerId),
}

/// `DiscoveryBehaviour` configuration.
#[derive(Clone)]
pub struct DiscoveryConfig {
    user_defined: Vec<Multiaddr>,
    discovery_max: u64,
    enable_kademlia: bool,
    enable_mdns: bool,
    kademlia_disjoint_query_paths: bool,
}
impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            user_defined: Vec::new(),
            discovery_max: u64::MAX,
            enable_kademlia: true,
            enable_mdns: true,
            kademlia_disjoint_query_paths: true,
        }
    }
}

#[derive(Default)]
pub struct DiscoveryConfigBuilder {
    config: DiscoveryConfig,
}

impl DiscoveryConfigBuilder {
    /// Set the number of active connections at which we pause discovery.
    pub fn discovery_limit(&mut self, limit: u64) -> &mut Self {
        self.config.discovery_max = limit;
        self
    }

    /// Set custom nodes which never expire, e.g. bootstrap or reserved nodes.
    pub fn with_user_defined<I>(&mut self, user_defined: I) -> &mut Self
    where
        I: IntoIterator<Item = Multiaddr>,
    {
        self.config.user_defined.extend(user_defined);
        self
    }

    pub fn use_kademlia_disjoint_query_paths(
        &mut self,
        value: bool,
    ) -> &mut Self {
        self.config.kademlia_disjoint_query_paths = value;
        self
    }

    /// Configures if mdns is enabled.
    pub fn with_mdns(&mut self, value: bool) -> &mut Self {
        self.config.enable_mdns = value;
        self
    }

    /// Configures if Kademlia is enabled.
    pub fn with_kademlia(&mut self, value: bool) -> &mut Self {
        self.config.enable_kademlia = value;
        self
    }

    pub fn build(&self) -> Result<DiscoveryConfig> {
        Ok(self.config.clone())
    }
}

/// Implementation of `NetworkBehaviour` that discovers the nodes on the
/// network.
pub struct DiscoveryBehaviour {
    /// User-defined list of nodes and their addresses. Typically includes
    /// bootstrap nodes and reserved nodes.
    user_defined: Vec<(PeerId, Multiaddr)>,
    /// Kademlia discovery.
    kademlia: Toggle<Kademlia<MemoryStore>>,
    /// Discovers nodes on the local network.
    mdns: Toggle<Mdns>,
    /// Stream that fires when we need to perform the next random Kademlia
    /// query.
    next_kad_random_query: Option<Interval>,
    /// After `next_kad_random_query` triggers, the next one triggers after
    /// this duration.
    duration_to_next_kad: Duration,
    /// Events to return in priority when polled.
    pending_events: VecDeque<DiscoveryEvent>,
    /// Number of nodes we're currently connected to.
    num_connections: u64,
    /// Keeps hash set of peers connected.
    peers: HashSet<PeerId>,
    /// Number of active connections to pause discovery on.
    discovery_max: u64,
}
impl Display for DiscoveryBehaviour {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            format!(
                "user_defined {:?}, kademlia: {:?}, mdns: {:?}, \
                 next_kad_random_query: {:?}, duration_to_next_kad {:?}, \
                 num_connection: {:?}, peers: {:?}, discovery_max: {:?}",
                self.user_defined,
                self.kademlia.is_enabled(),
                self.mdns.is_enabled(),
                self.next_kad_random_query,
                self.duration_to_next_kad,
                self.num_connections,
                self.peers,
                self.discovery_max
            )
            .as_str(),
        )
    }
}

impl DiscoveryBehaviour {
    /// Create a `DiscoveryBehaviour` from a config.
    pub fn new(
        local_peer_id: PeerId,
        config: DiscoveryConfig,
    ) -> Result<DiscoveryBehaviour> {
        let DiscoveryConfig {
            user_defined,
            discovery_max,
            enable_kademlia,
            enable_mdns,
            kademlia_disjoint_query_paths,
        } = config;

        let mut peers = HashSet::new();
        // TODO this parsing should probably be done when parsing config,
        // not initializing node
        let user_defined: Vec<(PeerId, Multiaddr)> = user_defined
            .into_iter()
            .filter_map(|mut multiaddr| {
                if let Some(Protocol::P2p(mh)) = multiaddr.pop() {
                    let peer_id = PeerId::from_multihash(mh).unwrap();
                    Some((peer_id, multiaddr))
                } else {
                    None
                }
            })
            .collect();

        let kademlia_opt = if enable_kademlia {
            // Kademlia config
            let store = MemoryStore::new(local_peer_id.to_owned());
            let mut kad_config = KademliaConfig::default();
            kad_config.disjoint_query_paths(kademlia_disjoint_query_paths);
            kad_config.set_protocol_name(
                format!("/anoma/kad/{}/kad/1.0.0", "anoma")
                    .as_bytes()
                    .to_vec(),
            );

            let mut kademlia =
                Kademlia::with_config(local_peer_id, store, kad_config);

            for (peer_id, addr) in user_defined.iter() {
                kademlia.add_address(&peer_id, addr.clone());
                peers.insert(*peer_id);
            }

            if let Err(err) = kademlia.bootstrap() {
                tracing::error!("failed to bootstrap kad : {:?}", err);
            };
            Some(kademlia)
        } else {
            None
        };

        let mdns_opt = if enable_mdns {
            let rt = tokio::runtime::Runtime::new().unwrap();
            Some(
                rt.block_on(Mdns::new(MdnsConfig::default()))
                    .map_err(Error::FailedMdns)?,
            )
        } else {
            None
        };

        Ok(DiscoveryBehaviour {
            user_defined,
            kademlia: kademlia_opt.into(),
            mdns: mdns_opt.into(),
            next_kad_random_query: None,
            duration_to_next_kad: Duration::from_secs(1),
            pending_events: VecDeque::new(),
            num_connections: 0,
            peers,
            discovery_max,
        })
    }
}

impl NetworkBehaviour for DiscoveryBehaviour {
    type OutEvent = DiscoveryEvent;
    type ProtocolsHandler =
        ToggleIntoProtoHandler<KademliaHandlerProto<QueryId>>;

    fn new_handler(&mut self) -> Self::ProtocolsHandler {
        self.kademlia.new_handler()
    }

    fn addresses_of_peer(&mut self, peer_id: &PeerId) -> Vec<Multiaddr> {
        let mut list = self
            .user_defined
            .iter()
            .filter_map(
                |(p, a)| if p == peer_id { Some(a.clone()) } else { None },
            )
            .collect::<Vec<_>>();

        list.extend(self.kademlia.addresses_of_peer(peer_id));

        list.extend(self.mdns.addresses_of_peer(peer_id));

        list
    }

    fn inject_connected(&mut self, peer_id: &PeerId) {
        self.peers.insert(*peer_id);
        self.pending_events
            .push_back(DiscoveryEvent::Connected(*peer_id));

        self.kademlia.inject_connected(peer_id)
    }

    fn inject_disconnected(&mut self, peer_id: &PeerId) {
        self.peers.remove(peer_id);
        self.pending_events
            .push_back(DiscoveryEvent::Disconnected(*peer_id));

        self.kademlia.inject_disconnected(peer_id)
    }

    fn inject_connection_established(
        &mut self,
        peer_id: &PeerId,
        conn: &ConnectionId,
        endpoint: &ConnectedPoint,
    ) {
        self.num_connections += 1;

        self.kademlia
            .inject_connection_established(peer_id, conn, endpoint)
    }

    fn inject_connection_closed(
        &mut self,
        peer_id: &PeerId,
        conn: &ConnectionId,
        endpoint: &ConnectedPoint,
    ) {
        self.num_connections -= 1;

        self.kademlia
            .inject_connection_closed(peer_id, conn, endpoint)
    }

    fn inject_address_change(
        &mut self,
        peer: &PeerId,
        id: &ConnectionId,
        old: &ConnectedPoint,
        new: &ConnectedPoint,
    ) {
        self.kademlia.inject_address_change(peer, id, old, new)
    }

    fn inject_event(
        &mut self,
        peer_id: PeerId,
        connection: ConnectionId,
        event: <<Self::ProtocolsHandler as IntoProtocolsHandler>::Handler as ProtocolsHandler>::OutEvent,
    ) {
        self.kademlia.inject_event(peer_id, connection, event)
    }

    fn inject_addr_reach_failure(
        &mut self,
        peer_id: Option<&PeerId>,
        addr: &Multiaddr,
        error: &dyn std::error::Error,
    ) {
        self.kademlia
            .inject_addr_reach_failure(peer_id, addr, error)
    }

    fn inject_dial_failure(&mut self, peer_id: &PeerId) {
        self.kademlia.inject_dial_failure(peer_id)
    }

    fn inject_new_listen_addr(&mut self, id: ListenerId, addr: &Multiaddr) {
        self.kademlia.inject_new_listen_addr(id, addr)
    }

    fn inject_expired_listen_addr(&mut self, id: ListenerId, addr: &Multiaddr) {
        self.kademlia.inject_expired_listen_addr(id, addr);
    }

    fn inject_listener_error(
        &mut self,
        id: ListenerId,
        err: &(dyn std::error::Error + 'static),
    ) {
        self.kademlia.inject_listener_error(id, err)
    }

    fn inject_listener_closed(
        &mut self,
        id: ListenerId,
        reason: std::result::Result<(), &io::Error>,
    ) {
        self.kademlia.inject_listener_closed(id, reason)
    }

    fn inject_new_external_addr(&mut self, addr: &Multiaddr) {
        self.kademlia.inject_new_external_addr(addr)
    }

    #[allow(clippy::type_complexity)]
    fn poll(
	&mut self,
	cx: &mut Context,
	params: &mut impl PollParameters,
    ) -> Poll<
	    NetworkBehaviourAction<
		    <<Self::ProtocolsHandler as IntoProtocolsHandler>::Handler as ProtocolsHandler>::InEvent,
		Self::OutEvent,
		>,
    >{
        // Immediately process the content of `discovered`.
        if let Some(ev) = self.pending_events.pop_front() {
            return Poll::Ready(NetworkBehaviourAction::GenerateEvent(ev));
        }

        // Poll Kademlia.
        while let Poll::Ready(ev) = self.kademlia.poll(cx, params) {
            match ev {
                NetworkBehaviourAction::GenerateEvent(ev) => match ev {
                    // Adding to Kademlia buckets is automatic with our config,
                    // no need to do manually.
                    KademliaEvent::RoutingUpdated { .. } => {}
                    KademliaEvent::UnroutablePeer { .. } => {}
                    KademliaEvent::RoutablePeer { .. } => {}
                    KademliaEvent::QueryResult { .. } => {}
                    KademliaEvent::PendingRoutablePeer { .. } => {}
                },
                NetworkBehaviourAction::DialAddress { address } => {
                    return Poll::Ready(NetworkBehaviourAction::DialAddress {
                        address,
                    });
                }
                NetworkBehaviourAction::DialPeer { peer_id, condition } => {
                    return Poll::Ready(NetworkBehaviourAction::DialPeer {
                        peer_id,
                        condition,
                    });
                }
                NetworkBehaviourAction::NotifyHandler {
                    peer_id,
                    handler,
                    event,
                } => {
                    return Poll::Ready(
                        NetworkBehaviourAction::NotifyHandler {
                            peer_id,
                            handler,
                            event,
                        },
                    );
                }
                NetworkBehaviourAction::ReportObservedAddr {
                    address,
                    score,
                } => {
                    return Poll::Ready(
                        NetworkBehaviourAction::ReportObservedAddr {
                            address,
                            score,
                        },
                    );
                }
            }
        }

        // Poll the stream that fires when we need to start a random Kademlia
        // query.
        if let Some(next_kad_random_query) = self.next_kad_random_query.as_mut()
        {
            while next_kad_random_query.poll_next_unpin(cx).is_ready() {
                if self.num_connections < self.discovery_max {
                    let random_peer_id = PeerId::random();
                    tracing::debug!(
                        "Libp2p <= Starting random Kademlia request for {:?}",
                        random_peer_id
                    );
                    if let Some(k) = self.kademlia.as_mut() {
                        k.get_closest_peers(random_peer_id);
                    }
                }

                *next_kad_random_query =
                    stream::interval(self.duration_to_next_kad);
                self.duration_to_next_kad = cmp::min(
                    self.duration_to_next_kad * 2,
                    Duration::from_secs(60),
                );
            }
        }

        // Poll mdns.
        while let Poll::Ready(ev) = self.mdns.poll(cx, params) {
            match ev {
                NetworkBehaviourAction::GenerateEvent(event) => match event {
                    MdnsEvent::Discovered(list) => {
                        if self.num_connections >= self.discovery_max {
                            tracing::info!(
                                "max reached {:?}, {:?}",
                                self.num_connections,
                                self.discovery_max
                            );
                            // Already over discovery max, don't add discovered
                            // peers.
                            // We could potentially buffer these addresses to be
                            // added later, but mdns
                            // is not an important use case and may be removed
                            // in future.
                            continue;
                        }

                        // Add any discovered peers to Kademlia
                        for (peer_id, multiaddr) in list {
                            if let Some(kad) = self.kademlia.as_mut() {
                                kad.add_address(&peer_id, multiaddr);
                            }
                        }
                    }
                    MdnsEvent::Expired(_) => {}
                },
                NetworkBehaviourAction::DialAddress { address } => {
                    return Poll::Ready(NetworkBehaviourAction::DialAddress {
                        address,
                    });
                }
                NetworkBehaviourAction::DialPeer { peer_id, condition } => {
                    return Poll::Ready(NetworkBehaviourAction::DialPeer {
                        peer_id,
                        condition,
                    });
                }
                // Nothing to notify handler
                NetworkBehaviourAction::NotifyHandler { event, .. } => {
                    match event {}
                }
                NetworkBehaviourAction::ReportObservedAddr {
                    address,
                    score,
                } => {
                    return Poll::Ready(
                        NetworkBehaviourAction::ReportObservedAddr {
                            address,
                            score,
                        },
                    );
                }
            }
        }
        Poll::Pending
    }
}
