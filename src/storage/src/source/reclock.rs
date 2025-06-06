// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

/// The `ReclockOperator` observes the progress of a stream that is
/// timestamped with some source time `FromTime` and generates bindings that describe how the
/// collection should evolve in target time `IntoTime`.
use differential_dataflow::consolidation;
use differential_dataflow::lattice::Lattice;
use mz_persist_client::error::UpperMismatch;
use mz_repr::Diff;
use mz_storage_client::util::remap_handle::RemapHandle;
use timely::order::PartialOrder;
use timely::progress::Timestamp;
use timely::progress::frontier::{Antichain, AntichainRef, MutableAntichain};

pub mod compat;

/// The `ReclockOperator` is responsible for observing progress in the `FromTime` domain and
/// consume messages from a ticker of progress in the `IntoTime` domain. When the source frontier
/// advances and the ticker ticks the `ReclockOperator` will generate the data that describe this
/// correspondence and write them out to its provided remap handle. The output generated by the
/// reclock operator can be thought of as `Collection<G, FromTime>` where `G::Timestamp` is
/// `IntoTime`.
///
/// The `ReclockOperator` will always maintain the invariant that for any time `IntoTime` the remap
/// collection accumulates into an Antichain where each `FromTime` timestamp has frequency `1`. In
/// other words the remap collection describes a well formed `Antichain<FromTime>` as it is
/// marching forwards.
#[derive(Debug)]
pub struct ReclockOperator<
    FromTime: Timestamp,
    IntoTime: Timestamp + Lattice,
    Handle: RemapHandle<FromTime = FromTime, IntoTime = IntoTime>,
