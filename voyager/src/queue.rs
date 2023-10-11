use std::{
    collections::{HashMap, VecDeque},
    error::Error,
    fmt::{Debug, Display},
    marker::PhantomData,
    ops::Add,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use chain_utils::{
    evm::{Evm, EvmClientId, EvmClientType},
    union::{Union, UnionClientId, UnionClientType},
    Chain, ClientState, EventSource,
};
use frunk::{hlist_pat, HList};
use futures::{
    future::BoxFuture, stream, Future, FutureExt, StreamExt, TryFutureExt, TryStreamExt,
};
use hubble::hasura::{Datastore, HasuraDataStore, InsertDemoTx};
use pg_queue::ProcessFlow;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sqlx::{error::BoxDynError, PgPool};
use unionlabs::{
    ethereum_consts_traits::{Mainnet, Minimal},
    events::{
        ConnectionOpenAck, ConnectionOpenConfirm, ConnectionOpenInit, ConnectionOpenTry,
        CreateClient, IbcEvent, UpdateClient,
    },
    ibc::core::{
        channel::{
            self, channel::Channel, msg_acknowledgement::MsgAcknowledgement,
            msg_channel_open_ack::MsgChannelOpenAck,
            msg_channel_open_confirm::MsgChannelOpenConfirm,
            msg_channel_open_try::MsgChannelOpenTry, msg_recv_packet::MsgRecvPacket,
            packet::Packet,
        },
        client::{
            height::{Height, IsHeight},
            msg_create_client::MsgCreateClient,
        },
        commitment::merkle_prefix::MerklePrefix,
        connection::{
            self, msg_connection_open_ack::MsgConnectionOpenAck,
            msg_connection_open_confirm::MsgConnectionOpenConfirm,
            msg_connection_open_try::MsgConnectionOpenTry,
        },
    },
};

use crate::{
    chain::{
        evm::{CometblsMainnet, CometblsMinimal},
        proof::{
            self, AcknowledgementPath, ChannelEndPath, ClientConsensusStatePath, ClientStatePath,
            CommitmentPath, ConnectionPath, IbcStateRead,
        },
        union::{EthereumMainnet, EthereumMinimal},
        AnyChain, ChainOf, HeightOf, LightClient, QueryHeight,
    },
    config::Config,
    msg::{
        aggregate::{
            Aggregate, AggregateAckPacket, AggregateChannelHandshakeUpdateClient,
            AggregateChannelOpenAck, AggregateChannelOpenConfirm, AggregateChannelOpenTry,
            AggregateConnectionFetchFromChannelEnd, AggregateConnectionOpenAck,
            AggregateConnectionOpenConfirm, AggregateConnectionOpenTry, AggregateCreateClient,
            AggregateFetchCounterpartyStateProof, AggregateMsgAfterUpdate,
            AggregatePacketUpdateClient, AggregateRecvPacket, AggregateUpdateClient,
            AggregateUpdateClientFromClientId, AggregateUpdateClientWithCounterpartyChainId,
            AggregateWaitForTrustedHeight, ChannelHandshakeEvent,
            ConsensusStateProofAtLatestHeight, LightClientSpecificAggregate, PacketEvent,
        },
        data::{
            AcknowledgementProof, ChannelEnd, ChannelEndProof, ClientConsensusStateProof,
            ClientStateProof, CommitmentProof, ConnectionEnd, ConnectionProof, Data,
            PacketAcknowledgement, SelfClientState, SelfConsensusState, TrustedClientState,
        },
        event::Event,
        fetch::{
            Fetch, FetchChannelEnd, FetchConnectionEnd, FetchPacketAcknowledgement,
            FetchSelfClientState, FetchSelfConsensusState, FetchStateProof,
            FetchTrustedClientState, FetchUpdateHeaders, LightClientSpecificFetch,
        },
        identified,
        msg::{
            Msg, MsgAckPacketData, MsgChannelOpenAckData, MsgChannelOpenConfirmData,
            MsgChannelOpenTryData, MsgConnectionOpenAckData, MsgConnectionOpenConfirmData,
            MsgConnectionOpenTryData, MsgCreateClientData, MsgRecvPacketData,
        },
        wait::{Wait, WaitForBlock, WaitForTimestamp, WaitForTrustedHeight},
        AggregateData, AggregateReceiver, AnyLcMsg, ChainIdOf, DoAggregate, Identified, LcMsg,
        RelayerMsg,
    },
    queue::aggregate_data::UseAggregate,
    DELAY_PERIOD,
};

pub mod msg_server;

pub mod aggregate_data;

#[derive(Debug, Clone)]
pub struct Voyager<Q> {
    // TODO: Use some sort of typemap here instead of individual fields
    evm_minimal:
        HashMap<<<Evm<Minimal> as Chain>::SelfClientState as ClientState>::ChainId, Evm<Minimal>>,
    evm_mainnet:
        HashMap<<<Evm<Mainnet> as Chain>::SelfClientState as ClientState>::ChainId, Evm<Mainnet>>,
    union: HashMap<<<Union as Chain>::SelfClientState as ClientState>::ChainId, Union>,
    msg_server: msg_server::MsgServer,

    hasura_config: Option<hubble::hasura::HasuraDataStore>,

    queue: Q,
}

pub trait Queue: Clone + Send + Sync + Sized {
    /// Error type returned by this queue, representing errors that are out of control of the consumer (i.e. unable to connect to database, can't insert into row, can't deserialize row, etc)
    type Error: Debug + Display + Error;
    type Config: Debug + Clone + Serialize + DeserializeOwned;

    fn new(cfg: Self::Config) -> impl Future<Output = Result<Self, Self::Error>>;

    fn enqueue(&mut self, item: RelayerMsg) -> impl Future<Output = Result<(), Self::Error>> + '_;

    fn process<'a, F, Fut>(
        &'a mut self,
        f: F,
    ) -> impl Future<Output = Result<(), Self::Error>> + '_
    where
        F: (FnOnce(RelayerMsg) -> Fut) + 'a,
        Fut: Future<Output = ProcessFlow<RelayerMsg>> + 'a;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "type")]
pub enum AnyQueueConfig {
    InMemory,
    PgQueue(<PgQueue as Queue>::Config),
}

#[derive(Debug, Clone)]
pub enum AnyQueue {
    InMemory(InMemoryQueue),
    PgQueue(PgQueue),
}

#[derive(Debug, thiserror::Error)]
pub enum AnyQueueError {
    #[error("{0}")]
    InMemory(#[from] <InMemoryQueue as Queue>::Error),
    #[error("{0}")]
    PgQueue(#[from] <PgQueue as Queue>::Error),
}

impl Queue for AnyQueue {
    type Error = AnyQueueError;
    type Config = AnyQueueConfig;

    fn new(cfg: Self::Config) -> impl Future<Output = Result<Self, Self::Error>> {
        async move {
            Ok(match cfg {
                AnyQueueConfig::InMemory => Self::InMemory(InMemoryQueue::new(()).await?),
                AnyQueueConfig::PgQueue(cfg) => Self::PgQueue(PgQueue::new(cfg).await?),
            })
        }
    }

    fn enqueue(&mut self, item: RelayerMsg) -> impl Future<Output = Result<(), Self::Error>> + '_ {
        async move {
            Ok(match self {
                AnyQueue::InMemory(queue) => queue.enqueue(item).await?,
                AnyQueue::PgQueue(queue) => queue.enqueue(item).await?,
            })
        }
    }

    fn process<'a, F, Fut>(&'a mut self, f: F) -> impl Future<Output = Result<(), Self::Error>> + '_
    where
        F: (FnOnce(RelayerMsg) -> Fut) + 'a,
        Fut: Future<Output = ProcessFlow<RelayerMsg>> + 'a,
    {
        async move {
            Ok(match self {
                AnyQueue::InMemory(queue) => queue.process(f).await?,
                AnyQueue::PgQueue(queue) => queue.process(f).await?,
            })
        }
    }
}

#[derive(Debug, Clone)]
pub struct InMemoryQueue(Arc<Mutex<VecDeque<RelayerMsg>>>);

impl Queue for InMemoryQueue {
    type Error = std::convert::Infallible;
    type Config = ();

    fn new(_cfg: Self::Config) -> impl Future<Output = Result<Self, Self::Error>> {
        futures::future::ok(Self(Arc::new(Mutex::new(VecDeque::default()))))
    }

    fn enqueue(&mut self, item: RelayerMsg) -> impl Future<Output = Result<(), Self::Error>> + '_ {
        self.0.lock().expect("mutex is poisoned").push_back(item);
        futures::future::ok(())
    }

    fn process<'a, F, Fut>(&'a mut self, f: F) -> impl Future<Output = Result<(), Self::Error>> + '_
    where
        F: (FnOnce(RelayerMsg) -> Fut) + 'a,
        Fut: Future<Output = ProcessFlow<RelayerMsg>> + 'a,
    {
        async move {
            let queue = &mut self.0.lock().expect("mutex is poisoned");

            match queue.pop_front() {
                Some(msg) => match f(msg.clone()).await {
                    ProcessFlow::Success(new_msgs) => {
                        queue.extend(new_msgs);
                        Ok(())
                    }
                    ProcessFlow::Requeue => {
                        queue.push_front(msg);
                        Ok(())
                    }
                    ProcessFlow::Fail(why) => panic!("{why}"),
                },
                None => Ok(()),
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct PgQueue(pg_queue::Queue<RelayerMsg>, sqlx::PgPool);

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PgQueueConfig {
    database_url: String,
}

impl Queue for PgQueue {
    type Error = sqlx::Error;

    type Config = PgQueueConfig;

    fn new(cfg: Self::Config) -> impl Future<Output = Result<Self, Self::Error>> {
        async move {
            Ok(Self(
                pg_queue::Queue::new(),
                PgPool::connect(&cfg.database_url).await?,
            ))
        }
    }

    fn enqueue(&mut self, item: RelayerMsg) -> impl Future<Output = Result<(), Self::Error>> + '_ {
        pg_queue::Queue::<RelayerMsg>::enqueue(&self.1, item)
    }

    fn process<'a, F, Fut>(&'a mut self, f: F) -> impl Future<Output = Result<(), Self::Error>> + '_
    where
        F: (FnOnce(RelayerMsg) -> Fut) + 'a,
        Fut: Future<Output = ProcessFlow<RelayerMsg>> + 'a,
    {
        pg_queue::Queue::<RelayerMsg>::process(&self.1, f)
    }
}

impl<Q: Queue> Voyager<Q> {
    pub async fn new(config: Config<Q>) -> Self {
        if config.voyager.hasura.is_none() {
            tracing::warn!("no hasura config supplied, no messages will be indexed");
        }

        let mut union = HashMap::new();
        let mut evm_minimal = HashMap::new();
        let mut evm_mainnet = HashMap::new();

        for (chain_name, chain_config) in config.chain {
            let chain = AnyChain::try_from_config(&config.voyager, chain_config).await;

            match chain {
                AnyChain::Union(c) => {
                    let chain_id = c.chain_id.clone();
                    assert!(union.insert(c.chain_id.clone(), c).is_none());

                    tracing::info!(
                        chain_name,
                        chain_id,
                        chain_type = "Union",
                        "registered chain"
                    );
                }
                AnyChain::EvmMainnet(c) => {
                    let chain_id = c.chain_id;
                    assert!(evm_mainnet.insert(c.chain_id, c).is_none());

                    tracing::info!(
                        chain_name,
                        %chain_id,
                        chain_type = "EvmMainnet",
                        "registered chain"
                    );
                }
                AnyChain::EvmMinimal(c) => {
                    let chain_id = c.chain_id;
                    assert!(evm_minimal.insert(c.chain_id, c).is_none());

                    tracing::info!(
                        chain_name,
                        %chain_id,
                        chain_type = "EvmMinimal",
                        "registered chain"
                    );
                }
            }
        }

        Self {
            evm_minimal,
            evm_mainnet,
            union,
            msg_server: msg_server::MsgServer,
            hasura_config: config
                .voyager
                .hasura
                .map(|hc| HasuraDataStore::new(reqwest::Client::new(), hc.url, hc.secret)),
            queue: Q::new(config.voyager.queue).await.unwrap(),
        }
    }

    pub async fn run(self) {
        let mut events = Box::pin(stream::select_all([
            stream::iter(&self.evm_minimal)
                .map(|(chain_id, chain)| {
                    chain
                        .events(())
                        // .inspect_ok(|e| {
                        //     dbg!(e);
                        // })
                        .map_ok(move |event| {
                            if chain_id != &event.chain_id {
                                tracing::warn!(
                                    "chain {chain_id} produced an event from chain {}",
                                    event.chain_id
                                );
                            }

                            let event = match event.event {
                                IbcEvent::CreateClient(create_client) => {
                                    match create_client.client_type {
                                        EvmClientType::Cometbls(_) => {
                                            LcMsg::<CometblsMinimal>::Event(Identified {
                                                chain_id: *chain_id,
                                                data: Event::Ibc(crate::msg::event::IbcEvent {
                                                    block_hash: event.block_hash,
                                                    height: event.height,
                                                    event: IbcEvent::CreateClient(CreateClient {
                                                        client_id: create_client
                                                            .client_id
                                                            .try_into()
                                                            .expect(
                                                                "only cometbls supported currently",
                                                            ),
                                                        client_type: chain_utils::evm::Cometbls,
                                                        consensus_height: create_client
                                                            .consensus_height,
                                                    }),
                                                }),
                                            })
                                        }
                                    }
                                }
                                IbcEvent::UpdateClient(_) => todo!(),
                                IbcEvent::ClientMisbehaviour(_) => todo!(),
                                IbcEvent::SubmitEvidence(_) => todo!(),
                                IbcEvent::ConnectionOpenInit(init) => match init.client_id {
                                    EvmClientId::Cometbls(client_id) => {
                                        if let Ok(counterparty_client_id) = init
                                            .counterparty_client_id
                                            .parse::<<EthereumMinimal as LightClient>::ClientId>()
                                        {
                                            LcMsg::<CometblsMinimal>::Event(Identified {
                                                chain_id: event.chain_id,
                                                data: Event::Ibc(crate::msg::event::IbcEvent {
                                                    block_hash: event.block_hash,
                                                    height: event.height,
                                                    event: IbcEvent::ConnectionOpenInit(
                                                        ConnectionOpenInit {
                                                            connection_id: init.connection_id,
                                                            client_id,
                                                            counterparty_client_id,
                                                            counterparty_connection_id: init
                                                                .counterparty_connection_id,
                                                        },
                                                    ),
                                                }),
                                            })
                                        } else {
                                            panic!()
                                        }
                                    }
                                },
                                IbcEvent::ConnectionOpenTry(try_) => match try_.client_id {
                                    EvmClientId::Cometbls(client_id) => {
                                        if let Ok(counterparty_client_id) = try_
                                            .counterparty_client_id
                                            .parse::<<EthereumMinimal as LightClient>::ClientId>()
                                        {
                                            LcMsg::<CometblsMinimal>::Event(Identified {
                                                chain_id: event.chain_id,
                                                data: Event::Ibc(crate::msg::event::IbcEvent {
                                                    block_hash: event.block_hash,
                                                    height: event.height,
                                                    event: IbcEvent::ConnectionOpenTry(
                                                        ConnectionOpenTry {
                                                            connection_id: try_.connection_id,
                                                            client_id,
                                                            counterparty_client_id,
                                                            counterparty_connection_id: try_
                                                                .counterparty_connection_id,
                                                        },
                                                    ),
                                                }),
                                            })
                                        } else {
                                            panic!()
                                        }
                                    }
                                },
                                IbcEvent::ConnectionOpenAck(ack) => match ack.client_id {
                                    EvmClientId::Cometbls(client_id) => {
                                        if let Ok(counterparty_client_id) = ack
                                            .counterparty_client_id
                                            .parse::<<EthereumMinimal as LightClient>::ClientId>()
                                        {
                                            LcMsg::<CometblsMinimal>::Event(Identified {
                                                chain_id: event.chain_id,
                                                data: Event::Ibc(crate::msg::event::IbcEvent {
                                                    block_hash: event.block_hash,
                                                    height: event.height,
                                                    event: IbcEvent::ConnectionOpenAck(
                                                        ConnectionOpenAck {
                                                            connection_id: ack.connection_id,
                                                            client_id,
                                                            counterparty_client_id,
                                                            counterparty_connection_id: ack
                                                                .counterparty_connection_id,
                                                        },
                                                    ),
                                                }),
                                            })
                                        } else {
                                            panic!()
                                        }
                                    }
                                },
                                IbcEvent::ConnectionOpenConfirm(confirm) => match confirm.client_id
                                {
                                    EvmClientId::Cometbls(client_id) => {
                                        if let Ok(counterparty_client_id) = confirm
                                            .counterparty_client_id
                                            .parse::<<EthereumMinimal as LightClient>::ClientId>()
                                        {
                                            LcMsg::<CometblsMinimal>::Event(Identified {
                                                chain_id: event.chain_id,
                                                data: Event::Ibc(crate::msg::event::IbcEvent {
                                                    block_hash: event.block_hash,
                                                    height: event.height,
                                                    event: IbcEvent::ConnectionOpenConfirm(
                                                        ConnectionOpenConfirm {
                                                            connection_id: confirm.connection_id,
                                                            client_id,
                                                            counterparty_client_id,
                                                            counterparty_connection_id: confirm
                                                                .counterparty_connection_id,
                                                        },
                                                    ),
                                                }),
                                            })
                                        } else {
                                            panic!()
                                        }
                                    }
                                },
                                // NOTE: CometblsMinimal assumed for now for channel events
                                IbcEvent::ChannelOpenInit(init) => {
                                    LcMsg::<CometblsMinimal>::Event(Identified {
                                        chain_id: event.chain_id,
                                        data: Event::Ibc(crate::msg::event::IbcEvent {
                                            block_hash: event.block_hash,
                                            height: event.height,
                                            event: IbcEvent::ChannelOpenInit(init),
                                        }),
                                    })
                                }
                                IbcEvent::ChannelOpenTry(try_) => {
                                    LcMsg::<CometblsMinimal>::Event(Identified {
                                        chain_id: event.chain_id,
                                        data: Event::Ibc(crate::msg::event::IbcEvent {
                                            block_hash: event.block_hash,
                                            height: event.height,
                                            event: IbcEvent::ChannelOpenTry(try_),
                                        }),
                                    })
                                }
                                IbcEvent::ChannelOpenAck(ack) => {
                                    LcMsg::<CometblsMinimal>::Event(Identified {
                                        chain_id: event.chain_id,
                                        data: Event::Ibc(crate::msg::event::IbcEvent {
                                            block_hash: event.block_hash,
                                            height: event.height,
                                            event: IbcEvent::ChannelOpenAck(ack),
                                        }),
                                    })
                                }
                                IbcEvent::ChannelOpenConfirm(confirm) => {
                                    LcMsg::<CometblsMinimal>::Event(Identified {
                                        chain_id: event.chain_id,
                                        data: Event::Ibc(crate::msg::event::IbcEvent {
                                            block_hash: event.block_hash,
                                            height: event.height,
                                            event: IbcEvent::ChannelOpenConfirm(confirm),
                                        }),
                                    })
                                }

                                // IbcEvent::WriteAcknowledgement(_) => todo!(),
                                IbcEvent::RecvPacket(packet) => {
                                    LcMsg::<CometblsMinimal>::Event(Identified {
                                        chain_id: event.chain_id,
                                        data: Event::Ibc(crate::msg::event::IbcEvent {
                                            block_hash: event.block_hash,
                                            height: event.height,
                                            event: IbcEvent::RecvPacket(packet),
                                        }),
                                    })
                                }
                                IbcEvent::SendPacket(packet) => {
                                    LcMsg::<CometblsMinimal>::Event(Identified {
                                        chain_id: event.chain_id,
                                        data: Event::Ibc(crate::msg::event::IbcEvent {
                                            block_hash: event.block_hash,
                                            height: event.height,
                                            event: IbcEvent::SendPacket(packet),
                                        }),
                                    })
                                }
                                IbcEvent::AcknowledgePacket(_) => todo!(),
                                IbcEvent::TimeoutPacket(_) => todo!(),
                                IbcEvent::WriteAcknowledgement(_) => todo!(),
                            };

                            RelayerMsg::Lc(AnyLcMsg::from(event))
                        })
                        .map_err(|x| Box::new(x) as Box<dyn Debug>)
                })
                .flatten()
                .boxed(),
            stream::iter(&self.union)
                .map(|(chain_id, chain)| {
                    chain
                        .events(())
                        .map_ok(move |event| {
                            if chain_id != &event.chain_id {
                                tracing::warn!(
                                    "chain {chain_id} produced an event from chain {}",
                                    event.chain_id
                                );
                            }

                            let event = match event.event {
                                IbcEvent::CreateClient(create_client) => {
                                    match create_client.client_type {
                                        // TODO: Introspect the contract for a client type beyond just "wasm"
                                        UnionClientType::Wasm(_) => {
                                            LcMsg::<EthereumMinimal>::Event(Identified {
                                                chain_id: chain_id.clone(),
                                                data: Event::Ibc(crate::msg::event::IbcEvent {
                                                    block_hash: event.block_hash,
                                                    height: event.height,
                                                    event: IbcEvent::CreateClient(CreateClient {
                                                        client_id: create_client
                                                            .client_id
                                                            .try_into()
                                                            .expect(
                                                                "only cometbls supported currently",
                                                            ),
                                                        client_type: chain_utils::union::Wasm,
                                                        consensus_height: create_client
                                                            .consensus_height,
                                                    }),
                                                }),
                                            })
                                        }
                                        UnionClientType::Tendermint(_) => todo!(),
                                    }
                                }
                                IbcEvent::UpdateClient(updated) => match updated.client_id {
                                    UnionClientId::Wasm(client_id) => {
                                        LcMsg::<EthereumMinimal>::Event(Identified {
                                            chain_id: event.chain_id,
                                            data: Event::Ibc(crate::msg::event::IbcEvent {
                                                block_hash: event.block_hash,
                                                height: event.height,
                                                event: IbcEvent::UpdateClient(UpdateClient {
                                                    client_id,
                                                    client_type: chain_utils::union::Wasm,
                                                    consensus_heights: updated.consensus_heights,
                                                    header: updated.header,
                                                }),
                                            }),
                                        })
                                    }
                                    UnionClientId::Tendermint(_) => todo!(),
                                },
                                IbcEvent::ClientMisbehaviour(_) => todo!(),
                                IbcEvent::SubmitEvidence(_) => todo!(),
                                IbcEvent::ConnectionOpenInit(init) => match init.client_id {
                                    UnionClientId::Wasm(client_id) => {
                                        if let Ok(counterparty_client_id) = init
                                            .counterparty_client_id
                                            .parse::<<CometblsMinimal as LightClient>::ClientId>()
                                        {
                                            LcMsg::<EthereumMinimal>::Event(Identified {
                                                chain_id: event.chain_id,
                                                data: Event::Ibc(crate::msg::event::IbcEvent {
                                                    block_hash: event.block_hash,
                                                    height: event.height,
                                                    event: IbcEvent::ConnectionOpenInit(
                                                        ConnectionOpenInit {
                                                            connection_id: init.connection_id,
                                                            client_id,
                                                            counterparty_client_id,
                                                            counterparty_connection_id: init
                                                                .counterparty_connection_id,
                                                        },
                                                    ),
                                                }),
                                            })
                                        } else {
                                            panic!()
                                        }
                                    }
                                    UnionClientId::Tendermint(_) => todo!(),
                                },
                                IbcEvent::ConnectionOpenTry(try_) => match try_.client_id {
                                    UnionClientId::Wasm(client_id) => {
                                        if let Ok(counterparty_client_id) = try_
                                            .counterparty_client_id
                                            .parse::<<CometblsMinimal as LightClient>::ClientId>()
                                        {
                                            LcMsg::<EthereumMinimal>::Event(Identified {
                                                chain_id: event.chain_id,
                                                data: Event::Ibc(crate::msg::event::IbcEvent {
                                                    block_hash: event.block_hash,
                                                    height: event.height,
                                                    event: IbcEvent::ConnectionOpenTry(
                                                        ConnectionOpenTry {
                                                            connection_id: try_.connection_id,
                                                            client_id,
                                                            counterparty_client_id,
                                                            counterparty_connection_id: try_
                                                                .counterparty_connection_id,
                                                        },
                                                    ),
                                                }),
                                            })
                                        } else {
                                            panic!()
                                        }
                                    }
                                    UnionClientId::Tendermint(_) => todo!(),
                                },
                                IbcEvent::ConnectionOpenAck(ack) => match ack.client_id {
                                    UnionClientId::Wasm(client_id) => {
                                        if let Ok(counterparty_client_id) = ack
                                            .counterparty_client_id
                                            .parse::<<CometblsMinimal as LightClient>::ClientId>()
                                        {
                                            LcMsg::<EthereumMinimal>::Event(Identified {
                                                chain_id: event.chain_id,
                                                data: Event::Ibc(crate::msg::event::IbcEvent {
                                                    block_hash: event.block_hash,
                                                    height: event.height,
                                                    event: IbcEvent::ConnectionOpenAck(
                                                        ConnectionOpenAck {
                                                            connection_id: ack.connection_id,
                                                            client_id,
                                                            counterparty_client_id,
                                                            counterparty_connection_id: ack
                                                                .counterparty_connection_id,
                                                        },
                                                    ),
                                                }),
                                            })
                                        } else {
                                            panic!()
                                        }
                                    }
                                    UnionClientId::Tendermint(_) => todo!(),
                                },
                                IbcEvent::ConnectionOpenConfirm(confirm) => match confirm.client_id
                                {
                                    UnionClientId::Wasm(client_id) => {
                                        if let Ok(counterparty_client_id) = confirm
                                            .counterparty_client_id
                                            .parse::<<CometblsMinimal as LightClient>::ClientId>()
                                        {
                                            LcMsg::<EthereumMinimal>::Event(Identified {
                                                chain_id: event.chain_id,
                                                data: Event::Ibc(crate::msg::event::IbcEvent {
                                                    block_hash: event.block_hash,
                                                    height: event.height,
                                                    event: IbcEvent::ConnectionOpenConfirm(
                                                        ConnectionOpenConfirm {
                                                            connection_id: confirm.connection_id,
                                                            client_id,
                                                            counterparty_client_id,
                                                            counterparty_connection_id: confirm
                                                                .counterparty_connection_id,
                                                        },
                                                    ),
                                                }),
                                            })
                                        } else {
                                            panic!()
                                        }
                                    }
                                    UnionClientId::Tendermint(_) => todo!(),
                                },

                                // NOTE: EthereumMinimal assumed for now for channel events
                                IbcEvent::ChannelOpenInit(init) => {
                                    LcMsg::<EthereumMinimal>::Event(Identified {
                                        chain_id: event.chain_id,
                                        data: Event::Ibc(crate::msg::event::IbcEvent {
                                            block_hash: event.block_hash,
                                            height: event.height,
                                            event: IbcEvent::ChannelOpenInit(init),
                                        }),
                                    })
                                }
                                IbcEvent::ChannelOpenTry(try_) => {
                                    LcMsg::<EthereumMinimal>::Event(Identified {
                                        chain_id: event.chain_id,
                                        data: Event::Ibc(crate::msg::event::IbcEvent {
                                            block_hash: event.block_hash,
                                            height: event.height,
                                            event: IbcEvent::ChannelOpenTry(try_),
                                        }),
                                    })
                                }
                                IbcEvent::ChannelOpenAck(ack) => {
                                    LcMsg::<EthereumMinimal>::Event(Identified {
                                        chain_id: event.chain_id,
                                        data: Event::Ibc(crate::msg::event::IbcEvent {
                                            block_hash: event.block_hash,
                                            height: event.height,
                                            event: IbcEvent::ChannelOpenAck(ack),
                                        }),
                                    })
                                }
                                IbcEvent::ChannelOpenConfirm(confirm) => {
                                    LcMsg::<EthereumMinimal>::Event(Identified {
                                        chain_id: event.chain_id,
                                        data: Event::Ibc(crate::msg::event::IbcEvent {
                                            block_hash: event.block_hash,
                                            height: event.height,
                                            event: IbcEvent::ChannelOpenConfirm(confirm),
                                        }),
                                    })
                                }

                                // IbcEvent::WriteAcknowledgement(_) => todo!(),
                                IbcEvent::RecvPacket(recv_packet) => {
                                    LcMsg::<EthereumMinimal>::Event(Identified {
                                        chain_id: event.chain_id,
                                        data: Event::Ibc(crate::msg::event::IbcEvent {
                                            block_hash: event.block_hash,
                                            height: event.height,
                                            event: IbcEvent::RecvPacket(recv_packet),
                                        }),
                                    })
                                }
                                IbcEvent::SendPacket(send_packet) => {
                                    LcMsg::<EthereumMinimal>::Event(Identified {
                                        chain_id: event.chain_id,
                                        data: Event::Ibc(crate::msg::event::IbcEvent {
                                            block_hash: event.block_hash,
                                            height: event.height,
                                            event: IbcEvent::SendPacket(send_packet),
                                        }),
                                    })
                                }
                                IbcEvent::AcknowledgePacket(_) => todo!(),
                                IbcEvent::TimeoutPacket(_) => todo!(),
                                IbcEvent::WriteAcknowledgement(write_ack) => {
                                    LcMsg::<EthereumMinimal>::Event(Identified {
                                        chain_id: event.chain_id,
                                        data: Event::Ibc(crate::msg::event::IbcEvent {
                                            block_hash: event.block_hash,
                                            height: event.height,
                                            event: IbcEvent::WriteAcknowledgement(write_ack),
                                        }),
                                    })
                                }
                            };

                            RelayerMsg::Lc(AnyLcMsg::from(event))
                        })
                        .map_err(|x| Box::new(x) as Box<dyn Debug>)
                })
                .flatten()
                .boxed(),
            self.msg_server
                .events(())
                .map_err(|x| Box::new(x) as Box<dyn Debug>)
                .boxed(),
        ]));

        let mut queue = self.queue.clone();

        loop {
            let buffer_time = tokio::time::sleep(Duration::from_secs(2));

            tracing::debug!("checking for new messages");

            tokio::select! {
                msg = events.select_next_some() => {
                    let msg = msg.unwrap();

                    tracing::info!(
                        json = %serde_json::to_string(&msg).unwrap(),
                        "received new message",
                    );

                    queue.enqueue(msg).await.unwrap();
                }
                _ = buffer_time => {
                    tracing::debug!("no new messages");
                }
            }

            queue
                .process(|msg| async {
                    let new_msgs = self.handle_msg(msg, 0).await;

                    ProcessFlow::Success(new_msgs)
                })
                .await
                .unwrap();
        }
    }

    async fn handle_buffered_queue(&self, mut buffered_queue: VecDeque<RelayerMsg>) {
        // TODO: Introspect the messages and factor out all messages that require an update
        while let Some(msg) = buffered_queue.pop_front() {
            let new_msgs = self.handle_msg(msg, 0).await;

            for msg in new_msgs {
                buffered_queue.push_back(msg)
            }
        }
    }

    // NOTE: Box is required bc recursion
    fn handle_msg(&self, msg: RelayerMsg, depth: usize) -> BoxFuture<'_, Vec<RelayerMsg>> {
        tracing::info!(
            depth,
            json = %serde_json::to_string(&msg).unwrap(),
            "handling message",
        );

        async move {
            match msg {
                RelayerMsg::Lc(any_lc_msg) => {
                    if let Some(hasura) = &self.hasura_config {
                        hasura
                            .do_post::<InsertDemoTx>(hubble::hasura::insert_demo_tx::Variables {
                                data: serde_json::to_value(&any_lc_msg).unwrap(),
                            })
                            .await
                            .unwrap();
                    }

                    match any_lc_msg {
                        AnyLcMsg::EthereumMainnet(msg) => {
                            self.handle_msg_generic::<EthereumMainnet>(msg).await
                        }
                        AnyLcMsg::EthereumMinimal(msg) => {
                            self.handle_msg_generic::<EthereumMinimal>(msg).await
                        }
                        AnyLcMsg::CometblsMainnet(msg) => {
                            self.handle_msg_generic::<CometblsMainnet>(msg).await
                        }
                        AnyLcMsg::CometblsMinimal(msg) => {
                            self.handle_msg_generic::<CometblsMinimal>(msg).await
                        }
                    }
                }

                RelayerMsg::DeferUntil { timestamp } => {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs();

                    // if we haven't hit the time yet, requeue the defer msg
                    if now < timestamp {
                        // TODO: Make the time configurable?
                        tokio::time::sleep(Duration::from_secs(1)).await;

                        [RelayerMsg::DeferUntil { timestamp }].into()
                    } else {
                        vec![]
                    }
                }

                RelayerMsg::Timeout {
                    timeout_timestamp,
                    msg,
                } => {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs();

                    // if we haven't hit the time yet, requeue the defer msg
                    if now > timeout_timestamp {
                        tracing::warn!(json = %serde_json::to_string(&msg).unwrap(), "message expired");

                        [].into()
                    } else {
                        self.handle_msg(*msg, depth + 1).await
                    }
                }

                RelayerMsg::Sequence(mut seq) => {
                    let msgs = match seq.pop_front() {
                        Some(msg) => self.handle_msg(msg, depth + 1).await,
                        None => return vec![],
                    };

                    for msg in msgs.into_iter().rev() {
                        seq.push_front(msg);
                    }

                    [flatten_seq(RelayerMsg::Sequence(seq))].into()
                }

                RelayerMsg::Retry(_, _) => todo!(),

                RelayerMsg::Aggregate {
                    mut queue,
                    mut data,
                    receiver,
                } => {
                    if let Some(msg) = queue.pop_front() {
                        let msgs = self.handle_msg(msg, depth + 1).await;

                        for m in msgs {
                            match m.try_into() {
                                Ok(d) => {
                                    data.push_back(d);
                                }
                                Err(m) => {
                                    queue.push_back(m);
                                }
                            }
                        }

                        [RelayerMsg::Aggregate {
                            queue,
                            data,
                            receiver,
                        }]
                        .into()
                    } else {
                        // queue is empty, handle msg

                        match receiver {
                            AggregateReceiver::EthereumMainnet(msg) => {
                                do_create::<EthereumMainnet>(msg, data)
                            }
                            AggregateReceiver::EthereumMinimal(msg) => {
                                do_create::<EthereumMinimal>(msg, data)
                            }
                            AggregateReceiver::CometblsMainnet(msg) => {
                                do_create::<CometblsMainnet>(msg, data)
                            }
                            AggregateReceiver::CometblsMinimal(msg) => {
                                do_create::<CometblsMinimal>(msg, data)
                            }
                        }
                    }
                }
            }
        }
        .boxed()
    }

    async fn handle_msg_generic<L>(&self, msg: LcMsg<L>) -> Vec<RelayerMsg>
    where
        L: LightClient,
        Self: GetLc<L>,
        AnyLcMsg: From<LcMsg<L>>,
        AnyLcMsg: From<LcMsg<L::Counterparty>>,
        AggregateReceiver: From<identified!(Aggregate<L>)>,
        // TODO: Remove once we no longer unwrap in handle_fetch
        <<L as LightClient>::ClientId as TryFrom<
            <<L as LightClient>::HostChain as Chain>::ClientId,
        >>::Error: Debug,
        <<L::Counterparty as LightClient>::ClientId as TryFrom<
            <<L::Counterparty as LightClient>::HostChain as Chain>::ClientId,
        >>::Error: Debug,
    {
        match msg {
            LcMsg::Event(event) => handle_event(self.get_lc(&event.chain_id), event.data),
            LcMsg::Data(data) => {
                // TODO: Figure out a way to bubble it up to the top level

                // if depth == 0 {
                tracing::error!(data = %serde_json::to_string(&data).unwrap(), "received data outside of an aggregation");

                // panic!();

                [].into()
                // } else {
                //     [RelayerMsg::Lc(AnyLcMsg::from(LcMsg::Data(data)))].into()
                // }
            }
            LcMsg::Fetch(fetch) => handle_fetch(self.get_lc(&fetch.chain_id), fetch.data).await,
            LcMsg::Msg(msg) => {
                // NOTE: `Msg`s don't requeue any `RelayerMsg`s; they are side-effect only.
                self.get_lc(&msg.chain_id).msg(msg.data).await;

                [].into()
            }
            LcMsg::Wait(wait) => handle_wait(&self.get_lc(&wait.chain_id), wait.data).await,
            LcMsg::Aggregate(_) => {
                todo!()
            }
        }
    }
}

