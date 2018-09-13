use failure::*;
use futures::sync::mpsc::{channel, Receiver, Sender};
use futures::sync::oneshot;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use web3::types::Block;
use web3::types::Transaction;

use graph::components::ethereum::*;
use graph::components::store::HeadBlockUpdateEvent;
use graph::components::subgraph::RuntimeHostEvent;
use graph::components::subgraph::SubgraphProviderEvent;
use graph::prelude::*;

// TODO choose a good number
const REORG_THRESHOLD: u64 = 300;

pub struct RuntimeManager {
    logger: Logger,
    input: Sender<SubgraphProviderEvent>,
}

impl RuntimeManager where {
    /// Creates a new runtime manager.
    pub fn new<S, E, T>(
        logger: &Logger,
        store: Arc<Mutex<S>>,
        eth_adapter: Arc<Mutex<E>>,
        host_builder: T,
    ) -> Self
    where
        S: Store + 'static,
        E: EthereumAdapter,
        T: RuntimeHostBuilder,
    {
        let logger = logger.new(o!("component" => "RuntimeManager"));

        // Create channel for receiving subgraph provider events.
        let (subgraph_sender, subgraph_receiver) = channel(100);

        // Handle incoming events from the subgraph provider.
        Self::handle_subgraph_events(
            logger.clone(),
            store,
            eth_adapter,
            host_builder,
            subgraph_receiver,
        );

        RuntimeManager {
            logger,
            input: subgraph_sender,
        }
    }

    /// Handle incoming events from subgraph providers.
    fn handle_subgraph_events<S, E, T>(
        logger: Logger,
        store: Arc<Mutex<S>>,
        eth_adapter: Arc<Mutex<E>>,
        mut host_builder: T,
        subgraph_events: Receiver<SubgraphProviderEvent>,
    ) where
        S: Store + 'static,
        E: EthereumAdapter,
        T: RuntimeHostBuilder,
    {
        // Create a mapping of subgraph IDs to head block cancel senders
        let head_block_update_cancelers: Arc<Mutex<HashMap<String, oneshot::Sender<_>>>> =
            Default::default();

        // Create a mapping of subgraph IDs to runtime hosts
        let runtime_hosts_by_subgraph: Arc<Mutex<HashMap<String, Vec<T::Host>>>> =
            Default::default();

        // Handle events coming in from the subgraph provider
        let subgraph_events_logger = logger.clone();
        tokio::spawn(subgraph_events.for_each(move |event| {
            match event {
                SubgraphProviderEvent::SubgraphAdded(manifest) => {
                    info!(subgraph_events_logger, "Host mapping runtimes for subgraph";
                      "location" => &manifest.location);

                    // Add entry to store for subgraph
                    store
                        .lock()
                        .unwrap()
                        .add_subgraph_if_missing(SubgraphId(manifest.id.clone()))
                        .unwrap();

                    // Create a new runtime host for each data source in the subgraph manifest
                    let mut new_hosts = manifest
                        .data_sources
                        .iter()
                        .map(|d| host_builder.build(manifest.clone(), d.clone()))
                        .collect::<Vec<_>>();

                    // Forward events from the runtime host to the store; this
                    // Tokio task will terminate when the corresponding subgraph
                    // is removed and the host and its event sender are dropped
                    Self::spawn_runtime_event_stream_handler_tasks(store.clone(), &mut new_hosts);

                    // Add the new hosts to the list of managed runtime hosts
                    runtime_hosts_by_subgraph
                        .lock()
                        .unwrap()
                        .insert(manifest.id.clone(), new_hosts);

                    // Spawn a new stream task to process head block updates for
                    // the subgraph; use a oneshot channel to be able to cancel
                    // this process whenever the subgraph is removed
                    let cancel = Self::spawn_head_block_update_task(
                        logger.clone(),
                        store.clone(),
                        eth_adapter.clone(),
                        runtime_hosts_by_subgraph.clone(),
                        manifest.id.clone(),
                    );
                    head_block_update_cancelers
                        .lock()
                        .unwrap()
                        .insert(manifest.id, cancel);
                }
                SubgraphProviderEvent::SubgraphRemoved(subgraph_id) => {
                    info!(subgraph_events_logger, "Remove mapping runtimes for subgraph";
                          "id" => &subgraph_id);

                    // Destroy all runtime hosts for this subgraph; this will
                    // also terminate the host's event stream
                    runtime_hosts_by_subgraph
                        .lock()
                        .unwrap()
                        .remove(&subgraph_id);

                    // Destroy the subgraph's head block sender; this will
                    // terminate its head block update task
                    let cancel = head_block_update_cancelers
                        .lock()
                        .unwrap()
                        .remove(&subgraph_id);
                    assert!(cancel.is_some());
                    drop(cancel);
                }
            }
            Ok(())
        }));
    }