> {
    /// Upper frontier of the partial remap trace
    upper: Antichain<IntoTime>,
    /// The upper frontier in terms of `FromTime`. Any attempt to reclock messages beyond this
    /// frontier will lead to minting new bindings.
    source_upper: MutableAntichain<FromTime>,

    /// A handle allowing this operator to publish updates to and read back from the remap collection
    remap_handle: Handle,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReclockBatch<FromTime, IntoTime> {
    pub updates: Vec<(FromTime, IntoTime, Diff)>,
    pub upper: Antichain<IntoTime>,
}

impl<FromTime, IntoTime, Handle> ReclockOperator<FromTime, IntoTime, Handle>
where
    FromTime: Timestamp,
    IntoTime: Timestamp + Lattice,
    Handle: RemapHandle<FromTime = FromTime, IntoTime = IntoTime>,
{
    /// Construct a new [ReclockOperator] from the given collection metadata
    pub async fn new(remap_handle: Handle) -> (Self, ReclockBatch<FromTime, IntoTime>) {
        let upper = remap_handle.upper().clone();

        let mut operator = Self {
            upper: Antichain::from_elem(IntoTime::minimum()),
            source_upper: MutableAntichain::new(),
            remap_handle,
        };

        // Load the initial state that might exist in the shard
        let trace_batch = if upper.elements() != [IntoTime::minimum()] {
            operator.sync(upper.borrow()).await
        } else {
            ReclockBatch {
                updates: vec![],
                upper: Antichain::from_elem(IntoTime::minimum()),
            }
        };

        (operator, trace_batch)
    }

    /// Syncs the state of this operator to match that of the persist shard until the provided
    /// frontier
    async fn sync(
        &mut self,
        target_upper: AntichainRef<'_, IntoTime>,
    ) -> ReclockBatch<FromTime, IntoTime> {
        let mut updates: Vec<(FromTime, IntoTime, Diff)> = Vec::new();

        // Tail the remap collection until we reach the target upper frontier. Note that, in the
        // common case, we are also the writer, so we are waiting to read-back what we wrote
        while PartialOrder::less_than(&self.upper.borrow(), &target_upper) {
            let (mut batch, upper) = self
                .remap_handle
                .next()
                .await
                .expect("requested data after empty antichain");
            self.upper = upper;
            updates.append(&mut batch);
        }

        self.source_upper.update_iter(
            updates
                .iter()
                .map(|(src_ts, _dest_ts, diff)| (src_ts.clone(), diff.into_inner())),
        );

        ReclockBatch {
            updates,
            upper: self.upper.clone(),
        }
    }

    pub async fn mint(
        &mut self,
        binding_ts: IntoTime,
        mut new_into_upper: Antichain<IntoTime>,
        new_from_upper: AntichainRef<'_, FromTime>,
    ) -> ReclockBatch<FromTime, IntoTime> {
        assert!(!new_into_upper.less_equal(&binding_ts));
        // The updates to the remap trace that occured during minting.
        let mut batch = ReclockBatch {
            updates: vec![],
            upper: self.upper.clone(),
        };

        while *self.upper == [IntoTime::minimum()]
            || (PartialOrder::less_equal(&self.source_upper.frontier(), &new_from_upper)
                && PartialOrder::less_than(&self.upper, &new_into_upper)
                && self.upper.less_equal(&binding_ts))
        {
            // If source is closed, close remap shard as well.
            if new_from_upper.is_empty() {
                new_into_upper = Antichain::new();
            }

            // If this is the first binding we mint then we will mint it at the minimum target
            // timestamp. The first source upper is always the upper of the snapshot and by mapping
            // it to the minimum target timestamp we make it so that the final shard never appears
            // empty at any timestamp.
            let binding_ts = if *self.upper == [IntoTime::minimum()] {
                IntoTime::minimum()
            } else {
                binding_ts.clone()
            };

            let mut updates = vec![];
            for src_ts in self.source_upper.frontier().iter().cloned() {
                updates.push((src_ts, binding_ts.clone(), Diff::MINUS_ONE));
            }
            for src_ts in new_from_upper.iter().cloned() {
                updates.push((src_ts, binding_ts.clone(), Diff::ONE));
            }
            consolidation::consolidate_updates(&mut updates);

            let new_batch = match self.append_batch(updates, &new_into_upper).await {
                Ok(trace_batch) => trace_batch,
                Err(UpperMismatch { current, .. }) => self.sync(current.borrow()).await,
            };
            batch.updates.extend(new_batch.updates);
            batch.upper = new_batch.upper;
        }

        batch
    }

    /// Appends the provided updates to the remap collection at the next available minting
    /// IntoTime and updates this operator's in-memory state accordingly.
    ///
    /// If an attempt to mint bindings fails due to another process having raced and appended
    /// bindings concurrently then the current global upper will be returned as an error. This is
    /// the frontier that this operator must be synced to for a future append attempt to have any
    /// chance of success.
    async fn append_batch(
        &mut self,
        updates: Vec<(FromTime, IntoTime, Diff)>,
        new_upper: &Antichain<IntoTime>,
    ) -> Result<ReclockBatch<FromTime, IntoTime>, UpperMismatch<IntoTime>> {
        match self
            .remap_handle
            .compare_and_append(updates, self.upper.clone(), new_upper.clone())
            .await
        {
            // We have successfully produced data in the remap collection so let's read back what
            // we wrote to update our local state
            Ok(()) => Ok(self.sync(new_upper.borrow()).await),
            Err(mismatch) => Err(mismatch),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::str::FromStr;
    use std::sync::Arc;
    use std::sync::LazyLock;
    use std::time::Duration;

    use mz_build_info::DUMMY_BUILD_INFO;
    use mz_ore::metrics::MetricsRegistry;
    use mz_ore::now::SYSTEM_TIME;
    use mz_ore::url::SensitiveUrl;
    use mz_persist_client::cache::PersistClientCache;
    use mz_persist_client::cfg::PersistConfig;
    use mz_persist_client::rpc::PubSubClientConnection;
    use mz_persist_client::{Diagnostics, PersistClient, PersistLocation, ShardId};
    use mz_persist_types::codec_impls::UnitSchema;
    use mz_repr::{GlobalId, RelationDesc, ScalarType, Timestamp};
    use mz_storage_client::util::remap_handle::RemapHandle;
    use mz_storage_types::StorageDiff;
    use mz_storage_types::controller::CollectionMetadata;
    use mz_storage_types::sources::kafka::{self, RangeBound as RB};
    use mz_storage_types::sources::{MzOffset, SourceData};
    use mz_timely_util::order::Partitioned;
    use timely::progress::Timestamp as _;
    use tokio::sync::watch;

    use super::*;

    // 15 minutes
    static PERSIST_READER_LEASE_TIMEOUT_MS: Duration = Duration::from_secs(60 * 15);

    static PERSIST_CACHE: LazyLock<Arc<PersistClientCache>> = LazyLock::new(|| {
        let persistcfg = PersistConfig::new_default_configs(&DUMMY_BUILD_INFO, SYSTEM_TIME.clone());
        persistcfg.set_reader_lease_duration(PERSIST_READER_LEASE_TIMEOUT_MS);
        Arc::new(PersistClientCache::new(
            persistcfg,
            &MetricsRegistry::new(),
            |_, _| PubSubClientConnection::noop(),
        ))
    });

    static PROGRESS_DESC: LazyLock<RelationDesc> = LazyLock::new(|| {
        RelationDesc::builder()
            .with_column(
                "partition",
                ScalarType::Range {
                    element_type: Box::new(ScalarType::Numeric { max_scale: None }),
                }
                .nullable(false),
            )
            .with_column("offset", ScalarType::UInt64.nullable(true))
            .finish()
    });

    async fn make_test_operator(
        shard: ShardId,
        as_of: Antichain<Timestamp>,
    ) -> (
        ReclockOperator<
            kafka::KafkaTimestamp,
            Timestamp,
            impl RemapHandle<FromTime = kafka::KafkaTimestamp, IntoTime = Timestamp>,
        >,
        ReclockBatch<kafka::KafkaTimestamp, Timestamp>,
    ) {
        let metadata = CollectionMetadata {
            persist_location: PersistLocation {
                blob_uri: SensitiveUrl::from_str("mem://").expect("invalid URL"),
                consensus_uri: SensitiveUrl::from_str("mem://").expect("invalid URL"),
            },
            remap_shard: Some(shard),
            data_shard: ShardId::new(),
            relation_desc: RelationDesc::empty(),
            txns_shard: None,
        };

        let write_frontier = Rc::new(RefCell::new(Antichain::from_elem(Timestamp::minimum())));

        // Always in read-write mode for tests.
        let (_read_only_tx, read_only_rx) = watch::channel(false);
        let remap_handle = crate::source::reclock::compat::PersistHandle::new(
            Arc::clone(&*PERSIST_CACHE),
            read_only_rx,
            metadata,
            as_of.clone(),
            write_frontier,
            GlobalId::Explain,
            "unittest",
            0,
            1,
            PROGRESS_DESC.clone(),
            GlobalId::Explain,
        )
        .await
        .unwrap();

        let (mut operator, mut initial_batch) = ReclockOperator::new(remap_handle).await;

        // Push any updates that might already exist in the persist shard to the follower.
        if *initial_batch.upper == [Timestamp::minimum()] {
            // In the tests we always reclock the minimum source frontier to the minimum target
            // frontier, which we do in this step.
            initial_batch = operator
                .mint(
                    0.into(),
                    Antichain::from_elem(1.into()),
                    Antichain::from_elem(Partitioned::minimum()).borrow(),
                )
                .await;
        }

        (operator, initial_batch)
    }

    /// Generates a [`kafka::NativeFrontier`] antichain where all the provided
    /// partitions are at the specified offset and the gaps in between are filled with range
    /// timestamps at offset zero.
    fn partitioned_frontier<I>(items: I) -> Antichain<kafka::KafkaTimestamp>
    where
        I: IntoIterator<Item = (i32, MzOffset)>,
    {
        let mut frontier = Antichain::new();
        let mut prev = RB::NegInfinity;
        for (pid, offset) in items {
            assert!(prev < RB::before(pid));
            let gap = Partitioned::new_range(prev, RB::before(pid), MzOffset::from(0));
            frontier.extend([gap, Partitioned::new_singleton(RB::exact(pid), offset)]);
            prev = RB::after(pid);
        }
        frontier.insert(Partitioned::new_range(
            prev,
            RB::PosInfinity,
            MzOffset::from(0),
        ));
        frontier
    }

    #[mz_ore::test(tokio::test)]
    #[cfg_attr(miri, ignore)] // error: unsupported operation: can't call foreign function `decNumberFromInt32` on OS `linux`
    async fn test_basic_usage() {
        let (mut operator, _) =
            make_test_operator(ShardId::new(), Antichain::from_elem(0.into())).await;

        // Reclock offsets 1 and 3 to timestamp 1000
        let source_upper = partitioned_frontier([(0, MzOffset::from(4))]);
        let mut batch = operator
            .mint(
                1000.into(),
                Antichain::from_elem(1001.into()),
                source_upper.borrow(),
            )
            .await;
        let mut expected_batch: ReclockBatch<_, Timestamp> = ReclockBatch {
            updates: vec![
                (
                    Partitioned::new_range(RB::NegInfinity, RB::before(0), MzOffset::from(0)),
                    1000.into(),
                    Diff::ONE,
                ),
                (
                    Partitioned::new_range(RB::after(0), RB::PosInfinity, MzOffset::from(0)),
                    1000.into(),
                    Diff::ONE,
                ),
                (
                    Partitioned::new_range(RB::NegInfinity, RB::PosInfinity, MzOffset::from(0)),
                    1000.into(),
                    Diff::MINUS_ONE,
                ),
                (
                    Partitioned::new_singleton(RB::exact(0), MzOffset::from(4)),
                    1000.into(),
                    Diff::ONE,
                ),
            ],
            upper: Antichain::from_elem(Timestamp::from(1001)),
        };
        batch.updates.sort();
        expected_batch.updates.sort();
        assert_eq!(batch, expected_batch);
    }

    #[mz_ore::test(tokio::test)]
    #[cfg_attr(miri, ignore)] // error: unsupported operation: can't call foreign function `decNumberFromInt32` on OS `linux`
    async fn test_compaction() {
        let persist_location = PersistLocation {
            blob_uri: SensitiveUrl::from_str("mem://").expect("invalid URL"),
            consensus_uri: SensitiveUrl::from_str("mem://").expect("invalid URL"),
        };

        let remap_shard = ShardId::new();

        let persist_client = PERSIST_CACHE
            .open(persist_location)
            .await
            .expect("error creating persist client");

        let mut remap_read_handle = persist_client
            .open_critical_since::<SourceData, (), Timestamp, StorageDiff, u64>(
                remap_shard,
                PersistClient::CONTROLLER_CRITICAL_SINCE,
                Diagnostics::from_purpose("test_since_hold"),
            )
            .await
            .expect("error opening persist shard");

        let (mut operator, _batch) =
            make_test_operator(remap_shard, Antichain::from_elem(0.into())).await;

        // Ming a few bindings
        let source_upper = partitioned_frontier([(0, MzOffset::from(3))]);
        operator
            .mint(
                1000.into(),
                Antichain::from_elem(1001.into()),
                source_upper.borrow(),
            )
            .await;

        let source_upper = partitioned_frontier([(0, MzOffset::from(5))]);
        operator
            .mint(
                2000.into(),
                Antichain::from_elem(2001.into()),
                source_upper.borrow(),
            )
            .await;

        // Compact enough so that offsets >= 3 remain uncompacted
        remap_read_handle
            .compare_and_downgrade_since(&0, (&0, &Antichain::from_elem(1000.into())))
            .await
            .unwrap();

        // Starting a new operator with an `as_of` is the same as having compacted
        let (_operator, mut initial_batch) =
            make_test_operator(remap_shard, Antichain::from_elem(1000.into())).await;

        let mut expected_batch: ReclockBatch<_, Timestamp> = ReclockBatch {
            updates: vec![
                (
                    Partitioned::new_range(RB::NegInfinity, RB::before(0), MzOffset::from(0)),
                    1000.into(),
                    Diff::ONE,
                ),
                (
                    Partitioned::new_range(RB::after(0), RB::PosInfinity, MzOffset::from(0)),
                    1000.into(),
                    Diff::ONE,
                ),
                (
                    Partitioned::new_singleton(RB::exact(0), MzOffset::from(3)),
                    1000.into(),
                    Diff::ONE,
                ),
                (
                    Partitioned::new_singleton(RB::exact(0), MzOffset::from(3)),
                    2000.into(),
                    Diff::MINUS_ONE,
                ),
                (
                    Partitioned::new_singleton(RB::exact(0), MzOffset::from(5)),
                    2000.into(),
                    Diff::ONE,
                ),
            ],
            upper: Antichain::from_elem(Timestamp::from(2001)),
        };
        expected_batch.updates.sort();
        initial_batch.updates.sort();
        assert_eq!(initial_batch, expected_batch);
    }

    #[mz_ore::test(tokio::test)]
    #[cfg_attr(miri, ignore)] // error: unsupported operation: can't call foreign function `decNumberFromInt32` on OS `linux`
    async fn test_concurrency() {
        // Create two operators pointing to the same shard
        let shared_shard = ShardId::new();
        let (mut op_a, _) = make_test_operator(shared_shard, Antichain::from_elem(0.into())).await;
        let (mut op_b, _) = make_test_operator(shared_shard, Antichain::from_elem(0.into())).await;

        // Mint some bindings through operator A
        let source_upper = partitioned_frontier([(0, MzOffset::from(3))]);
        let mut batch = op_a
            .mint(
                1000.into(),
                Antichain::from_elem(1001.into()),
                source_upper.borrow(),
            )
            .await;
        let mut expected_batch: ReclockBatch<_, Timestamp> = ReclockBatch {
            updates: vec![
                (
                    Partitioned::new_range(RB::NegInfinity, RB::before(0), MzOffset::from(0)),
                    1000.into(),
                    Diff::ONE,
                ),
                (
                    Partitioned::new_range(RB::after(0), RB::PosInfinity, MzOffset::from(0)),
                    1000.into(),
                    Diff::ONE,
                ),
                (
                    Partitioned::new_range(RB::NegInfinity, RB::PosInfinity, MzOffset::from(0)),
                    1000.into(),
                    Diff::MINUS_ONE,
                ),
                (
                    Partitioned::new_singleton(RB::exact(0), MzOffset::from(3)),
                    1000.into(),
                    Diff::ONE,
                ),
            ],
            upper: Antichain::from_elem(Timestamp::from(1001)),
        };
        batch.updates.sort();
        expected_batch.updates.sort();
        assert_eq!(batch, expected_batch);

        // Operator B should attempt to mint in one go, fail, re-sync, and retry only for the
        // bindings that still need minting
        let source_upper = partitioned_frontier([(0, MzOffset::from(5))]);
        let mut batch = op_b
            .mint(
                11000.into(),
                Antichain::from_elem(11001.into()),
                source_upper.borrow(),
            )
            .await;
        expected_batch.updates.extend([
            (
                Partitioned::new_singleton(RB::exact(0), MzOffset::from(3)),
                11000.into(),
                Diff::MINUS_ONE,
            ),
            (
                Partitioned::new_singleton(RB::exact(0), MzOffset::from(5)),
                11000.into(),
                Diff::ONE,
            ),
        ]);
        expected_batch.upper = Antichain::from_elem(Timestamp::from(11001));
        batch.updates.sort();
        expected_batch.updates.sort();
        assert_eq!(batch, expected_batch);
    }

    // Regression test for
    // https://github.com/MaterializeInc/database-issues/issues/4216.
    #[mz_ore::test(tokio::test(start_paused = true))]
    #[cfg_attr(miri, ignore)] // error: unsupported operation: can't call foreign function `decNumberFromInt32` on OS `linux`
    async fn test_since_hold() {
        let binding_shard = ShardId::new();

        let (mut operator, _) =
            make_test_operator(binding_shard, Antichain::from_elem(0.into())).await;

        // We do multiple rounds of minting. This will downgrade the since of
        // the internal listen. If we didn't make sure to also heartbeat the
        // internal handle that holds back the overall remap since the checks
        // below would fail.
        //
        // We do two rounds and advance the time by half the lease timeout in
        // between so that the "listen handle" will not timeout but the internal
        // handle used for holding back the since will timeout.

        tokio::time::advance(PERSIST_READER_LEASE_TIMEOUT_MS / 2 + Duration::from_millis(1)).await;
        let source_upper = partitioned_frontier([(0, MzOffset::from(3))]);
        let _ = operator
            .mint(
                1000.into(),
                Antichain::from_elem(1001.into()),
                source_upper.borrow(),
            )
            .await;

        tokio::time::advance(PERSIST_READER_LEASE_TIMEOUT_MS / 2 + Duration::from_millis(1)).await;
        let source_upper = partitioned_frontier([(0, MzOffset::from(5))]);
        let _ = operator
            .mint(
                2000.into(),
                Antichain::from_elem(2001.into()),
                source_upper.borrow(),
            )
            .await;

        // Allow time for background maintenance work, which does lease
        // expiration. 1 ms is enough here, we just need to yield to allow the
        // background task to be "scheduled".
        tokio::time::sleep(Duration::from_millis(1)).await;

        // Starting a new operator with an `as_of` of `0`, to verify that
        // holding back the `since` of the remap shard works as expected.
        let (_operator, _batch) =
            make_test_operator(binding_shard, Antichain::from_elem(0.into())).await;

        // Also manually assert the since of the remap shard.
        let persist_location = PersistLocation {
            blob_uri: SensitiveUrl::from_str("mem://").expect("invalid URL"),
            consensus_uri: SensitiveUrl::from_str("mem://").expect("invalid URL"),
        };

        let persist_client = PERSIST_CACHE
            .open(persist_location)
            .await
            .expect("error creating persist client");

        let read_handle = persist_client
            .open_leased_reader::<SourceData, (), Timestamp, StorageDiff>(
                binding_shard,
                Arc::new(PROGRESS_DESC.clone()),
                Arc::new(UnitSchema),
                Diagnostics::from_purpose("test_since_hold"),
                true,
            )
            .await
            .expect("error opening persist shard");

        assert_eq!(
            Antichain::from_elem(0.into()),
            read_handle.since().to_owned()
        );
    }
}