trait GetLc<L: LightClient> {
    fn get_lc(&self, chain_id: &ChainIdOf<L>) -> L;
}

impl<Q> GetLc<CometblsMinimal> for Voyager<Q> {
    fn get_lc(&self, chain_id: &ChainIdOf<CometblsMinimal>) -> CometblsMinimal {
        CometblsMinimal::from_chain(self.evm_minimal.get(chain_id).unwrap().clone())
    }
}

impl<Q> GetLc<CometblsMainnet> for Voyager<Q> {
    fn get_lc(&self, chain_id: &ChainIdOf<CometblsMainnet>) -> CometblsMainnet {
        CometblsMainnet::from_chain(self.evm_mainnet.get(chain_id).unwrap().clone())
    }
}

impl<Q> GetLc<EthereumMinimal> for Voyager<Q> {
    fn get_lc(&self, chain_id: &ChainIdOf<EthereumMinimal>) -> EthereumMinimal {
        // TODO: Ensure that the wasm code is for the correct config
        EthereumMinimal::from_chain(self.union.get(chain_id).unwrap().clone())
    }
}

impl<Q> GetLc<EthereumMainnet> for Voyager<Q> {
    fn get_lc(&self, chain_id: &ChainIdOf<EthereumMainnet>) -> EthereumMainnet {
        // TODO: Ensure that the wasm code is for the correct config
        EthereumMainnet::from_chain(self.union.get(chain_id).unwrap().clone())
    }
}

