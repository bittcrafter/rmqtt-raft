use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bincode::{deserialize, serialize};
use futures::channel::{mpsc, oneshot};
use futures::future::FutureExt;
use futures::SinkExt;
use log::{debug, info, warn};
use tikv_raft::eraftpb::{ConfChange, ConfChangeType};
use tokio::time::timeout;
use tonic::Request;

use crate::error::{Error, Result};
use crate::message::{Message, RaftResponse, Status};
use crate::raft_node::{Peer, RaftNode};
use crate::raft_server::RaftServer;
use crate::raft_service::connect;
use crate::raft_service::{ConfChange as RiteraftConfChange, Empty, ResultCode};
use crate::Config;

type DashMap<K, V> = dashmap::DashMap<K, V, ahash::RandomState>;

#[async_trait]
pub trait Store {
    async fn apply(&mut self, message: &[u8]) -> Result<Vec<u8>>;
    async fn query(&self, query: &[u8]) -> Result<Vec<u8>>;
    async fn snapshot(&self) -> Result<Vec<u8>>;
    async fn restore(&mut self, snapshot: &[u8]) -> Result<()>;
}

struct ProposalSender {
    proposal: Vec<u8>,
    client: Peer,
}

impl ProposalSender {
    async fn send(self) -> Result<RaftResponse> {
        match self.client.send_proposal(self.proposal).await {
            Ok(reply) => {
                let raft_response: RaftResponse = deserialize(&reply)?;
                Ok(raft_response)
            }
            Err(e) => {
                warn!("error sending proposal {:?}", e);
                Err(e)
            }
        }
    }
}

/// A mailbox to send messages to a running raft node.
#[derive(Clone)]
pub struct Mailbox {
    peers: Arc<DashMap<(u64, String), Peer>>,
    sender: mpsc::Sender<Message>,
    grpc_timeout: Duration,
    grpc_concurrency_limit: usize,
    grpc_breaker_threshold: u64,
    grpc_breaker_retry_interval: i64,
}

lazy_static::lazy_static! {
    static ref MAILBOX_SENDS: Arc<AtomicIsize> = Arc::new(AtomicIsize::new(0));
    static ref MAILBOX_QUERYS: Arc<AtomicIsize> = Arc::new(AtomicIsize::new(0));
}

pub fn active_mailbox_sends() -> isize {
    MAILBOX_SENDS.load(Ordering::SeqCst)
}

pub fn active_mailbox_querys() -> isize {
    MAILBOX_QUERYS.load(Ordering::SeqCst)
}

impl Mailbox {
    #[inline]
    pub fn pears(&self) -> Vec<(u64, Peer)> {
        self.peers
            .iter()
            .map(|p| {
                let (id, _) = p.key();
                (*id, p.value().clone())
            })
            .collect::<Vec<_>>()
    }

    #[inline]
    async fn peer(&self, leader_id: u64, leader_addr: String) -> Peer {
        self.peers
            .entry((leader_id, leader_addr.clone()))
            .or_insert_with(|| {
                Peer::new(
                    leader_addr,
                    self.grpc_timeout,
                    self.grpc_concurrency_limit,
                    self.grpc_breaker_threshold,
                    self.grpc_breaker_retry_interval,
                )
            })
            .clone()
    }

    #[inline]
    async fn send_to_leader(
        &self,
        proposal: Vec<u8>,
        leader_id: u64,
        leader_addr: String,
    ) -> Result<RaftResponse> {
        let peer = self.peer(leader_id, leader_addr).await;
        let proposal_sender = ProposalSender {
            proposal,
            client: peer,
        };
        proposal_sender.send().await
    }

    #[inline]
    pub async fn send(&self, message: Vec<u8>) -> Result<Vec<u8>> {
        MAILBOX_SENDS.fetch_add(1, Ordering::SeqCst);
        let reply = self._send(message).await;
        MAILBOX_SENDS.fetch_sub(1, Ordering::SeqCst);
        reply
    }

