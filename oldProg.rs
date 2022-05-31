use libp2p::core::transport::MemoryTransport;
use libp2p::kad::record::Key;
use libp2p::kad::store::MemoryStore;
use libp2p::kad::{Kademlia, KademliaConfig, KademliaEvent, QueryResult, PutRecordOk, AddProviderOk, PeerRecord, Record, Quorum};
use futures::prelude::*;
use libp2p::swarm::{Swarm, SwarmEvent, behaviour, NetworkBehaviourEventProcess, SwarmBuilder};
use libp2p::tcp::TcpConfig;
use libp2p::{identity, ping, Multiaddr, PeerId, NetworkBehaviour};
use libp2p::swarm::NetworkBehaviour;
use tokio::io::AsyncBufReadExt;
use std::env;
use std::error::Error;
use std::str::FromStr;
use libp2p::core::{transport::Transport};
use libp2p_gossipsub::{MessageAuthenticity, GossipsubEvent,Gossipsub};
use libp2p::mdns::{Mdns, MdnsConfig, MdnsEvent};
use async_std::{io, task};
use futures::select;
use libp2p::{noise, mplex};
use libp2p::tcp::TokioTcpConfig;

#[async_std::main]

async fn main() -> Result<(), Box<dyn Error>> {
    

    let opt = Opt::parse();
    let (mut network_client, mut network_events, network_event_loop) = network::new(opt.secret_key_seed).await?;

    


    println!("Hello, world!");
    Ok(())
}

mod network {
    use super::*;
    use async_trait::async_trait;
    use futures::channel::{mpsc, oneshot};
    use libp2p::core::either::EitherError;
    use libp2p::core::upgrade::{read_length_prefixed, write_length_prefixed, ProtocolName};
    use libp2p::identity;
    use libp2p::identity::ed25519;
    use libp2p::kad::record::store::MemoryStore;
    use libp2p::kad::{GetProvidersOk, Kademlia, KademliaEvent, QueryId, QueryResult};
    use libp2p::multiaddr::Protocol;
    use libp2p::request_response::{
        ProtocolSupport, RequestId, RequestResponse, RequestResponseCodec, RequestResponseEvent,
        RequestResponseMessage, ResponseChannel,
    };
    use libp2p::swarm::{ConnectionHandlerUpgrErr, SwarmBuilder, SwarmEvent};
    use libp2p::{NetworkBehaviour, Swarm};
    use std::collections::{HashMap, HashSet};
    use std::iter;

    pub async fn new(
        secret_key_seed: Option<u8>,
    ) -> Result<(Client, impl Stream<Item = Event>, EventLoop), Box<dyn Error>> {
        let id_keys = match secret_key_seed {
            Some(seed) => {
                let mut bytes = [0u8; 32];
                bytes[0] = seed;
                let secret_key = ed25519::SecretKey::from_bytes(&mut bytes).expect(
                    "this returns Err if len is wrong"
                );
                identity::Keypair::Ed25519(secret_key.into())
            }
            None => identity::Keypair::generate_ed25519(),
        };
        let peer_id = id_keys.public().to_peer_id();
    

        let swarm = SwarmBuilder::new(
            libp2p::development_transport(id_keys).await?,
            ComposedBehaviour {
                kademlia: Kademlia::new(peer_id, MemoryStore::new(peer_id)),
                request_response: RequestResponse::new(
                    FileExchangeCodec(),
                    iter::once((FileExchangeProtocol(), ProtocolSupport::Full)),
                    Default::default(),
                ),
            },
            peer_id,
        ).build();
        
        let (command_sender, command_receiver) = mpsc::channel(0);
        let (event_sender, event_receiver) = mpsc::channel(0);

        Ok((
            Client {
                sender: command_sender,
            },
            event_receiver,
            EventLoop::new(swarm, command_receiver, event_sender),
        ))
    }

    #[derive(Clone)]

    pub struct Client {
        sender: mpsc::Sender<Command>,
    }

    impl Client {
        pub async fn start_listening(
            &mut self,
            addr: Multiaddr,
        ) -> Result <(), Box<dyn Error>> {
            let (sender, receiver) = oneshot::channel();
            self.sender
                .send(Command::StartListening {addr, sender})
                .await
                .expect("Command receiver not to be dropped");
            receiver.await.expect("sender not to be dropped")
        }