fn handle_event<L: LightClient>(l: L, event: crate::msg::event::Event<L>) -> Vec<RelayerMsg>
where
    AnyLcMsg: From<LcMsg<L>>,
    AggregateReceiver: From<identified!(Aggregate<L>)>,
{
    match event {
        Event::Ibc(ibc_event) => match ibc_event.event {
            IbcEvent::CreateClient(e) => {
                println!("client created: {e:#?}");

                vec![]
            }
            IbcEvent::UpdateClient(e) => {
                println!(
                    "client updated: {:#?} to {:#?}",
                    e.client_id, e.consensus_heights
                );

                vec![]
            }

            IbcEvent::ClientMisbehaviour(_) => unimplemented!(),
            IbcEvent::SubmitEvidence(_) => unimplemented!(),

            IbcEvent::ConnectionOpenInit(init) => [RelayerMsg::Sequence(
                [
                    RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Wait(Identified {
                        chain_id: l.chain().chain_id(),
                        data: Wait::Block(WaitForBlock(ibc_event.height.increment())),
                    }))),
                    RelayerMsg::Aggregate {
                        data: [].into(),
                        queue: [mk_aggregate_update(
                            l.chain().chain_id(),
                            init.client_id.clone(),
                            init.counterparty_client_id.clone(),
                            ibc_event.height,
                        )]
                        .into(),
                        receiver: AggregateReceiver::from(Identified::new(
                            l.chain().chain_id(),
                            Aggregate::AggregateMsgAfterUpdate(
                                AggregateMsgAfterUpdate::ConnectionOpenTry(
                                    AggregateConnectionOpenTry {
                                        event_height: ibc_event.height,
                                        event: init,
                                    },
                                ),
                            ),
                        )),
                    },
                ]
                .into(),
            )]
            .into(),
            IbcEvent::ConnectionOpenTry(try_) => [RelayerMsg::Sequence(
                [
                    RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Wait(Identified {
                        chain_id: l.chain().chain_id(),
                        data: Wait::Block(WaitForBlock(ibc_event.height.increment())),
                    }))),
                    RelayerMsg::Aggregate {
                        data: [].into(),
                        queue: [mk_aggregate_update(
                            l.chain().chain_id(),
                            try_.client_id.clone(),
                            try_.counterparty_client_id.clone(),
                            ibc_event.height,
                        )]
                        .into(),
                        receiver: AggregateReceiver::from(Identified::new(
                            l.chain().chain_id(),
                            Aggregate::AggregateMsgAfterUpdate(
                                AggregateMsgAfterUpdate::ConnectionOpenAck(
                                    AggregateConnectionOpenAck {
                                        event_height: ibc_event.height,
                                        event: try_,
                                    },
                                ),
                            ),
                        )),
                    },
                ]
                .into(),
            )]
            .into(),
            IbcEvent::ConnectionOpenAck(ack) => [RelayerMsg::Sequence(
                [
                    RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Wait(Identified {
                        chain_id: l.chain().chain_id(),
                        data: Wait::Block(WaitForBlock(ibc_event.height.increment())),
                    }))),
                    RelayerMsg::Aggregate {
                        data: [].into(),
                        queue: [mk_aggregate_update(
                            l.chain().chain_id(),
                            ack.client_id.clone(),
                            ack.counterparty_client_id.clone(),
                            ibc_event.height,
                        )]
                        .into(),
                        receiver: AggregateReceiver::from(Identified::new(
                            l.chain().chain_id(),
                            Aggregate::AggregateMsgAfterUpdate(
                                AggregateMsgAfterUpdate::ConnectionOpenConfirm(
                                    AggregateConnectionOpenConfirm {
                                        event_height: ibc_event.height,
                                        event: ack,
                                    },
                                ),
                            ),
                        )),
                    },
                ]
                .into(),
            )]
            .into(),
            IbcEvent::ConnectionOpenConfirm(confirm) => {
                println!("connection opened: {confirm:#?}");

                vec![]
            }

            IbcEvent::ChannelOpenInit(init) => [RelayerMsg::Sequence(
                [
                    RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Wait(Identified {
                        chain_id: l.chain().chain_id(),
                        data: Wait::Block(WaitForBlock(ibc_event.height.increment())),
                    }))),
                    RelayerMsg::Aggregate {
                        data: [].into(),
                        queue: [RelayerMsg::Aggregate {
                            data: [].into(),
                            queue: [RelayerMsg::Lc(AnyLcMsg::from(LcMsg::Fetch(
                                Identified::new(
                                    l.chain().chain_id(),
                                    Fetch::ChannelEnd(FetchChannelEnd {
                                        at: ibc_event.height.increment(),
                                        port_id: init.port_id.clone(),
                                        channel_id: init.channel_id.clone(),
                                    }),
                                ),
                            )))]
                            .into(),
                            receiver: AggregateReceiver::from(Identified::new(
                                l.chain().chain_id(),
                                Aggregate::ConnectionFetchFromChannelEnd(
                                    AggregateConnectionFetchFromChannelEnd {
                                        at: ibc_event.height.increment(),
                                    },
                                ),
                            )),
                        }]
                        .into(),
                        receiver: AggregateReceiver::from(Identified::new(
                            l.chain().chain_id(),
                            Aggregate::ChannelHandshakeUpdateClient(
                                AggregateChannelHandshakeUpdateClient {
                                    update_to: ibc_event.height.increment(),
                                    event_height: ibc_event.height,
                                    channel_handshake_event: ChannelHandshakeEvent::Init(init),
                                },
                            ),
                        )),
                    },
                ]
                .into(),
            )]
            .into(),
            IbcEvent::ChannelOpenTry(try_) => [RelayerMsg::Sequence(
                [
                    RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Wait(Identified {
                        chain_id: l.chain().chain_id(),
                        data: Wait::Block(WaitForBlock(ibc_event.height.increment())),
                    }))),
                    RelayerMsg::Aggregate {
                        data: [].into(),
                        queue: [RelayerMsg::Aggregate {
                            data: [].into(),
                            queue: [RelayerMsg::Lc(AnyLcMsg::from(LcMsg::Fetch(
                                Identified::new(
                                    l.chain().chain_id(),
                                    Fetch::ChannelEnd(FetchChannelEnd {
                                        at: ibc_event.height.increment(),
                                        port_id: try_.port_id.clone(),
                                        channel_id: try_.channel_id.clone(),
                                    }),
                                ),
                            )))]
                            .into(),
                            receiver: AggregateReceiver::from(Identified::new(
                                l.chain().chain_id(),
                                Aggregate::ConnectionFetchFromChannelEnd(
                                    AggregateConnectionFetchFromChannelEnd {
                                        at: ibc_event.height.increment(),
                                    },
                                ),
                            )),
                        }]
                        .into(),
                        receiver: AggregateReceiver::from(Identified::new(
                            l.chain().chain_id(),
                            Aggregate::ChannelHandshakeUpdateClient(
                                AggregateChannelHandshakeUpdateClient {
                                    update_to: ibc_event.height.increment(),
                                    event_height: ibc_event.height,
                                    channel_handshake_event: ChannelHandshakeEvent::Try(try_),
                                },
                            ),
                        )),
                    },
                ]
                .into(),
            )]
            .into(),
            IbcEvent::ChannelOpenAck(ack) => [RelayerMsg::Sequence(
                [
                    RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Wait(Identified {
                        chain_id: l.chain().chain_id(),
                        data: Wait::Block(WaitForBlock(ibc_event.height.increment())),
                    }))),
                    RelayerMsg::Aggregate {
                        data: [].into(),
                        queue: [RelayerMsg::Aggregate {
                            data: [].into(),
                            queue: [RelayerMsg::Lc(AnyLcMsg::from(LcMsg::Fetch(
                                Identified::new(
                                    l.chain().chain_id(),
                                    Fetch::ChannelEnd(FetchChannelEnd {
                                        at: ibc_event.height.increment(),
                                        port_id: ack.port_id.clone(),
                                        channel_id: ack.channel_id.clone(),
                                    }),
                                ),
                            )))]
                            .into(),
                            receiver: AggregateReceiver::from(Identified::new(
                                l.chain().chain_id(),
                                Aggregate::ConnectionFetchFromChannelEnd(
                                    AggregateConnectionFetchFromChannelEnd {
                                        at: ibc_event.height.increment(),
                                    },
                                ),
                            )),
                        }]
                        .into(),
                        receiver: AggregateReceiver::from(Identified::new(
                            l.chain().chain_id(),
                            Aggregate::ChannelHandshakeUpdateClient(
                                AggregateChannelHandshakeUpdateClient {
                                    update_to: ibc_event.height.increment(),
                                    event_height: ibc_event.height,
                                    channel_handshake_event: ChannelHandshakeEvent::Ack(ack),
                                },
                            ),
                        )),
                    },
                ]
                .into(),
            )]
            .into(),

            IbcEvent::ChannelOpenConfirm(confirm) => {
                println!("channel opened: {confirm:#?}");

                vec![]
            }

            IbcEvent::RecvPacket(_packet) => {
                //
                // [RelayerMsg::Sequence(
                //     [
                //         RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Wait(Identified {
                //             chain_id: l.chain().chain_id(),
                //             data: Wait::Block(WaitForBlock(ibc_event.height.increment())),
                //         }))),
                //         RelayerMsg::Aggregate {
                //             data: [].into(),
                //             queue: [RelayerMsg::Lc(AnyLcMsg::from(LcMsg::Fetch(
                //                 Identified::new(
                //                     l.chain().chain_id(),
                //                     Fetch::ConnectionEnd(FetchConnectionEnd {
                //                         at: ibc_event.height,
                //                         connection_id: packet.connection_id.clone(),
                //                     }),
                //                 ),
                //             )))]
                //             .into(),
                //             receiver: AggregateReceiver::from(Identified::new(
                //                 l.chain().chain_id(),
                //                 Aggregate::PacketUpdateClient(AggregatePacketUpdateClient {
                //                     update_to: ibc_event.height.increment(),
                //                     event_height: ibc_event.height,
                //                     block_hash: ibc_event.block_hash,
                //                     packet_event: PacketEvent::Recv(packet),
                //                 }),
                //             )),
                //         },
                //     ]
                //     .into(),
                // )]
                // .into()
                [].into()
            }
            IbcEvent::SendPacket(packet) => [RelayerMsg::Sequence(
                [
                    RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Wait(Identified {
                        chain_id: l.chain().chain_id(),
                        data: Wait::Block(WaitForBlock(ibc_event.height.increment())),
                    }))),
                    RelayerMsg::Aggregate {
                        data: [].into(),
                        queue: [RelayerMsg::Lc(AnyLcMsg::from(LcMsg::Fetch(
                            Identified::new(
                                l.chain().chain_id(),
                                Fetch::ConnectionEnd(FetchConnectionEnd {
                                    at: ibc_event.height,
                                    connection_id: packet.connection_id.clone(),
                                }),
                            ),
                        )))]
                        .into(),
                        receiver: AggregateReceiver::from(Identified::new(
                            l.chain().chain_id(),
                            Aggregate::PacketUpdateClient(AggregatePacketUpdateClient {
                                update_to: ibc_event.height.increment(),
                                event_height: ibc_event.height,
                                block_hash: ibc_event.block_hash,
                                packet_event: PacketEvent::Send(packet),
                            }),
                        )),
                    },
                ]
                .into(),
            )]
            .into(),
            IbcEvent::AcknowledgePacket(ack) => {
                tracing::info!(?ack, "packet acknowledged");
                [].into()
            }
            IbcEvent::TimeoutPacket(timeout) => {
                tracing::error!(?timeout, "packet timed out");
                [].into()
            }
            IbcEvent::WriteAcknowledgement(write_ack) => {
                tracing::info!(?write_ack, "packet acknowledgement written");
                [].into()
            }
        },
        Event::Command(command) => match command {
            crate::msg::event::Command::UpdateClient {
                client_id,
                counterparty_client_id,
            } => [RelayerMsg::Aggregate {
                queue: [RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Fetch(
                    Identified::new(
                        l.chain().chain_id(),
                        Fetch::TrustedClientState(FetchTrustedClientState {
                            at: QueryHeight::Latest,
                            client_id: client_id.clone(),
                        }),
                    ),
                )))]
                .into(),
                data: [].into(),
                receiver: AggregateReceiver::from(Identified::new(
                    l.chain().chain_id(),
                    Aggregate::<L>::UpdateClientFromClientId(AggregateUpdateClientFromClientId {
                        client_id,
                        counterparty_client_id,
                    }),
                )),
            }]
            .into(),
        },
    }
}