    // Handles each incoming event from the subgraph.
    fn handle_runtime_event<S>(store: Arc<Mutex<S>>, event: RuntimeHostEvent)
    where
        S: Store + 'static,
    {
        match event {
            RuntimeHostEvent::EntitySet(store_key, entity, block) => {
                let store = store.lock().unwrap();
                // TODO this code is incorrect. One TX should be used for entire block.
                let mut tx = store
                    .begin_transaction(SubgraphId(store_key.subgraph.clone()), block)
                    .unwrap();
                tx.set(store_key, entity)
                    .expect("Failed to set entity in the store");
                tx.commit_no_ptr_update().unwrap();
            }
            RuntimeHostEvent::EntityRemoved(store_key, block) => {
                let store = store.lock().unwrap();
                // TODO this code is incorrect. One TX should be used for entire block.
                let mut tx = store
                    .begin_transaction(SubgraphId(store_key.subgraph.clone()), block)
                    .unwrap();
                tx.delete(store_key)
                    .expect("Failed to delete entity from the store");
                tx.commit_no_ptr_update().unwrap();
            }
        }
    }

    fn spawn_runtime_event_stream_handler_tasks<H, S>(store: Arc<Mutex<S>>, hosts: &mut Vec<H>)
    where
        S: Store + 'static,
        H: RuntimeHost,
    {
        for mut host in hosts.iter_mut() {
            let store = store.clone();
            tokio::spawn(host.take_event_stream().unwrap().for_each(move |event| {
                Self::handle_runtime_event(store.clone(), event);
                Ok(())
            }));
        }
    }