    #[inline]
    async fn _send(&self, message: Vec<u8>) -> Result<Vec<u8>> {
        let (target_leader_id, target_leader_addr) = {
            let (tx, rx) = oneshot::channel();
            let proposal = Message::Propose {
                proposal: message.clone(),
                chan: tx,
            };
            let mut sender = self.sender.clone();
            sender
                .try_send(proposal)
                .map_err(|e| Error::SendError(e.to_string()))?;
            let reply = timeout(self.grpc_timeout, rx).await;
            let reply = reply
                .map_err(|e| Error::RecvError(e.to_string()))?
                .map_err(|e| Error::RecvError(e.to_string()))?;
            match reply {
                RaftResponse::Response { data } => return Ok(data),
                RaftResponse::WrongLeader {
                    leader_id,
                    leader_addr,
                } => (leader_id, leader_addr),
                RaftResponse::Error(e) => return Err(Error::from(e)),
                _ => {
                    warn!("Recv other raft response: {:?}", reply);
                    return Err(Error::Unknown);
                }
            }
        };

        debug!(
            "This node not is Leader, leader_id: {:?}, leader_addr: {:?}",
            target_leader_id, target_leader_addr
        );

        if let Some(target_leader_addr) = target_leader_addr {
            if target_leader_id != 0 {
                return match self
                    .send_to_leader(message, target_leader_id, target_leader_addr.clone())
                    .await?
                {
                    RaftResponse::Response { data } => Ok(data),
                    RaftResponse::WrongLeader {
                        leader_id,
                        leader_addr,
                    } => {
                        warn!("The target node is not the Leader, target_leader_id: {}, target_leader_addr: {:?}, actual_leader_id: {}, actual_leader_addr: {:?}",
                            target_leader_id, target_leader_addr, leader_id, leader_addr);
                        Err(Error::NotLeader)
                    }
                    RaftResponse::Error(e) => Err(Error::from(e)),
                    _ => {
                        warn!("Recv other raft response, target_leader_id: {}, target_leader_addr: {:?}", target_leader_id, target_leader_addr);
                        Err(Error::Unknown)
                    }
                };
            }
        }

        Err(Error::LeaderNotExist)
    }

    #[inline]
    pub async fn query(&self, query: Vec<u8>) -> Result<Vec<u8>> {
        MAILBOX_QUERYS.fetch_add(1, Ordering::SeqCst);
        let reply = self._query(query).await;
        MAILBOX_QUERYS.fetch_sub(1, Ordering::SeqCst);
        reply
    }

    #[inline]
    async fn _query(&self, query: Vec<u8>) -> Result<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        let mut sender = self.sender.clone();
        match sender.try_send(Message::Query { query, chan: tx }) {
            Ok(()) => match timeout(self.grpc_timeout, rx).await {
                Ok(Ok(RaftResponse::Response { data })) => Ok(data),
                Ok(Ok(RaftResponse::Error(e))) => Err(Error::from(e)),
                _ => Err(Error::Unknown),
            },
            Err(e) => Err(Error::SendError(e.to_string())),
        }
    }

    #[inline]
    pub async fn leave(&self) -> Result<()> {
        let mut change = ConfChange::default();
        // set node id to 0, the node will set it to self when it receives it.
        change.set_node_id(0);
        change.set_change_type(ConfChangeType::RemoveNode);
        let mut sender = self.sender.clone();
        let (chan, rx) = oneshot::channel();
        match sender.send(Message::ConfigChange { change, chan }).await {
            Ok(()) => match rx.await {
                Ok(RaftResponse::Ok) => Ok(()),
                Ok(RaftResponse::Error(e)) => Err(Error::from(e)),
                _ => Err(Error::Unknown),
            },
            Err(e) => Err(Error::SendError(e.to_string())),
        }
    }

    #[inline]
    pub async fn status(&self) -> Result<Status> {
        let (tx, rx) = oneshot::channel();
        let mut sender = self.sender.clone();
        match sender.send(Message::Status { chan: tx }).await {
            Ok(_) => match timeout(self.grpc_timeout, rx).await {
                Ok(Ok(RaftResponse::Status(status))) => Ok(status),
                Ok(Ok(RaftResponse::Error(e))) => Err(Error::from(e)),
                _ => Err(Error::Unknown),
            },
            Err(e) => Err(Error::SendError(e.to_string())),
        }
    }
}