/// For updating a client, the information we have originally is:
///
/// - `chain_id`: the id of the chain that the client to be updated is on
/// - `height`: the height to update *to*
/// - `client_id`: id of the client to update
/// - `counterparty_client_id`: id of the counterparty of the client to update
///
/// Given this information, multiple aggregations are required:
///
/// - given (`chain_id`, `client_id`), fetch the counterparty client's `chain_id`
///   (contained within the client's client state)
///   - `FetchLatestTrustedClientState<L>`, aggregated down into `UpdateClientData<L>`,
///     producing `UpdateClientWithCounterpartyChainIdData<L>`
///
/// - then, with (`counterparty_chain_id`, `counterparty_client_id`), fetch the latest
///   client state of the counterparty client (which contains the latest trusted height)
///   - `FetchLatestTrustedClientState<L::Counterparty>`, aggregated down into
///     `UpdateClientWithCounterpartyChainIdData<L>`, producing `FetchUpdateHeaders<L>`
///
/// - finally, with the latest client state, build the headers between
///   `latest_client_state..=update_to` (note that the client may be updated to a height
///   greater than `update_to`, but never less; as such the latest trusted height should
///   always be fetched whenever it's needed)
///   - `FetchUpdateHeaders<L>`, which delegates to `L::generate_counterparty_updates`
fn mk_aggregate_update<L: LightClient>(
    chain_id: ChainIdOf<L>,
    client_id: L::ClientId,
    counterparty_client_id: <L::Counterparty as LightClient>::ClientId,
    event_height: HeightOf<ChainOf<L>>,
) -> RelayerMsg
where
    AnyLcMsg: From<LcMsg<L>>,
    AggregateReceiver: From<identified!(Aggregate<L>)>,
{
    RelayerMsg::Aggregate {
        queue: [RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Fetch(
            Identified::new(
                chain_id.clone(),
                Fetch::TrustedClientState(FetchTrustedClientState {
                    at: QueryHeight::Latest,
                    client_id: client_id.clone(),
                }),
            ),
        )))]
        .into(),
        data: [].into(),
        receiver: AggregateReceiver::from(Identified::new(
            chain_id,
            Aggregate::<L>::UpdateClient(AggregateUpdateClient {
                // Proof is only valid at N + 1 for tendermint
                update_to: event_height.increment(),
                client_id: client_id.clone(),
                counterparty_client_id,
            }),
        )),
    }
}

async fn handle_fetch<L: LightClient>(l: L, fetch: Fetch<L>) -> Vec<RelayerMsg>
where
    AnyLcMsg: From<LcMsg<L>>,
// TODO: Remove once we no longer unwrap
    <<L as LightClient>::ClientId as TryFrom<
        <<L as LightClient>::HostChain as Chain>::ClientId,
    >>::Error: Debug,
    <<L::Counterparty as LightClient>::ClientId as TryFrom<
        <<L::Counterparty as LightClient>::HostChain as Chain>::ClientId,
    >>::Error: Debug,
{
    let relayer_msg = match fetch {
        Fetch::TrustedClientState(FetchTrustedClientState { at, client_id }) => {
            // TODO: Split this into a separate query and aggregate
            let height = match at {
                QueryHeight::Latest => l.chain().query_latest_height().await,
                QueryHeight::Specific(h) => h,
            };

            [RelayerMsg::Lc(AnyLcMsg::from(LcMsg::Data(
                Identified::new(
                    l.chain().chain_id(),
                    Data::TrustedClientState(TrustedClientState {
                        fetched_at: height,
                        client_id: client_id.clone(),
                        trusted_client_state: l.query_client_state(client_id.into(), height).await,
                    }),
                ),
            )))]
            .into()
        }
        Fetch::StateProof(FetchStateProof { at, path }) => [RelayerMsg::Lc(AnyLcMsg::from(
            LcMsg::Data(Identified::new(
                l.chain().chain_id(),
                match path {
                    proof::Path::ClientStatePath(path) => Data::ClientStateProof(ClientStateProof(
                        l.chain().state_proof(path, at).await,
                    )),
                    proof::Path::ClientConsensusStatePath(path) => Data::ClientConsensusStateProof(
                        ClientConsensusStateProof(l.chain().state_proof(path, at).await),
                    ),
                    proof::Path::ConnectionPath(path) => Data::ConnectionProof(ConnectionProof(
                        l.chain().state_proof(path, at).await,
                    )),
                    proof::Path::ChannelEndPath(path) => Data::ChannelEndProof(ChannelEndProof(
                        l.chain().state_proof(path, at).await,
                    )),
                    proof::Path::CommitmentPath(path) => Data::CommitmentProof(CommitmentProof(
                        l.chain().state_proof(path, at).await,
                    )),
                    proof::Path::AcknowledgementPath(path) => Data::AcknowledgementProof(
                        AcknowledgementProof(l.chain().state_proof(path, at).await),
                    ),
                },
            )),
        ))]
        .into(),
        Fetch::SelfClientState(FetchSelfClientState { at: height }) => {
            // TODO: Split this into a separate query and aggregate
            let height = match height {
                QueryHeight::Latest => l.chain().query_latest_height().await,
                QueryHeight::Specific(h) => h,
            };

            [RelayerMsg::Lc(AnyLcMsg::from(LcMsg::Data(
                Identified::new(
                    l.chain().chain_id(),
                    Data::SelfClientState(SelfClientState(
                        l.chain().self_client_state(height).await,
                    )),
                ),
            )))]
            .into()
        }
        Fetch::SelfConsensusState(FetchSelfConsensusState { at: height }) => {
            // TODO: Split this into a separate query and aggregate
            let height = match height {
                QueryHeight::Latest => l.chain().query_latest_height().await,
                QueryHeight::Specific(h) => h,
            };

            [RelayerMsg::Lc(AnyLcMsg::from(LcMsg::Data(
                Identified::new(
                    l.chain().chain_id(),
                    Data::SelfConsensusState(SelfConsensusState(
                        l.chain().self_consensus_state(height).await,
                    )),
                ),
            )))]
            .into()
        }
        Fetch::PacketAcknowledgement(FetchPacketAcknowledgement {
            block_hash,
            destination_port_id,
            destination_channel_id,
            sequence,
            __marker,
        }) => {
            let ack = l
                .chain()
                .read_ack(
                    block_hash.clone(),
                    destination_channel_id.clone(),
                    destination_port_id.clone(),
                    sequence,
                )
                .await;

            [RelayerMsg::Lc(AnyLcMsg::from(LcMsg::Data(
                Identified::new(
                    l.chain().chain_id(),
                    Data::PacketAcknowledgement(PacketAcknowledgement {
                        fetched_by: FetchPacketAcknowledgement {
                            block_hash,
                            destination_port_id,
                            destination_channel_id,
                            sequence,
                            __marker,
                        },
                        ack,
                    }),
                ),
            )))]
            .into()
        }
        Fetch::UpdateHeaders(fetch_update_headers) => {
            l.generate_counterparty_updates(fetch_update_headers)
        }
        Fetch::LightClientSpecific(LightClientSpecificFetch(fetch)) => l.do_fetch(fetch).await,
        Fetch::ChannelEnd(FetchChannelEnd {
            at,
            port_id,
            channel_id,
        }) => [RelayerMsg::Lc(AnyLcMsg::from(LcMsg::Data(
            Identified::new(
                l.chain().chain_id(),
                Data::ChannelEnd(ChannelEnd {
                    channel: l
                        .chain()
                        .state_proof(
                            proof::ChannelEndPath {
                                port_id,
                                channel_id,
                            },
                            at,
                        )
                        .map(|channel_end_proof| channel_end_proof.state)
                        .await,
                    __marker: PhantomData,
                }),
            ),
        )))]
        .into(),
        Fetch::ConnectionEnd(FetchConnectionEnd { at, connection_id }) => {
            [RelayerMsg::Lc(AnyLcMsg::from(LcMsg::Data(
                Identified::new(
                    l.chain().chain_id(),
                    Data::ConnectionEnd(ConnectionEnd(
                        l.chain()
                            .state_proof(proof::ConnectionPath { connection_id }, at)
                            .map(|connection_end_proof| {
                                unionlabs::ibc::core::connection::connection_end::ConnectionEnd::<
                                L::ClientId,
                                <L::Counterparty as LightClient>::ClientId,
                                // NOTE: String used here since it may be empty; figure out a way to more strongly type this
                                String,
                            > {
                                client_id: connection_end_proof.state.client_id.try_into().expect(
                                    "state proof for a client should return it's own client id",
                                ),
                                versions: connection_end_proof.state.versions,
                                state: connection_end_proof.state.state,
                                counterparty:
                                    unionlabs::ibc::core::connection::counterparty::Counterparty {
                                        client_id: <<L::Counterparty as LightClient>::ClientId>::try_from(connection_end_proof
                                            .state
                                            .counterparty
                                            .client_id).unwrap(),
                                        connection_id: connection_end_proof
                                            .state
                                            .counterparty
                                            .connection_id,
                                        prefix: connection_end_proof.state.counterparty.prefix,
                                    },
                                delay_period: connection_end_proof.state.delay_period,
                            }
                            })
                            .await,
                    )),
                ),
            )))]
            .into()
        }
    };

    relayer_msg
}

