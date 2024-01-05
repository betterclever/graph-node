use graph::blockchain::firehose_block_ingestor::FirehoseBlockIngestor;
use graph::blockchain::BlockIngestor;
use graph::env::EnvVars;
use graph::prelude::MetricsRegistry;
use graph::prelude::diesel::r2d2::event;
use graph::substreams::Clock;
use std::collections::HashSet;
use std::sync::Arc;

use graph::blockchain::block_stream::{BlockStreamMapper, FirehoseCursor};
use graph::blockchain::client::ChainClient;
use graph::blockchain::{BasicBlockchainBuilder, BlockchainBuilder, NoopRuntimeAdapter};
use graph::cheap_clone::CheapClone;
use graph::components::store::DeploymentCursorTracker;
use graph::data::subgraph::UnifiedMappingApiVersion;
use graph::{
    blockchain::{
        block_stream::{
            BlockStream, BlockStreamEvent, BlockWithTriggers, FirehoseError,
            FirehoseMapper as FirehoseMapperTrait, TriggersAdapter as TriggersAdapterTrait,
        },
        firehose_block_stream::FirehoseBlockStream,
        Block as _, BlockHash, BlockPtr, Blockchain, BlockchainKind, EmptyNodeCapabilities,
        IngestorError, RuntimeAdapter as RuntimeAdapterTrait,
    },
    components::store::DeploymentLocator,
    firehose::{self, FirehoseEndpoint, ForkStep},
    prelude::{async_trait, o, BlockNumber, ChainStore, Error, Logger, LoggerFactory},
};
use prost::Message;

use crate::adapter::{CosmosEventTypeFilter, CosmosBlockFilter};
use crate::data_source::{
    DataSource, DataSourceTemplate, EventOrigin, UnresolvedDataSource, UnresolvedDataSourceTemplate,
};
use crate::trigger::{CosmosTrigger, self};
use crate::{codec, TriggerFilter};

pub struct Chain {
    logger_factory: LoggerFactory,
    name: String,
    client: Arc<ChainClient<Self>>,
    chain_store: Arc<dyn ChainStore>,
    metrics_registry: Arc<MetricsRegistry>,
}

impl std::fmt::Debug for Chain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "chain: cosmos")
    }
}

impl BlockchainBuilder<Chain> for BasicBlockchainBuilder {
    fn build(self, _config: &Arc<EnvVars>) -> Chain {
        Chain {
            logger_factory: self.logger_factory,
            name: self.name,
            client: Arc::new(ChainClient::new_firehose(self.firehose_endpoints)),
            chain_store: self.chain_store,
            metrics_registry: self.metrics_registry,
        }
    }
}

#[async_trait]
impl Blockchain for Chain {
    const KIND: BlockchainKind = BlockchainKind::Cosmos;

    type Client = ();
    type Block = codec::Block;

    type DataSource = DataSource;

    type UnresolvedDataSource = UnresolvedDataSource;

    type DataSourceTemplate = DataSourceTemplate;

    type UnresolvedDataSourceTemplate = UnresolvedDataSourceTemplate;

    type TriggerData = CosmosTrigger;

    type MappingTrigger = CosmosTrigger;

    type TriggerFilter = TriggerFilter;

    type NodeCapabilities = EmptyNodeCapabilities<Self>;

    fn is_refetch_block_required(&self) -> bool {
        false
    }
    async fn refetch_firehose_block(
        &self,
        _logger: &Logger,
        _cursor: FirehoseCursor,
    ) -> Result<codec::Block, Error> {
        unimplemented!("This chain does not support Dynamic Data Sources. is_refetch_block_required always returns false, this shouldn't be called.")
    }

    fn triggers_adapter(
        &self,
        _loc: &DeploymentLocator,
        _capabilities: &Self::NodeCapabilities,
        _unified_api_version: UnifiedMappingApiVersion,
    ) -> Result<Arc<dyn TriggersAdapterTrait<Self>>, Error> {
        let adapter = TriggersAdapter {};
        Ok(Arc::new(adapter))
    }

    async fn new_block_stream(
        &self,
        deployment: DeploymentLocator,
        store: impl DeploymentCursorTracker,
        start_blocks: Vec<BlockNumber>,
        filter: Arc<Self::TriggerFilter>,
        unified_api_version: UnifiedMappingApiVersion,
    ) -> Result<Box<dyn BlockStream<Self>>, Error> {
        let adapter = self
            .triggers_adapter(
                &deployment,
                &EmptyNodeCapabilities::default(),
                unified_api_version,
            )
            .unwrap_or_else(|_| panic!("no adapter for network {}", self.name));

        let logger = self
            .logger_factory
            .subgraph_logger(&deployment)
            .new(o!("component" => "FirehoseBlockStream"));

        let firehose_mapper = Arc::new(FirehoseMapper { adapter, filter });

        Ok(Box::new(FirehoseBlockStream::new(
            deployment.hash,
            self.chain_client(),
            store.block_ptr(),
            store.firehose_cursor(),
            firehose_mapper,
            start_blocks,
            logger,
            self.metrics_registry.clone(),
        )))
    }

    fn chain_store(&self) -> Arc<dyn ChainStore> {
        self.chain_store.cheap_clone()
    }

    async fn block_pointer_from_number(
        &self,
        logger: &Logger,
        number: BlockNumber,
    ) -> Result<BlockPtr, IngestorError> {
        let firehose_endpoint = self.client.firehose_endpoint()?;

        firehose_endpoint
            .block_ptr_for_number::<codec::HeaderOnlyBlock>(logger, number)
            .await
            .map_err(Into::into)
    }

    fn runtime_adapter(&self) -> Arc<dyn RuntimeAdapterTrait<Self>> {
        Arc::new(NoopRuntimeAdapter::default())
    }

    fn chain_client(&self) -> Arc<ChainClient<Self>> {
        self.client.clone()
    }

    fn block_ingestor(&self) -> anyhow::Result<Box<dyn BlockIngestor>> {
        let ingestor = FirehoseBlockIngestor::<crate::Block, Self>::new(
            self.chain_store.cheap_clone(),
            self.chain_client(),
            self.logger_factory
                .component_logger("CosmosFirehoseBlockIngestor", None),
            self.name.clone(),
        );
        Ok(Box::new(ingestor))
    }
}

pub struct TriggersAdapter {}

#[async_trait]
impl TriggersAdapterTrait<Chain> for TriggersAdapter {
    async fn ancestor_block(
        &self,
        _ptr: BlockPtr,
        _offset: BlockNumber,
    ) -> Result<Option<codec::Block>, Error> {
        panic!("Should never be called since not used by FirehoseBlockStream")
    }

    async fn scan_triggers(
        &self,
        _from: BlockNumber,
        _to: BlockNumber,
        _filter: &TriggerFilter,
    ) -> Result<Vec<BlockWithTriggers<Chain>>, Error> {
        panic!("Should never be called since not used by FirehoseBlockStream")
    }

    async fn triggers_in_block(
        &self,
        logger: &Logger,
        block: codec::Block,
        filter: &TriggerFilter,
    ) -> Result<BlockWithTriggers<Chain>, Error> {
        let shared_block = Arc::new(block.clone());

        let header_only_block = codec::HeaderOnlyBlock::from(&block);

        let mut triggers: Vec<_> = shared_block
            .begin_block_events()?
            .cloned()
            // FIXME (Cosmos): Optimize. Should use an Arc instead of cloning the
            // block. This is not currently possible because EventData is automatically
            // generated.
            .filter_map(|event| {
                filter_event_trigger(
                    filter,
                    event,
                    &header_only_block,
                    None,
                    EventOrigin::BeginBlock,
                )
            })
            .chain(shared_block.transactions().flat_map(|tx| {
                tx.result
                    .as_ref()
                    .unwrap()
                    .events
                    .iter()
                    .filter_map(|e| {
                        filter_event_trigger(
                            filter,
                            e.clone(),
                            &header_only_block,
                            Some(build_tx_context(tx)),
                            EventOrigin::DeliverTx,
                        )
                    })
                    .collect::<Vec<_>>()
            }))
            .chain(
                shared_block
                    .end_block_events()?
                    .cloned()
                    .filter_map(|event| {
                        filter_event_trigger(
                            filter,
                            event,
                            &header_only_block,
                            None,
                            EventOrigin::EndBlock,
                        )
                    }),
            )
            .collect();

        triggers.extend(shared_block.transactions().cloned().flat_map(|tx_result| {
            let mut triggers: Vec<_> = Vec::new();
            if let Some(tx) = tx_result.tx.clone() {
                if let Some(tx_body) = tx.body {
                    triggers.extend(tx_body.messages.into_iter().map(|message| {
                        CosmosTrigger::with_message(
                            message,
                            header_only_block.clone(),
                            build_tx_context(&tx_result),
                        )
                    }));
                }
            }
            triggers.push(CosmosTrigger::with_transaction(
                tx_result,
                header_only_block.clone(),
            ));
            triggers
        }));

        if filter.block_filter.trigger_every_block {
            triggers.push(CosmosTrigger::Block(shared_block.cheap_clone()));
        }

        Ok(BlockWithTriggers::new(block, triggers, logger))
    }

    async fn is_on_main_chain(&self, _ptr: BlockPtr) -> Result<bool, Error> {
        panic!("Should never be called since not used by FirehoseBlockStream")
    }

    /// Panics if `block` is genesis.
    /// But that's ok since this is only called when reverting `block`.
    async fn parent_ptr(&self, block: &BlockPtr) -> Result<Option<BlockPtr>, Error> {
        Ok(Some(BlockPtr {
            hash: BlockHash::from(vec![0xff; 32]),
            number: block.number.saturating_sub(1),
        }))
    }
}

/// Returns a new event trigger only if the given event matches the event filter.
fn filter_event_trigger(
    filter: &TriggerFilter,
    event: codec::Event,
    block: &codec::HeaderOnlyBlock,
    tx_context: Option<codec::TransactionContext>,
    origin: EventOrigin,
) -> Option<CosmosTrigger> {
    if filter.event_type_filter.matches(&event.event_type) {
        Some(CosmosTrigger::with_event(
            event,
            block.clone(),
            tx_context,
            origin,
        ))
    } else {
        None
    }
}

fn build_tx_context(tx: &codec::TxResult) -> codec::TransactionContext {
    codec::TransactionContext {
        hash: tx.hash.clone(),
        index: tx.index,
        code: tx.result.as_ref().unwrap().code,
        gas_wanted: tx.result.as_ref().unwrap().gas_wanted,
        gas_used: tx.result.as_ref().unwrap().gas_used,
    }
}

pub struct FirehoseMapper {
    adapter: Arc<dyn TriggersAdapterTrait<Chain>>,
    filter: Arc<TriggerFilter>,
}

#[async_trait]
impl BlockStreamMapper<Chain> for FirehoseMapper {
    fn decode_block(&self, output: Option<&[u8]>) -> Result<Option<crate::Block>, Error> {
        let block = match output {
            Some(block) => crate::Block::decode(block)?,
            None => anyhow::bail!("cosmos mapper is expected to always have a block"),
        };

        Ok(Some(block))
    }

    async fn block_with_triggers(
        &self,
        logger: &Logger,
        block: crate::Block,
    ) -> Result<BlockWithTriggers<Chain>, Error> {
        self.adapter
            .triggers_in_block(logger, block, self.filter.as_ref())
            .await
    }

    async fn handle_substreams_block(
        &self,
        _logger: &Logger,
        _clock: Clock,
        _cursor: FirehoseCursor,
        _block: Vec<u8>,
    ) -> Result<BlockStreamEvent<Chain>, Error> {
        unimplemented!()
    }
}

#[async_trait]
impl FirehoseMapperTrait<Chain> for FirehoseMapper {
    fn trigger_filter(&self) -> &TriggerFilter {
        self.filter.as_ref()
    }

    async fn to_block_stream_event(
        &self,
        logger: &Logger,
        response: &firehose::Response,
    ) -> Result<BlockStreamEvent<Chain>, FirehoseError> {
        let step = ForkStep::from_i32(response.step).unwrap_or_else(|| {
            panic!(
                "unknown step i32 value {}, maybe you forgot update & re-regenerate the protobuf definitions?",
                response.step
            )
        });

        let any_block = response
            .block
            .as_ref()
            .expect("block payload information should always be present");

        // Right now, this is done in all cases but in reality, with how the BlockStreamEvent::Revert
        // is defined right now, only block hash and block number is necessary. However, this information
        // is not part of the actual bstream::BlockResponseV2 payload. As such, we need to decode the full
        // block which is useless.
        //
        // Check about adding basic information about the block in the bstream::BlockResponseV2 or maybe
        // define a slimmed down struct that would decode only a few fields and ignore all the rest.
        // unwrap: Input cannot be None so output will be error or block.
        let block = self.decode_block(Some(any_block.value.as_ref()))?.unwrap();

        match step {
            ForkStep::StepNew => Ok(BlockStreamEvent::ProcessBlock(
                self.block_with_triggers(logger, block).await?,
                FirehoseCursor::from(response.cursor.clone()),
            )),

            ForkStep::StepUndo => {
                let parent_ptr = block
                    .parent_ptr()
                    .map_err(FirehoseError::from)?
                    .expect("Genesis block should never be reverted");

                Ok(BlockStreamEvent::Revert(
                    parent_ptr,
                    FirehoseCursor::from(response.cursor.clone()),
                ))
            }

            ForkStep::StepFinal => {
                panic!(
                    "final step is not handled and should not be requested in the Firehose request"
                )
            }

            ForkStep::StepUnset => {
                panic!("unknown step should not happen in the Firehose response")
            }
        }
    }

    async fn block_ptr_for_number(
        &self,
        logger: &Logger,
        endpoint: &Arc<FirehoseEndpoint>,
        number: BlockNumber,
    ) -> Result<BlockPtr, Error> {
        endpoint
            .block_ptr_for_number::<codec::HeaderOnlyBlock>(logger, number)
            .await
    }

    async fn final_block_ptr_for(
        &self,
        logger: &Logger,
        endpoint: &Arc<FirehoseEndpoint>,
        block: &codec::Block,
    ) -> Result<BlockPtr, Error> {
        // Cosmos provides instant block finality.
        self.block_ptr_for_number(logger, endpoint, block.number())
            .await
    }
}

#[cfg(test)]
mod test {
    use graph::prelude::{
        slog::{o, Discard, Logger},
        tokio,
    };

    use super::*;

    use codec::{
        Block, Event, Header, HeaderOnlyBlock, ResponseBeginBlock, ResponseDeliverTx,
        ResponseEndBlock, TxResult,
    };