    fn spawn_head_block_update_task<E, H, S>(
        logger: Logger,
        store: Arc<Mutex<S>>,
        eth_adapter: Arc<Mutex<E>>,
        runtime_hosts_by_subgraph: Arc<Mutex<HashMap<String, Vec<H>>>>,
        subgraph_id: String,
    ) -> oneshot::Sender<()>
    where
        S: Store + 'static,
        E: EthereumAdapter,
        H: RuntimeHost + 'static,
    {
        warn!(logger, "Spawn head block update task for subgraph"; "id"  => &subgraph_id);

        // Obtain a stream of head block updates
        let err_logger = logger.clone();
        let head_block_updates = store
            .lock()
            .unwrap()
            .head_block_updates()
            .map_err(move |e| {
                error!(err_logger, "head block update stream error: {}", e);
            });

        let (cancel, canceled) = oneshot::channel();

        let cancel_head_block_update = Arc::new(AtomicBool::new(false));
        let cancel_head_block_update_trigger = cancel_head_block_update.clone();

        let cancel_logger = logger.clone();
        tokio::spawn(canceled.map_err(move |_| {
            warn!(cancel_logger, "Cancel head block update processing");
            cancel_head_block_update_trigger.store(true, Ordering::SeqCst);
        }));

        let cancel_check_logger = logger.clone();
        tokio::spawn(head_block_updates.for_each(move |_update| {
            info!(logger, "Runtime manager received head block update");

            if cancel_head_block_update.clone().load(Ordering::SeqCst) {
                warn!(
                    cancel_check_logger,
                    "Cancelled; terminate head block updates stream"
                );
                return Err(());
            }

            let (event_filter, event_sinks) = {
                let runtime_hosts_by_subgraph = runtime_hosts_by_subgraph.lock().unwrap();

                let tmp = vec![];
                let runtime_hosts = runtime_hosts_by_subgraph.get(&subgraph_id).unwrap_or(&tmp);

                if runtime_hosts.is_empty() {
                    return Ok(());
                }

                // Create a combined event filter for the data source events in the subgraph
                let mut event_filter = EthereumEventFilter::empty();
                let mut event_filter_failed = false;
                for runtime_host in runtime_hosts.iter() {
                    match runtime_host.event_filter() {
                        Ok(filter) => event_filter += filter,
                        Err(e) => {
                            error!(logger, "Failed to obtain event filter";
                                   "error" => format!("{}", e));
                            event_filter_failed = true;
                        }
                    }
                }

                // Collect Ethereum event sinks from all runtime hosts, so we can send them
                // relevant Ethereum events for processing
                let mut event_sinks = runtime_hosts
                    .iter()
                    .map(|host| host.event_sink())
                    .collect::<Vec<_>>();

                (event_filter, event_sinks)
            };

            let err_logger = logger.clone();
            let err_subgraph_id = subgraph_id.clone();

            handle_head_block_update(
                logger.clone(),
                store.clone(),
                eth_adapter.clone(),
                subgraph_id.clone(),
                event_filter,
                event_sinks,
                cancel_head_block_update.clone(),
            ).err()
                .map(move |e| {
                    warn!(err_logger, "Problem while handling head block update: {}",
                          e; "subgraph_id" => err_subgraph_id);
                });

            warn!(logger, "Terminated head block update loop");
            Ok(())
        }));

        cancel
    }
}

impl EventConsumer<SubgraphProviderEvent> for RuntimeManager {
    /// Get the wrapped event sink.
    fn event_sink(&self) -> Box<Sink<SinkItem = SubgraphProviderEvent, SinkError = ()> + Send> {
        let logger = self.logger.clone();
        Box::new(self.input.clone().sink_map_err(move |e| {
            error!(logger, "Component was dropped: {}", e);
        }))
    }
}