async fn handle_wait<L: LightClient>(l: &L, wait: Wait<L>) -> Vec<RelayerMsg>
where
    AnyLcMsg: From<LcMsg<L>>,
    AnyLcMsg: From<LcMsg<L::Counterparty>>,
{
    match wait {
        Wait::Block(WaitForBlock(height)) => {
            let chain_height = l.chain().query_latest_height().await;

            assert_eq!(
                Into::<Height>::into(chain_height).revision_number,
                Into::<Height>::into(height).revision_number,
                "chain_height: {chain_height}, height: {height}",
                height = Into::<Height>::into(height)
            );

            if Into::<Height>::into(chain_height).revision_height
                >= Into::<Height>::into(height).revision_height
            {
                [].into()
            } else {
                [RelayerMsg::Sequence(
                    [
                        RelayerMsg::DeferUntil {
                            timestamp: SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                // REVIEW: Defer until `now + chain.block_time()`? Would require a new method on chain
                                .add(Duration::from_secs(1))
                                .as_secs(),
                        },
                        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Wait(Identified {
                            chain_id: l.chain().chain_id(),
                            data: Wait::Block(WaitForBlock(height)),
                        }))),
                    ]
                    .into(),
                )]
                .into()
            }
        }
        Wait::Timestamp(WaitForTimestamp {
            timestamp,
            __marker,
        }) => {
            let chain_ts = l.chain().query_latest_timestamp().await;

            if chain_ts >= timestamp {
                [].into()
            } else {
                [RelayerMsg::Sequence(
                    [
                        RelayerMsg::DeferUntil {
                            timestamp: SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                // REVIEW: Defer until `now + chain.block_time()`? Would require a new method on chain
                                .add(Duration::from_secs(1))
                                .as_secs(),
                        },
                        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Wait(Identified {
                            chain_id: l.chain().chain_id(),
                            data: Wait::Timestamp(WaitForTimestamp {
                                timestamp,
                                __marker,
                            }),
                        }))),
                    ]
                    .into(),
                )]
                .into()
            }
        }
        Wait::TrustedHeight(WaitForTrustedHeight {
            client_id,
            height,
            counterparty_client_id,
            counterparty_chain_id,
        }) => {
            let latest_height = dbg!(l.chain().query_latest_height_as_destination().await);
            let trusted_client_state = dbg!(
                l.query_client_state(client_id.clone().into(), latest_height)
                    .await
            );

            if trusted_client_state.height().revision_height() >= height.revision_height() {
                tracing::debug!(
                    "client height reached ({} >= {})",
                    trusted_client_state.height(),
                    height
                );

                [RelayerMsg::Lc(AnyLcMsg::from(
                    LcMsg::<L::Counterparty>::Fetch(Identified::new(
                        counterparty_chain_id,
                        Fetch::TrustedClientState(FetchTrustedClientState {
                            at: QueryHeight::Specific(trusted_client_state.height()),
                            client_id: counterparty_client_id.clone(),
                        }),
                    )),
                ))]
                .into()
            } else {
                [RelayerMsg::Sequence(
                    [
                        RelayerMsg::DeferUntil {
                            timestamp: SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                // REVIEW: Defer until `now + chain.block_time()`? Would require a new method on chain
                                .add(Duration::from_secs(1))
                                .as_secs(),
                        },
                        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Wait(Identified {
                            chain_id: l.chain().chain_id(),
                            data: Wait::TrustedHeight(WaitForTrustedHeight {
                                client_id,
                                height,
                                counterparty_client_id,
                                counterparty_chain_id,
                            }),
                        }))),
                    ]
                    .into(),
                )]
                .into()
            }
        }
    }
}