pub struct Raft<S: Store + 'static> {
    store: S,
    tx: mpsc::Sender<Message>,
    rx: mpsc::Receiver<Message>,
    addr: SocketAddr,
    logger: slog::Logger,
    cfg: Arc<Config>,
}

impl<S: Store + Send + Sync + 'static> Raft<S> {
    /// creates a new node with the given address and store.
    pub fn new<A: ToSocketAddrs>(
        addr: A,
        store: S,
        logger: slog::Logger,
        cfg: Config,
    ) -> Result<Self> {
        let addr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| Error::from("None"))?;
        let (tx, rx) = mpsc::channel(100_000);
        let cfg = Arc::new(cfg);
        Ok(Self {
            store,
            tx,
            rx,
            addr,
            logger,
            cfg,
        })
    }

    /// gets the node's `Mailbox`.
    pub fn mailbox(&self) -> Mailbox {
        Mailbox {
            peers: Arc::new(DashMap::default()),
            sender: self.tx.clone(),
            grpc_timeout: self.cfg.grpc_timeout,
            grpc_concurrency_limit: self.cfg.grpc_concurrency_limit,
            grpc_breaker_threshold: self.cfg.grpc_breaker_threshold,
            grpc_breaker_retry_interval: self.cfg.grpc_breaker_retry_interval.as_millis() as i64,
        }
    }

    /// find leader id and leader address
    pub async fn find_leader_info(&self, peer_addrs: Vec<String>) -> Result<Option<(u64, String)>> {
        let mut futs = Vec::new();
        for addr in peer_addrs {
            let fut = async {
                let _addr = addr.clone();
                match self.request_leader(addr).await {
                    Ok(reply) => Ok(reply),
                    Err(e) => {
                        info!("find_leader, addr: {}, {:?}", _addr, e);
                        Err(e)
                    }
                }
            };
            futs.push(fut.boxed());
        }

        let (leader_id, leader_addr) = match futures::future::select_ok(futs).await {
            Ok((Some((leader_id, leader_addr)), _)) => (leader_id, leader_addr),
            Ok((None, _)) => return Err(Error::LeaderNotExist),
            Err(_e) => return Ok(None),
        };

        info!("leader_id: {}, leader_addr: {}", leader_id, leader_addr);
        if leader_id == 0 {
            Ok(None)
        } else {
            Ok(Some((leader_id, leader_addr)))
        }
    }

    async fn request_leader(&self, peer_addr: String) -> Result<Option<(u64, String)>> {
        let (leader_id, leader_addr): (u64, String) = {
            let mut client = connect(&peer_addr, 1, self.cfg.grpc_timeout).await?;
            let response = client
                .request_id(Request::new(Empty::default()))
                .await?
                .into_inner();
            match response.code() {
                ResultCode::WrongLeader => {
                    let (leader_id, addr): (u64, Option<String>) = deserialize(&response.data)?;
                    if let Some(addr) = addr {
                        (leader_id, addr)
                    } else {
                        return Ok(None);
                    }
                }
                ResultCode::Ok => (deserialize(&response.data)?, peer_addr),
                ResultCode::Error => return Ok(None),
            }
        };
        Ok(Some((leader_id, leader_addr)))
    }

    /// Create a new leader for the cluster, with id 1. There has to be exactly one node in the
    /// cluster that is initialised that way
    pub async fn lead(self, node_id: u64) -> Result<()> {
        let node = RaftNode::new_leader(
            self.rx,
            self.tx.clone(),
            node_id,
            self.store,
            &self.logger,
            self.cfg.clone(),
        )?;

        let server = RaftServer::new(self.tx, self.addr, self.cfg.clone());
        let server_handle = async {
            if let Err(e) = server.run().await {
                warn!("raft server run error: {:?}", e);
                Err(e)
            } else {
                Ok(())
            }
        };
        let node_handle = async {
            if let Err(e) = node.run().await {
                warn!("node run error: {:?}", e);
                Err(e)
            } else {
                Ok(())
            }
        };

        tokio::try_join!(server_handle, node_handle)?;
        info!("leaving leader node");

        Ok(())
    }

    /// Tries to join a new cluster at `addr`, getting an id from the leader, or finding it if
    /// `addr` is not the current leader of the cluster
    pub async fn join(
        self,
        node_id: u64,
        leader_id: Option<u64>,
        leader_addr: String,
    ) -> Result<()> {
        // 1. try to discover the leader and obtain an id from it, if leader_id is None.
        info!("attempting to join peer cluster at {}", leader_addr);
        let (leader_id, leader_addr): (u64, String) = if let Some(leader_id) = leader_id {
            (leader_id, leader_addr)
        } else {
            self.request_leader(leader_addr)
                .await?
                .ok_or(Error::JoinError)?
        };

        // 2. run server and node to prepare for joining
        let mut node = RaftNode::new_follower(
            self.rx,
            self.tx.clone(),
            node_id,
            self.store,
            &self.logger,
            self.cfg.clone(),
        )?;
        let peer = node.add_peer(&leader_addr, leader_id);
        let mut client = peer.client().await?;
        let server = RaftServer::new(self.tx, self.addr, self.cfg.clone());
        let server_handle = async {
            if let Err(e) = server.run().await {
                warn!("raft server run error: {:?}", e);
                Err(e)
            } else {
                Ok(())
            }
        };

        //try remove from the cluster
        let mut change_remove = ConfChange::default();
        change_remove.set_node_id(node_id);
        change_remove.set_change_type(ConfChangeType::RemoveNode);
        let change_remove = RiteraftConfChange {
            inner: protobuf::Message::write_to_bytes(&change_remove)?,
        };

        let raft_response = client
            .change_config(Request::new(change_remove))
            .await?
            .into_inner();

        info!(
            "change_remove raft_response: {:?}",
            deserialize(&raft_response.inner)?
        );

        // 3. Join the cluster
        // TODO: handle wrong leader
        let mut change = ConfChange::default();
        change.set_node_id(node_id);
        change.set_change_type(ConfChangeType::AddNode);
        change.set_context(prost::bytes::Bytes::from(serialize(
            &self.addr.to_string(),
        )?));
        // change.set_context(serialize(&self.addr)?);

        let change = RiteraftConfChange {
            inner: protobuf::Message::write_to_bytes(&change)?,
        };
        let raft_response = client
            .change_config(Request::new(change))
            .await?
            .into_inner();
        if let RaftResponse::JoinSuccess {
            assigned_id,
            peer_addrs,
        } = deserialize(&raft_response.inner)?
        {
            info!(
                "change_config response.assigned_id: {:?}, peer_addrs: {:?}",
                assigned_id, peer_addrs
            );
            for (id, addr) in peer_addrs {
                if id != assigned_id {
                    node.add_peer(&addr, id);
                }
            }
        } else {
            return Err(Error::JoinError);
        }

        let node_handle = async {
            if let Err(e) = node.run().await {
                warn!("node run error: {:?}", e);
                Err(e)
            } else {
                Ok(())
            }
        };
        let _ = tokio::try_join!(server_handle, node_handle)?;
        info!("leaving follower node");
        Ok(())
    }
}