        pub async fn dial(
            &mut self,
            peer_id: PeerId,
            peer_addr: Multiaddr,
        ) -> Result<(), Box<dyn Error>>{
            let (sender, receiver) = oneshot::channel();
            self.sender
                .send(Command::Dial {
                    peer_id,
                    peer_addr,
                    sender,
                })
                .await
                .expect("Command receiver not to be dropped");
            receiver.await.expect("Sender not to be dropped")
        }

        pub async fn start_providing(&mut self, file_name: String) -> HashSet<PeerId> {
            let (sender, receiver) = oneshot::channel();
            self.sender
                .send(Command::GetProviders { file_name, sender})
                .await
                .expect("command receiver not to be dropped");
            receiver.await.expect("sender not to be dropped")
        }

        pub async fn request_file(
            &mut self,
            peer: PeerId,
            file_name: String,
        ) -> Result <String, Box<dyn Error>> {
            let (sender, receiver) = oneshot::channel();
            self.sender
                .send(Command::RequestFile {
                    file_name,
                    peer,
                    sender,
                })
                .await
                .expect("Command receiver not to be dropped");
            receiver.await.expect("sender not to be dropped")
        }
    }

    pub struct EventLoop {
        swarm: Swarm<ComposedBehaviour>,
        command_receiver: mpsc::Receiver<Command>,
        event_sender: mpsc::Sender<Event>,
        pending_dial: HashMap<PeerId, oneshot::Sender<Result<(), Box<dyn Error>>>>,
        pending_start_providing: HashMap <QueryId, oneshot::Sender<()>>,
        pending_get_providers: HashMap <QueryId, oneshot::Sender<HashSet<PeerId>>>,
        pending_request_file: HashMap<RequestId, oneshot::Sender<Result<String, Box<dyn Error>>>>,
    }

    impl EventLoop {
        fn new(
            swarm: Swarm<ComposedBehaviour>,
            command_receiver: mpsc::Receiver<Command>,
            event_sender: mpsc::Sender<Event>,
        ) -> Self {
            Self { swarm, command_receiver,
                    event_sender,
                    pending_dial: Default::default(),
                    pending_start_providing: Default::default(),
                    pending_get_providers: Default::default(),
                    pending_request_file: Default::default(),
                }
        }

        pub async fn run(mut self) {
            loop {
                futures::select! {
                    event = self.swarm.next() => self.handle_event(event.expect("Swarm stream be inf")).await,
                    command = self.command_receiver.next() => match command {
                        Some(c) => self.handle_command(c).await,
                        None => return,
                    },
                }
            }
        }

        async fn handle_event(
            &mut self,
            event: SwarmEvent<ComposedEvent,EitherError<ConnectionHandlerUpgrErr<io::Error>, io::Error>,>,
        ) {
            match event {SwarmEvent::Behaviour(ComposedEvent::Kademlia(
                KademliaEvent::OutboundQueryCompleted { id, result: QueryResult::StartProviding(_),
                    .. 
                },
            )) => {
                let sender: oneshot::Sender<()> = self.pending_start_providing
                .remove(&id)
                .expect("completed prev pending query");
                let _ = sender.send(());
            }
            SwarmEvent::Behaviour(
                ComposedEvent::Kademlia(
                    KademliaEvent::OutboundQueryCompleted 
                    { id, result: QueryResult::GetProviders(Ok(GetProvidersOk { providers, .. })), .. }
                )) => {
                    let _ = self
                        .pending_get_providers
                        .remove(&id)
                        .expect("Completed query to be prev pending")
                        .send(providers);
                }
                SwarmEvent::Behaviour(ComposedEvent::Kademlia(_)) => {}
                SwarmEvent::Behaviour(ComposedEvent::RequestResponse(
                    RequestResponseEvent::Message { message, .. },
                )) => match message {RequestResponseMessage::Request { request, channel, .. } => {
                    self.event_sender
                        .send(Event::InboundRequest {
                            request: request.0,
                            channel,
                        })
                        .await
                        .expect("Event receiver not to be dropped")
                }
            }
        }
        }
    }
}