fn do_create<L: LightClient>(
    Identified {
        chain_id,
        data: msg,
    }: identified!(Aggregate<L>),
    data: VecDeque<AggregateData>,
) -> Vec<RelayerMsg>
where
    identified!(TrustedClientState<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(TrustedClientState<L::Counterparty>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,

    identified!(ClientStateProof<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(ClientConsensusStateProof<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(ConnectionProof<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(ChannelEndProof<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(CommitmentProof<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(AcknowledgementProof<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,

    identified!(SelfClientState<L::Counterparty>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(SelfConsensusState<L::Counterparty>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,

    identified!(ChannelEnd<L>): TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(ConnectionEnd<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(PacketAcknowledgement<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,

    AnyLcMsg: From<LcMsg<L>>,
    AnyLcMsg: From<LcMsg<L::Counterparty>>,
    AggregateData: From<identified!(Data<L>)>,
    AggregateReceiver: From<identified!(Aggregate<L>)>,
{
    match msg {
        Aggregate::ConnectionOpenTry(init) => [aggregate_data::do_aggregate::<L, _>(
            Identified {
                chain_id,
                data: init,
            },
            data,
        )]
        .into(),
        Aggregate::ConnectionOpenAck(ack) => [aggregate_data::do_aggregate::<L, _>(
            Identified {
                chain_id,
                data: ack,
            },
            data,
        )]
        .into(),
        Aggregate::ConnectionOpenConfirm(confirm) => [aggregate_data::do_aggregate::<L, _>(
            Identified {
                chain_id,
                data: confirm,
            },
            data,
        )]
        .into(),
        Aggregate::ChannelOpenTry(try_) => [aggregate_data::do_aggregate::<L, _>(
            Identified {
                chain_id,
                data: try_,
            },
            data,
        )]
        .into(),
        Aggregate::ChannelOpenAck(ack) => [aggregate_data::do_aggregate::<L, _>(
            Identified {
                chain_id,
                data: ack,
            },
            data,
        )]
        .into(),
        Aggregate::ChannelOpenConfirm(confirm) => [aggregate_data::do_aggregate::<L, _>(
            Identified {
                chain_id,
                data: confirm,
            },
            data,
        )]
        .into(),
        Aggregate::UpdateClientFromClientId(update_client) => {
            [aggregate_data::do_aggregate::<L, _>(
                Identified {
                    chain_id,
                    data: update_client,
                },
                data,
            )]
            .into()
        }
        Aggregate::UpdateClient(update_client) => [aggregate_data::do_aggregate::<L, _>(
            Identified {
                chain_id,
                data: update_client,
            },
            data,
        )]
        .into(),
        Aggregate::UpdateClientWithCounterpartyChainIdData(aggregate) => {
            [aggregate_data::do_aggregate::<L, _>(
                Identified {
                    chain_id,
                    data: aggregate,
                },
                data,
            )]
            .into()
        }
        Aggregate::CreateClient(create_client) => [aggregate_data::do_aggregate::<L, _>(
            Identified {
                chain_id,
                data: create_client,
            },
            data,
        )]
        .into(),
        Aggregate::ConsensusStateProofAtLatestHeight(make_consensus_state_proof) => {
            [aggregate_data::do_aggregate::<L, _>(
                Identified {
                    chain_id,
                    data: make_consensus_state_proof,
                },
                data,
            )]
            .into()
        }
        Aggregate::AggregateMsgAfterUpdate(aggregate) => [aggregate_data::do_aggregate::<L, _>(
            Identified {
                chain_id,
                data: aggregate,
            },
            data,
        )]
        .into(),
        Aggregate::LightClientSpecific(LightClientSpecificAggregate(aggregate)) => {
            L::Aggregate::do_aggregate(
                Identified {
                    chain_id,
                    data: aggregate,
                },
                data,
            )
        }
        Aggregate::ConnectionFetchFromChannelEnd(aggregate) => {
            [aggregate_data::do_aggregate::<L, _>(
                Identified {
                    chain_id,
                    data: aggregate,
                },
                data,
            )]
            .into()
        }
        Aggregate::ChannelHandshakeUpdateClient(channel_handshake_update_client) => {
            [aggregate_data::do_aggregate::<L, _>(
                Identified {
                    chain_id,
                    data: channel_handshake_update_client,
                },
                data,
            )]
            .into()
        }
        Aggregate::PacketUpdateClient(packet_update_client) => {
            [aggregate_data::do_aggregate::<L, _>(
                Identified {
                    chain_id,
                    data: packet_update_client,
                },
                data,
            )]
            .into()
        }
        Aggregate::RecvPacket(recv_packet) => [aggregate_data::do_aggregate::<L, _>(
            Identified {
                chain_id,
                data: recv_packet,
            },
            data,
        )]
        .into(),
        Aggregate::AckPacket(ack_packet) => [aggregate_data::do_aggregate::<L, _>(
            Identified {
                chain_id,
                data: ack_packet,
            },
            data,
        )]
        .into(),
        Aggregate::WaitForTrustedHeight(agg) => [aggregate_data::do_aggregate::<L, _>(
            Identified {
                chain_id,
                data: agg,
            },
            data,
        )]
        .into(),
        Aggregate::FetchCounterpartyStateproof(agg) => [aggregate_data::do_aggregate::<L, _>(
            Identified {
                chain_id,
                data: agg,
            },
            data,
        )]
        .into(),
    }
}

impl<L: LightClient> UseAggregate<L> for identified!(AggregateChannelHandshakeUpdateClient<L>)
where
    identified!(ConnectionEnd<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    AnyLcMsg: From<LcMsg<L>>,
    AggregateReceiver: From<identified!(Aggregate<L>)>,
{
    type AggregatedData = HList![identified!(ConnectionEnd<L>)];

    fn aggregate(
        Identified {
            chain_id: this_chain_id,
            data:
                AggregateChannelHandshakeUpdateClient {
                    update_to,
                    channel_handshake_event,
                    event_height,
                },
        }: Self,
        hlist_pat![Identified {
            chain_id: self_chain_id,
            data: ConnectionEnd(connection),
        }]: Self::AggregatedData,
    ) -> RelayerMsg {
        assert_eq!(this_chain_id, self_chain_id);

        let event_msg = match channel_handshake_event {
            ChannelHandshakeEvent::Init(init) => {
                AggregateMsgAfterUpdate::ChannelOpenTry(AggregateChannelOpenTry {
                    event_height,
                    event: init,
                })
            }
            ChannelHandshakeEvent::Try(try_) => {
                AggregateMsgAfterUpdate::ChannelOpenAck(AggregateChannelOpenAck {
                    event_height,
                    event: try_,
                })
            }
            ChannelHandshakeEvent::Ack(ack) => {
                AggregateMsgAfterUpdate::ChannelOpenConfirm(AggregateChannelOpenConfirm {
                    event_height,
                    event: ack,
                })
            }
        };

        RelayerMsg::Aggregate {
            data: [].into(),
            queue: [mk_aggregate_update(
                this_chain_id.clone(),
                connection.client_id.clone(),
                connection.counterparty.client_id.clone(),
                update_to,
            )]
            .into(),
            receiver: AggregateReceiver::from(Identified::new(
                this_chain_id,
                Aggregate::AggregateMsgAfterUpdate(event_msg),
            )),
        }
    }
}

impl<L: LightClient> UseAggregate<L> for identified!(AggregatePacketUpdateClient<L>)
where
    identified!(ConnectionEnd<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    AnyLcMsg: From<LcMsg<L>>,
    AggregateReceiver: From<identified!(Aggregate<L>)>,
{
    type AggregatedData = HList![identified!(ConnectionEnd<L>)];

    fn aggregate(
        Identified {
            chain_id: this_chain_id,
            data:
                AggregatePacketUpdateClient {
                    update_to,
                    event_height,
                    block_hash,
                    packet_event,
                },
        }: Self,
        hlist_pat![Identified {
            chain_id: self_chain_id,
            data: ConnectionEnd(connection),
        }]: Self::AggregatedData,
    ) -> RelayerMsg {
        assert_eq!(this_chain_id, self_chain_id);

        let event = match packet_event {
            PacketEvent::Send(send) => Aggregate::AggregateMsgAfterUpdate(
                AggregateMsgAfterUpdate::RecvPacket(AggregateRecvPacket {
                    event_height,
                    event: send,
                }),
            ),
            PacketEvent::Recv(recv) => Aggregate::AggregateMsgAfterUpdate(
                AggregateMsgAfterUpdate::AckPacket(AggregateAckPacket {
                    event_height,
                    event: recv,
                    block_hash,
                    counterparty_client_id: connection.counterparty.client_id.clone(),
                }),
            ),
        };

        RelayerMsg::Aggregate {
            data: [].into(),
            queue: [RelayerMsg::Aggregate {
                queue: [RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Fetch(
                    Identified::new(
                        this_chain_id.clone().clone(),
                        Fetch::TrustedClientState(FetchTrustedClientState {
                            at: QueryHeight::Latest,
                            client_id: connection.client_id.clone().clone(),
                        }),
                    ),
                )))]
                .into(),
                data: [].into(),
                receiver: AggregateReceiver::from(Identified::new(
                    this_chain_id.clone(),
                    Aggregate::<L>::WaitForTrustedHeight(AggregateWaitForTrustedHeight {
                        wait_for: update_to,
                        client_id: connection.client_id.clone().clone(),
                        counterparty_client_id: connection.counterparty.client_id.clone(),
                    }),
                )),
            }]
            .into(),
            receiver: AggregateReceiver::from(Identified::new(this_chain_id, event)),
        }
    }
}

impl<L: LightClient> UseAggregate<L> for identified!(AggregateConnectionFetchFromChannelEnd<L>)
where
    identified!(ChannelEnd<L>): TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    AnyLcMsg: From<LcMsg<L>>,
{
    type AggregatedData = HList![identified!(ChannelEnd<L>)];

    fn aggregate(
        Identified {
            chain_id: this_chain_id,
            data: AggregateConnectionFetchFromChannelEnd { at },
        }: Self,
        hlist_pat![Identified {
            chain_id: self_chain_id,
            data: ChannelEnd {
                channel,
                __marker: _
            },
        }]: Self::AggregatedData,
    ) -> RelayerMsg {
        assert_eq!(this_chain_id, self_chain_id);

        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::Fetch(Identified::new(
            this_chain_id,
            Fetch::ConnectionEnd(FetchConnectionEnd {
                at,
                connection_id: channel.connection_hops[0].clone(),
            }),
        ))))
    }
}

impl<L: LightClient> UseAggregate<L> for identified!(AggregateUpdateClientFromClientId<L>)
where
    identified!(TrustedClientState<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    // AnyLcMsg: From<LcMsg<L>>,
    AnyLcMsg: From<LcMsg<L::Counterparty>>,
    AggregateReceiver: From<identified!(Aggregate<L>)>,
{
    type AggregatedData = HList![identified!(TrustedClientState<L>)];

    fn aggregate(
        Identified {
            chain_id: this_chain_id,
            data:
                AggregateUpdateClientFromClientId {
                    client_id,
                    counterparty_client_id,
                },
        }: Self,
        hlist_pat![Identified {
            chain_id: self_chain_id,
            data: TrustedClientState {
                fetched_at,
                client_id: trusted_client_state_client_id,
                trusted_client_state,
            },
        }]: Self::AggregatedData,
    ) -> RelayerMsg {
        assert_eq!(this_chain_id, self_chain_id);

        let counterparty_chain_id = trusted_client_state.chain_id();

        RelayerMsg::Aggregate {
            queue: [RelayerMsg::Lc(AnyLcMsg::from(
                LcMsg::<L::Counterparty>::Fetch(Identified {
                    chain_id: counterparty_chain_id.clone(),
                    data: Fetch::TrustedClientState(FetchTrustedClientState {
                        at: QueryHeight::Specific(trusted_client_state.height()),
                        client_id: counterparty_client_id.clone(),
                    }),
                }),
            ))]
            .into(),
            data: [].into(),
            receiver: AggregateReceiver::from(Identified::new(
                this_chain_id,
                Aggregate::UpdateClientWithCounterpartyChainIdData(
                    AggregateUpdateClientWithCounterpartyChainId {
                        update_to: fetched_at,
                        client_id,
                        counterparty_client_id,
                        counterparty_chain_id,
                    },
                ),
            )),
        }
    }
}

impl<L: LightClient> UseAggregate<L> for identified!(AggregateUpdateClient<L>)
where
    identified!(TrustedClientState<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    // AnyLcMsg: From<LcMsg<L::Counterparty>>,
    AnyLcMsg: From<LcMsg<L::Counterparty>>,
    AggregateReceiver: From<identified!(Aggregate<L>)>,
{
    type AggregatedData = HList![identified!(TrustedClientState<L>)];

    fn aggregate(
        Identified {
            chain_id: this_chain_id,
            data:
                AggregateUpdateClient {
                    update_to,
                    client_id: update_client_id,
                    counterparty_client_id: update_counterparty_client_id,
                },
        }: Self,
        hlist_pat![Identified {
            chain_id: self_chain_id,
            data: TrustedClientState {
                fetched_at: _,
                client_id: trusted_client_state_client_id,
                trusted_client_state,
            },
        }]: Self::AggregatedData,
    ) -> RelayerMsg {
        assert_eq!(this_chain_id, self_chain_id);
        assert_eq!(update_client_id, trusted_client_state_client_id);

        let counterparty_chain_id: ChainIdOf<L::Counterparty> = trusted_client_state.chain_id();

        RelayerMsg::Aggregate {
            queue: [RelayerMsg::Lc(AnyLcMsg::from(
                LcMsg::<L::Counterparty>::Fetch(Identified {
                    chain_id: counterparty_chain_id.clone(),
                    data: Fetch::TrustedClientState(FetchTrustedClientState {
                        at: QueryHeight::Latest,
                        client_id: update_counterparty_client_id.clone(),
                    }),
                }),
            ))]
            .into(),
            data: [].into(),
            receiver: AggregateReceiver::from(Identified::new(
                this_chain_id,
                Aggregate::UpdateClientWithCounterpartyChainIdData(
                    AggregateUpdateClientWithCounterpartyChainId {
                        update_to,
                        client_id: update_client_id,
                        counterparty_client_id: update_counterparty_client_id,
                        counterparty_chain_id,
                    },
                ),
            )),
        }
    }
}

impl<L: LightClient> UseAggregate<L>
    for identified!(AggregateUpdateClientWithCounterpartyChainId<L>)
where
    identified!(TrustedClientState<L::Counterparty>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    AnyLcMsg: From<LcMsg<L>>,
    AggregateReceiver: From<identified!(Aggregate<L>)>,
{
    type AggregatedData = HList![identified!(TrustedClientState<L::Counterparty>)];

    fn aggregate(
        Identified {
            chain_id: this_chain_id,
            data:
                AggregateUpdateClientWithCounterpartyChainId {
                    update_to,
                    client_id: update_client_id,
                    counterparty_client_id: update_counterparty_client_id,
                    counterparty_chain_id: update_counterparty_chain_id,
                },
        }: Self,
        hlist_pat![Identified {
            chain_id: counterparty_chain_id,
            data: TrustedClientState {
                fetched_at: _,
                client_id: latest_trusted_client_state_client_id,
                trusted_client_state
            },
        }]: Self::AggregatedData,
    ) -> RelayerMsg {
        let self_chain_id: ChainIdOf<L> = trusted_client_state.chain_id();

        assert_eq!(this_chain_id, self_chain_id);
        assert_eq!(
            latest_trusted_client_state_client_id,
            update_counterparty_client_id
        );
        assert_eq!(counterparty_chain_id, update_counterparty_chain_id);

        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Fetch(Identified {
            chain_id: this_chain_id,
            data: Fetch::UpdateHeaders(FetchUpdateHeaders {
                client_id: update_client_id,
                counterparty_client_id: update_counterparty_client_id,
                counterparty_chain_id,
                update_from: trusted_client_state.height(),
                update_to,
            }),
        })))
    }
}

impl<L: LightClient> UseAggregate<L> for identified!(AggregateWaitForTrustedHeight<L>)
where
    identified!(TrustedClientState<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    AnyLcMsg: From<LcMsg<L::Counterparty>>,
    AggregateReceiver: From<identified!(Aggregate<L>)>,
{
    type AggregatedData = HList![identified!(TrustedClientState<L>)];

    fn aggregate(
        Identified {
            chain_id: this_chain_id,
            data:
                AggregateWaitForTrustedHeight {
                    wait_for,
                    client_id,
                    counterparty_client_id,
                },
        }: Self,
        hlist_pat![Identified {
            chain_id,
            data: TrustedClientState {
                fetched_at: _,
                client_id: latest_trusted_client_state_client_id,
                trusted_client_state
            },
        }]: Self::AggregatedData,
    ) -> RelayerMsg {
        let counterparty_chain_id: ChainIdOf<L::Counterparty> = trusted_client_state.chain_id();

        tracing::debug!("building WaitForTrustedHeight");

        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L::Counterparty>::Wait(Identified {
            chain_id: counterparty_chain_id,
            data: Wait::TrustedHeight(WaitForTrustedHeight {
                height: wait_for,
                client_id: counterparty_client_id,
                counterparty_client_id: client_id,
                counterparty_chain_id: this_chain_id,
            }),
        })))
    }
}

// TODO: Remove, unused
impl<L: LightClient> UseAggregate<L> for identified!(ConsensusStateProofAtLatestHeight<L>)
where
    identified!(TrustedClientState<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    AnyLcMsg: From<LcMsg<L>>,
    AggregateReceiver: From<identified!(Aggregate<L>)>,
{
    type AggregatedData = HList![identified!(TrustedClientState<L>)];

    fn aggregate(
        Identified {
            chain_id: this_chain_id,
            data: ConsensusStateProofAtLatestHeight { client_id, at },
        }: Self,
        hlist_pat![Identified {
            chain_id: self_chain_id,
            data: TrustedClientState {
                fetched_at: _,
                client_id: latest_trusted_client_state_client_id,
                trusted_client_state
            },
        }]: Self::AggregatedData,
    ) -> RelayerMsg {
        assert_eq!(this_chain_id, self_chain_id);
        assert_eq!(client_id, latest_trusted_client_state_client_id);

        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::Fetch(Identified {
            chain_id: this_chain_id,
            data: Fetch::<L>::StateProof(FetchStateProof {
                at,
                path: proof::Path::ClientConsensusStatePath(ClientConsensusStatePath {
                    client_id: client_id.into(),
                    height: trusted_client_state.height(),
                }),
            }),
        })))
    }
}

impl<L: LightClient> UseAggregate<L> for identified!(AggregateMsgAfterUpdate<L>)
where
    identified!(TrustedClientState<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    AnyLcMsg: From<LcMsg<L>>,
    AnyLcMsg: From<LcMsg<L::Counterparty>>,
    AggregateData: From<identified!(Data<L>)>,
    AggregateReceiver: From<identified!(Aggregate<L>)>,
{
    type AggregatedData = HList![identified!(TrustedClientState<L>)];

    fn aggregate(
        Identified {
            chain_id: this_chain_id,
            data: msg_to_aggregate,
        }: Self,
        hlist_pat![Identified {
            chain_id: self_chain_id,
            data: TrustedClientState {
                fetched_at: trusted_client_state_fetched_at_height,
                client_id: trusted_client_state_client_id,
                trusted_client_state
            },
        }]: Self::AggregatedData,
    ) -> RelayerMsg {
        assert_eq!(this_chain_id, self_chain_id);
        // assert_eq!(client_id, trusted_client_state_client_id);

        match msg_to_aggregate {
            AggregateMsgAfterUpdate::ConnectionOpenTry(AggregateConnectionOpenTry {
                event_height,
                event,
            }) => {
                let consensus_state_height = trusted_client_state_fetched_at_height;

                assert_eq!(
                    consensus_state_height.revision_number(),
                    event_height.revision_number(),
                    "{consensus_state_height}, {event_height}",
                );

                assert!(
                    consensus_state_height.revision_height() >= event_height.revision_height(),
                    "{} < {}",
                    consensus_state_height.revision_height(),
                    event_height.revision_height()
                );

                let trusted_client_state_height = trusted_client_state.height();

                RelayerMsg::Aggregate {
                    data: [AggregateData::from(Identified::new(
                        this_chain_id.clone(),
                        Data::TrustedClientState(TrustedClientState {
                            fetched_at: trusted_client_state_fetched_at_height,
                            client_id: trusted_client_state_client_id,
                            trusted_client_state,
                        }),
                    ))]
                    .into(),
                    queue: [
                        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Fetch(Identified {
                            chain_id: this_chain_id.clone(),
                            data: Fetch::StateProof(FetchStateProof {
                                at: trusted_client_state_fetched_at_height,
                                path: proof::Path::ClientStatePath(ClientStatePath {
                                    client_id: event.client_id.clone().into(),
                                }),
                            }),
                        }))),
                        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Fetch(Identified {
                            chain_id: this_chain_id.clone(),
                            data: Fetch::StateProof(FetchStateProof {
                                at: trusted_client_state_fetched_at_height,
                                path: proof::Path::ClientConsensusStatePath(
                                    ClientConsensusStatePath {
                                        client_id: event.client_id.clone().into(),
                                        height: trusted_client_state_height,
                                    },
                                ),
                            }),
                        }))),
                        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Fetch(Identified {
                            chain_id: this_chain_id.clone(),
                            data: Fetch::StateProof(FetchStateProof {
                                at: trusted_client_state_fetched_at_height,
                                path: proof::Path::ConnectionPath(ConnectionPath {
                                    connection_id: event.connection_id.clone(),
                                }),
                            }),
                        }))),
                    ]
                    .into(),
                    receiver: AggregateReceiver::from(Identified::new(
                        this_chain_id,
                        Aggregate::ConnectionOpenTry(AggregateConnectionOpenTry {
                            event_height,
                            event,
                        }),
                    )),
                }
            }
            AggregateMsgAfterUpdate::ConnectionOpenAck(AggregateConnectionOpenAck {
                event_height,
                event,
            }) => {
                let consensus_state_height = trusted_client_state_fetched_at_height;

                assert_eq!(
                    consensus_state_height.revision_number(),
                    event_height.revision_number(),
                    "{consensus_state_height}, {event_height}",
                );

                assert!(
                    consensus_state_height.revision_height() >= event_height.revision_height(),
                    "{} < {}",
                    consensus_state_height.revision_height(),
                    event_height.revision_height()
                );

                let trusted_client_state_height = trusted_client_state.height();

                RelayerMsg::Aggregate {
                    data: [AggregateData::from(Identified::new(
                        this_chain_id.clone(),
                        Data::TrustedClientState(TrustedClientState {
                            fetched_at: trusted_client_state_fetched_at_height,
                            client_id: trusted_client_state_client_id,
                            trusted_client_state,
                        }),
                    ))]
                    .into(),
                    queue: [
                        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Fetch(Identified {
                            chain_id: this_chain_id.clone(),
                            data: Fetch::StateProof(FetchStateProof {
                                at: trusted_client_state_fetched_at_height,
                                path: proof::Path::ClientStatePath(ClientStatePath {
                                    client_id: event.client_id.clone().into(),
                                }),
                            }),
                        }))),
                        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Fetch(Identified {
                            chain_id: this_chain_id.clone(),
                            data: Fetch::StateProof(FetchStateProof {
                                at: trusted_client_state_fetched_at_height,
                                path: proof::Path::ClientConsensusStatePath(
                                    ClientConsensusStatePath {
                                        client_id: event.client_id.clone().into(),
                                        height: trusted_client_state_height,
                                    },
                                ),
                            }),
                        }))),
                        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Fetch(Identified {
                            chain_id: this_chain_id.clone(),
                            data: Fetch::StateProof(FetchStateProof {
                                at: trusted_client_state_fetched_at_height,
                                path: proof::Path::ConnectionPath(ConnectionPath {
                                    connection_id: event.connection_id.clone(),
                                }),
                            }),
                        }))),
                    ]
                    .into(),
                    receiver: AggregateReceiver::from(Identified::new(
                        this_chain_id,
                        Aggregate::ConnectionOpenAck(AggregateConnectionOpenAck {
                            event_height,
                            event,
                        }),
                    )),
                }
            }
            AggregateMsgAfterUpdate::ConnectionOpenConfirm(AggregateConnectionOpenConfirm {
                event_height,
                event,
            }) => {
                let consensus_state_height = trusted_client_state_fetched_at_height;

                assert_eq!(
                    consensus_state_height.revision_number(),
                    event_height.revision_number(),
                    "{consensus_state_height}, {event_height}",
                );

                assert!(
                    consensus_state_height.revision_height() >= event_height.revision_height(),
                    "{} < {}",
                    consensus_state_height.revision_height(),
                    event_height.revision_height()
                );

                RelayerMsg::Aggregate {
                    data: [AggregateData::from(Identified::new(
                        this_chain_id.clone(),
                        Data::TrustedClientState(TrustedClientState {
                            fetched_at: trusted_client_state_fetched_at_height,
                            client_id: trusted_client_state_client_id,
                            trusted_client_state,
                        }),
                    ))]
                    .into(),
                    queue: [RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Fetch(
                        Identified {
                            chain_id: this_chain_id.clone(),
                            data: Fetch::StateProof(FetchStateProof {
                                at: trusted_client_state_fetched_at_height,
                                path: proof::Path::ConnectionPath(ConnectionPath {
                                    connection_id: event.connection_id.clone(),
                                }),
                            }),
                        },
                    )))]
                    .into(),
                    receiver: AggregateReceiver::from(Identified::new(
                        this_chain_id,
                        Aggregate::ConnectionOpenConfirm(AggregateConnectionOpenConfirm {
                            event_height,
                            event,
                        }),
                    )),
                }
            }
            AggregateMsgAfterUpdate::ChannelOpenTry(AggregateChannelOpenTry {
                event_height,
                event,
            }) => {
                let consensus_state_height = trusted_client_state_fetched_at_height;

                assert_eq!(
                    consensus_state_height.revision_number(),
                    event_height.revision_number(),
                    "{consensus_state_height}, {event_height}",
                );

                assert!(
                    consensus_state_height.revision_height() >= event_height.revision_height(),
                    "{} < {}",
                    consensus_state_height.revision_height(),
                    event_height.revision_height()
                );

                RelayerMsg::Aggregate {
                    data: [AggregateData::from(Identified::new(
                        this_chain_id.clone(),
                        Data::TrustedClientState(TrustedClientState {
                            fetched_at: trusted_client_state_fetched_at_height,
                            client_id: trusted_client_state_client_id,
                            trusted_client_state,
                        }),
                    ))]
                    .into(),
                    queue: [
                        RelayerMsg::Aggregate {
                            data: [].into(),
                            queue: [RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Fetch(
                                Identified::new(
                                    this_chain_id.clone(),
                                    Fetch::ChannelEnd(FetchChannelEnd {
                                        at: trusted_client_state_fetched_at_height,
                                        port_id: event.port_id.clone(),
                                        channel_id: event.channel_id.clone(),
                                    }),
                                ),
                            )))]
                            .into(),
                            receiver: AggregateReceiver::from(Identified::new(
                                this_chain_id.clone(),
                                Aggregate::ConnectionFetchFromChannelEnd(
                                    AggregateConnectionFetchFromChannelEnd {
                                        at: trusted_client_state_fetched_at_height,
                                    },
                                ),
                            )),
                        },
                        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Fetch(Identified {
                            chain_id: this_chain_id.clone(),
                            data: Fetch::StateProof(FetchStateProof {
                                at: trusted_client_state_fetched_at_height,
                                path: proof::Path::ChannelEndPath(ChannelEndPath {
                                    port_id: event.port_id.clone(),
                                    channel_id: event.channel_id.clone(),
                                }),
                            }),
                        }))),
                    ]
                    .into(),
                    receiver: AggregateReceiver::from(Identified::new(
                        this_chain_id,
                        Aggregate::ChannelOpenTry(AggregateChannelOpenTry {
                            event_height,
                            event,
                        }),
                    )),
                }
            }
            AggregateMsgAfterUpdate::ChannelOpenAck(AggregateChannelOpenAck {
                event_height,
                event,
            }) => {
                let consensus_state_height = trusted_client_state_fetched_at_height;

                assert_eq!(
                    consensus_state_height.revision_number(),
                    event_height.revision_number(),
                    "{consensus_state_height}, {event_height}",
                );

                assert!(
                    consensus_state_height.revision_height() >= event_height.revision_height(),
                    "{} < {}",
                    consensus_state_height.revision_height(),
                    event_height.revision_height()
                );

                // RelayerMsg::Sequence([].into());
                RelayerMsg::Aggregate {
                    data: [AggregateData::from(Identified::new(
                        this_chain_id.clone(),
                        Data::TrustedClientState(TrustedClientState {
                            fetched_at: trusted_client_state_fetched_at_height,
                            client_id: trusted_client_state_client_id,
                            trusted_client_state,
                        }),
                    ))]
                    .into(),
                    queue: [
                        // RelayerMsg::Aggregate {
                        //     data: [].into(),
                        //     queue: [RelayerMsg::Lc(AnyLcMsg::from(LcMsg::Fetch(
                        //         Identified::new(
                        //             this_chain_id.clone(),
                        //             Fetch::ChannelEnd(FetchChannelEnd {
                        //                 at: trusted_client_state_fetched_at_height,
                        //                 port_id: event.port_id.clone(),
                        //                 channel_id: event.channel_id.clone(),
                        //             }),
                        //         ),
                        //     )))]
                        //     .into(),
                        //     receiver: AggregateReceiver::from(Identified::new(
                        //         this_chain_id.clone(),
                        //         Aggregate::ConnectionFetchFromChannelEnd(
                        //             AggregateConnectionFetchFromChannelEnd {
                        //                 at: trusted_client_state_fetched_at_height,
                        //             },
                        //         ),
                        //     )),
                        // },
                        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Fetch(Identified {
                            chain_id: this_chain_id.clone(),
                            data: Fetch::StateProof(FetchStateProof {
                                at: trusted_client_state_fetched_at_height,
                                path: proof::Path::ChannelEndPath(ChannelEndPath {
                                    port_id: event.port_id.clone(),
                                    channel_id: event.channel_id.clone(),
                                }),
                            }),
                        }))),
                    ]
                    .into(),
                    receiver: AggregateReceiver::from(Identified::new(
                        this_chain_id,
                        Aggregate::ChannelOpenAck(AggregateChannelOpenAck {
                            event_height,
                            event,
                        }),
                    )),
                }
            }
            AggregateMsgAfterUpdate::ChannelOpenConfirm(AggregateChannelOpenConfirm {
                event_height,
                event,
            }) => {
                let consensus_state_height = trusted_client_state_fetched_at_height;

                assert_eq!(
                    consensus_state_height.revision_number(),
                    event_height.revision_number(),
                    "{consensus_state_height}, {event_height}",
                );

                assert!(
                    consensus_state_height.revision_height() >= event_height.revision_height(),
                    "{} < {}",
                    consensus_state_height.revision_height(),
                    event_height.revision_height()
                );

                RelayerMsg::Aggregate {
                    data: [AggregateData::from(Identified::new(
                        this_chain_id.clone(),
                        Data::TrustedClientState(TrustedClientState {
                            fetched_at: trusted_client_state_fetched_at_height,
                            client_id: trusted_client_state_client_id,
                            trusted_client_state,
                        }),
                    ))]
                    .into(),
                    queue: [RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Fetch(
                        Identified {
                            chain_id: this_chain_id.clone(),
                            data: Fetch::StateProof(FetchStateProof {
                                at: trusted_client_state_fetched_at_height,
                                path: proof::Path::ChannelEndPath(ChannelEndPath {
                                    port_id: event.port_id.clone(),
                                    channel_id: event.channel_id.clone(),
                                }),
                            }),
                        },
                    )))]
                    .into(),
                    receiver: AggregateReceiver::from(Identified::new(
                        this_chain_id,
                        Aggregate::ChannelOpenConfirm(AggregateChannelOpenConfirm {
                            event_height,
                            event,
                        }),
                    )),
                }
            }
            AggregateMsgAfterUpdate::RecvPacket(AggregateRecvPacket {
                event_height,
                event,
            }) => {
                //
                tracing::debug!("building aggregate for RecvPacket");

                RelayerMsg::Aggregate {
                    data: [AggregateData::from(Identified::new(
                        this_chain_id.clone(),
                        Data::TrustedClientState(TrustedClientState {
                            fetched_at: trusted_client_state_fetched_at_height,
                            client_id: trusted_client_state_client_id,
                            trusted_client_state,
                        }),
                    ))]
                    .into(),
                    queue: [RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Fetch(
                        Identified {
                            chain_id: this_chain_id.clone(),
                            data: Fetch::StateProof(FetchStateProof {
                                at: trusted_client_state_fetched_at_height,
                                path: proof::Path::CommitmentPath(CommitmentPath {
                                    port_id: event.packet_src_port.clone(),
                                    channel_id: event.packet_src_channel.clone(),
                                    sequence: event.packet_sequence,
                                }),
                            }),
                        },
                    )))]
                    .into(),
                    receiver: AggregateReceiver::from(Identified::new(
                        this_chain_id,
                        Aggregate::RecvPacket(AggregateRecvPacket {
                            event_height,
                            event,
                        }),
                    )),
                }
            }
            AggregateMsgAfterUpdate::AckPacket(AggregateAckPacket {
                event_height,
                event,
                block_hash,
                counterparty_client_id,
            }) => RelayerMsg::Aggregate {
                data: [AggregateData::from(Identified::new(
                    this_chain_id.clone(),
                    Data::TrustedClientState(TrustedClientState {
                        fetched_at: trusted_client_state_fetched_at_height,
                        client_id: trusted_client_state_client_id,
                        trusted_client_state: trusted_client_state.clone(),
                    }),
                ))]
                .into(),
                queue: [
                    RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Fetch(Identified::new(
                        this_chain_id.clone(),
                        Fetch::PacketAcknowledgement(FetchPacketAcknowledgement {
                            block_hash: block_hash.clone(),
                            destination_port_id: event.packet_dst_port.clone(),
                            destination_channel_id: event.packet_dst_channel.clone(),
                            sequence: event.packet_sequence,
                            __marker: PhantomData,
                        }),
                    )))),
                    // RelayerMsg::Aggregate {
                    //     queue: [RelayerMsg::Lc(AnyLcMsg::from(
                    //         LcMsg::<L::Counterparty>::Fetch(Fetch::TrustedClientState(
                    //             Identified::new(FetchTrustedClientState {
                    //                 at: QueryHeight::Specific(trusted_client_state.height()),
                    //                 client_id: counterparty_client_id.clone(),
                    //             }),
                    //         )),
                    //     ))]
                    //     .into(),
                    //     data: [].into(),
                    //     receiver: AggregateReceiver::from(Identified::new(
                    //         this_chain_id,
                    //         Aggregate::FetchCounterpartyStateproof(
                    //             AggregateFetchCounterpartyStateProof {
                    //                 counterparty_client_id: counterparty_client_id.clone(),
                    //                 fetch: FetchStateProof {
                    //                     at: trusted_client_state.height(),
                    //                     path: proof::Path::AcknowledgementPath(
                    //                         AcknowledgementPath {
                    //                             port_id: event.packet_dst_port.clone(),
                    //                             channel_id: event.packet_dst_channel.clone(),
                    //                             sequence: event.packet_sequence,
                    //                         },
                    //                     ),
                    //                 },
                    //             },
                    //         ),
                    //     )),
                    // },
                    // RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L::Counterparty>::Fetch(
                    //     Identified::new(
                    //         trusted_client_state.chain_id(),
                    //         Fetch::StateProof(FetchStateProof {
                    //             at: trusted_client_state.height(),
                    //             path: proof::Path::AcknowledgementPath(AcknowledgementPath {
                    //                 port_id: event.packet_dst_port.clone(),
                    //                 channel_id: event.packet_dst_channel.clone(),
                    //                 sequence: event.packet_sequence,
                    //             }),
                    //         }),
                    //     ),
                    // ))),
                    RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L>::Fetch(Identified::new(
                        this_chain_id.clone(),
                        Fetch::StateProof(FetchStateProof {
                            at: trusted_client_state_fetched_at_height,
                            path: proof::Path::AcknowledgementPath(AcknowledgementPath {
                                port_id: event.packet_dst_port.clone(),
                                channel_id: event.packet_dst_channel.clone(),
                                sequence: event.packet_sequence,
                            }),
                        }),
                    )))),
                ]
                .into(),
                receiver: AggregateReceiver::from(Identified::new(
                    this_chain_id,
                    Aggregate::AckPacket(AggregateAckPacket {
                        event_height,
                        event,
                        block_hash,
                        counterparty_client_id,
                    }),
                )),
            },
        }
    }
}