    #[test]
    fn test_decode_block() {
        let base64_data = "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Cn8KB3Jld2FyZHMSKgoGYW1vdW50Eh40ODY4OTMuMDQ4ODgwNTU5OTY2NjU2OTUwdXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjEzZnJ4ZHR5cHp6NzIyd3kzeWx6bG1oOHRxY3lqZThsaHpjaHRxcBgBCoEBCgpjb21taXNzaW9uEikKBmFtb3VudBIdMTExMjQuODYyMDgxMDAzMTU0OTc2NTMydXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFycTU5OGtleHBzZG1oeHE2M3FxNzR2M3RmMjJ1Nnl2bHJlNHIwMBgBCn8KB3Jld2FyZHMSKgoGYW1vdW50Eh4yMjI0OTcuMjQxNjIwMDYzMDk5NTMwNjMxdXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFycTU5OGtleHBzZG1oeHE2M3FxNzR2M3RmMjJ1Nnl2bHJlNHIwMBgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcOTIxMC4xNDQ3NTA4NjQ2NDk5NDQ0MTF1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXR6bjhyazA5ZXoyZ201NXNmZnB5enQ3Y2NuNXl6c2hwcWw4cnVnGAEKfwoHcmV3YXJkcxIqCgZhbW91bnQSHjE4NDIwMi44OTUwMTcyOTI5OTg4ODgyMTl1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXR6bjhyazA5ZXoyZ201NXNmZnB5enQ3Y2NuNXl6c2hwcWw4cnVnGAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50Ehw4NTY5LjgxOTg4ODgwMTU4ODQzNTY2MXV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxbGNxNXQ3Zm1uOGxmZ2FkbWFmNWtoZ3NuenJuc2RxY2NlZmR2OTIYAQp/CgdyZXdhcmRzEioKBmFtb3VudBIeMTIyNDI1Ljk5ODQxMTQ1MTI2MzM2NjU4M3V4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxbGNxNXQ3Zm1uOGxmZ2FkbWFmNWtoZ3NuenJuc2RxY2NlZmR2OTIYAQqAAQoKY29tbWlzc2lvbhIoCgZhbW91bnQSHDQ3MzMuODE4NDE5NDQ0MjIwMTgyNzcxdXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFhZHJ2cDNja3Fqd2xzemo5ejNtZm5kcDNkMHhrdTQ5N3BuaG55ZBgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh05NDY3Ni4zNjgzODg4ODQ0MDM2NTU0MTF1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMWFkcnZwM2NrcWp3bHN6ajl6M21mbmRwM2QweGt1NDk3cG5obnlkGAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50Ehw0NTk5LjU1MTUwOTUwODE0ODc2OTU1OHV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxcXRnZ3RzbWV4bHV2enVsZWh4czd5cHNmbDgyeWs1YXpucnIyemQYAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdOTE5OTEuMDMwMTkwMTYyOTc1MzkxMTYwdXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFxdGdndHNtZXhsdXZ6dWxlaHhzN3lwc2ZsODJ5azVhem5ycjJ6ZBgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcMzM0NC40NDk5MzU3ODM1ODQxNDkzOTB1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXF6Nnhzc2toeXlkNm1ycW5zMmUzZW1wdWxsN2VsMGdxcDVka3J1GAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTY2ODg4Ljk5ODcxNTY3MTY4Mjk4Nzc5M3V4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxcXo2eHNza2h5eWQ2bXJxbnMyZTNlbXB1bGw3ZWwwZ3FwNWRrcnUYAQqAAQoKY29tbWlzc2lvbhIoCgZhbW91bnQSHDY1NDUuODgxOTc3MTM2MzU3ODU1MzkzdXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFmZ2tscDloZW1jemx3dHFwOWpxenEzeGFoaDM4aHpueGF0dHkzOBgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh02NTQ1OC44MTk3NzEzNjM1Nzg1NTM5MzF1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMWZna2xwOWhlbWN6bHd0cXA5anF6cTN4YWhoMzhoem54YXR0eTM4GAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50EhwzMDcyLjkzNDU3MzY0MjQzOTc3ODc4NHV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxZ3A5NTdjenJ5Zmd5dnh3bjN0Zm55eTJmMHQ5ZzJwNHB6OXRxZGYYAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdNjE0NTguNjkxNDcyODQ4Nzk1NTc1Njc2dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFncDk1N2N6cnlmZ3l2eHduM3Rmbnl5MmYwdDlnMnA0cHo5dHFkZhgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcMjk3MC44Mzk3NTQ1MzcxNTIyMDg4MDd1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMWhtdWd6OWdlNWttZnZmYWYzbGpqeno0cGhsMHJkM21rczAwdjl6GAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTU5NDE2Ljc5NTA5MDc0MzA0NDE3NjEzN3V4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxaG11Z3o5Z2U1a21mdmZhZjNsamp6ejRwaGwwcmQzbWtzMDB2OXoYAQqAAQoKY29tbWlzc2lvbhIoCgZhbW91bnQSHDM1MTguNzE4MTI4NzA0ODAzMzA4NzMxdXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFudTY5ZDg5MDZkN2phemMzOXQ3NjlhcWN2MGhjNnlzeGhjNTd3eRgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh01ODY0NS4zMDIxNDUwODAwNTUxNDU1MTd1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMW51NjlkODkwNmQ3amF6YzM5dDc2OWFxY3YwaGM2eXN4aGM1N3d5GAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50EhwyOTA5LjI0NTAxOTAzOTgxMzQ4NzQ2NHV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxenlxdmZhZTU3enlmbGVwajRrN3FtYTg5eGpzNHRwMHZhdGtoN2MYAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdNTgxODQuOTAwMzgwNzk2MjY5NzQ5MjgydXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjF6eXF2ZmFlNTd6eWZsZXBqNGs3cW1hODl4anM0dHAwdmF0a2g3YxgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcMjg0NC4wMjQ2NDAyNDg5ODYxNzI1Nzd1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMTA1NXUwbGxmY2RydnI1dXFhamxkeHBua3pkMnBhbmdsNHZqZXV1GAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTU2ODgwLjQ5MjgwNDk3OTcyMzQ1MTUzOHV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxMDU1dTBsbGZjZHJ2cjV1cWFqbGR4cG5remQycGFuZ2w0dmpldXUYAQqAAQoKY29tbWlzc2lvbhIoCgZhbW91bnQSHDI4MTIuNzM1NjEzMjk2MTI0MTI3MDE0dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFneWR2eGNubTk1endkejdoN3docG11c3k1ZDVjM2NrMHA5bXVjORgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh01NjI1NC43MTIyNjU5MjI0ODI1NDAyNzR1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMWd5ZHZ4Y25tOTV6d2R6N2g3d2hwbXVzeTVkNWMzY2swcDltdWM5GAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50EhwyODExLjY5NzM2MDg5ODQ0MzA3NjI1NHV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxbmNobnJleTM2bnJ2empzbHNjdTBjM2w4ajByNHo5Mmhsc3ozZ2sYAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdNTYyMzMuOTQ3MjE3OTY4ODYxNTI1MDcwdXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFuY2hucmV5MzZucnZ6anNsc2N1MGMzbDhqMHI0ejkyaGxzejNnaxgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcMjgxMC4yNzczMTcyNzUxNTk5NzA1ODV1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMTJjemdwMng1eHNxNnRjMmt2NzVsanh6bDR2dnR5czUzZXlnMHlqGAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTU2MjA1LjU0NjM0NTUwMzE5OTQxMTY5OHV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxMmN6Z3AyeDV4c3E2dGMya3Y3NWxqeHpsNHZ2dHlzNTNleWcweWoYAQqAAQoKY29tbWlzc2lvbhIoCgZhbW91bnQSHDI3NzIuOTE4MDg0NTA1MTY4ODg2MTU4dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjF4MjBseXR5ZjZ6a2NydjVlZHBrZmtuOHN6NTc4cWc1c2o0dmRnMhgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh01NTQ1OC4zNjE2OTAxMDMzNzc3MjMxNjd1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXgyMGx5dHlmNnprY3J2NWVkcGtma244c3o1NzhxZzVzajR2ZGcyGAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50Ehw1NTQzLjc0NTkzMDcxNzY1MjE2ODgyMXV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxaHFrZDc0amF0c2xjYzZsZTlzbmh1bjJsemp6cWQ0ZnM5NDQ4Z2EYAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdNTU0MzcuNDU5MzA3MTc2NTIxNjg4MjA2dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFocWtkNzRqYXRzbGNjNmxlOXNuaHVuMmx6anpxZDRmczk0NDhnYRgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcMzYzMy4xNzE5OTY3MjE3NDQzNDQ2MDl1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMTBzYzk4dnQ2c2F1eDhhc2V4bnNwMmhndmtnbWptZnVsOHc1Y3V3GAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTUxOTAyLjQ1NzA5NjAyNDkxOTIwODY5OXV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxMHNjOTh2dDZzYXV4OGFzZXhuc3AyaGd2a2dtam1mdWw4dzVjdXcYAQqAAQoKY29tbWlzc2lvbhIoCgZhbW91bnQSHDI1MjIuMTQ0NDIzMzcyMTkxOTM4NDYzdXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjE2amQ2NjRyZDA0ajVseWNreWtqZWR0N3ZyaGE3azgyMnZsa3h0eRgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh01MDQ0Mi44ODg0Njc0NDM4Mzg3NjkyNTV1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMTZqZDY2NHJkMDRqNWx5Y2t5a2plZHQ3dnJoYTdrODIydmxreHR5GAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50EhwyNTA0LjI2MzQwOTg1NjU3NjM5ODEyMnV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxZXo4NXhhaHR5cHJwa2d3M2xoMG5lcHJxbHUwd3M2eHVlZ3Vma3IYAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdNTAwODUuMjY4MTk3MTMxNTI3OTYyNDM0dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFlejg1eGFodHlwcnBrZ3czbGgwbmVwcnFsdTB3czZ4dWVndWZrchgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcMjQzNi4yNTkyNTExNTgyMTA1MjY2MzB1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMTdxYW1jN2pqd2ZyNnllN2NmZnJhbnhrZ3hmdW02ZXN4ZTg5dnZ2GAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTQ4NzI1LjE4NTAyMzE2NDIxMDUzMjU5N3V4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxN3FhbWM3amp3ZnI2eWU3Y2ZmcmFueGtneGZ1bTZlc3hlODl2dnYYAQqAAQoKY29tbWlzc2lvbhIoCgZhbW91bnQSHDI0MzMuNjg1NTkzNzU5NzI2NDg1ODIxdXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjEybGtyenphOXJlbTJtdjdkMjQ3Y214anZoN2NxbDZyY3VnODJxaxgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh00ODY3My43MTE4NzUxOTQ1Mjk3MTY0MjZ1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMTJsa3J6emE5cmVtMm12N2QyNDdjbXhqdmg3Y3FsNnJjdWc4MnFrGAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50Ehw0NzY0Ljc0OTAwMjExNjkzMDQxOTU3MnV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxM3Q2NmgwdndzamY5MjQyMGxtd2FrZXgycWRkenl0Y3RkZjNnN2MYAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdNDc2NDcuNDkwMDIxMTY5MzA0MTk1NzE5dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjEzdDY2aDB2d3NqZjkyNDIwbG13YWtleDJxZGR6eXRjdGRmM2c3YxgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcNDY4Mi4xNzc3MjI4MDcxODcxMjEzNTJ1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXh5a215dnprODhxcmxxaDN3dXc0amNrZXdsZXl5Z3Vwc3VteWo1GAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTQ2ODIxLjc3NzIyODA3MTg3MTIxMzUxOXV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxeHlrbXl2ems4OHFybHFoM3d1dzRqY2tld2xleXlndXBzdW15ajUYAQqAAQoKY29tbWlzc2lvbhIoCgZhbW91bnQSHDIzMjQuMjc5MDYwNjc5MjM2MjcwNTQ3dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFsOW03bDZsOGs4ZzdzczdtZ2p3amdjaHBjbHJ0NzRhMnV5djg3ORgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh00NjQ4NS41ODEyMTM1ODQ3MjU0MTA5Mzd1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMWw5bTdsNmw4azhnN3NzN21nandqZ2NocGNscnQ3NGEydXl2ODc5GAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50EhwyMzIzLjc0MDcwNzU4NDE0MjU3MDUyNnV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxNm5mdnBxbWZlZzhxNm16czI0Nm53cGNuamRrdnNqcjJnbnVxdm4YAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdNDY0NzQuODE0MTUxNjgyODUxNDEwNTI1dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjE2bmZ2cHFtZmVnOHE2bXpzMjQ2bndwY25qZGt2c2pyMmdudXF2bhgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcMjI5MS45NzY1MDE2MjM4NzYyMzA2NjN1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMTlqczhnNzVuZm15OXQ5ZmpqcWRlN25rdGw0YXh5dXVjMjkydjU3GAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTQ1ODM5LjUzMDAzMjQ3NzUyNDYxMzI1OXV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxOWpzOGc3NW5mbXk5dDlmampxZGU3bmt0bDRheHl1dWMyOTJ2NTcYAQqAAQoKY29tbWlzc2lvbhIoCgZhbW91bnQSHDQ1NzYuNzg5NjExMDQzNjQ3MTA4OTk0dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjEyaDA0bG1jdWxyZWxjMmplcWhmZDg3Njg4anNuOGVkZmxrNXQ1cRgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh00NTc2Ny44OTYxMTA0MzY0NzEwODk5NDF1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMTJoMDRsbWN1bHJlbGMyamVxaGZkODc2ODhqc244ZWRmbGs1dDVxGAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50EhwyMjI3LjI3MjUwMjMzMjQyNDU1MjU0MHV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxZDB4ZHkwdjk3Z3JzOHJ1OG5jY3F5enljOWw4cHB2MHp2NnA1eGcYAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdNDQ1NDUuNDUwMDQ2NjQ4NDkxMDUwNzk3dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFkMHhkeTB2OTdncnM4cnU4bmNjcXl6eWM5bDhwcHYwenY2cDV4ZxgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcNDQ0NS44MTg3NDA0NjUyOTI5MjIyMTF1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXJ4am1nM2wwbXlhejZmenp2a3VtODJsenlxejVhY3l4cXFjMDhsGAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTQ0NDU4LjE4NzQwNDY1MjkyOTIyMjExM3V4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxcnhqbWczbDBteWF6NmZ6enZrdW04Mmx6eXF6NWFjeXhxcWMwOGwYAQqAAQoKY29tbWlzc2lvbhIoCgZhbW91bnQSHDIyMDIuMjM5MDgzNDEwNTYzMDA1MTk3dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjF0OWV2ZHcwOWhndDhwNHo1YXQ1MzI1c2N3ZGhzemthOTJ2YWZ2chgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh00NDA0NC43ODE2NjgyMTEyNjAxMDM5MzF1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXQ5ZXZkdzA5aGd0OHA0ejVhdDUzMjVzY3dkaHN6a2E5MnZhZnZyGAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50EhwyMTc4Ljg4Mzg5Nzg2MTY3MjU5MTAzOHV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxcGRzZTVycjVuamtra2E2cWV1NW04dTcwNGg2ejY3dzVyNzAwdm4YAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdNDM1NzcuNjc3OTU3MjMzNDUxODIwNzYzdXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFwZHNlNXJyNW5qa2trYTZxZXU1bTh1NzA0aDZ6Njd3NXI3MDB2bhgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcMjE2MC4yNjk1MTU1ODg5NjUxNDcxNDl1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXR2Mjg4dGc4ZmEwYTUzNzQyOTQ5bTRzd3BoMHgybWZnN2pzYzIyGAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTQzMjA1LjM5MDMxMTc3OTMwMjk0Mjk4MXV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxdHYyODh0ZzhmYTBhNTM3NDI5NDltNHN3cGgweDJtZmc3anNjMjIYAQqAAQoKY29tbWlzc2lvbhIoCgZhbW91bnQSHDIxNDUuMzM1NzEwNTk5MDQxNzY4NjQ3dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjE0bXB2Nmp2bnplMDRnbGg0d3g3aDZqMGdwNjA5cmFmc2tqbGNseBgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh00MjkwNi43MTQyMTE5ODA4MzUzNzI5Mzd1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMTRtcHY2anZuemUwNGdsaDR3eDdoNmowZ3A2MDlyYWZza2psY2x4GAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50EhwyMTAwLjQ0MjI4MTE5NzIwMjQxMzE3NnV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxYzM4NmNyOWt6eHM4M2g2MHFzdXd5djhmMGdubDJ5NDd0NzQ0c3EYAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdNDIwMDguODQ1NjIzOTQ0MDQ4MjYzNTExdXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFjMzg2Y3I5a3p4czgzaDYwcXN1d3l2OGYwZ25sMnk0N3Q3NDRzcRgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcNDE3MC4xNzA5Njg5Nzk0MTA4NTYzMzR1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMTBrc2trYzBscXA1M3ZrbWRwZTNrMHhweWNzeDRxenU5bWNoN3RoGAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTQxNzAxLjcwOTY4OTc5NDEwODU2MzMzNXV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxMGtza2tjMGxxcDUzdmttZHBlM2sweHB5Y3N4NHF6dTltY2g3dGgYAQqAAQoKY29tbWlzc2lvbhIoCgZhbW91bnQSHDIwODAuOTk3MDIyMzM2NDAzNjUwMjczdXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFhaDJmOXVscmx3ZGEza2hzMHdnZGFqMDd4d2pnY3pqMHh4M2U5dRgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh00MTYxOS45NDA0NDY3MjgwNzMwMDU0Njh1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMWFoMmY5dWxybHdkYTNraHMwd2dkYWowN3h3amdjemoweHgzZTl1GAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50EhwyMDc1LjgzMzIyNzM0MjY0Njg2NDM2M3V4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxNWtxNXA0ejNjMjZramtsczVwcHF2dnE5Mzl3djNleTNlcjN2Y3oYAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdNDE1MTYuNjY0NTQ2ODUyOTM3Mjg3MjY1dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjE1a3E1cDR6M2MyNmtqa2xzNXBwcXZ2cTkzOXd2M2V5M2VyM3ZjehgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcNDA4OS41OTM3OTM0ODEwODYxMzgxNjl1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXJ6YXV1M3VuZGg5N3l2ZG5qN3d1Mnd3c3RtOXdqOGhlZXEydmN6GAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTQwODk1LjkzNzkzNDgxMDg2MTM4MTY5MHV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxcnphdXUzdW5kaDk3eXZkbmo3d3Uyd3dzdG05d2o4aGVlcTJ2Y3oYAQqAAQoKY29tbWlzc2lvbhIoCgZhbW91bnQSHDIwMzcuMTg5OTEyNTcyODc4NDM5MDE0dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjF5ZHRrYTc5bmhnNjJ2MzZsZ3JlNnZsZmphdXZsZWxhdTlydmd3aBgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh00MDc0My43OTgyNTE0NTc1Njg3ODAyODN1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXlkdGthNzluaGc2MnYzNmxncmU2dmxmamF1dmxlbGF1OXJ2Z3doGAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50Ehw0MDUyLjI3OTg4MTI1MjI2Mzc4MTQxNHV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxdGhsNXN5aG1zY2duajd3aGR5cnlkdzN3NnZ5ODAwNDQ0d25nZGUYAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdNDA1MjIuNzk4ODEyNTIyNjM3ODE0MTM5dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjF0aGw1c3lobXNjZ25qN3doZHlyeWR3M3c2dnk4MDA0NDR3bmdkZRgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcMTk5Mi4yNTI1MzU5Nzk2MDI3NDc4OTZ1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXhmZmZ4MmFna2d5NGQwanBsN2x1bGdrbHBmN2V2Z2xhNjc5cm4yGAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTM5ODQ1LjA1MDcxOTU5MjA1NDk1NzkxNXV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxeGZmZngyYWdrZ3k0ZDBqcGw3bHVsZ2tscGY3ZXZnbGE2NzlybjIYAQqAAQoKY29tbWlzc2lvbhIoCgZhbW91bnQSHDM5MjQuMTkwMjk4NDEyNDQ3MTQ4NDMydXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjF1cDk1NW1zY3I1bDAzOHFmbjc4NnVxanpjdTlydWFuaHFxd3N1dhgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh0zOTI0MS45MDI5ODQxMjQ0NzE0ODQzMjN1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXVwOTU1bXNjcjVsMDM4cWZuNzg2dXFqemN1OXJ1YW5ocXF3c3V2GAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50EhwxOTU0LjI1NjA2ODkzMzc4NjM2NTA5M3V4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxc3I0eDhjczVhZWxldHR6cmh3ZnFkYXJqOGp6eTBqaG1lZXN0eWoYAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdMzkwODUuMTIxMzc4Njc1NzI3MzAxODY4dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFzcjR4OGNzNWFlbGV0dHpyaHdmcWRhcmo4anp5MGpobWVlc3R5ahgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcMTg3Ny4yOTc2Njk5ODAyODg5MDc3NTR1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMW42enVkMnh2OGNyMjZrZHFkcG1kNnowejN2eGZ5Mmo4bTNubTBqGAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTM3NTQ1Ljk1MzM5OTYwNTc3ODE1NTA4MHV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxbjZ6dWQyeHY4Y3IyNmtkcWRwbWQ2ejB6M3Z4ZnkyajhtM25tMGoYAQqAAQoKY29tbWlzc2lvbhIoCgZhbW91bnQSHDM3MzEuOTEwMzQ5MDgxMDg5ODk5MTc3dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFzeW1leTI5OGhjYTVrY2drcnRrZGx1NWNqNWVxanVsaHh2bjA2MBgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh0zNzMxOS4xMDM0OTA4MTA4OTg5OTE3NzF1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXN5bWV5Mjk4aGNhNWtjZ2tydGtkbHU1Y2o1ZXFqdWxoeHZuMDYwGAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50EhwxODMxLjEwMjMwNTAzMjE1MTExNTA0M3V4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxeXZjcmFoZHBjdHlnNzdsNjdjbm5ocWY0ZWY1ajhrYW5xOGplcXYYAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdMzY2MjIuMDQ2MTAwNjQzMDIyMzAwODU0dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjF5dmNyYWhkcGN0eWc3N2w2N2NubmhxZjRlZjVqOGthbnE4amVxdhgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcMTgyOS4yOTkwOTY4MzM1MzM1NjI0NDF1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMWV0dWVhcWU5dGVhYW1xNDBwbG45eHJuY3dnZm5zOG10ZGZyMDJjGAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTM2NTg1Ljk4MTkzNjY3MDY3MTI0ODgyN3V4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxZXR1ZWFxZTl0ZWFhbXE0MHBsbjl4cm5jd2dmbnM4bXRkZnIwMmMYAQqAAQoKY29tbWlzc2lvbhIoCgZhbW91bnQSHDE4MDcuNTQ3OTgzNzcyMDY1MzIxMjg0dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFmOXAyM3J1NHN3OHAyMDQ0MjM3Y2tmaHdkcGtscm4wYWhkYXVqZxgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh0zNjE1MC45NTk2NzU0NDEzMDY0MjU2NzJ1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMWY5cDIzcnU0c3c4cDIwNDQyMzdja2Zod2Rwa2xybjBhaGRhdWpnGAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50EhwzOTcyLjMwOTE3Njk5NTc1OTMyNDAyN3V4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxeTJzdm4yenZjMHB1djNyeDZ3MzlhYTR6bGdqN3FlMGZ6OHNoNngYAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdMzYxMTEuOTAxNjA5MDUyMzU3NDkxMTU4dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjF5MnN2bjJ6dmMwcHV2M3J4NnczOWFhNHpsZ2o3cWUwZno4c2g2eBgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcMTcyNi43MTM5OTE4NzM3ODUzODU4OTZ1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMTV1cnEyZHRwOXFjZTRmeWM4NW02dXB3bTl4dWwzMDQ5bW5jOXlzGAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTM0NTM0LjI3OTgzNzQ3NTcwNzcxNzkxNHV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxNXVycTJkdHA5cWNlNGZ5Yzg1bTZ1cHdtOXh1bDMwNDltbmM5eXMYAQqAAQoKY29tbWlzc2lvbhIoCgZhbW91bnQSHDI3NDQuNjU4MTE3NzIyMDQ4OTM5Njg3dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjEydHB3eXN4NmZxY2VxZzB6cGZ6d2o4OGt3OHd5NGRkczR6NGs2dxgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh0zNDMwOC4yMjY0NzE1MjU2MTE3NDYwODJ1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMTJ0cHd5c3g2ZnFjZXFnMHpwZnp3ajg4a3c4d3k0ZGRzNHo0azZ3GAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50EhwyMzI0LjQzOTQ2NzkyNzk3ODY1NTE1NHV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxeTloMjBncGxoajU1YWdxZTNudGdjaDY2NnlqeDRxeTB5cjNtaHUYAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdMzMyMDYuMjc4MTEzMjU2ODM3OTMwNzc2dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjF5OWgyMGdwbGhqNTVhZ3FlM250Z2NoNjY2eWp4NHF5MHlyM21odRgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcMTYxMS4yMDI5MTkyMzI4NTY3NDk0MTZ1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMWhtd255M3hwdG16bGhjNWt2cWdwODlzdHB5bnl3ZXpzanZzbXA1GAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTMyMjI0LjA1ODM4NDY1NzEzNDk4ODMxOXV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxaG13bnkzeHB0bXpsaGM1a3ZxZ3A4OXN0cHlueXdlenNqdnNtcDUYAQqAAQoKY29tbWlzc2lvbhIoCgZhbW91bnQSHDMwNDUuMjQ2NDY5Njg3ODgzMjcwMzI2dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFhamU5ZGNydW5wc3E0N25majRsdDg0ZnA3NG44bnByc21maHd5ZxgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh0zMDQ1Mi40NjQ2OTY4Nzg4MzI3MDMyNTZ1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMWFqZTlkY3J1bnBzcTQ3bmZqNGx0ODRmcDc0bjhucHJzbWZod3lnGAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50EhwxNTExLjg0MTA2NjA5NTAwOTY0OTQwOXV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxa2QwcXUyYW5mYWg0dXN3bG0yYzlnd21kOTlrbmUwZzdkNmU5cGoYAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdMzAyMzYuODIxMzIxOTAwMTkyOTg4MTczdXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFrZDBxdTJhbmZhaDR1c3dsbTJjOWd3bWQ5OWtuZTBnN2Q2ZTlwahgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcMTQ4OC44MzMzMzgwMjg0MTE2OTg2ODJ1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXJ1cmo3ejJ4Z3I5ZGs3eGFkZXA2bHk4NHhxZ3psYzU0M2VxNThyGAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTI5Nzc2LjY2Njc2MDU2ODIzMzk3MzY0OHV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxcnVyajd6Mnhncjlkazd4YWRlcDZseTg0eHFnemxjNTQzZXE1OHIYAQqAAQoKY29tbWlzc2lvbhIoCgZhbW91bnQSHDE0MjQuMjA0ODcyOTcyMjQxMTYzNjM0dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFxOXdtaHhhamxmZzhqdW45OTk0dDhzZ2V2dnlwdWw2aG02MHF6YRgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh0yODQ4NC4wOTc0NTk0NDQ4MjMyNzI2NzV1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXE5d21oeGFqbGZnOGp1bjk5OTR0OHNnZXZ2eXB1bDZobTYwcXphGAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50EhwxNDE2LjM5MTg4NjM0NDcxOTE5Mzg0MXV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxY3A0cmpkc3V5aHpoZ3Zya3N3c2F5NDR0dDgyMDdzNDg3amN1NTkYAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdMjgzMjcuODM3NzI2ODk0MzgzODc2ODEzdXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFjcDRyamRzdXloemhndnJrc3dzYXk0NHR0ODIwN3M0ODdqY3U1ORgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcMTQxMi4xMjc2MzU0MjU2NzI3MDA3NjJ1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXM0eWY1a3M4eW5tZWFscGEwcXE3cmM0cHN1ODl5bTN2OTQyZTJzGAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTI4MjQyLjU1MjcwODUxMzQ1NDAxNTIzM3V4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxczR5ZjVrczh5bm1lYWxwYTBxcTdyYzRwc3U4OXltM3Y5NDJlMnMYAQqAAQoKY29tbWlzc2lvbhIoCgZhbW91bnQSHDE0MDkuNjg5OTM5NjUwNjk0NDI0Njk5dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFrMzRwYTN0ZGZlaG1oeHpoeHMyZmZ6dWxuZDJ3NW12cTY1ejl0MhgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh0yODE5My43OTg3OTMwMTM4ODg0OTM5ODJ1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMWszNHBhM3RkZmVobWh4emh4czJmZnp1bG5kMnc1bXZxNjV6OXQyGAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50EhwxNDAzLjAxMjcxMzI1MTg1MjUwMjg2M3V4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxeDNlY3hqZ2c2bWxxMzRnM3BtN2R5dXo4bjRqejJleHh4N2VoMG4YAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdMjgwNjAuMjU0MjY1MDM3MDUwMDU3MjU3dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjF4M2VjeGpnZzZtbHEzNGczcG03ZHl1ejhuNGp6MmV4eHg3ZWgwbhgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcMTQwMC40Njc4OTYxOTc3NDgyNzYyOTl1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXZ5cDBranNxejRyYzVoajI1cWFhZDU1YWdhdGY0azN4cjZncTZuGAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTI4MDA5LjM1NzkyMzk1NDk2NTUyNTk3MXV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxdnlwMGtqc3F6NHJjNWhqMjVxYWFkNTVhZ2F0ZjRrM3hyNmdxNm4YAQqAAQoKY29tbWlzc2lvbhIoCgZhbW91bnQSHDIxNDguNjI4NzI4NTg3MzQ3Nzc0NDUwdXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFwNTA5eGt1YzlhMDg1cmpldHJwNmdzMnE2Y2EyMjQ2cjU5azlxMxgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh0yNjg1Ny44NTkxMDczNDE4NDcxODA2MjF1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXA1MDl4a3VjOWEwODVyamV0cnA2Z3MycTZjYTIyNDZyNTlrOXEzGAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50EhwxMzMwLjQzNjY3MzI5NDM4NjQxNjE2NXV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxNzhlY3kwdmNhbnBxcWpldGdqZHlqanFhZHZ1N3lxYXB0bGE3bjQYAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdMjY2MDguNzMzNDY1ODg3NzI4MzIzMzA1dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjE3OGVjeTB2Y2FucHFxamV0Z2pkeWpqcWFkdnU3eXFhcHRsYTduNBgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcMTI5MC42MDY3ODQzNTU4Mzk4NTYyMjR1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXR1ZWRwNzJ1cHJ1cjg5cDU1MG52ZDJsZjJkeXkwOWt2OXd6NHRyGAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTI1ODEyLjEzNTY4NzExNjc5NzEyNDQ4OHV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxdHVlZHA3MnVwcnVyODlwNTUwbnZkMmxmMmR5eTA5a3Y5d3o0dHIYAQqAAQoKY29tbWlzc2lvbhIoCgZhbW91bnQSHDI0NzUuOTU3Mjk4NTIyNDQ4MDc5NjMwdXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjE3bnc4YzV6a3c2N2Q1MGU5aHVyOTVlM3A2NXF1eXoycDVxNXFxYRgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh0yNDc1OS41NzI5ODUyMjQ0ODA3OTYyOTV1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMTdudzhjNXprdzY3ZDUwZTlodXI5NWUzcDY1cXV5ejJwNXE1cXFhGAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50EhwxMjE4Ljc1MzEyNjM1NzYwNjc0NjAxNHV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxdTd6MDQ4Y3phZmhuemttemF4ZXozbTdwc2s3dXRxeG42N3lmajMYAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdMjQzNzUuMDYyNTI3MTUyMTM0OTIwMjc0dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjF1N3owNDhjemFmaG56a216YXhlejNtN3Bzazd1dHF4bjY3eWZqMxgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcMTY4NS4xMzM2MzEzNjYzNDQ0NTc4NjJ1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMTJsNHA5ZzR6NXEzbXR6d2RhbTJ5M214ZnU2dDhxa3c4dXN3emphGAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTI0MDczLjMzNzU5MDk0Nzc3Nzk2OTQ1MHV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxMmw0cDlnNHo1cTNtdHp3ZGFtMnkzbXhmdTZ0OHFrdzh1c3d6amEYAQqAAQoKY29tbWlzc2lvbhIoCgZhbW91bnQSHDExNDIuOTgxMzAxNTcyODg3NDIzMjY2dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjE5djk0YzN6N2NrYXJ3c3VtNzZrYWFnbWEwd3FzcWhoNXF6cDl0NxgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh0yMjg1OS42MjYwMzE0NTc3NDg0NjUzMjF1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMTl2OTRjM3o3Y2thcndzdW03NmthYWdtYTB3cXNxaGg1cXpwOXQ3GAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50EhwxMTA2LjY1MzQ1NDQ1MTkxNTIyMDM0M3V4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxcjdnZGM4YWc0a3RtcnZoZWQyeHAwOW4za2xyanV6bndkenNydTIYAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdMjIxMzMuMDY5MDg5MDM4MzA0NDA2ODY5dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFyN2dkYzhhZzRrdG1ydmhlZDJ4cDA5bjNrbHJqdXpud2R6c3J1MhgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcMTA1Mi45ODcwNjY5NTk2MTg3NzE0NDJ1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXYzMHNhaGc4NWpjOXN4ZXZuNG1uMnBtOHdja3BsbHJkYTg3eDN1GAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTIxMDU5Ljc0MTMzOTE5MjM3NTQyODg0MXV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxdjMwc2FoZzg1amM5c3hldm40bW4ycG04d2NrcGxscmRhODd4M3UYAQqAAQoKY29tbWlzc2lvbhIoCgZhbW91bnQSHDE4OTAuOTQxODk5NTcxNTA1NDYwODkwdXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFlNTNhYWd5czZrc2o5eXFwcTY1Zzc4cW10Y2o1anBsaGc4dTh3cxgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh0yMTAxMC40NjU1NTA3OTQ1MDUxMjA5OTZ1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMWU1M2FhZ3lzNmtzajl5cXBxNjVnNzhxbXRjajVqcGxoZzh1OHdzGAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50EhwxNDk1LjEwODE3Mjk1NTY2NTU4MDY3N3V4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxODRmbGMyMHMzbjYybHZscWRrdXdkdnVmOGhwdTlwZ3A4M3g0cnUYAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdMTk5MzQuNzc1NjM5NDA4ODc0NDA5MDI3dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjE4NGZsYzIwczNuNjJsdmxxZGt1d2R2dWY4aHB1OXBncDgzeDRydRgBCn8KCmNvbW1pc3Npb24SJwoGYW1vdW50Ehs5ODAuMDM2MTAyNTI1MjEyMzgwODI0dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjF3MHZ3YzgyZW16MHVmeXV2bXZ3cHM4czBsY3l4bDM5bnNjdDB3MBgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh0xOTYwMC43MjIwNTA1MDQyNDc2MTY0Nzl1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXcwdndjODJlbXowdWZ5dXZtdndwczhzMGxjeXhsMzluc2N0MHcwGAEKgAEKCmNvbW1pc3Npb24SKAoGYW1vdW50EhwxNjg2LjQ5NTE3MDI5MTAyODEyMzM4NXV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxajZ6Zm4yeWRmZ3kwbngwMnVkOThoM3Y4cHRucjVqZng2bmNja3AYAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdMTg3MzguODM1MjI1NDU1ODY4MDM3NjE0dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFqNnpmbjJ5ZGZneTBueDAydWQ5OGgzdjhwdG5yNWpmeDZuY2NrcBgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcMTg1NS4zMTAzNDA3NjQ4NTA5MTE0NzF1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMW12ZzQ5dHFlcDY4cG5kYXU2Y2hkbDlsbDlwc3Zxc2FjNTI3cmp4GAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTE4NTUzLjEwMzQwNzY0ODUwOTExNDcwNnV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxbXZnNDl0cWVwNjhwbmRhdTZjaGRsOWxsOXBzdnFzYWM1MjdyangYAQp/Cgpjb21taXNzaW9uEicKBmFtb3VudBIbODM3Ljk5NjAzMzEwMzg1Nzk0ODI1OHV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxZ2MzYXlqZDlmcTdhYzNrYWN6NHEzeGxwdXB1dzNmdGV0dXR3bmoYAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdMTY3NTkuOTIwNjYyMDc3MTU4OTY1MTU5dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFnYzNheWpkOWZxN2FjM2thY3o0cTN4bHB1cHV3M2Z0ZXR1dHduahgBCn8KCmNvbW1pc3Npb24SJwoGYW1vdW50Ehs2ODcuODUzMjAwMjYxNDQ5MTI5MjI0dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjF2NGp0aDJoOW5ta2dld3ZzODNuY3c0bTJyN3hxa2Fsd2hkZ3B5MhgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh0xMzc1Ny4wNjQwMDUyMjg5ODI1ODQ0NzJ1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXY0anRoMmg5bm1rZ2V3dnM4M25jdzRtMnI3eHFrYWx3aGRncHkyGAEKfwoKY29tbWlzc2lvbhInCgZhbW91bnQSGzY0OC4xMTM5NTI0MDUyMzk4MTU0OTJ1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXJqZmNncXJnbWxtcnB0cmFtY2p5N2t5bW5jNWs2ajdxZXQ1c3ByGAEKfgoHcmV3YXJkcxIpCgZhbW91bnQSHTEyOTYyLjI3OTA0ODEwNDc5NjMwOTgzMHV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxcmpmY2dxcmdtbG1ycHRyYW1jank3a3ltbmM1azZqN3FldDVzcHIYAQp/Cgpjb21taXNzaW9uEicKBmFtb3VudBIbOTAzLjU5NjgyOTc3MDU1NTMwMjQ3MXV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxemg0N3Q1d3N0OTM0ZzhweTN4MjBkaDN5OWVzd2F3NXRuMjI4Z2cYAQp+CgdyZXdhcmRzEikKBmFtb3VudBIdMTI5MDguNTI2MTM5NTc5MzYxNDYzODY3dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjF6aDQ3dDV3c3Q5MzRnOHB5M3gyMGRoM3k5ZXN3YXc1dG4yMjhnZxgBCn8KCmNvbW1pc3Npb24SJwoGYW1vdW50Ehs2MDEuOTUyOTIxMjAwNDExNjE0MjU2dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFnNjV1eXp2OGR4a2w0NmFtdGszMHBhbnljOG5qa3NsdTl2bWdjbRgBCn4KB3Jld2FyZHMSKQoGYW1vdW50Eh0xMjAzOS4wNTg0MjQwMDgyMzIyODUxMjh1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMWc2NXV5enY4ZHhrbDQ2YW10azMwcGFueWM4bmprc2x1OXZtZ2NtGAEKfwoKY29tbWlzc2lvbhInCgZhbW91bnQSGzQ5NS45NjQ2NTU2MDM4MjA2NzkwNDh1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXBzazlmNGxsNnEydmVoa3F1M2c0eWEyY2gwMDZodDh6NXc5cDZtGAEKfQoHcmV3YXJkcxIoCgZhbW91bnQSHDk5MTkuMjkzMTEyMDc2NDEzNTgwOTU1dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFwc2s5ZjRsbDZxMnZlaGtxdTNnNHlhMmNoMDA2aHQ4ejV3OXA2bRgBCn8KCmNvbW1pc3Npb24SJwoGYW1vdW50Ehs5NjQuMTA1MjQ1NjI5NTc5Mjk0MDI2dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjF1NDQzdWg1d25ndXF6bXVyeWd6NWx4Z205bDNrcTNhYWVrNXd5ZhgBCn0KB3Jld2FyZHMSKAoGYW1vdW50Ehw5NjQxLjA1MjQ1NjI5NTc5Mjk0MDI1NXV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxdTQ0M3VoNXduZ3Vxem11cnlnejVseGdtOWwza3EzYWFlazV3eWYYAQp/Cgpjb21taXNzaW9uEicKBmFtb3VudBIbNDQwLjQzNjAwNTg3NDQxNDAyOTYyOXV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxenU5cHB2d244NGo0bXpzN2xtbTQwcDZ0YTZ5MHhrOHpzbnNwam0YAQp9CgdyZXdhcmRzEigKBmFtb3VudBIcODgwOC43MjAxMTc0ODgyODA1OTI1ODd1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXp1OXBwdnduODRqNG16czdsbW00MHA2dGE2eTB4azh6c25zcGptGAEKfwoKY29tbWlzc2lvbhInCgZhbW91bnQSGzQxNS4xNjA4NzczOTk2NTMzNjgxOTd1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMWtyM2E1MmxoMzJ4a2x3Yzk2bjl0aDRhdHhxZzAzdmY5dmMzMGE1GAEKfQoHcmV3YXJkcxIoCgZhbW91bnQSHDgzMDMuMjE3NTQ3OTkzMDY3MzYzOTM5dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFrcjNhNTJsaDMyeGtsd2M5Nm45dGg0YXR4cWcwM3ZmOXZjMzBhNRgBCn8KCmNvbW1pc3Npb24SJwoGYW1vdW50Ehs0MDMuNzkyMjg4MzE0OTkzOTYxNTY4dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjE5NnRxcmhtZGMzbDB5ZnZ5bDI4d2pubmE0eW13djRlczd0NHAyeBgBCn0KB3Jld2FyZHMSKAoGYW1vdW50Ehw4MDc1Ljg0NTc2NjI5OTg3OTIzMTM1M3V4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxOTZ0cXJobWRjM2wweWZ2eWwyOHdqbm5hNHltd3Y0ZXM3dDRwMngYAQp/Cgpjb21taXNzaW9uEicKBmFtb3VudBIbMzQzLjM0NTY3MzE5MzkyNjgxNjQwMXV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxeXhybnM5ajZ1a2d2eHZtMmp5bGNkdG1qdjZwZ2ZtY2R5bHc1ZTQYAQp9CgdyZXdhcmRzEigKBmFtb3VudBIcNjg2Ni45MTM0NjM4Nzg1MzYzMjgwMjl1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXl4cm5zOWo2dWtndnh2bTJqeWxjZHRtanY2cGdmbWNkeWx3NWU0GAEKfwoKY29tbWlzc2lvbhInCgZhbW91bnQSGzI2Ni4yMTU2MDU1MjM4ODE2MTA4MTV1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMWs4aGdyZnkybjU1dHhla2RtNTd6N25hMms0ZndqczhscDJyNTdrGAEKfQoHcmV3YXJkcxIoCgZhbW91bnQSHDUzMjQuMzEyMTEwNDc3NjMyMjE2MzA5dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFrOGhncmZ5Mm41NXR4ZWtkbTU3ejduYTJrNGZ3anM4bHAycjU3axgBCoABCgpjb21taXNzaW9uEigKBmFtb3VudBIcMTMyNS4yMDY5NTc1MTM0NzUwMjIyMTd1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMTdoamRxa2RsY3Bqd2VzY2tsemN3emY3ZXZ0ZGhtNGcyeWNjc3doGAEKfQoHcmV3YXJkcxIoCgZhbW91bnQSHDUzMDAuODI3ODMwMDUzOTAwMDg4ODY5dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjE3aGpkcWtkbGNwandlc2NrbHpjd3pmN2V2dGRobTRnMnljY3N3aBgBCn8KCmNvbW1pc3Npb24SJwoGYW1vdW50EhsyMTYuNTA3MjExOTYwMzEwNTM0MDI1dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjF4NnQ3cWdtOHlyZjNzeGw1Z2w1dWh2NDRyZ3h2MmpjY3I4a2Y3dBgBCn0KB3Jld2FyZHMSKAoGYW1vdW50Ehw0MzMwLjE0NDIzOTIwNjIxMDY4MDQ5N3V4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxeDZ0N3FnbTh5cmYzc3hsNWdsNXVodjQ0cmd4djJqY2NyOGtmN3QYAQp/Cgpjb21taXNzaW9uEicKBmFtb3VudBIbNDEyLjIyNDY1NTY3MTgyMDM0MjU1M3V4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxeWszajltOXozNDRmajAybGN0d3Rzam5zMmt5ejR2c2hwcXUzMm4YAQp9CgdyZXdhcmRzEigKBmFtb3VudBIcNDEyMi4yNDY1NTY3MTgyMDM0MjU1MzR1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMXlrM2o5bTl6MzQ0ZmowMmxjdHd0c2puczJreXo0dnNocHF1MzJuGAEKfwoKY29tbWlzc2lvbhInCgZhbW91bnQSGzE3OC40OTQyNjQ3MTc3MDU0NDY5MzB1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMTA5eWc2eWhjeXk1bWZ5dGVxbWNuM3BqY2E5bnU5czM5Znh3aDA3GAEKfQoHcmV3YXJkcxIoCgZhbW91bnQSHDM1NjkuODg1Mjk0MzU0MTA4OTM4NTkwdXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjEwOXlnNnloY3l5NW1meXRlcW1jbjNwamNhOW51OXMzOWZ4d2gwNxgBCn4KCmNvbW1pc3Npb24SJgoGYW1vdW50EhozNS4yODk1OTQ3MjMyOTExNTIxNjJ1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMW1yM3U0cGxwOWs1NTV3dHAwbHJjOW52bmhleG1reWprbjNjYWwwGAEKfAoHcmV3YXJkcxInCgZhbW91bnQSGzcwNS43OTE4OTQ0NjU4MjMwNDMyNDh1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMW1yM3U0cGxwOWs1NTV3dHAwbHJjOW52bmhleG1reWprbjNjYWwwGAEKfgoKY29tbWlzc2lvbhImCgZhbW91bnQSGjEzLjMyMDExOTA1NDM3NDI1MjE5NnV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxNGw2dDRseXd3d3E2Z2VxY2E5Mmo5anBubnYwbGd3bm5oeGs4MHEYAQp8CgdyZXdhcmRzEicKBmFtb3VudBIbMjY2LjQwMjM4MTA4NzQ4NTA0MzkyOHV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxNGw2dDRseXd3d3E2Z2VxY2E5Mmo5anBubnYwbGd3bm5oeGs4MHEYAQp9Cgpjb21taXNzaW9uEiUKBmFtb3VudBIZNy4xNzU3NTIzNTE2OTY0NjIyODN1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMTN3Mng1OW0zNjI2cHBwanNxbjQ3ZWxwNGFlOHZmdWE1Znc5dmY4GAEKfAoHcmV3YXJkcxInCgZhbW91bnQSGzE0My41MTUwNDcwMzM5MjkyNDU2NjF1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMTN3Mng1OW0zNjI2cHBwanNxbjQ3ZWxwNGFlOHZmdWE1Znc5dmY4GAEKfQoKY29tbWlzc2lvbhIlCgZhbW91bnQSGTYuOTU0NjQzMDQ0NzgzMDE5NDI2dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjE2d3RzaG5qcnJnMnA3OGpoOG1zYXZjdzBtd2F4c3N1bm40azR4axgBCnsKB3Jld2FyZHMSJgoGYW1vdW50Eho2OS41NDY0MzA0NDc4MzAxOTQyNTl1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMTZ3dHNobmpycmcycDc4amg4bXNhdmN3MG13YXhzc3VubjRrNHhrGAEKfQoKY29tbWlzc2lvbhIlCgZhbW91bnQSGTIuMTQxMDUyMjMyNzgzNjkwMTMxdXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFqbmNwMDVkcmphY25lbXloZ2sybmh6bXJsbHI0Nzc3MmFxYTduNxgBCnsKB3Jld2FyZHMSJgoGYW1vdW50Eho0Mi44MjEwNDQ2NTU2NzM4MDI2MjF1eHBydBgBEkgKCXZhbGlkYXRvchI5cGVyc2lzdGVuY2V2YWxvcGVyMWpuY3AwNWRyamFjbmVteWhnazJuaHptcmxscjQ3NzcyYXFhN243GAEKfQoKY29tbWlzc2lvbhIlCgZhbW91bnQSGTAuMDA1NDkzMzk4OTI5MTQ5ODI5dXhwcnQYARJICgl2YWxpZGF0b3ISOXBlcnNpc3RlbmNldmFsb3BlcjFrdGdudTg1emxkZXpoMzNoeWRoOHBmdDZtNjg5cHM4c3JqaGNlcxgBCnoKB3Jld2FyZHMSJQoGYW1vdW50EhkwLjA1NDkzMzk4OTI5MTQ5ODI5MnV4cHJ0GAESSAoJdmFsaWRhdG9yEjlwZXJzaXN0ZW5jZXZhbG9wZXIxa3RnbnU4NXpsZGV6aDMzaHlkaDhwZnQ2bTY4OXBzOHNyamhjZXMYAQprCg1jb2luX3JlY2VpdmVkEkAKCHJlY2VpdmVyEjJwZXJzaXN0ZW5jZTFtM2gzMHdsdnNmOGxscnV4dHB1a2R2c3kwa20ya3VtOHhhcHRmeRgBEhgKBmFtb3VudBIMNDY0MzQyN3V4cHJ0GAEKZAoIY29pbmJhc2USPgoGbWludGVyEjJwZXJzaXN0ZW5jZTFtM2gzMHdsdnNmOGxscnV4dHB1a2R2c3kwa20ya3VtOHhhcHRmeRgBEhgKBmFtb3VudBIMNDY0MzQyN3V4cHJ0GAEKZwoKY29pbl9zcGVudBI/CgdzcGVuZGVyEjJwZXJzaXN0ZW5jZTFtM2gzMHdsdnNmOGxscnV4dHB1a2R2c3kwa20ya3VtOHhhcHRmeRgBEhgKBmFtb3VudBIMNDY0MzQyN3V4cHJ0GAEKawoNY29pbl9yZWNlaXZlZBJACghyZWNlaXZlchIycGVyc2lzdGVuY2UxN3hwZnZha20yYW1nOTYyeWxzNmY4NHoza2VsbDhjNWw3NDluOWUYARIYCgZhbW91bnQSDDQ2NDM0Mjd1eHBydBgBCqcBCgh0cmFuc2ZlchJBCglyZWNpcGllbnQSMnBlcnNpc3RlbmNlMTd4cGZ2YWttMmFtZzk2MnlsczZmODR6M2tlbGw4YzVsNzQ5bjllGAESPgoGc2VuZGVyEjJwZXJzaXN0ZW5jZTFtM2gzMHdsdnNmOGxscnV4dHB1a2R2c3kwa20ya3VtOHhhcHRmeRgBEhgKBmFtb3VudBIMNDY0MzQyN3V4cHJ0GAEKSQoHbWVzc2FnZRI+CgZzZW5kZXISMnBlcnNpc3RlbmNlMW0zaDMwd2x2c2Y4bGxydXh0cHVrZHZzeTBrbTJrdW04eGFwdGZ5GAEKogEKBG1pbnQSJgoMYm9uZGVkX3JhdGlvEhQwLjc3OTQwNDExNzExNjI5NTI4MhgBEiMKCWluZmxhdGlvbhIUMC4xMjUwMDAwMDAwMDAwMDAwMDAYARI4ChFhbm51YWxfcHJvdmlzaW9ucxIhMjQ0MjI1NzI3MTI4NjQuNzUwMDAwMDAwMDAwMDAwMDAwGAESEwoGYW1vdW50Egc0NjQzNDI3GAEq9DcSJwoKCICAwAIQgMLXLxIOCMCEPRIECIDGChjQhgMaCQoHZWQyNTUxORpqCgpjb2luX3NwZW50Ej8KB3NwZW5kZXISMnBlcnNpc3RlbmNlMTBkMDd5MjY1Z21tdXZ0NHowdzlhdzg4MGpuc3I3MDBqNXc0a2NoGAESGwoGYW1vdW50Eg8zNTAwMDAwMDAwdXhwcnQYARqCAQoNY29pbl9yZWNlaXZlZBJUCghyZWNlaXZlchJGcGVyc2lzdGVuY2UxNnZjc2Z6ZHRhczdqZmxxNm1qdG1ocDAzemdhOGRnZ3BlcHA3MG55Y3g3aDZwMnJ3ZTg0c3UzY3p0eRgBEhsKBmFtb3VudBIPMzUwMDAwMDAwMHV4cHJ0GAEavgEKCHRyYW5zZmVyElUKCXJlY2lwaWVudBJGcGVyc2lzdGVuY2UxNnZjc2Z6ZHRhczdqZmxxNm1qdG1ocDAzemdhOGRnZ3BlcHA3MG55Y3g3aDZwMnJ3ZTg0c3UzY3p0eRgBEj4KBnNlbmRlchIycGVyc2lzdGVuY2UxMGQwN3kyNjVnbW11dnQ0ejB3OWF3ODgwam5zcjcwMGo1dzRrY2gYARIbCgZhbW91bnQSDzM1MDAwMDAwMDB1eHBydBgBGkkKB21lc3NhZ2USPgoGc2VuZGVyEjJwZXJzaXN0ZW5jZTEwZDA3eTI2NWdtbXV2dDR6MHc5YXc4ODBqbnNyNzAwajV3NGtjaBgBGmgKB2V4ZWN1dGUSXQoRX2NvbnRyYWN0X2FkZHJlc3MSRnBlcnNpc3RlbmNlMTZ2Y3NmemR0YXM3amZscTZtanRtaHAwM3pnYThkZ2dwZXBwNzBueWN4N2g2cDJyd2U4NHN1M2N6dHkYARrSCwo9d2FzbS1kZXh0ZXItZ292ZXJuYW5jZS1hZG1pbjo6cmVzdW1lX3Jld2FyZF9zY2hlZHVsZV9jcmVhdGlvbhJdChFfY29udHJhY3RfYWRkcmVzcxJGcGVyc2lzdGVuY2UxNnZjc2Z6ZHRhczdqZmxxNm1qdG1ocDAzemdhOGRnZ3BlcHA3MG55Y3g3aDZwMnJ3ZTg0c3UzY3p0eRgBEj4KBnNlbmRlchIycGVyc2lzdGVuY2UxMGQwN3kyNjVnbW11dnQ0ejB3OWF3ODgwam5zcjcwMGo1dzRrY2gYARIcCgtwcm9wb3NhbF9pZBILcHJvcG9zYWxfaWQYARIqCiNyZXdhcmRfc2NoZWR1bGVfY3JlYXRpb25fcmVxdWVzdF9pZBIBMRgBEqcJCiFyZXdhcmRfc2NoZWR1bGVfY3JlYXRpb25fcmVxdWVzdHMS/whbeyJscF90b2tlbl9hZGRyIjoicGVyc2lzdGVuY2UxZGs5Yzg2aDdnbXZ1YXE4OWN2NzJjamhxNGM5N3Iyd2dsNWd5ZnJ1djZzaHF1d3NwYWxncWpna3V2eiIsInRpdGxlIjoiWFBSVCBpbmNlbnRpdmVzIGZvciBBVE9NL3N0a0FUT00gcG9vbCBmcm9tIERleHRlciBHcmFudCBNdWx0aXNpZyIsImFzc2V0Ijp7Im5hdGl2ZV90b2tlbiI6eyJkZW5vbSI6InV4cHJ0In19LCJhbW91bnQiOiI0NTAwMDAwMDAwIiwic3RhcnRfYmxvY2tfdGltZSI6MTcwNDM2NjAwMCwiZW5kX2Jsb2NrX3RpbWUiOjE3MDcwNDQ0MDB9LHsibHBfdG9rZW5fYWRkciI6InBlcnNpc3RlbmNlMWwyNmwycXJ2dmYwbWM0bXJ0M2dwenVucWw2dDJjcDhqeDUyeDBjNGh1NmhybGNjdzZsNXNjMnBsZmgiLCJ0aXRsZSI6IlhQUlQgaW5jZW50aXZlcyBmb3IgUFNUQUtFL1hQUlQgcG9vbCBmcm9tIERleHRlciBHcmFudCBNdWx0aXNpZyIsImFzc2V0Ijp7Im5hdGl2ZV90b2tlbiI6eyJkZW5vbSI6InV4cHJ0In19LCJhbW91bnQiOiIzMDAwMDAwMDAwIiwic3RhcnRfYmxvY2tfdGltZSI6MTcwNDM2NjAwMCwiZW5kX2Jsb2NrX3RpbWUiOjE3MDcwNDQ0MDB9LHsibHBfdG9rZW5fYWRkciI6InBlcnNpc3RlbmNlMTNsZ3k4ZzRsNTlydDlxdzN2cDByYW5oc2oweHlyNnRudGxocWNseXNxcjZjN3d4eHNlbnFsZDZzbW0iLCJ0aXRsZSI6IlhQUlQgaW5jZW50aXZlcyBmb3IgVVNEQy9VU0RUIHBvb2wgZnJvbSBEZXh0ZXIgR3JhbnQgTXVsdGlzaWciLCJhc3NldCI6eyJuYXRpdmVfdG9rZW4iOnsiZGVub20iOiJ1eHBydCJ9fSwiYW1vdW50IjoiMTUwMDAwMDAwMDAiLCJzdGFydF9ibG9ja190aW1lIjoxNzA0MzY2MDAwLCJlbmRfYmxvY2tfdGltZSI6MTcwNzA0NDQwMH0seyJscF90b2tlbl9hZGRyIjoicGVyc2lzdGVuY2UxNDJ6Z2t4dTBxaDU0eTJzY2xwMnNkbHltdnl2YXp5d2FqbHkyOTh6dHY0dG05dXZ1dzh4czNodTB6YSIsInRpdGxlIjoiWFBSVCBpbmNlbnRpdmVzIGZvciBEWURYL1VTREMgcG9vbCBmcm9tIERleHRlciBHcmFudCBNdWx0aXNpZyIsImFzc2V0Ijp7Im5hdGl2ZV90b2tlbiI6eyJkZW5vbSI6InV4cHJ0In19LCJhbW91bnQiOiIyMDAwMDAwMDAwIiwic3RhcnRfYmxvY2tfdGltZSI6MTcwNDM2NjAwMCwiZW5kX2Jsb2NrX3RpbWUiOjE3MDcwNDQ0MDB9XRgBGn4KCmNvaW5fc3BlbnQSGwoGYW1vdW50Eg80NTAwMDAwMDAwdXhwcnQYARJTCgdzcGVuZGVyEkZwZXJzaXN0ZW5jZTE2dmNzZnpkdGFzN2pmbHE2bWp0bWhwMDN6Z2E4ZGdncGVwcDcwbnljeDdoNnAycndlODRzdTNjenR5GAEaggEKDWNvaW5fcmVjZWl2ZWQSGwoGYW1vdW50Eg80NTAwMDAwMDAwdXhwcnQYARJUCghyZWNlaXZlchJGcGVyc2lzdGVuY2UxZXJ5OGw2anF1eW5uOWE0Y3oycGZmNmtoZzhjNjhmN3VydDMzbDVuOWRuZzJjd3p6NGM0cXM3Mm4wcRgBGtIBCgh0cmFuc2ZlchIbCgZhbW91bnQSDzQ1MDAwMDAwMDB1eHBydBgBElUKCXJlY2lwaWVudBJGcGVyc2lzdGVuY2UxZXJ5OGw2anF1eW5uOWE0Y3oycGZmNmtoZzhjNjhmN3VydDMzbDVuOWRuZzJjd3p6NGM0cXM3Mm4wcRgBElIKBnNlbmRlchJGcGVyc2lzdGVuY2UxNnZjc2Z6ZHRhczdqZmxxNm1qdG1ocDAzemdhOGRnZ3BlcHA3MG55Y3g3aDZwMnJ3ZTg0c3UzY3p0eRgBGmgKB2V4ZWN1dGUSXQoRX2NvbnRyYWN0X2FkZHJlc3MSRnBlcnNpc3RlbmNlMWVyeThsNmpxdXlubjlhNGN6MnBmZjZraGc4YzY4Zjd1cnQzM2w1bjlkbmcyY3d6ejRjNHFzNzJuMHEYARqKBQoxd2FzbS1kZXh0ZXItbXVsdGktc3Rha2luZzo6Y3JlYXRlX3Jld2FyZF9zY2hlZHVsZRJdChFfY29udHJhY3RfYWRkcmVzcxJGcGVyc2lzdGVuY2UxZXJ5OGw2anF1eW5uOWE0Y3oycGZmNmtoZzhjNjhmN3VydDMzbDVuOWRuZzJjd3p6NGM0cXM3Mm4wcRgBEkwKBWFzc2V0EkF7ImluZm8iOnsibmF0aXZlX3Rva2VuIjp7ImRlbm9tIjoidXhwcnQifX0sImFtb3VudCI6IjQ1MDAwMDAwMDAifRgBElMKB2NyZWF0b3ISRnBlcnNpc3RlbmNlMXBoY3p4ZnloMmpteW1kM3FuMHUwdW5sYXp5dHFucnRhc3A4Y2R5MjBqNnc2eTMyM3E4ZnNjdDc1NWgYARIeCg5lbmRfYmxvY2tfdGltZRIKMTcwNzA0NDQwMBgBElQKCGxwX3Rva2VuEkZwZXJzaXN0ZW5jZTFkazljODZoN2dtdnVhcTg5Y3Y3MmNqaHE0Yzk3cjJ3Z2w1Z3lmcnV2NnNocXV3c3BhbGdxamdrdXZ6GAESGgoScmV3YXJkX3NjaGVkdWxlX2lkEgI0NRgBElIKBnNlbmRlchJGcGVyc2lzdGVuY2UxNnZjc2Z6ZHRhczdqZmxxNm1qdG1ocDAzemdhOGRnZ3BlcHA3MG55Y3g3aDZwMnJ3ZTg0c3UzY3p0eRgBEiAKEHN0YXJ0X2Jsb2NrX3RpbWUSCjE3MDQzNjYwMDAYARJLCgV0aXRsZRJAWFBSVCBpbmNlbnRpdmVzIGZvciBBVE9NL3N0a0FUT00gcG9vbCBmcm9tIERleHRlciBHcmFudCBNdWx0aXNpZxgBGn4KCmNvaW5fc3BlbnQSGwoGYW1vdW50Eg8zMDAwMDAwMDAwdXhwcnQYARJTCgdzcGVuZGVyEkZwZXJzaXN0ZW5jZTE2dmNzZnpkdGFzN2pmbHE2bWp0bWhwMDN6Z2E4ZGdncGVwcDcwbnljeDdoNnAycndlODRzdTNjenR5GAEaggEKDWNvaW5fcmVjZWl2ZWQSGwoGYW1vdW50Eg8zMDAwMDAwMDAwdXhwcnQYARJUCghyZWNlaXZlchJGcGVyc2lzdGVuY2UxZXJ5OGw2anF1eW5uOWE0Y3oycGZmNmtoZzhjNjhmN3VydDMzbDVuOWRuZzJjd3p6NGM0cXM3Mm4wcRgBGtIBCgh0cmFuc2ZlchIbCgZhbW91bnQSDzMwMDAwMDAwMDB1eHBydBgBElUKCXJlY2lwaWVudBJGcGVyc2lzdGVuY2UxZXJ5OGw2anF1eW5uOWE0Y3oycGZmNmtoZzhjNjhmN3VydDMzbDVuOWRuZzJjd3p6NGM0cXM3Mm4wcRgBElIKBnNlbmRlchJGcGVyc2lzdGVuY2UxNnZjc2Z6ZHRhczdqZmxxNm1qdG1ocDAzemdhOGRnZ3BlcHA3MG55Y3g3aDZwMnJ3ZTg0c3UzY3p0eRgBGmgKB2V4ZWN1dGUSXQoRX2NvbnRyYWN0X2FkZHJlc3MSRnBlcnNpc3RlbmNlMWVyeThsNmpxdXlubjlhNGN6MnBmZjZraGc4YzY4Zjd1cnQzM2w1bjlkbmcyY3d6ejRjNHFzNzJuMHEYARqJBQoxd2FzbS1kZXh0ZXItbXVsdGktc3Rha2luZzo6Y3JlYXRlX3Jld2FyZF9zY2hlZHVsZRJdChFfY29udHJhY3RfYWRkcmVzcxJGcGVyc2lzdGVuY2UxZXJ5OGw2anF1eW5uOWE0Y3oycGZmNmtoZzhjNjhmN3VydDMzbDVuOWRuZzJjd3p6NGM0cXM3Mm4wcRgBEkwKBWFzc2V0EkF7ImluZm8iOnsibmF0aXZlX3Rva2VuIjp7ImRlbm9tIjoidXhwcnQifX0sImFtb3VudCI6IjMwMDAwMDAwMDAifRgBElMKB2NyZWF0b3ISRnBlcnNpc3RlbmNlMXBoY3p4ZnloMmpteW1kM3FuMHUwdW5sYXp5dHFucnRhc3A4Y2R5MjBqNnc2eTMyM3E4ZnNjdDc1NWgYARIeCg5lbmRfYmxvY2tfdGltZRIKMTcwNzA0NDQwMBgBElQKCGxwX3Rva2VuEkZwZXJzaXN0ZW5jZTFsMjZsMnFydnZmMG1jNG1ydDNncHp1bnFsNnQyY3A4ang1MngwYzRodTZocmxjY3c2bDVzYzJwbGZoGAESGgoScmV3YXJkX3NjaGVkdWxlX2lkEgI0NhgBElIKBnNlbmRlchJGcGVyc2lzdGVuY2UxNnZjc2Z6ZHRhczdqZmxxNm1qdG1ocDAzemdhOGRnZ3BlcHA3MG55Y3g3aDZwMnJ3ZTg0c3UzY3p0eRgBEiAKEHN0YXJ0X2Jsb2NrX3RpbWUSCjE3MDQzNjYwMDAYARJKCgV0aXRsZRI/WFBSVCBpbmNlbnRpdmVzIGZvciBQU1RBS0UvWFBSVCBwb29sIGZyb20gRGV4dGVyIEdyYW50IE11bHRpc2lnGAEafwoKY29pbl9zcGVudBIcCgZhbW91bnQSEDE1MDAwMDAwMDAwdXhwcnQYARJTCgdzcGVuZGVyEkZwZXJzaXN0ZW5jZTE2dmNzZnpkdGFzN2pmbHE2bWp0bWhwMDN6Z2E4ZGdncGVwcDcwbnljeDdoNnAycndlODRzdTNjenR5GAEagwEKDWNvaW5fcmVjZWl2ZWQSHAoGYW1vdW50EhAxNTAwMDAwMDAwMHV4cHJ0GAESVAoIcmVjZWl2ZXISRnBlcnNpc3RlbmNlMWVyeThsNmpxdXlubjlhNGN6MnBmZjZraGc4YzY4Zjd1cnQzM2w1bjlkbmcyY3d6ejRjNHFzNzJuMHEYARrTAQoIdHJhbnNmZXISHAoGYW1vdW50EhAxNTAwMDAwMDAwMHV4cHJ0GAESVQoJcmVjaXBpZW50EkZwZXJzaXN0ZW5jZTFlcnk4bDZqcXV5bm45YTRjejJwZmY2a2hnOGM2OGY3dXJ0MzNsNW45ZG5nMmN3eno0YzRxczcybjBxGAESUgoGc2VuZGVyEkZwZXJzaXN0ZW5jZTE2dmNzZnpkdGFzN2pmbHE2bWp0bWhwMDN6Z2E4ZGdncGVwcDcwbnljeDdoNnAycndlODRzdTNjenR5GAEaaAoHZXhlY3V0ZRJdChFfY29udHJhY3RfYWRkcmVzcxJGcGVyc2lzdGVuY2UxZXJ5OGw2anF1eW5uOWE0Y3oycGZmNmtoZzhjNjhmN3VydDMzbDVuOWRuZzJjd3p6NGM0cXM3Mm4wcRgBGogFCjF3YXNtLWRleHRlci1tdWx0aS1zdGFraW5nOjpjcmVhdGVfcmV3YXJkX3NjaGVkdWxlEl0KEV9jb250cmFjdF9hZGRyZXNzEkZwZXJzaXN0ZW5jZTFlcnk4bDZqcXV5bm45YTRjejJwZmY2a2hnOGM2OGY3dXJ0MzNsNW45ZG5nMmN3eno0YzRxczcybjBxGAESTQoFYXNzZXQSQnsiaW5mbyI6eyJuYXRpdmVfdG9rZW4iOnsiZGVub20iOiJ1eHBydCJ9fSwiYW1vdW50IjoiMTUwMDAwMDAwMDAifRgBElMKB2NyZWF0b3ISRnBlcnNpc3RlbmNlMXBoY3p4ZnloMmpteW1kM3FuMHUwdW5sYXp5dHFucnRhc3A4Y2R5MjBqNnc2eTMyM3E4ZnNjdDc1NWgYARIeCg5lbmRfYmxvY2tfdGltZRIKMTcwNzA0NDQwMBgBElQKCGxwX3Rva2VuEkZwZXJzaXN0ZW5jZTEzbGd5OGc0bDU5cnQ5cXczdnAwcmFuaHNqMHh5cjZ0bnRsaHFjbHlzcXI2Yzd3eHhzZW5xbGQ2c21tGAESGgoScmV3YXJkX3NjaGVkdWxlX2lkEgI0NxgBElIKBnNlbmRlchJGcGVyc2lzdGVuY2UxNnZjc2Z6ZHRhczdqZmxxNm1qdG1ocDAzemdhOGRnZ3BlcHA3MG55Y3g3aDZwMnJ3ZTg0c3UzY3p0eRgBEiAKEHN0YXJ0X2Jsb2NrX3RpbWUSCjE3MDQzNjYwMDAYARJICgV0aXRsZRI9WFBSVCBpbmNlbnRpdmVzIGZvciBVU0RDL1VTRFQgcG9vbCBmcm9tIERleHRlciBHcmFudCBNdWx0aXNpZxgBGn4KCmNvaW5fc3BlbnQSGwoGYW1vdW50Eg8yMDAwMDAwMDAwdXhwcnQYARJTCgdzcGVuZGVyEkZwZXJzaXN0ZW5jZTE2dmNzZnpkdGFzN2pmbHE2bWp0bWhwMDN6Z2E4ZGdncGVwcDcwbnljeDdoNnAycndlODRzdTNjenR5GAEaggEKDWNvaW5fcmVjZWl2ZWQSGwoGYW1vdW50Eg8yMDAwMDAwMDAwdXhwcnQYARJUCghyZWNlaXZlchJGcGVyc2lzdGVuY2UxZXJ5OGw2anF1eW5uOWE0Y3oycGZmNmtoZzhjNjhmN3VydDMzbDVuOWRuZzJjd3p6NGM0cXM3Mm4wcRgBGtIBCgh0cmFuc2ZlchIbCgZhbW91bnQSDzIwMDAwMDAwMDB1eHBydBgBElUKCXJlY2lwaWVudBJGcGVyc2lzdGVuY2UxZXJ5OGw2anF1eW5uOWE0Y3oycGZmNmtoZzhjNjhmN3VydDMzbDVuOWRuZzJjd3p6NGM0cXM3Mm4wcRgBElIKBnNlbmRlchJGcGVyc2lzdGVuY2UxNnZjc2Z6ZHRhczdqZmxxNm1qdG1ocDAzemdhOGRnZ3BlcHA3MG55Y3g3aDZwMnJ3ZTg0c3UzY3p0eRgBGmgKB2V4ZWN1dGUSXQoRX2NvbnRyYWN0X2FkZHJlc3MSRnBlcnNpc3RlbmNlMWVyeThsNmpxdXlubjlhNGN6MnBmZjZraGc4YzY4Zjd1cnQzM2w1bjlkbmcyY3d6ejRjNHFzNzJuMHEYARqHBQoxd2FzbS1kZXh0ZXItbXVsdGktc3Rha2luZzo6Y3JlYXRlX3Jld2FyZF9zY2hlZHVsZRJdChFfY29udHJhY3RfYWRkcmVzcxJGcGVyc2lzdGVuY2UxZXJ5OGw2anF1eW5uOWE0Y3oycGZmNmtoZzhjNjhmN3VydDMzbDVuOWRuZzJjd3p6NGM0cXM3Mm4wcRgBEkwKBWFzc2V0EkF7ImluZm8iOnsibmF0aXZlX3Rva2VuIjp7ImRlbm9tIjoidXhwcnQifX0sImFtb3VudCI6IjIwMDAwMDAwMDAifRgBElMKB2NyZWF0b3ISRnBlcnNpc3RlbmNlMXBoY3p4ZnloMmpteW1kM3FuMHUwdW5sYXp5dHFucnRhc3A4Y2R5MjBqNnc2eTMyM3E4ZnNjdDc1NWgYARIeCg5lbmRfYmxvY2tfdGltZRIKMTcwNzA0NDQwMBgBElQKCGxwX3Rva2VuEkZwZXJzaXN0ZW5jZTE0Mnpna3h1MHFoNTR5MnNjbHAyc2RseW12eXZhenl3YWpseTI5OHp0djR0bTl1dnV3OHhzM2h1MHphGAESGgoScmV3YXJkX3NjaGVkdWxlX2lkEgI0OBgBElIKBnNlbmRlchJGcGVyc2lzdGVuY2UxNnZjc2Z6ZHRhczdqZmxxNm1qdG1ocDAzemdhOGRnZ3BlcHA3MG55Y3g3aDZwMnJ3ZTg0c3UzY3p0eRgBEiAKEHN0YXJ0X2Jsb2NrX3RpbWUSCjE3MDQzNjYwMDAYARJICgV0aXRsZRI9WFBSVCBpbmNlbnRpdmVzIGZvciBEWURYL1VTREMgcG9vbCBmcm9tIERleHRlciBHcmFudCBNdWx0aXNpZxgBGkwKD2FjdGl2ZV9wcm9wb3NhbBITCgtwcm9wb3NhbF9pZBICNjQYARIkCg9wcm9wb3NhbF9yZXN1bHQSD3Byb3Bvc2FsX3Bhc3NlZBgBOroUCL/x/wYaxgIKmgEKlwEKHC9jb3Ntb3MuYmFuay52MWJldGExLk1zZ1NlbmQSdwoycGVyc2lzdGVuY2UxeW43eWZ6Y3B6YTdoeDNuZnNzZ3RkdGh1a2FmM2FhZzYweW43bmMSMnBlcnNpc3RlbmNlMW5leTl3NHB1cjY4OWw5ZHBqM2d1anF3YXN6MHM2NDV5bGdhdWYzGg0KBXV4cHJ0EgQ5MDAwEmUKTgpGCh8vY29zbW9zLmNyeXB0by5zZWNwMjU2azEuUHViS2V5EiMKIQLjl5SmZcOkmVTXjJ4mf0BH865UMdV3pU41s25ofJjzbRIECgIIARITCg0KBXV4cHJ0EgQxMDAwEMCaDBpAdJs4y0nVIuHqWnyG6O0N64IJ302Q8KyYEGshWAHWyjkG87d9Us9JUCCVAk1+p9+itwT8AjdUnSh7uT0gbg/ljCLHERIoEiYKJC9jb3Ntb3MuYmFuay52MWJldGExLk1zZ1NlbmRSZXNwb25zZRrxBlt7Im1zZ19pbmRleCI6MCwiZXZlbnRzIjpbeyJ0eXBlIjoibWVzc2FnZSIsImF0dHJpYnV0ZXMiOlt7ImtleSI6ImFjdGlvbiIsInZhbHVlIjoiL2Nvc21vcy5iYW5rLnYxYmV0YTEuTXNnU2VuZCJ9LHsia2V5Ijoic2VuZGVyIiwidmFsdWUiOiJwZXJzaXN0ZW5jZTF5bjd5ZnpjcHphN2h4M25mc3NndGR0aHVrYWYzYWFnNjB5bjduYyJ9LHsia2V5IjoibW9kdWxlIiwidmFsdWUiOiJiYW5rIn1dfSx7InR5cGUiOiJjb2luX3NwZW50IiwiYXR0cmlidXRlcyI6W3sia2V5Ijoic3BlbmRlciIsInZhbHVlIjoicGVyc2lzdGVuY2UxeW43eWZ6Y3B6YTdoeDNuZnNzZ3RkdGh1a2FmM2FhZzYweW43bmMifSx7ImtleSI6ImFtb3VudCIsInZhbHVlIjoiOTAwMHV4cHJ0In1dfSx7InR5cGUiOiJjb2luX3JlY2VpdmVkIiwiYXR0cmlidXRlcyI6W3sia2V5IjoicmVjZWl2ZXIiLCJ2YWx1ZSI6InBlcnNpc3RlbmNlMW5leTl3NHB1cjY4OWw5ZHBqM2d1anF3YXN6MHM2NDV5bGdhdWYzIn0seyJrZXkiOiJhbW91bnQiLCJ2YWx1ZSI6IjkwMDB1eHBydCJ9XX0seyJ0eXBlIjoidHJhbnNmZXIiLCJhdHRyaWJ1dGVzIjpbeyJrZXkiOiJyZWNpcGllbnQiLCJ2YWx1ZSI6InBlcnNpc3RlbmNlMW5leTl3NHB1cjY4OWw5ZHBqM2d1anF3YXN6MHM2NDV5bGdhdWYzIn0seyJrZXkiOiJzZW5kZXIiLCJ2YWx1ZSI6InBlcnNpc3RlbmNlMXluN3lmemNwemE3aHgzbmZzc2d0ZHRodWthZjNhYWc2MHluN25jIn0seyJrZXkiOiJhbW91bnQiLCJ2YWx1ZSI6IjkwMDB1eHBydCJ9XX0seyJ0eXBlIjoibWVzc2FnZSIsImF0dHJpYnV0ZXMiOlt7ImtleSI6InNlbmRlciIsInZhbHVlIjoicGVyc2lzdGVuY2UxeW43eWZ6Y3B6YTdoeDNuZnNzZ3RkdGh1a2FmM2FhZzYweW43bmMifV19XX1dKMCaDDD7hwQ6ZAoKY29pbl9zcGVudBI/CgdzcGVuZGVyEjJwZXJzaXN0ZW5jZTF5bjd5ZnpjcHphN2h4M25mc3NndGR0aHVrYWYzYWFnNjB5bjduYxgBEhUKBmFtb3VudBIJMTAwMHV4cHJ0GAE6aAoNY29pbl9yZWNlaXZlZBJACghyZWNlaXZlchIycGVyc2lzdGVuY2UxN3hwZnZha20yYW1nOTYyeWxzNmY4NHoza2VsbDhjNWw3NDluOWUYARIVCgZhbW91bnQSCTEwMDB1eHBydBgBOqQBCgh0cmFuc2ZlchJBCglyZWNpcGllbnQSMnBlcnNpc3RlbmNlMTd4cGZ2YWttMmFtZzk2MnlsczZmODR6M2tlbGw4YzVsNzQ5bjllGAESPgoGc2VuZGVyEjJwZXJzaXN0ZW5jZTF5bjd5ZnpjcHphN2h4M25mc3NndGR0aHVrYWYzYWFnNjB5bjduYxgBEhUKBmFtb3VudBIJMTAwMHV4cHJ0GAE6SQoHbWVzc2FnZRI+CgZzZW5kZXISMnBlcnNpc3RlbmNlMXluN3lmemNwemE3aHgzbmZzc2d0ZHRodWthZjNhYWc2MHluN25jGAE6WwoCdHgSEgoDZmVlEgkxMDAwdXhwcnQYARJBCglmZWVfcGF5ZXISMnBlcnNpc3RlbmNlMXluN3lmemNwemE3aHgzbmZzc2d0ZHRodWthZjNhYWc2MHluN25jGAE6RwoCdHgSQQoHYWNjX3NlcRI0cGVyc2lzdGVuY2UxeW43eWZ6Y3B6YTdoeDNuZnNzZ3RkdGh1a2FmM2FhZzYweW43bmMvMBgBOm0KAnR4EmcKCXNpZ25hdHVyZRJYZEpzNHkwblZJdUhxV255RzZPME42NElKMzAyUThLeVlFR3NoV0FIV3lqa0c4N2Q5VXM5SlVDQ1ZBazErcDkraXR3VDhBamRVblNoN3VUMGdiZy9sakE9PRgBOoUBCgdtZXNzYWdlEigKBmFjdGlvbhIcL2Nvc21vcy5iYW5rLnYxYmV0YTEuTXNnU2VuZBgBEj4KBnNlbmRlchIycGVyc2lzdGVuY2UxeW43eWZ6Y3B6YTdoeDNuZnNzZ3RkdGh1a2FmM2FhZzYweW43bmMYARIQCgZtb2R1bGUSBGJhbmsYATpkCgpjb2luX3NwZW50Ej8KB3NwZW5kZXISMnBlcnNpc3RlbmNlMXluN3lmemNwemE3aHgzbmZzc2d0ZHRodWthZjNhYWc2MHluN25jGAESFQoGYW1vdW50Egk5MDAwdXhwcnQYATpoCg1jb2luX3JlY2VpdmVkEkAKCHJlY2VpdmVyEjJwZXJzaXN0ZW5jZTFuZXk5dzRwdXI2ODlsOWRwajNndWpxd2FzejBzNjQ1eWxnYXVmMxgBEhUKBmFtb3VudBIJOTAwMHV4cHJ0GAE6pAEKCHRyYW5zZmVyEkEKCXJlY2lwaWVudBIycGVyc2lzdGVuY2UxbmV5OXc0cHVyNjg5bDlkcGozZ3VqcXdhc3owczY0NXlsZ2F1ZjMYARI+CgZzZW5kZXISMnBlcnNpc3RlbmNlMXluN3lmemNwemE3aHgzbmZzc2d0ZHRodWthZjNhYWc2MHluN25jGAESFQoGYW1vdW50Egk5MDAwdXhwcnQYATpJCgdtZXNzYWdlEj4KBnNlbmRlchIycGVyc2lzdGVuY2UxeW43eWZ6Y3B6YTdoeDNuZnNzZ3RkdGh1a2FmM2FhZzYweW43bmMYASogTFu6B3Fca6mWJDo642vLr6qDFHTa++ogdXtOAUGCPq86ozUIv/H/BhABGrQDCoYCCoMCCikvaWJjLmFwcGxpY2F0aW9ucy50cmFuc2Zlci52MS5Nc2dUcmFuc2ZlchLVAQoIdHJhbnNmZXISCmNoYW5uZWwtMjQaUQpEaWJjL0M4QTc0QUJCRTJBRjg5MkUxNTY4MEQ5MTZBN0MyMjEzMDU4NUNFNTcwNEY5QjE3QTEwRjE4NEE5MEQ1M0JFQ0ESCTIwNTc0MTc3MSIycGVyc2lzdGVuY2UxdzB1bWRuaDJtenlwNDQ2bmU3d2Q2c2VtcTl4eTBsbXo1c2pyeGwqLWNvc21vczF3MHVtZG5oMm16eXA0NDZuZTd3ZDZzZW1xOXh5MGxtejZ1NXNnbTIHCAQQntznCBJnClAKRgofL2Nvc21vcy5jcnlwdG8uc2VjcDI1NmsxLlB1YktleRIjCiECar56zO9YRiYPVRnNsyR084ovgW0H6XxAU3pjxc9mNnESBAoCCAEYFBITCg0KBXV4cHJ0EgQzMDg1EOvDBxpAxjRUIwxqhZb0jn5zWzO/oUvI+2mWhTUgfjanxIYvaP1jDXlJojFFjoMgLBK+fOfuOx5FFpnh71cUWmJI/OlekiLAMRI7EjkKMS9pYmMuYXBwbGljYXRpb25zLnRyYW5zZmVyLnYxLk1zZ1RyYW5zZmVyUmVzcG9uc2USBAjuiwEawRhbeyJtc2dfaW5kZXgiOjAsImV2ZW50cyI6W3sidHlwZSI6Im1lc3NhZ2UiLCJhdHRyaWJ1dGVzIjpbeyJrZXkiOiJhY3Rpb24iLCJ2YWx1ZSI6Ii9pYmMuYXBwbGljYXRpb25zLnRyYW5zZmVyLnYxLk1zZ1RyYW5zZmVyIn0seyJrZXkiOiJzZW5kZXIiLCJ2YWx1ZSI6InBlcnNpc3RlbmNlMXcwdW1kbmgybXp5cDQ0Nm5lN3dkNnNlbXE5eHkwbG16NXNqcnhsIn1dfSx7InR5cGUiOiJjb2luX3NwZW50IiwiYXR0cmlidXRlcyI6W3sia2V5Ijoic3BlbmRlciIsInZhbHVlIjoicGVyc2lzdGVuY2UxdzB1bWRuaDJtenlwNDQ2bmU3d2Q2c2VtcTl4eTBsbXo1c2pyeGwifSx7ImtleSI6ImFtb3VudCIsInZhbHVlIjoiMjA1NzQxNzcxaWJjL0M4QTc0QUJCRTJBRjg5MkUxNTY4MEQ5MTZBN0MyMjEzMDU4NUNFNTcwNEY5QjE3QTEwRjE4NEE5MEQ1M0JFQ0EifV19LHsidHlwZSI6ImNvaW5fcmVjZWl2ZWQiLCJhdHRyaWJ1dGVzIjpbeyJrZXkiOiJyZWNlaXZlciIsInZhbHVlIjoicGVyc2lzdGVuY2UxeWw2aGRqaG1rZjM3NjM5NzMwZ2ZmYW5wem5kemRwbWhxbnM2ZTgifSx7ImtleSI6ImFtb3VudCIsInZhbHVlIjoiMjA1NzQxNzcxaWJjL0M4QTc0QUJCRTJBRjg5MkUxNTY4MEQ5MTZBN0MyMjEzMDU4NUNFNTcwNEY5QjE3QTEwRjE4NEE5MEQ1M0JFQ0EifV19LHsidHlwZSI6InRyYW5zZmVyIiwiYXR0cmlidXRlcyI6W3sia2V5IjoicmVjaXBpZW50IiwidmFsdWUiOiJwZXJzaXN0ZW5jZTF5bDZoZGpobWtmMzc2Mzk3MzBnZmZhbnB6bmR6ZHBtaHFuczZlOCJ9LHsia2V5Ijoic2VuZGVyIiwidmFsdWUiOiJwZXJzaXN0ZW5jZTF3MHVtZG5oMm16eXA0NDZuZTd3ZDZzZW1xOXh5MGxtejVzanJ4bCJ9LHsia2V5IjoiYW1vdW50IiwidmFsdWUiOiIyMDU3NDE3NzFpYmMvQzhBNzRBQkJFMkFGODkyRTE1NjgwRDkxNkE3QzIyMTMwNTg1Q0U1NzA0RjlCMTdBMTBGMTg0QTkwRDUzQkVDQSJ9XX0seyJ0eXBlIjoibWVzc2FnZSIsImF0dHJpYnV0ZXMiOlt7ImtleSI6InNlbmRlciIsInZhbHVlIjoicGVyc2lzdGVuY2UxdzB1bWRuaDJtenlwNDQ2bmU3d2Q2c2VtcTl4eTBsbXo1c2pyeGwifV19LHsidHlwZSI6ImNvaW5fc3BlbnQiLCJhdHRyaWJ1dGVzIjpbeyJrZXkiOiJzcGVuZGVyIiwidmFsdWUiOiJwZXJzaXN0ZW5jZTF5bDZoZGpobWtmMzc2Mzk3MzBnZmZhbnB6bmR6ZHBtaHFuczZlOCJ9LHsia2V5IjoiYW1vdW50IiwidmFsdWUiOiIyMDU3NDE3NzFpYmMvQzhBNzRBQkJFMkFGODkyRTE1NjgwRDkxNkE3QzIyMTMwNTg1Q0U1NzA0RjlCMTdBMTBGMTg0QTkwRDUzQkVDQSJ9XX0seyJ0eXBlIjoiYnVybiIsImF0dHJpYnV0ZXMiOlt7ImtleSI6ImJ1cm5lciIsInZhbHVlIjoicGVyc2lzdGVuY2UxeWw2aGRqaG1rZjM3NjM5NzMwZ2ZmYW5wem5kemRwbWhxbnM2ZTgifSx7ImtleSI6ImFtb3VudCIsInZhbHVlIjoiMjA1NzQxNzcxaWJjL0M4QTc0QUJCRTJBRjg5MkUxNTY4MEQ5MTZBN0MyMjEzMDU4NUNFNTcwNEY5QjE3QTEwRjE4NEE5MEQ1M0JFQ0EifV19LHsidHlwZSI6InNlbmRfcGFja2V0IiwiYXR0cmlidXRlcyI6W3sia2V5IjoicGFja2V0X2RhdGEiLCJ2YWx1ZSI6IntcImFtb3VudFwiOlwiMjA1NzQxNzcxXCIsXCJkZW5vbVwiOlwidHJhbnNmZXIvY2hhbm5lbC0yNC91YXRvbVwiLFwicmVjZWl2ZXJcIjpcImNvc21vczF3MHVtZG5oMm16eXA0NDZuZTd3ZDZzZW1xOXh5MGxtejZ1NXNnbVwiLFwic2VuZGVyXCI6XCJwZXJzaXN0ZW5jZTF3MHVtZG5oMm16eXA0NDZuZTd3ZDZzZW1xOXh5MGxtejVzanJ4bFwifSJ9LHsia2V5IjoicGFja2V0X2RhdGFfaGV4IiwidmFsdWUiOiI3YjIyNjE2ZDZmNzU2ZTc0MjIzYTIyMzIzMDM1MzczNDMxMzczNzMxMjIyYzIyNjQ2NTZlNmY2ZDIyM2EyMjc0NzI2MTZlNzM2NjY1NzIyZjYzNjg2MTZlNmU2NTZjMmQzMjM0MmY3NTYxNzQ2ZjZkMjIyYzIyNzI2NTYzNjU2OTc2NjU3MjIyM2EyMjYzNmY3MzZkNmY3MzMxNzczMDc1NmQ2NDZlNjgzMjZkN2E3OTcwMzQzNDM2NmU2NTM3Nzc2NDM2NzM2NTZkNzEzOTc4NzkzMDZjNmQ3YTM2NzUzNTczNjc2ZDIyMmMyMjczNjU2ZTY0NjU3MjIyM2EyMjcwNjU3MjczNjk3Mzc0NjU2ZTYzNjUzMTc3MzA3NTZkNjQ2ZTY4MzI2ZDdhNzk3MDM0MzQzNjZlNjUzNzc3NjQzNjczNjU2ZDcxMzk3ODc5MzA2YzZkN2EzNTczNmE3Mjc4NmMyMjdkIn0seyJrZXkiOiJwYWNrZXRfdGltZW91dF9oZWlnaHQiLCJ2YWx1ZSI6IjQtMTg0NzY1NzQifSx7ImtleSI6InBhY2tldF90aW1lb3V0X3RpbWVzdGFtcCIsInZhbHVlIjoiMCJ9LHsia2V5IjoicGFja2V0X3NlcXVlbmNlIiwidmFsdWUiOiIxNzkwMiJ9LHsia2V5IjoicGFja2V0X3NyY19wb3J0IiwidmFsdWUiOiJ0cmFuc2ZlciJ9LHsia2V5IjoicGFja2V0X3NyY19jaGFubmVsIiwidmFsdWUiOiJjaGFubmVsLTI0In0seyJrZXkiOiJwYWNrZXRfZHN0X3BvcnQiLCJ2YWx1ZSI6InRyYW5zZmVyIn0seyJrZXkiOiJwYWNrZXRfZHN0X2NoYW5uZWwiLCJ2YWx1ZSI6ImNoYW5uZWwtMTkwIn0seyJrZXkiOiJwYWNrZXRfY2hhbm5lbF9vcmRlcmluZyIsInZhbHVlIjoiT1JERVJfVU5PUkRFUkVEIn0seyJrZXkiOiJwYWNrZXRfY29ubmVjdGlvbiIsInZhbHVlIjoiY29ubmVjdGlvbi0zMCJ9LHsia2V5IjoiY29ubmVjdGlvbl9pZCIsInZhbHVlIjoiY29ubmVjdGlvbi0zMCJ9XX0seyJ0eXBlIjoibWVzc2FnZSIsImF0dHJpYnV0ZXMiOlt7ImtleSI6Im1vZHVsZSIsInZhbHVlIjoiaWJjX2NoYW5uZWwifV19LHsidHlwZSI6ImliY190cmFuc2ZlciIsImF0dHJpYnV0ZXMiOlt7ImtleSI6InNlbmRlciIsInZhbHVlIjoicGVyc2lzdGVuY2UxdzB1bWRuaDJtenlwNDQ2bmU3d2Q2c2VtcTl4eTBsbXo1c2pyeGwifSx7ImtleSI6InJlY2VpdmVyIiwidmFsdWUiOiJjb3Ntb3MxdzB1bWRuaDJtenlwNDQ2bmU3d2Q2c2VtcTl4eTBsbXo2dTVzZ20ifSx7ImtleSI6ImFtb3VudCIsInZhbHVlIjoiMjA1NzQxNzcxIn0seyJrZXkiOiJkZW5vbSIsInZhbHVlIjoiaWJjL0M4QTc0QUJCRTJBRjg5MkUxNTY4MEQ5MTZBN0MyMjEzMDU4NUNFNTcwNEY5QjE3QTEwRjE4NEE5MEQ1M0JFQ0EifSx7ImtleSI6Im1lbW8ifV19LHsidHlwZSI6Im1lc3NhZ2UiLCJhdHRyaWJ1dGVzIjpbeyJrZXkiOiJtb2R1bGUiLCJ2YWx1ZSI6InRyYW5zZmVyIn1dfV19XSjrwwcw7qsGOmQKCmNvaW5fc3BlbnQSPwoHc3BlbmRlchIycGVyc2lzdGVuY2UxdzB1bWRuaDJtenlwNDQ2bmU3d2Q2c2VtcTl4eTBsbXo1c2pyeGwYARIVCgZhbW91bnQSCTMwODV1eHBydBgBOmgKDWNvaW5fcmVjZWl2ZWQSQAoIcmVjZWl2ZXISMnBlcnNpc3RlbmNlMTd4cGZ2YWttMmFtZzk2MnlsczZmODR6M2tlbGw4YzVsNzQ5bjllGAESFQoGYW1vdW50EgkzMDg1dXhwcnQYATqkAQoIdHJhbnNmZXISQQoJcmVjaXBpZW50EjJwZXJzaXN0ZW5jZTE3eHBmdmFrbTJhbWc5NjJ5bHM2Zjg0ejNrZWxsOGM1bDc0OW45ZRgBEj4KBnNlbmRlchIycGVyc2lzdGVuY2UxdzB1bWRuaDJtenlwNDQ2bmU3d2Q2c2VtcTl4eTBsbXo1c2pyeGwYARIVCgZhbW91bnQSCTMwODV1eHBydBgBOkkKB21lc3NhZ2USPgoGc2VuZGVyEjJwZXJzaXN0ZW5jZTF3MHVtZG5oMm16eXA0NDZuZTd3ZDZzZW1xOXh5MGxtejVzanJ4bBgBOlsKAnR4EhIKA2ZlZRIJMzA4NXV4cHJ0GAESQQoJZmVlX3BheWVyEjJwZXJzaXN0ZW5jZTF3MHVtZG5oMm16eXA0NDZuZTd3ZDZzZW1xOXh5MGxtejVzanJ4bBgBOkgKAnR4EkIKB2FjY19zZXESNXBlcnNpc3RlbmNlMXcwdW1kbmgybXp5cDQ0Nm5lN3dkNnNlbXE5eHkwbG16NXNqcnhsLzIwGAE6bQoCdHgSZwoJc2lnbmF0dXJlElh4alJVSXd4cWhaYjBqbjV6V3pPL29VdkkrMm1XaFRVZ2ZqYW54SVl2YVAxakRYbEpvakZGam9NZ0xCSytmT2Z1T3g1RkZwbmg3MWNVV21KSS9PbGVrZz09GAE6gAEKB21lc3NhZ2USNQoGYWN0aW9uEikvaWJjLmFwcGxpY2F0aW9ucy50cmFuc2Zlci52MS5Nc2dUcmFuc2ZlchgBEj4KBnNlbmRlchIycGVyc2lzdGVuY2UxdzB1bWRuaDJtenlwNDQ2bmU3d2Q2c2VtcTl4eTBsbXo1c2pyeGwYATqoAQoKY29pbl9zcGVudBI/CgdzcGVuZGVyEjJwZXJzaXN0ZW5jZTF3MHVtZG5oMm16eXA0NDZuZTd3ZDZzZW1xOXh5MGxtejVzanJ4bBgBElkKBmFtb3VudBJNMjA1NzQxNzcxaWJjL0M4QTc0QUJCRTJBRjg5MkUxNTY4MEQ5MTZBN0MyMjEzMDU4NUNFNTcwNEY5QjE3QTEwRjE4NEE5MEQ1M0JFQ0EYATqsAQoNY29pbl9yZWNlaXZlZBJACghyZWNlaXZlchIycGVyc2lzdGVuY2UxeWw2aGRqaG1rZjM3NjM5NzMwZ2ZmYW5wem5kemRwbWhxbnM2ZTgYARJZCgZhbW91bnQSTTIwNTc0MTc3MWliYy9DOEE3NEFCQkUyQUY4OTJFMTU2ODBEOTE2QTdDMjIxMzA1ODVDRTU3MDRGOUIxN0ExMEYxODRBOTBENTNCRUNBGAE66AEKCHRyYW5zZmVyEkEKCXJlY2lwaWVudBIycGVyc2lzdGVuY2UxeWw2aGRqaG1rZjM3NjM5NzMwZ2ZmYW5wem5kemRwbWhxbnM2ZTgYARI+CgZzZW5kZXISMnBlcnNpc3RlbmNlMXcwdW1kbmgybXp5cDQ0Nm5lN3dkNnNlbXE5eHkwbG16NXNqcnhsGAESWQoGYW1vdW50Ek0yMDU3NDE3NzFpYmMvQzhBNzRBQkJFMkFGODkyRTE1NjgwRDkxNkE3QzIyMTMwNTg1Q0U1NzA0RjlCMTdBMTBGMTg0QTkwRDUzQkVDQRgBOkkKB21lc3NhZ2USPgoGc2VuZGVyEjJwZXJzaXN0ZW5jZTF3MHVtZG5oMm16eXA0NDZuZTd3ZDZzZW1xOXh5MGxtejVzanJ4bBgBOqgBCgpjb2luX3NwZW50Ej8KB3NwZW5kZXISMnBlcnNpc3RlbmNlMXlsNmhkamhta2YzNzYzOTczMGdmZmFucHpuZHpkcG1ocW5zNmU4GAESWQoGYW1vdW50Ek0yMDU3NDE3NzFpYmMvQzhBNzRBQkJFMkFGODkyRTE1NjgwRDkxNkE3QzIyMTMwNTg1Q0U1NzA0RjlCMTdBMTBGMTg0QTkwRDUzQkVDQRgBOqEBCgRidXJuEj4KBmJ1cm5lchIycGVyc2lzdGVuY2UxeWw2aGRqaG1rZjM3NjM5NzMwZ2ZmYW5wem5kemRwbWhxbnM2ZTgYARJZCgZhbW91bnQSTTIwNTc0MTc3MWliYy9DOEE3NEFCQkUyQUY4OTJFMTU2ODBEOTE2QTdDMjIxMzA1ODVDRTU3MDRGOUIxN0ExMEYxODRBOTBENTNCRUNBGAE6tQcKC3NlbmRfcGFja2V0EsUBCgtwYWNrZXRfZGF0YRKzAXsiYW1vdW50IjoiMjA1NzQxNzcxIiwiZGVub20iOiJ0cmFuc2Zlci9jaGFubmVsLTI0L3VhdG9tIiwicmVjZWl2ZXIiOiJjb3Ntb3MxdzB1bWRuaDJtenlwNDQ2bmU3d2Q2c2VtcTl4eTBsbXo2dTVzZ20iLCJzZW5kZXIiOiJwZXJzaXN0ZW5jZTF3MHVtZG5oMm16eXA0NDZuZTd3ZDZzZW1xOXh5MGxtejVzanJ4bCJ9GAES/AIKD3BhY2tldF9kYXRhX2hleBLmAjdiMjI2MTZkNmY3NTZlNzQyMjNhMjIzMjMwMzUzNzM0MzEzNzM3MzEyMjJjMjI2NDY1NmU2ZjZkMjIzYTIyNzQ3MjYxNmU3MzY2NjU3MjJmNjM2ODYxNmU2ZTY1NmMyZDMyMzQyZjc1NjE3NDZmNmQyMjJjMjI3MjY1NjM2NTY5NzY2NTcyMjIzYTIyNjM2ZjczNmQ2ZjczMzE3NzMwNzU2ZDY0NmU2ODMyNmQ3YTc5NzAzNDM0MzY2ZTY1Mzc3NzY0MzY3MzY1NmQ3MTM5Nzg3OTMwNmM2ZDdhMzY3NTM1NzM2NzZkMjIyYzIyNzM2NTZlNjQ2NTcyMjIzYTIyNzA2NTcyNzM2OTczNzQ2NTZlNjM2NTMxNzczMDc1NmQ2NDZlNjgzMjZkN2E3OTcwMzQzNDM2NmU2NTM3Nzc2NDM2NzM2NTZkNzEzOTc4NzkzMDZjNmQ3YTM1NzM2YTcyNzg2YzIyN2QYARIlChVwYWNrZXRfdGltZW91dF9oZWlnaHQSCjQtMTg0NzY1NzQYARIfChhwYWNrZXRfdGltZW91dF90aW1lc3RhbXASATAYARIaCg9wYWNrZXRfc2VxdWVuY2USBTE3OTAyGAESHQoPcGFja2V0X3NyY19wb3J0Egh0cmFuc2ZlchgBEiIKEnBhY2tldF9zcmNfY2hhbm5lbBIKY2hhbm5lbC0yNBgBEh0KD3BhY2tldF9kc3RfcG9ydBIIdHJhbnNmZXIYARIjChJwYWNrZXRfZHN0X2NoYW5uZWwSC2NoYW5uZWwtMTkwGAESLAoXcGFja2V0X2NoYW5uZWxfb3JkZXJpbmcSD09SREVSX1VOT1JERVJFRBgBEiQKEXBhY2tldF9jb25uZWN0aW9uEg1jb25uZWN0aW9uLTMwGAESIAoNY29ubmVjdGlvbl9pZBINY29ubmVjdGlvbi0zMBgBOiIKB21lc3NhZ2USFwoGbW9kdWxlEgtpYmNfY2hhbm5lbBgBOv0BCgxpYmNfdHJhbnNmZXISPgoGc2VuZGVyEjJwZXJzaXN0ZW5jZTF3MHVtZG5oMm16eXA0NDZuZTd3ZDZzZW1xOXh5MGxtejVzanJ4bBgBEjsKCHJlY2VpdmVyEi1jb3Ntb3MxdzB1bWRuaDJtenlwNDQ2bmU3d2Q2c2VtcTl4eTBsbXo2dTVzZ20YARIVCgZhbW91bnQSCTIwNTc0MTc3MRgBEk8KBWRlbm9tEkRpYmMvQzhBNzRBQkJFMkFGODkyRTE1NjgwRDkxNkE3QzIyMTMwNTg1Q0U1NzA0RjlCMTdBMTBGMTg0QTkwRDUzQkVDQRgBEggKBG1lbW8YATofCgdtZXNzYWdlEhQKBm1vZHVsZRIIdHJhbnNmZXIYASogq/slwSRZCApl4b2cNcrJTFgzPj8uzmb9V2VAMAiSB7c6vBQIv/H/BhACGsYCCpoBCpcBChwvY29zbW9zLmJhbmsudjFiZXRhMS5Nc2dTZW5kEncKMnBlcnNpc3RlbmNlMXE5ems1M2Z0cDloZDNldTZuNXJ6ZDc4bmpuNDdqdXBlejJ4MzUwEjJwZXJzaXN0ZW5jZTFuZXk5dzRwdXI2ODlsOWRwajNndWpxd2FzejBzNjQ1eWxnYXVmMxoNCgV1eHBydBIEOTAwMBJlCk4KRgofL2Nvc21vcy5jcnlwdG8uc2VjcDI1NmsxLlB1YktleRIjCiEDqcZ6M+zdKaItENkNKIL+XMlb9UAJVrKgL7W60WwVphoSBAoCCAESEwoNCgV1eHBydBIEMTAwMBDAmgwaQPYlYqNQNtgU8qOTXUP8PU2DEvq+iLYsqWVdo9RQ5AHOReDr4tPTJDMsyWFSl7Dg5VwRaLE99VertRNwl/ghowcixxESKBImCiQvY29zbW9zLmJhbmsudjFiZXRhMS5Nc2dTZW5kUmVzcG9uc2Ua8QZbeyJtc2dfaW5kZXgiOjAsImV2ZW50cyI6W3sidHlwZSI6Im1lc3NhZ2UiLCJhdHRyaWJ1dGVzIjpbeyJrZXkiOiJhY3Rpb24iLCJ2YWx1ZSI6Ii9jb3Ntb3MuYmFuay52MWJldGExLk1zZ1NlbmQifSx7ImtleSI6InNlbmRlciIsInZhbHVlIjoicGVyc2lzdGVuY2UxcTl6azUzZnRwOWhkM2V1Nm41cnpkNzhuam40N2p1cGV6MngzNTAifSx7ImtleSI6Im1vZHVsZSIsInZhbHVlIjoiYmFuayJ9XX0seyJ0eXBlIjoiY29pbl9zcGVudCIsImF0dHJpYnV0ZXMiOlt7ImtleSI6InNwZW5kZXIiLCJ2YWx1ZSI6InBlcnNpc3RlbmNlMXE5ems1M2Z0cDloZDNldTZuNXJ6ZDc4bmpuNDdqdXBlejJ4MzUwIn0seyJrZXkiOiJhbW91bnQiLCJ2YWx1ZSI6IjkwMDB1eHBydCJ9XX0seyJ0eXBlIjoiY29pbl9yZWNlaXZlZCIsImF0dHJpYnV0ZXMiOlt7ImtleSI6InJlY2VpdmVyIiwidmFsdWUiOiJwZXJzaXN0ZW5jZTFuZXk5dzRwdXI2ODlsOWRwajNndWpxd2FzejBzNjQ1eWxnYXVmMyJ9LHsia2V5IjoiYW1vdW50IiwidmFsdWUiOiI5MDAwdXhwcnQifV19LHsidHlwZSI6InRyYW5zZmVyIiwiYXR0cmlidXRlcyI6W3sia2V5IjoicmVjaXBpZW50IiwidmFsdWUiOiJwZXJzaXN0ZW5jZTFuZXk5dzRwdXI2ODlsOWRwajNndWpxd2FzejBzNjQ1eWxnYXVmMyJ9LHsia2V5Ijoic2VuZGVyIiwidmFsdWUiOiJwZXJzaXN0ZW5jZTFxOXprNTNmdHA5aGQzZXU2bjVyemQ3OG5qbjQ3anVwZXoyeDM1MCJ9LHsia2V5IjoiYW1vdW50IiwidmFsdWUiOiI5MDAwdXhwcnQifV19LHsidHlwZSI6Im1lc3NhZ2UiLCJhdHRyaWJ1dGVzIjpbeyJrZXkiOiJzZW5kZXIiLCJ2YWx1ZSI6InBlcnNpc3RlbmNlMXE5ems1M2Z0cDloZDNldTZuNXJ6ZDc4bmpuNDdqdXBlejJ4MzUwIn1dfV19XSjAmgww+4cEOmQKCmNvaW5fc3BlbnQSPwoHc3BlbmRlchIycGVyc2lzdGVuY2UxcTl6azUzZnRwOWhkM2V1Nm41cnpkNzhuam40N2p1cGV6MngzNTAYARIVCgZhbW91bnQSCTEwMDB1eHBydBgBOmgKDWNvaW5fcmVjZWl2ZWQSQAoIcmVjZWl2ZXISMnBlcnNpc3RlbmNlMTd4cGZ2YWttMmFtZzk2MnlsczZmODR6M2tlbGw4YzVsNzQ5bjllGAESFQoGYW1vdW50EgkxMDAwdXhwcnQYATqkAQoIdHJhbnNmZXISQQoJcmVjaXBpZW50EjJwZXJzaXN0ZW5jZTE3eHBmdmFrbTJhbWc5NjJ5bHM2Zjg0ejNrZWxsOGM1bDc0OW45ZRgBEj4KBnNlbmRlchIycGVyc2lzdGVuY2UxcTl6azUzZnRwOWhkM2V1Nm41cnpkNzhuam40N2p1cGV6MngzNTAYARIVCgZhbW91bnQSCTEwMDB1eHBydBgBOkkKB21lc3NhZ2USPgoGc2VuZGVyEjJwZXJzaXN0ZW5jZTFxOXprNTNmdHA5aGQzZXU2bjVyemQ3OG5qbjQ3anVwZXoyeDM1MBgBOlsKAnR4EhIKA2ZlZRIJMTAwMHV4cHJ0GAESQQoJZmVlX3BheWVyEjJwZXJzaXN0ZW5jZTFxOXprNTNmdHA5aGQzZXU2bjVyemQ3OG5qbjQ3anVwZXoyeDM1MBgBOkcKAnR4EkEKB2FjY19zZXESNHBlcnNpc3RlbmNlMXE5ems1M2Z0cDloZDNldTZuNXJ6ZDc4bmpuNDdqdXBlejJ4MzUwLzAYATptCgJ0eBJnCglzaWduYXR1cmUSWDlpVmlvMUEyMkJUeW81TmRRL3c5VFlNUytyNkl0aXlwWlYyajFGRGtBYzVGNE92aTA5TWtNeXpKWVZLWHNPRGxYQkZvc1QzMVY2dTFFM0NYK0NHakJ3PT0YATqFAQoHbWVzc2FnZRIoCgZhY3Rpb24SHC9jb3Ntb3MuYmFuay52MWJldGExLk1zZ1NlbmQYARI+CgZzZW5kZXISMnBlcnNpc3RlbmNlMXE5ems1M2Z0cDloZDNldTZuNXJ6ZDc4bmpuNDdqdXBlejJ4MzUwGAESEAoGbW9kdWxlEgRiYW5rGAE6ZAoKY29pbl9zcGVudBI/CgdzcGVuZGVyEjJwZXJzaXN0ZW5jZTFxOXprNTNmdHA5aGQzZXU2bjVyemQ3OG5qbjQ3anVwZXoyeDM1MBgBEhUKBmFtb3VudBIJOTAwMHV4cHJ0GAE6aAoNY29pbl9yZWNlaXZlZBJACghyZWNlaXZlchIycGVyc2lzdGVuY2UxbmV5OXc0cHVyNjg5bDlkcGozZ3VqcXdhc3owczY0NXlsZ2F1ZjMYARIVCgZhbW91bnQSCTkwMDB1eHBydBgBOqQBCgh0cmFuc2ZlchJBCglyZWNpcGllbnQSMnBlcnNpc3RlbmNlMW5leTl3NHB1cjY4OWw5ZHBqM2d1anF3YXN6MHM2NDV5bGdhdWYzGAESPgoGc2VuZGVyEjJwZXJzaXN0ZW5jZTFxOXprNTNmdHA5aGQzZXU2bjVyemQ3OG5qbjQ3anVwZXoyeDM1MBgBEhUKBmFtb3VudBIJOTAwMHV4cHJ0GAE6SQoHbWVzc2FnZRI+CgZzZW5kZXISMnBlcnNpc3RlbmNlMXE5ems1M2Z0cDloZDNldTZuNXJ6ZDc4bmpuNDdqdXBlejJ4MzUwGAEqIEutgJIweBrWTktr4ZiIDsuiqmDTJTpE0Dm+7wblVEHdOrwUCL/x/wYQAxrGAgqaAQqXAQocL2Nvc21vcy5iYW5rLnYxYmV0YTEuTXNnU2VuZBJ3CjJwZXJzaXN0ZW5jZTE5NmVtaHEwaHdxa2sycDd6cnMyd3c4c21oemtqZjZhMHIyMDNhNhIycGVyc2lzdGVuY2UxbmV5OXc0cHVyNjg5bDlkcGozZ3VqcXdhc3owczY0NXlsZ2F1ZjMaDQoFdXhwcnQSBDkwMDASZQpOCkYKHy9jb3Ntb3MuY3J5cHRvLnNlY3AyNTZrMS5QdWJLZXkSIwohArd2d6hGr2gHobX1Ays1prUNpZJAsxQvM+kfC8GzFfdqEgQKAggBEhMKDQoFdXhwcnQSBDEwMDAQwJoMGkBDrYyO98CBUF6Yvv7DF9KzQwKwT0Vx1I+2fMVCtLYUPGkUk28XuQliUPi/PRcL+sdlvq6oPxkramuTV3jdIAmFIscREigSJgokL2Nvc21vcy5iYW5rLnYxYmV0YTEuTXNnU2VuZFJlc3BvbnNlGvEGW3sibXNnX2luZGV4IjowLCJldmVudHMiOlt7InR5cGUiOiJtZXNzYWdlIiwiYXR0cmlidXRlcyI6W3sia2V5IjoiYWN0aW9uIiwidmFsdWUiOiIvY29zbW9zLmJhbmsudjFiZXRhMS5Nc2dTZW5kIn0seyJrZXkiOiJzZW5kZXIiLCJ2YWx1ZSI6InBlcnNpc3RlbmNlMTk2ZW1ocTBod3FrazJwN3pyczJ3dzhzbWh6a2pmNmEwcjIwM2E2In0seyJrZXkiOiJtb2R1bGUiLCJ2YWx1ZSI6ImJhbmsifV19LHsidHlwZSI6ImNvaW5fc3BlbnQiLCJhdHRyaWJ1dGVzIjpbeyJrZXkiOiJzcGVuZGVyIiwidmFsdWUiOiJwZXJzaXN0ZW5jZTE5NmVtaHEwaHdxa2sycDd6cnMyd3c4c21oemtqZjZhMHIyMDNhNiJ9LHsia2V5IjoiYW1vdW50IiwidmFsdWUiOiI5MDAwdXhwcnQifV19LHsidHlwZSI6ImNvaW5fcmVjZWl2ZWQiLCJhdHRyaWJ1dGVzIjpbeyJrZXkiOiJyZWNlaXZlciIsInZhbHVlIjoicGVyc2lzdGVuY2UxbmV5OXc0cHVyNjg5bDlkcGozZ3VqcXdhc3owczY0NXlsZ2F1ZjMifSx7ImtleSI6ImFtb3VudCIsInZhbHVlIjoiOTAwMHV4cHJ0In1dfSx7InR5cGUiOiJ0cmFuc2ZlciIsImF0dHJpYnV0ZXMiOlt7ImtleSI6InJlY2lwaWVudCIsInZhbHVlIjoicGVyc2lzdGVuY2UxbmV5OXc0cHVyNjg5bDlkcGozZ3VqcXdhc3owczY0NXlsZ2F1ZjMifSx7ImtleSI6InNlbmRlciIsInZhbHVlIjoicGVyc2lzdGVuY2UxOTZlbWhxMGh3cWtrMnA3enJzMnd3OHNtaHpramY2YTByMjAzYTYifSx7ImtleSI6ImFtb3VudCIsInZhbHVlIjoiOTAwMHV4cHJ0In1dfSx7InR5cGUiOiJtZXNzYWdlIiwiYXR0cmlidXRlcyI6W3sia2V5Ijoic2VuZGVyIiwidmFsdWUiOiJwZXJzaXN0ZW5jZTE5NmVtaHEwaHdxa2sycDd6cnMyd3c4c21oemtqZjZhMHIyMDNhNiJ9XX1dfV0owJoMMPuHBDpkCgpjb2luX3NwZW50Ej8KB3NwZW5kZXISMnBlcnNpc3RlbmNlMTk2ZW1ocTBod3FrazJwN3pyczJ3dzhzbWh6a2pmNmEwcjIwM2E2GAESFQoGYW1vdW50EgkxMDAwdXhwcnQYATpoCg1jb2luX3JlY2VpdmVkEkAKCHJlY2VpdmVyEjJwZXJzaXN0ZW5jZTE3eHBmdmFrbTJhbWc5NjJ5bHM2Zjg0ejNrZWxsOGM1bDc0OW45ZRgBEhUKBmFtb3VudBIJMTAwMHV4cHJ0GAE6pAEKCHRyYW5zZmVyEkEKCXJlY2lwaWVudBIycGVyc2lzdGVuY2UxN3hwZnZha20yYW1nOTYyeWxzNmY4NHoza2VsbDhjNWw3NDluOWUYARI+CgZzZW5kZXISMnBlcnNpc3RlbmNlMTk2ZW1ocTBod3FrazJwN3pyczJ3dzhzbWh6a2pmNmEwcjIwM2E2GAESFQoGYW1vdW50EgkxMDAwdXhwcnQYATpJCgdtZXNzYWdlEj4KBnNlbmRlchIycGVyc2lzdGVuY2UxOTZlbWhxMGh3cWtrMnA3enJzMnd3OHNtaHpramY2YTByMjAzYTYYATpbCgJ0eBISCgNmZWUSCTEwMDB1eHBydBgBEkEKCWZlZV9wYXllchIycGVyc2lzdGVuY2UxOTZlbWhxMGh3cWtrMnA3enJzMnd3OHNtaHpramY2YTByMjAzYTYYATpHCgJ0eBJBCgdhY2Nfc2VxEjRwZXJzaXN0ZW5jZTE5NmVtaHEwaHdxa2sycDd6cnMyd3c4c21oemtqZjZhMHIyMDNhNi8wGAE6bQoCdHgSZwoJc2lnbmF0dXJlElhRNjJNanZmQWdWQmVtTDcrd3hmU3MwTUNzRTlGY2RTUHRuekZRclMyRkR4cEZKTnZGN2tKWWxENHZ6MFhDL3JIWmI2dXFEOFpLMnByazFkNDNTQUpoUT09GAE6hQEKB21lc3NhZ2USKAoGYWN0aW9uEhwvY29zbW9zLmJhbmsudjFiZXRhMS5Nc2dTZW5kGAESPgoGc2VuZGVyEjJwZXJzaXN0ZW5jZTE5NmVtaHEwaHdxa2sycDd6cnMyd3c4c21oemtqZjZhMHIyMDNhNhgBEhAKBm1vZHVsZRIEYmFuaxgBOmQKCmNvaW5fc3BlbnQSPwoHc3BlbmRlchIycGVyc2lzdGVuY2UxOTZlbWhxMGh3cWtrMnA3enJzMnd3OHNtaHpramY2YTByMjAzYTYYARIVCgZhbW91bnQSCTkwMDB1eHBydBgBOmgKDWNvaW5fcmVjZWl2ZWQSQAoIcmVjZWl2ZXISMnBlcnNpc3RlbmNlMW5leTl3NHB1cjY4OWw5ZHBqM2d1anF3YXN6MHM2NDV5bGdhdWYzGAESFQoGYW1vdW50Egk5MDAwdXhwcnQYATqkAQoIdHJhbnNmZXISQQoJcmVjaXBpZW50EjJwZXJzaXN0ZW5jZTFuZXk5dzRwdXI2ODlsOWRwajNndWpxd2FzejBzNjQ1eWxnYXVmMxgBEj4KBnNlbmRlchIycGVyc2lzdGVuY2UxOTZlbWhxMGh3cWtrMnA3enJzMnd3OHNtaHpramY2YTByMjAzYTYYARIVCgZhbW91bnQSCTkwMDB1eHBydBgBOkkKB21lc3NhZ2USPgoGc2VuZGVyEjJwZXJzaXN0ZW5jZTE5NmVtaHEwaHdxa2sycDd6cnMyd3c4c21oemtqZjZhMHIyMDNhNhgBKiA63YLO6oztNZE3/CedFn/mWxl3WOwgfKGDUlxksbPaXg==";

        // decode bytes from base64
        let block_data: &[u8] = &base64::decode(base64_data).unwrap();
        let block = codec::Block::decode(block_data).unwrap();

        let mut event_types = HashSet::new();
        event_types.insert(String::from("wasm-dexter-multi-staking::create_reward_schedule"));
        
        let filter = &TriggerFilter { 
            event_type_filter: CosmosEventTypeFilter {
                event_types
            },
            block_filter: CosmosBlockFilter {
                trigger_every_block: false,
            }, 
        };

        let shared_block = Arc::new(block.clone());

        let header_only_block = codec::HeaderOnlyBlock::from(&block);

        let mut triggers: Vec<_> = shared_block
            .begin_block_events()
            .unwrap()
            .cloned()
            // FIXME (Cosmos): Optimize. Should use an Arc instead of cloning the
            // block. This is not currently possible because EventData is automatically
            // generated.
            .filter_map(|event| {
                filter_event_trigger(
                    filter,
                    event,
                    &header_only_block,
                    None,
                    EventOrigin::BeginBlock,
                )
            })
            .chain(shared_block.transactions().flat_map(|tx| {
                tx.result
                    .as_ref()
                    .unwrap()
                    .events
                    .iter()
                    .filter_map(|e| {
                        filter_event_trigger(
                            filter,
                            e.clone(),
                            &header_only_block,
                            Some(build_tx_context(tx)),
                            EventOrigin::DeliverTx,
                        )
                    })
                    .collect::<Vec<_>>()
            }))
            .chain(
                shared_block
                    .end_block_events()
                    .unwrap()
                    .cloned()
                    .filter_map(|event| {
                        filter_event_trigger(
                            filter,
                            event,
                            &header_only_block,
                            None,
                            EventOrigin::EndBlock,
                        )
                    }),
            )
            .collect();

        triggers.extend(shared_block.transactions().cloned().flat_map(|tx_result| {
            let mut triggers: Vec<_> = Vec::new();
            if let Some(tx) = tx_result.tx.clone() {
                if let Some(tx_body) = tx.body {
                    triggers.extend(tx_body.messages.into_iter().map(|message| {
                        CosmosTrigger::with_message(
                            message,
                            header_only_block.clone(),
                            build_tx_context(&tx_result),
                        )
                    }));
                }
            }
            triggers.push(CosmosTrigger::with_transaction(
                tx_result,
                header_only_block.clone(),
            ));
            triggers
        }));

        if filter.block_filter.trigger_every_block {
            triggers.push(CosmosTrigger::Block(shared_block.cheap_clone()));
        }

        
        println!("triggers {:?} \n\n", triggers);

        // Log the reward schedule ID parameter of each `wasm-dexter-multi-staking::create_reward_schedule` parameter to demonstrate that the values are different.
        for trigger in triggers.iter() {
            match trigger {
                CosmosTrigger::Event { event_data, origin } => {
                    let event = event_data.event.clone().unwrap();
                    let event_type = event.event_type.clone();
                    if event_type == "wasm-dexter-multi-staking::create_reward_schedule" {
                        let reward_schedule_id = event
                            .attributes
                            .iter()
                            .find(|attribute| attribute.key == "reward_schedule_id")
                            .unwrap()
                            .value
                            .clone();

                        println!("reward_schedule_id {:?}", reward_schedule_id);
                    }
                },
                _ => {}
            }
        }

        // deduping
        triggers.sort();
        triggers.dedup();

        println!("\n\ntriggers deduped {:?}\n\n", triggers);

        // Run the same block again for different results.
        for trigger in triggers.iter() {
            match trigger {
                CosmosTrigger::Event { event_data, origin } => {
                    let event = event_data.event.clone().unwrap();
                    let event_type = event.event_type.clone();
                    if event_type == "wasm-dexter-multi-staking::create_reward_schedule" {
                        let reward_schedule_id = event
                            .attributes
                            .iter()
                            .find(|attribute| attribute.key == "reward_schedule_id")
                            .unwrap()
                            .value
                            .clone();
                        
                        println!("reward_schedule_id {:?}", reward_schedule_id);
                    }
                },
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn test_trigger_filters() {
        let adapter = TriggersAdapter {};
        let logger = Logger::root(Discard, o!());

        let block_with_events = Block::test_with_event_types(
            vec!["begin_event_1", "begin_event_2", "begin_event_3"],
            vec!["tx_event_1", "tx_event_2", "tx_event_3"],
            vec!["end_event_1", "end_event_2", "end_event_3"],
        );

        let header_only_block = HeaderOnlyBlock::from(&block_with_events);

        let cases = [
            (
                Block::test_new(),
                TriggerFilter::test_new(false, &[]),
                vec![],
            ),
            (
                Block::test_new(),
                TriggerFilter::test_new(true, &[]),
                vec![CosmosTrigger::Block(Arc::new(Block::test_new()))],
            ),
            (
                Block::test_new(),
                TriggerFilter::test_new(false, &["event_1", "event_2", "event_3"]),
                vec![],
            ),
            (
                block_with_events.clone(),
                TriggerFilter::test_new(false, &["begin_event_3", "tx_event_3", "end_event_3"]),
                vec![
                    CosmosTrigger::with_event(
                        Event::test_with_type("begin_event_3"),
                        header_only_block.clone(),
                        None,
                        EventOrigin::BeginBlock,
                    ),
                    CosmosTrigger::with_event(
                        Event::test_with_type("tx_event_3"),
                        header_only_block.clone(),
                        Some(build_tx_context(&block_with_events.transactions[2])),
                        EventOrigin::DeliverTx,
                    ),
                    CosmosTrigger::with_event(
                        Event::test_with_type("end_event_3"),
                        header_only_block.clone(),
                        None,
                        EventOrigin::EndBlock,
                    ),
                    CosmosTrigger::with_transaction(
                        TxResult::test_with_event_type("tx_event_1"),
                        header_only_block.clone(),
                    ),
                    CosmosTrigger::with_transaction(
                        TxResult::test_with_event_type("tx_event_2"),
                        header_only_block.clone(),
                    ),
                    CosmosTrigger::with_transaction(
                        TxResult::test_with_event_type("tx_event_3"),
                        header_only_block.clone(),
                    ),
                ],
            ),
            (
                block_with_events.clone(),
                TriggerFilter::test_new(true, &["begin_event_3", "tx_event_2", "end_event_1"]),
                vec![
                    CosmosTrigger::Block(Arc::new(block_with_events.clone())),
                    CosmosTrigger::with_event(
                        Event::test_with_type("begin_event_3"),
                        header_only_block.clone(),
                        None,
                        EventOrigin::BeginBlock,
                    ),
                    CosmosTrigger::with_event(
                        Event::test_with_type("tx_event_2"),
                        header_only_block.clone(),
                        Some(build_tx_context(&block_with_events.transactions[1])),
                        EventOrigin::DeliverTx,
                    ),
                    CosmosTrigger::with_event(
                        Event::test_with_type("end_event_1"),
                        header_only_block.clone(),
                        None,
                        EventOrigin::EndBlock,
                    ),
                    CosmosTrigger::with_transaction(
                        TxResult::test_with_event_type("tx_event_1"),
                        header_only_block.clone(),
                    ),
                    CosmosTrigger::with_transaction(
                        TxResult::test_with_event_type("tx_event_2"),
                        header_only_block.clone(),
                    ),
                    CosmosTrigger::with_transaction(
                        TxResult::test_with_event_type("tx_event_3"),
                        header_only_block.clone(),
                    ),
                ],
            ),
        ];

        for (block, trigger_filter, expected_triggers) in cases {
            let triggers = adapter
                .triggers_in_block(&logger, block, &trigger_filter)
                .await
                .expect("failed to get triggers in block");

            assert_eq!(
                triggers.trigger_data.len(),
                expected_triggers.len(),
                "Expected trigger list to contain exactly {:?}, but it didn't: {:?}",
                expected_triggers,
                triggers.trigger_data
            );

            // they may not be in the same order
            for trigger in expected_triggers {
                assert!(
                    triggers.trigger_data.contains(&trigger),
                    "Expected trigger list to contain {:?}, but it only contains: {:?}",
                    trigger,
                    triggers.trigger_data
                );
            }
        }
    }

    impl Block {
        fn test_new() -> Block {
            Block::test_with_event_types(vec![], vec![], vec![])
        }

        fn test_with_event_types(
            begin_event_types: Vec<&str>,
            tx_event_types: Vec<&str>,
            end_event_types: Vec<&str>,
        ) -> Block {
            Block {
                header: Some(Header {
                    version: None,
                    chain_id: "test".to_string(),
                    height: 1,
                    time: None,
                    last_block_id: None,
                    last_commit_hash: vec![],
                    data_hash: vec![],
                    validators_hash: vec![],
                    next_validators_hash: vec![],
                    consensus_hash: vec![],
                    app_hash: vec![],
                    last_results_hash: vec![],
                    evidence_hash: vec![],
                    proposer_address: vec![],
                    hash: vec![],
                }),
                evidence: None,
                last_commit: None,
                result_begin_block: Some(ResponseBeginBlock {
                    events: begin_event_types
                        .into_iter()
                        .map(Event::test_with_type)
                        .collect(),
                }),
                result_end_block: Some(ResponseEndBlock {
                    validator_updates: vec![],
                    consensus_param_updates: None,
                    events: end_event_types
                        .into_iter()
                        .map(Event::test_with_type)
                        .collect(),
                }),
                transactions: tx_event_types
                    .into_iter()
                    .map(TxResult::test_with_event_type)
                    .collect(),
                validator_updates: vec![],
            }
        }
    }

    impl Event {
        fn test_with_type(event_type: &str) -> Event {
            Event {
                event_type: event_type.to_string(),
                attributes: vec![],
            }
        }
    }

    impl TxResult {
        fn test_with_event_type(event_type: &str) -> TxResult {
            TxResult {
                height: 1,
                index: 1,
                tx: None,
                result: Some(ResponseDeliverTx {
                    code: 1,
                    data: vec![],
                    log: "".to_string(),
                    info: "".to_string(),
                    gas_wanted: 1,
                    gas_used: 1,
                    codespace: "".to_string(),
                    events: vec![Event::test_with_type(event_type)],
                }),
                hash: vec![],
            }
        }
    }
}