fn handle_head_block_update<S, E>(
    logger: Logger,
    store: Arc<Mutex<S>>,
    eth_adapter: Arc<Mutex<E>>,
    subgraph_id: String,
    event_filter: EthereumEventFilter,
    mut event_sinks: Vec<
        Box<
            Sink<SinkItem = (EthereumEvent, oneshot::Sender<Result<(), Error>>), SinkError = ()>
                + Send,
        >,
    >,
    cancelled: Arc<AtomicBool>,
) -> Result<(), Error>
where
    S: Store + 'static,
    E: EthereumAdapter,
{
    // TODO handle VersionConflicts
    // TODO remove .wait()s, maybe?

    info!(
        logger,
        "Handling head block update for subgraph {}", subgraph_id
    );

    while !cancelled.load(Ordering::SeqCst) {
        // Get pointers from database for comparison
        let head_ptr = store
            .lock()
            .unwrap()
            .head_block_ptr()?
            .expect("should not receive head block update before head block pointer is set");
        let subgraph_ptr = store
            .lock()
            .unwrap()
            .block_ptr(SubgraphId(subgraph_id.to_owned()))?;

        debug!(logger, "head_ptr = {:?}", head_ptr);
        debug!(logger, "subgraph_ptr = {:?}", subgraph_ptr);

        // Only continue if the subgraph block ptr is behind the head block ptr.
        // subgraph_ptr > head_ptr shouldn't happen, but if it does, it's safest to just stop.
        if subgraph_ptr.number >= head_ptr.number {
            break;
        }

        // Subgraph ptr is behind head ptr.
        // Each loop iteration, we'll move the subgraph ptr one step in the right direction.
        // First question: which direction should the ptr be moved?
        enum Step {
            ToParent,                               // backwards one block
            ToDescendants(Vec<Block<Transaction>>), // forwards, processing one or more blocks
        }
        let step = {
            if cancelled.load(Ordering::SeqCst) {
                return Ok(());
            }

            // We will use a different approach to deciding the step direction depending on how far
            // the subgraph ptr is behind the head ptr.
            //
            // Normally, we need to worry about chain reorganizations -- situations where the
            // Ethereum client discovers a new longer chain of blocks different from the one we had
            // processed so far, forcing us to rollback one or more blocks we had already
            // processed.
            // We can't assume that blocks we receive are permanent.
            //
            // However, as a block receives more and more confirmations, eventually it becomes safe
            // to assume that that block will be permanent.
            // The probability of a block being "uncled" approaches zero as more and more blocks
            // are chained on after that block.
            // Eventually, the probability is so low, that a block is effectively permanent.
            // The "effectively permanent" part is what makes blockchains useful.
            //
            // Accordingly, if the subgraph ptr is really far behind the head ptr, then we can
            // trust that the Ethereum node knows what the real, permanent block is for that block
            // number.
            // We'll define "really far" to mean "greater than REORG_THRESHOLD blocks".
            //
            // If the subgraph ptr is not too far behind the head ptr (i.e. less than
            // REORG_THRESHOLD blocks behind), then we have to allow for the possibility that the
            // block might be on the main chain now, but might become uncled in the future.
            //
            // Most importantly: Our ability to make this assumption (or not) will determine what
            // Ethereum RPC calls can give us accurate data without race conditions.
            // (This is mostly due to some unfortunate API design decisions on the Ethereum side)
            if (head_ptr.number - subgraph_ptr.number) > REORG_THRESHOLD {
                // Since we are beyond the reorg threshold, the Ethereum node knows what block has
                // been permanently assigned this block number.
                // This allows us to ask the node: does subgraph_ptr point to a block that was
                // permanently accepted into the main chain, or does it point to a block that was
                // uncled?
                let is_on_main_chain = eth_adapter
                    .lock()
                    .unwrap()
                    .is_on_main_chain(subgraph_ptr)
                    .wait()?;
                if is_on_main_chain {
                    // The subgraph ptr points to a block on the main chain.
                    // This means that the last block we processed does not need to be reverted.
                    // Therefore, our direction of travel will be forward, towards the chain head.

                    // As an optimization, instead of advancing one block, we will use an Ethereum
                    // RPC call to find the first few blocks between the subgraph ptr and the reorg
                    // threshold that has event(s) we are interested in.
                    // Note that we use block numbers here.
                    // This is an artifact of Ethereum RPC limitations.
                    // It is only safe to use block numbers because we are beyond the reorg
                    // threshold.

                    // Start with first block after subgraph ptr
                    let from = subgraph_ptr.number + 1;

                    // End just prior to reorg threshold.
                    // It isn't safe to go any farther due to race conditions.
                    let to = head_ptr.number - REORG_THRESHOLD;

                    debug!(logger, "Finding next blocks with relevant events...");
                    let descendant_ptrs = eth_adapter
                        .lock()
                        .unwrap()
                        .find_first_blocks_with_events(from, to, event_filter.clone())
                        .wait()?;
                    debug!(logger, "Done finding next blocks.");

                    if descendant_ptrs.is_empty() {
                        // No matching events in range.
                        // Therefore, we can update the subgraph ptr without any changes to the
                        // entity data.

                        // We need to look up what block hash corresponds to the block number
                        // `to`.
                        // Again, this is only safe from race conditions due to being beyond
                        // the reorg threshold.
                        let new_ptr = eth_adapter
                            .lock()
                            .unwrap()
                            .block_by_number(to)
                            .wait()?
                            .into();

                        store.lock().unwrap().set_block_ptr_with_no_changes(
                            SubgraphId(subgraph_id.to_owned()),
                            subgraph_ptr,
                            new_ptr,
                        )?;

                        // There were no events to process in this case, so we have already
                        // completed the subgraph ptr step.
                        // Continue outer loop.
                        continue;
                    } else {
                        // The next few interesting blocks are at descendant_ptrs.
                        // In particular, descendant_ptrs is a list of all blocks between
                        // subgraph_ptr and descendant_ptrs.last() that contain relevant events.
                        // This will allow us to advance the subgraph_ptr to descendant_ptrs.last()
                        // while being confident that we did not miss any relevant events.

                        // Load the blocks
                        debug!(
                            logger,
                            "Found {} block(s) with events. Loading blocks...",
                            descendant_ptrs.len()
                        );
                        let descendant_blocks = stream::futures_ordered(
                            descendant_ptrs.into_iter().map(|descendant_ptr| {
                                let eth_adapter = eth_adapter.clone();
                                let store = store.clone();

                                // Try locally first. Otherwise, get block from Ethereum node.
                                let block_result = store.lock().unwrap().block(descendant_ptr.hash);
                                future::result(block_result).and_then(
                                    move |block_from_store| -> Box<Future<Item = _, Error = _>> {
                                        if let Some(block) = block_from_store {
                                            Box::new(future::ok(block))
                                        } else {
                                            eth_adapter
                                                .lock()
                                                .unwrap()
                                                .block_by_hash(descendant_ptr.hash)
                                        }
                                    },
                                )
                            }),
                        ).collect()
                            .wait()?;

                        // Proceed to those blocks
                        Step::ToDescendants(descendant_blocks)
                    }
                } else {
                    // The subgraph ptr points to a block that was uncled.
                    // We need to revert this block.
                    Step::ToParent
                }
            } else {
                // The subgraph ptr is not too far behind the head ptr.
                // This means a few things.
                //
                // First, because we are still within the reorg threshold,
                // we can't trust the Ethereum RPC methods that use block numbers.
                // Block numbers in this region are not yet immutable pointers to blocks;
                // the block associated with a particular block number on the Ethereum node could
                // change under our feet at any time.
                //
                // Second, due to how the BlockIngestor is designed, we get a helpful guarantee:
                // the head block and at least its REORG_THRESHOLD most recent ancestors will be
                // present in the block store.
                // This allows us to work locally in the block store instead of relying on
                // Ethereum RPC calls, so that we are not subject to the limitations of the RPC
                // API.

                // To determine the step direction, we need to find out if the subgraph ptr refers
                // to a block that is an ancestor of the head block.
                // We can do so by walking back up the chain from the head block to the appropriate
                // block number, and checking to see if the block we found matches the
                // subgraph_ptr.

                // Precondition: subgraph_ptr.number < head_ptr.number
                // Walk back to one block short of subgraph_ptr.number
                let offset = head_ptr.number - subgraph_ptr.number - 1;
                let ancestor_block_opt = store.lock().unwrap().ancestor_block(head_ptr, offset)?;
                match ancestor_block_opt {
                    None => {
                        // Block is missing in the block store.
                        // This generally won't happen often, but can happen if the head ptr has
                        // been updated since we retrieved the head ptr, and the block store has
                        // been garbage collected.
                        // It's easiest to start over at this point.
                        continue;
                    }
                    Some(ancestor_block) => {
                        // We stopped one block short, so we'll compare the parent hash to the
                        // subgraph ptr.
                        if ancestor_block.parent_hash == subgraph_ptr.hash {
                            // The subgraph ptr is an ancestor of the head block.
                            // We cannot use an RPC call here to find the first interesting block
                            // due to the race conditions previously mentioned,
                            // so instead we will advance the subgraph ptr by one block.
                            // Note that ancestor_block is a child of subgraph_ptr.
                            Step::ToDescendants(vec![ancestor_block.into()])
                        } else {
                            // The subgraph ptr is not on the main chain.
                            // We will need to step back (possibly repeatedly) one block at a time
                            // until we are back on the main chain.
                            Step::ToParent
                        }
                    }
                }
            }
        };

        // We now know where to take the subgraph ptr.
        match step {
            Step::ToParent => {
                // We would like to move to the parent of the current block.
                // This means we need to revert this block.

                // First, we need the block data.
                let block = {
                    // Try locally first. Otherwise, get block from Ethereum node.
                    let block_from_store = store.lock().unwrap().block(subgraph_ptr.hash)?;
                    if let Some(block) = block_from_store {
                        Ok(block)
                    } else {
                        eth_adapter
                            .lock()
                            .unwrap()
                            .block_by_hash(subgraph_ptr.hash)
                            .wait()
                    }
                }?;

                // Revert entity changes from this block, and update subgraph ptr.
                store
                    .lock()
                    .unwrap()
                    .revert_block(SubgraphId(subgraph_id.to_owned()), block)?;

                // At this point, the loop repeats, and we try to move the subgraph ptr another
                // step in the right direction.
            }
            Step::ToDescendants(descendant_blocks) => {
                let descendant_block_count = descendant_blocks.len();
                debug!(
                    logger,
                    "Advancing subgraph ptr to process {} block(s)...", descendant_block_count
                );

                // Advance the subgraph ptr to each of the specified descendants.
                let mut subgraph_ptr = subgraph_ptr;
                for descendant_block in descendant_blocks.into_iter() {
                    // First, check if there are blocks between subgraph_ptr and descendant_block.
                    let descendant_parent_ptr = EthereumBlockPointer::to_parent(&descendant_block);
                    if subgraph_ptr != descendant_parent_ptr {
                        // descendant_block is not a direct child.
                        // Therefore, there are blocks that are irrelevant to this subgraph that we can skip.

                        // Update subgraph_ptr in store to skip the irrelevant blocks.
                        store.lock().unwrap().set_block_ptr_with_no_changes(
                            SubgraphId(subgraph_id.to_owned()),
                            subgraph_ptr,
                            descendant_parent_ptr,
                        )?;
                    }

                    // subgraph ptr is now the direct parent of descendant_block
                    subgraph_ptr = descendant_parent_ptr;
                    let descendant_ptr = EthereumBlockPointer::from(descendant_block.clone());

                    // TODO future enhancement: load a recent history of blocks before running mappings

                    // Next, we will determine what relevant events are contained in this block.
                    let events = eth_adapter
                        .lock()
                        .unwrap()
                        .get_events_in_block(descendant_block, event_filter.clone())
                        .wait()?;

                    debug!(
                        logger,
                        "Processing block #{}. {} event(s) are relevant to this subgraph.",
                        descendant_ptr.number,
                        events.len()
                    );

                    // Then, we will distribute each event to each of the runtime hosts.
                    // The execution order is important to ensure entity data is produced
                    // deterministically.
                    // TODO runtime host order should be deterministic
                    // TODO use a single StoreTransaction, use commit instead of set_block_ptr
                    events.iter().for_each(|event| {
                        let event = event.clone();
                        event_sinks.iter_mut().for_each(move |event_sink| {
                            let (confirm, confirmed) = oneshot::channel();
                            event_sink
                                .send((event.clone(), confirm))
                                .map_err(|_| {
                                    format_err!("failed to send Ethereum event to RuntimeHost mappings thread")
                                })
                                .and_then(move |_| {
                                    confirmed.map_err(|_| {
                                        format_err!("failed to receive result of sending Ethereum event to RuntimeHost mappings thread")
                                    })
                                })
                                .and_then(|result| result)
                                .wait()
                                .ok();
                        })
                    });
                    store.lock().unwrap().set_block_ptr_with_no_changes(
                        SubgraphId(subgraph_id.to_owned()),
                        subgraph_ptr,
                        descendant_ptr,
                    )?;
                    subgraph_ptr = descendant_ptr;

                    debug!(logger, "Done processing block #{}.", descendant_ptr.number);
                }

                debug!(logger, "Processed {} block(s).", descendant_block_count);

                // At this point, the loop repeats, and we try to move the subgraph ptr another
                // step in the right direction.
            }
        }
    }

    Ok(())
}