impl<L: LightClient> UseAggregate<L> for identified!(AggregateConnectionOpenTry<L>)
where
    identified!(TrustedClientState<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(ClientStateProof<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(ClientConsensusStateProof<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(ConnectionProof<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    AnyLcMsg: From<LcMsg<L::Counterparty>>,
{
    type AggregatedData = HList![
        identified!(TrustedClientState<L>),
        identified!(ClientStateProof<L>),
        identified!(ClientConsensusStateProof<L>),
        identified!(ConnectionProof<L>),
    ];

    fn aggregate(
        Identified {
            chain_id: this_chain_id,
            data:
                AggregateConnectionOpenTry {
                    event_height: trusted_height,
                    event,
                },
        }: Self,
        hlist_pat![
            Identified {
                chain_id: trusted_client_state_chain_id,
                data: TrustedClientState {
                    fetched_at: _,
                    client_id: _,
                    trusted_client_state
                }
            },
            Identified {
                chain_id: client_state_proof_chain_id,
                data: ClientStateProof(client_state_proof)
            },
            Identified {
                chain_id: consensus_state_proof_chain_id,
                data: ClientConsensusStateProof(consensus_state_proof)
            },
            Identified {
                chain_id: connection_proof_chain_id,
                data: ConnectionProof(connection_proof)
            },
        ]: Self::AggregatedData,
    ) -> RelayerMsg {
        assert_eq!(this_chain_id, trusted_client_state_chain_id);

        assert!(
            consensus_state_proof.proof_height.revision_height
                >= trusted_height.into_height().revision_height
        );
        assert!(
            client_state_proof.proof_height.revision_height
                >= trusted_height.into_height().revision_height
        );

        let counterparty_chain_id: ChainIdOf<L::Counterparty> = trusted_client_state.chain_id();

        // assert_eq!(counterparty_chain_id, client_updated_chain_id);

        assert_eq!(client_state_proof_chain_id, this_chain_id);
        assert_eq!(consensus_state_proof_chain_id, this_chain_id);
        assert_eq!(connection_proof_chain_id, this_chain_id);

        let consensus_height = trusted_client_state.height();

        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L::Counterparty>::Msg(Identified {
            chain_id: counterparty_chain_id,
            data: Msg::ConnectionOpenTry(MsgConnectionOpenTryData {
                msg: MsgConnectionOpenTry {
                    client_id: event.counterparty_client_id,
                    client_state: client_state_proof.state,
                    counterparty: connection::counterparty::Counterparty {
                        client_id: event.client_id,
                        connection_id: event.connection_id,
                        prefix: MerklePrefix {
                            key_prefix: b"ibc".to_vec(),
                        },
                    },
                    delay_period: DELAY_PERIOD,
                    counterparty_versions: connection_proof.state.versions,
                    proof_height: connection_proof.proof_height.into(),
                    proof_init: connection_proof.proof,
                    proof_client: client_state_proof.proof,
                    proof_consensus: consensus_state_proof.proof,
                    // consensus_height: fetched_at,
                    consensus_height,
                },
            }),
        })))
    }
}

impl<L: LightClient> UseAggregate<L> for identified!(AggregateConnectionOpenAck<L>)
where
    identified!(TrustedClientState<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(ClientStateProof<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(ClientConsensusStateProof<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(ConnectionProof<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    AnyLcMsg: From<LcMsg<L::Counterparty>>,
{
    type AggregatedData = HList![
        identified!(TrustedClientState<L>),
        identified!(ClientStateProof<L>),
        identified!(ClientConsensusStateProof<L>),
        identified!(ConnectionProof<L>),
    ];

    fn aggregate(
        Identified {
            chain_id: this_chain_id,
            data:
                AggregateConnectionOpenAck {
                    event_height: trusted_height,
                    event,
                },
        }: Self,
        hlist_pat![
            Identified {
                chain_id: trusted_client_state_chain_id,
                data: TrustedClientState {
                    fetched_at: _,
                    client_id: _,
                    trusted_client_state
                }
            },
            Identified {
                chain_id: client_state_proof_chain_id,
                data: ClientStateProof(client_state_proof)
            },
            Identified {
                chain_id: consensus_state_proof_chain_id,
                data: ClientConsensusStateProof(consensus_state_proof)
            },
            Identified {
                chain_id: connection_proof_chain_id,
                data: ConnectionProof(connection_proof)
            },
        ]: Self::AggregatedData,
    ) -> RelayerMsg {
        assert_eq!(this_chain_id, trusted_client_state_chain_id);

        assert!(
            consensus_state_proof.proof_height.revision_height
                >= trusted_height.into_height().revision_height
        );
        assert!(
            client_state_proof.proof_height.revision_height
                >= trusted_height.into_height().revision_height
        );

        let counterparty_chain_id: ChainIdOf<L::Counterparty> = trusted_client_state.chain_id();

        // assert_eq!(counterparty_chain_id, client_updated_chain_id);

        assert_eq!(client_state_proof_chain_id, this_chain_id);
        assert_eq!(consensus_state_proof_chain_id, this_chain_id);
        assert_eq!(connection_proof_chain_id, this_chain_id);

        let consensus_height = trusted_client_state.height();

        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L::Counterparty>::Msg(Identified {
            chain_id: counterparty_chain_id,
            data: Msg::ConnectionOpenAck(MsgConnectionOpenAckData {
                msg: MsgConnectionOpenAck {
                    connection_id: event.counterparty_connection_id,
                    counterparty_connection_id: event.connection_id,
                    // TODO: Figure out a way to not panic here, likely by encoding this invariant into the type somehow
                    version: connection_proof.state.versions[0].clone(),
                    client_state: client_state_proof.state,
                    proof_height: connection_proof.proof_height,
                    proof_try: connection_proof.proof,
                    proof_client: client_state_proof.proof,
                    proof_consensus: consensus_state_proof.proof,
                    // consensus_height: consensus_state_proof.proof_height,
                    consensus_height: consensus_height.into(),
                },
            }),
        })))
    }
}

impl<L: LightClient> UseAggregate<L> for identified!(AggregateConnectionOpenConfirm<L>)
where
    identified!(TrustedClientState<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(ClientStateProof<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(ClientConsensusStateProof<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(ConnectionProof<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    AnyLcMsg: From<LcMsg<L::Counterparty>>,
{
    type AggregatedData = HList![
        identified!(TrustedClientState<L>),
        identified!(ConnectionProof<L>),
    ];

    fn aggregate(
        Identified {
            chain_id: this_chain_id,
            data:
                AggregateConnectionOpenConfirm {
                    event_height: _,
                    event,
                },
        }: Self,
        hlist_pat![
            Identified {
                chain_id: trusted_client_state_chain_id,
                data: TrustedClientState {
                    fetched_at: _,
                    client_id: _,
                    trusted_client_state
                }
            },
            Identified {
                chain_id: connection_proof_chain_id,
                data: ConnectionProof(connection_proof)
            },
        ]: Self::AggregatedData,
    ) -> RelayerMsg {
        assert_eq!(this_chain_id, trusted_client_state_chain_id);

        let counterparty_chain_id: ChainIdOf<L::Counterparty> = trusted_client_state.chain_id();

        // assert_eq!(counterparty_chain_id, client_updated_chain_id);
        assert_eq!(connection_proof_chain_id, this_chain_id);

        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L::Counterparty>::Msg(Identified {
            chain_id: counterparty_chain_id,
            data: Msg::ConnectionOpenConfirm(MsgConnectionOpenConfirmData(
                MsgConnectionOpenConfirm {
                    connection_id: event.counterparty_connection_id,
                    proof_height: connection_proof.proof_height.into(),
                    proof_ack: connection_proof.proof,
                },
            )),
        })))
    }
}

impl<L: LightClient> UseAggregate<L> for identified!(AggregateChannelOpenTry<L>)
where
    identified!(TrustedClientState<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(ChannelEndProof<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(ConnectionEnd<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    AnyLcMsg: From<LcMsg<L::Counterparty>>,
{
    type AggregatedData = HList![
        identified!(TrustedClientState<L>),
        identified!(ChannelEndProof<L>),
        identified!(ConnectionEnd<L>),
    ];

    fn aggregate(
        Identified {
            chain_id: this_chain_id,
            data:
                AggregateChannelOpenTry {
                    event_height: _,
                    event,
                },
        }: Self,
        hlist_pat![
            Identified {
                chain_id: trusted_client_state_chain_id,
                data: TrustedClientState {
                    fetched_at: _,
                    client_id: _,
                    trusted_client_state
                }
            },
            Identified {
                chain_id: channel_proof_chain_id,
                data: ChannelEndProof(channel_proof)
            },
            Identified {
                chain_id: _connection_end_chain_id,
                data: ConnectionEnd(connection)
            },
        ]: Self::AggregatedData,
    ) -> RelayerMsg {
        assert_eq!(this_chain_id, trusted_client_state_chain_id);

        let counterparty_chain_id: ChainIdOf<L::Counterparty> = trusted_client_state.chain_id();

        assert_eq!(channel_proof_chain_id, this_chain_id);

        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L::Counterparty>::Msg(Identified {
            chain_id: counterparty_chain_id,
            data: Msg::ChannelOpenTry(MsgChannelOpenTryData {
                msg: MsgChannelOpenTry {
                    port_id: channel_proof.state.counterparty.port_id.clone(),
                    channel: Channel {
                        state: channel::state::State::Tryopen,
                        ordering: channel_proof.state.ordering,
                        counterparty: channel::counterparty::Counterparty {
                            port_id: event.port_id.clone(),
                            channel_id: event.channel_id.clone().to_string(),
                        },
                        connection_hops: vec![connection
                            .counterparty
                            .connection_id
                            .parse()
                            .unwrap()],
                        version: event.version.clone(),
                    },
                    // NOTE: Review behaviour here
                    counterparty_version: event.version,
                    proof_init: channel_proof.proof,
                    proof_height: channel_proof.proof_height,
                },
                __marker: PhantomData,
            }),
        })))
    }
}

impl<L: LightClient> UseAggregate<L> for identified!(AggregateChannelOpenAck<L>)
where
    identified!(TrustedClientState<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(ChannelEndProof<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    AnyLcMsg: From<LcMsg<L::Counterparty>>,
{
    type AggregatedData = HList![
        identified!(TrustedClientState<L>),
        identified!(ChannelEndProof<L>),
    ];

    fn aggregate(
        Identified {
            chain_id: this_chain_id,
            data:
                AggregateChannelOpenAck {
                    event_height: _,
                    event,
                },
        }: Self,
        hlist_pat![
            Identified {
                chain_id: trusted_client_state_chain_id,
                data: TrustedClientState {
                    fetched_at: _,
                    client_id: _,
                    trusted_client_state
                }
            },
            Identified {
                chain_id: channel_proof_chain_id,
                data: ChannelEndProof(channel_proof)
            },
        ]: Self::AggregatedData,
    ) -> RelayerMsg {
        assert_eq!(this_chain_id, trusted_client_state_chain_id);

        let counterparty_chain_id: ChainIdOf<L::Counterparty> = trusted_client_state.chain_id();

        assert_eq!(channel_proof_chain_id, this_chain_id);

        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L::Counterparty>::Msg(Identified {
            chain_id: counterparty_chain_id,
            data: Msg::ChannelOpenAck(MsgChannelOpenAckData {
                msg: MsgChannelOpenAck {
                    port_id: channel_proof.state.counterparty.port_id.clone(),
                    channel_id: event.counterparty_channel_id.to_string(),
                    counterparty_channel_id: event.channel_id.to_string(),
                    counterparty_version: event.version,
                    proof_try: channel_proof.proof,
                    proof_height: channel_proof.proof_height,
                },
                __marker: PhantomData,
            }),
        })))
    }
}

impl<L: LightClient> UseAggregate<L> for identified!(AggregateChannelOpenConfirm<L>)
where
    identified!(TrustedClientState<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(ChannelEndProof<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    AnyLcMsg: From<LcMsg<L::Counterparty>>,
{
    type AggregatedData = HList![
        identified!(TrustedClientState<L>),
        identified!(ChannelEndProof<L>),
    ];

    fn aggregate(
        Identified {
            chain_id: this_chain_id,
            data:
                AggregateChannelOpenConfirm {
                    event_height: _,
                    event,
                },
        }: Self,
        hlist_pat![
            Identified {
                chain_id: trusted_client_state_chain_id,
                data: TrustedClientState {
                    fetched_at: _,
                    client_id: _,
                    trusted_client_state
                }
            },
            Identified {
                chain_id: channel_proof_chain_id,
                data: ChannelEndProof(channel_proof)
            },
        ]: Self::AggregatedData,
    ) -> RelayerMsg {
        assert_eq!(this_chain_id, trusted_client_state_chain_id);

        let counterparty_chain_id: ChainIdOf<L::Counterparty> = trusted_client_state.chain_id();

        assert_eq!(channel_proof_chain_id, this_chain_id);

        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L::Counterparty>::Msg(Identified {
            chain_id: counterparty_chain_id,
            data: Msg::ChannelOpenConfirm(MsgChannelOpenConfirmData {
                msg: MsgChannelOpenConfirm {
                    port_id: channel_proof.state.counterparty.port_id.clone(),
                    channel_id: event.counterparty_channel_id.to_string(),
                    proof_ack: channel_proof.proof,
                    proof_height: channel_proof.proof_height,
                },
                __marker: PhantomData,
            }),
        })))
    }
}

impl<L: LightClient> UseAggregate<L> for identified!(AggregateRecvPacket<L>)
where
    identified!(TrustedClientState<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(CommitmentProof<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    AnyLcMsg: From<LcMsg<L::Counterparty>>,
{
    type AggregatedData = HList![
        identified!(TrustedClientState<L>),
        identified!(CommitmentProof<L>),
    ];

    fn aggregate(
        Identified {
            chain_id: this_chain_id,
            data:
                AggregateRecvPacket {
                    event_height: _,
                    event,
                },
        }: Self,
        hlist_pat![
            Identified {
                chain_id: trusted_client_state_chain_id,
                data: TrustedClientState {
                    fetched_at: _,
                    client_id: _,
                    trusted_client_state
                }
            },
            Identified {
                chain_id: commitment_proof_chain_id,
                data: CommitmentProof(commitment_proof)
            },
        ]: Self::AggregatedData,
    ) -> RelayerMsg {
        assert_eq!(this_chain_id, trusted_client_state_chain_id);

        let counterparty_chain_id: ChainIdOf<L::Counterparty> = trusted_client_state.chain_id();

        assert_eq!(commitment_proof_chain_id, this_chain_id);

        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L::Counterparty>::Msg(Identified {
            chain_id: counterparty_chain_id,
            data: Msg::RecvPacket(MsgRecvPacketData {
                msg: MsgRecvPacket {
                    proof_height: commitment_proof.proof_height,
                    packet: Packet {
                        sequence: event.packet_sequence,
                        source_port: event.packet_src_port,
                        source_channel: event.packet_src_channel,
                        destination_port: event.packet_dst_port,
                        destination_channel: event.packet_dst_channel,
                        data: event.packet_data_hex,
                        timeout_height: event.packet_timeout_height,
                        timeout_timestamp: event.packet_timeout_timestamp,
                    },
                    proof_commitment: commitment_proof.proof,
                },
                __marker: PhantomData,
            }),
        })))
    }
}

impl<L: LightClient> UseAggregate<L> for identified!(AggregateAckPacket<L>)
where
    identified!(TrustedClientState<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(PacketAcknowledgement<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(AcknowledgementProof<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    AnyLcMsg: From<LcMsg<L::Counterparty>>,
{
    type AggregatedData = HList![
        identified!(TrustedClientState<L>),
        identified!(PacketAcknowledgement<L>),
        identified!(AcknowledgementProof<L>),
    ];

    fn aggregate(
        Identified {
            chain_id: this_chain_id,
            data:
                AggregateAckPacket {
                    event_height: _,
                    event,
                    block_hash,
                    counterparty_client_id,
                },
        }: Self,
        hlist_pat![
            Identified {
                chain_id: trusted_client_state_chain_id,
                data: TrustedClientState {
                    fetched_at: _,
                    client_id: _,
                    trusted_client_state
                }
            },
            Identified {
                chain_id: packet_acknowledgement_chain_id,
                data: PacketAcknowledgement { fetched_by, ack }
            },
            Identified {
                chain_id: commitment_proof_chain_id,
                data: AcknowledgementProof(acknowledgement_proof)
            },
        ]: Self::AggregatedData,
    ) -> RelayerMsg {
        assert_eq!(this_chain_id, trusted_client_state_chain_id);

        let counterparty_chain_id: ChainIdOf<L::Counterparty> = trusted_client_state.chain_id();

        assert_eq!(commitment_proof_chain_id, this_chain_id);

        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L::Counterparty>::Msg(Identified {
            chain_id: counterparty_chain_id,
            data: Msg::AckPacket(MsgAckPacketData {
                msg: MsgAcknowledgement {
                    proof_height: acknowledgement_proof.proof_height,
                    packet: Packet {
                        sequence: event.packet_sequence,
                        source_port: event.packet_src_port,
                        source_channel: event.packet_src_channel,
                        destination_port: event.packet_dst_port,
                        destination_channel: event.packet_dst_channel,
                        data: event.packet_data_hex,
                        timeout_height: event.packet_timeout_height,
                        timeout_timestamp: event.packet_timeout_timestamp,
                    },
                    acknowledgement: ack,
                    proof_acked: acknowledgement_proof.proof,
                },
                __marker: PhantomData,
            }),
        })))
    }
}

impl<L: LightClient> UseAggregate<L> for identified!(AggregateFetchCounterpartyStateProof<L>)
where
    identified!(TrustedClientState<L>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    AnyLcMsg: From<LcMsg<L::Counterparty>>,
{
    type AggregatedData = HList![identified!(TrustedClientState<L>),];

    fn aggregate(
        Identified {
            chain_id: this_chain_id,
            data:
                AggregateFetchCounterpartyStateProof {
                    counterparty_client_id,
                    fetch,
                },
        }: Self,
        hlist_pat![Identified {
            chain_id: trusted_client_state_chain_id,
            data: TrustedClientState {
                fetched_at: _,
                client_id: _,
                trusted_client_state
            }
        }]: Self::AggregatedData,
    ) -> RelayerMsg {
        assert_eq!(this_chain_id, trusted_client_state_chain_id);

        let counterparty_chain_id: ChainIdOf<L::Counterparty> = trusted_client_state.chain_id();

        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::<L::Counterparty>::Fetch(
            Identified {
                chain_id: counterparty_chain_id,
                data: Fetch::StateProof(fetch),
            },
        )))
    }
}

impl<L: LightClient> UseAggregate<L> for identified!(AggregateCreateClient<L>)
where
    identified!(SelfClientState<L::Counterparty>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    identified!(SelfConsensusState<L::Counterparty>):
        TryFrom<AggregateData, Error = AggregateData> + Into<AggregateData>,
    AnyLcMsg: From<LcMsg<L>>,
{
    type AggregatedData = HList![
        identified!(SelfClientState<L::Counterparty>),
        identified!(SelfConsensusState<L::Counterparty>),
    ];

    fn aggregate(
        Identified {
            chain_id: this_chain_id,
            data: this,
        }: Self,
        hlist_pat![
            Identified {
                chain_id: self_client_state_chain_id,
                data: SelfClientState(self_client_state)
            },
            Identified {
                chain_id: self_consensus_state_chain_id,
                data: SelfConsensusState(self_consensus_state)
            },
        ]: Self::AggregatedData,
    ) -> RelayerMsg {
        assert_eq!(self_client_state_chain_id, self_consensus_state_chain_id);

        // let counterparty_chain_id = self_client_state_chain_id;

        RelayerMsg::Lc(AnyLcMsg::from(LcMsg::Msg(Identified {
            chain_id: this_chain_id,
            data: Msg::<L>::CreateClient(MsgCreateClientData {
                config: this.config,
                msg: MsgCreateClient {
                    client_state: self_client_state,
                    consensus_state: self_consensus_state,
                },
            }),
        })))
    }
}

fn flatten_seq(msg: RelayerMsg) -> RelayerMsg {
    fn flatten(msg: RelayerMsg) -> VecDeque<RelayerMsg> {
        if let RelayerMsg::Sequence(new_seq) = msg {
            new_seq.into_iter().flat_map(flatten).collect()
        } else {
            [msg].into()
        }
    }

    let mut msgs = flatten(msg);

    if msgs.len() == 1 {
        msgs.pop_front().unwrap()
    } else {
        RelayerMsg::Sequence(msgs)
    }
}

#[test]
fn flatten() {
    let msg = RelayerMsg::Sequence(
        [
            RelayerMsg::DeferUntil { timestamp: 1 },
            RelayerMsg::Sequence(
                [
                    RelayerMsg::DeferUntil { timestamp: 2 },
                    RelayerMsg::DeferUntil { timestamp: 3 },
                ]
                .into(),
            ),
            RelayerMsg::Sequence([RelayerMsg::DeferUntil { timestamp: 4 }].into()),
            RelayerMsg::DeferUntil { timestamp: 5 },
        ]
        .into(),
    );

    let msg = flatten_seq(msg);

    dbg!(msg);
}