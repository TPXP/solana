//! The `retransmit_stage` retransmits shreds between validators
#![allow(clippy::rc_buffer)]

use {
    crate::{
        ancestor_hashes_service::AncestorHashesReplayUpdateReceiver,
        cluster_info_vote_listener::VerifiedVoteReceiver,
        cluster_nodes::ClusterNodesCache,
        cluster_slots::ClusterSlots,
        cluster_slots_service::{ClusterSlotsService, ClusterSlotsUpdateReceiver},
        completed_data_sets_service::CompletedDataSetsSender,
        repair_service::{DuplicateSlotsResetSender, RepairInfo},
        window_service::{should_retransmit_and_persist, WindowService},
    },
    crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender},
    lru::LruCache,
    rand::Rng,
    rayon::{prelude::*, ThreadPool, ThreadPoolBuilder},
    solana_client::rpc_response::SlotUpdate,
    solana_gossip::{
        cluster_info::{ClusterInfo, DATA_PLANE_FANOUT},
        contact_info::ContactInfo,
    },
    solana_ledger::{
        blockstore::Blockstore,
        leader_schedule_cache::LeaderScheduleCache,
        shred::{Shred, ShredId},
    },
    solana_measure::measure::Measure,
    solana_perf::{packet::PacketBatch, sigverify::Deduper},
    solana_rayon_threadlimit::get_thread_count,
    solana_rpc::{max_slots::MaxSlots, rpc_subscriptions::RpcSubscriptions},
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_sdk::{clock::Slot, epoch_schedule::EpochSchedule, pubkey::Pubkey, timing::timestamp},
    solana_streamer::sendmmsg::{multi_target_send, SendPktsError},
    std::{
        collections::{BTreeSet, HashMap, HashSet},
        net::UdpSocket,
        ops::AddAssign,
        sync::{
            atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
            Arc, Mutex, RwLock,
        },
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

const MAX_DUPLICATE_COUNT: usize = 2;
const DEDUPER_FALSE_POSITIVE_RATE: f64 = 0.001;
const DEDUPER_NUM_BITS: u64 = 637_534_199; // 76MB
const DEDUPER_RESET_CYCLE: Duration = Duration::from_secs(5 * 60);

const CLUSTER_NODES_CACHE_NUM_EPOCH_CAP: usize = 8;
const CLUSTER_NODES_CACHE_TTL: Duration = Duration::from_secs(5);

#[derive(Default)]
struct RetransmitSlotStats {
    asof: u64,   // Latest timestamp struct was updated.
    outset: u64, // 1st shred retransmit timestamp.
    // Number of shreds sent and received at different
    // distances from the turbine broadcast root.
    num_shreds_received: [usize; 3],
    num_shreds_sent: [usize; 3],
}

struct RetransmitStats {
    since: Instant,
    num_nodes: AtomicUsize,
    num_addrs_failed: AtomicUsize,
    num_shreds: usize,
    num_shreds_skipped: AtomicUsize,
    total_batches: usize,
    total_time: u64,
    epoch_fetch: u64,
    epoch_cache_update: u64,
    retransmit_total: AtomicU64,
    compute_turbine_peers_total: AtomicU64,
    slot_stats: LruCache<Slot, RetransmitSlotStats>,
    unknown_shred_slot_leader: AtomicUsize,
}

impl RetransmitStats {
    fn maybe_submit(
        &mut self,
        root_bank: &Bank,
        working_bank: &Bank,
        cluster_info: &ClusterInfo,
        cluster_nodes_cache: &ClusterNodesCache<RetransmitStage>,
    ) {
        const SUBMIT_CADENCE: Duration = Duration::from_secs(2);
        if self.since.elapsed() < SUBMIT_CADENCE {
            return;
        }
        cluster_nodes_cache
            .get(root_bank.slot(), root_bank, working_bank, cluster_info)
            .submit_metrics("cluster_nodes_retransmit", timestamp());
        datapoint_info!(
            "retransmit-stage",
            ("total_time", self.total_time, i64),
            ("epoch_fetch", self.epoch_fetch, i64),
            ("epoch_cache_update", self.epoch_cache_update, i64),
            ("total_batches", self.total_batches, i64),
            ("num_nodes", *self.num_nodes.get_mut(), i64),
            ("num_addrs_failed", *self.num_addrs_failed.get_mut(), i64),
            ("num_shreds", self.num_shreds, i64),
            (
                "num_shreds_skipped",
                *self.num_shreds_skipped.get_mut(),
                i64
            ),
            ("retransmit_total", *self.retransmit_total.get_mut(), i64),
            (
                "compute_turbine",
                *self.compute_turbine_peers_total.get_mut(),
                i64
            ),
            (
                "unknown_shred_slot_leader",
                *self.unknown_shred_slot_leader.get_mut(),
                i64
            ),
        );
        // slot_stats are submited at a different cadence.
        let old = std::mem::replace(self, Self::new(Instant::now()));
        self.slot_stats = old.slot_stats;
    }
}

struct ShredDeduper<const K: usize> {
    deduper: Deduper<K, /*shred:*/ [u8]>,
    shred_id_filter: Deduper<K, (ShredId, /*0..MAX_DUPLICATE_COUNT:*/ usize)>,
}

impl<const K: usize> ShredDeduper<K> {
    fn new<R: Rng>(rng: &mut R, num_bits: u64) -> Self {
        Self {
            deduper: Deduper::new(rng, num_bits),
            shred_id_filter: Deduper::new(rng, num_bits),
        }
    }

    fn maybe_reset<R: Rng>(
        &mut self,
        rng: &mut R,
        false_positive_rate: f64,
        reset_cycle: Duration,
    ) {
        self.deduper
            .maybe_reset(rng, false_positive_rate, reset_cycle);
        self.shred_id_filter
            .maybe_reset(rng, false_positive_rate, reset_cycle);
    }

    fn dedup(&self, shred: &Shred, max_duplicate_count: usize) -> bool {
        // In order to detect duplicate blocks across cluster, we retransmit
        // max_duplicate_count different shreds for each ShredId.
        let key = shred.id();
        self.deduper.dedup(&shred.payload)
            || (0..max_duplicate_count).all(|i| self.shred_id_filter.dedup(&(key, i)))
    }
}

// Returns true if this is the first time receiving a shred for `shred_slot`.
fn check_if_first_shred_received(
    shred_slot: Slot,
    first_shreds_received: &Mutex<BTreeSet<Slot>>,
    root_bank: &Bank,
) -> bool {
    if shred_slot <= root_bank.slot() {
        return false;
    }

    let mut first_shreds_received_locked = first_shreds_received.lock().unwrap();
    if first_shreds_received_locked.insert(shred_slot) {
        datapoint_info!("retransmit-first-shred", ("slot", shred_slot, i64));
        if first_shreds_received_locked.len() > 100 {
            *first_shreds_received_locked =
                first_shreds_received_locked.split_off(&(root_bank.slot() + 1));
        }
        true
    } else {
        false
    }
}

#[allow(clippy::too_many_arguments)]
fn retransmit(
    thread_pool: &ThreadPool,
    bank_forks: &RwLock<BankForks>,
    leader_schedule_cache: &LeaderScheduleCache,
    cluster_info: &ClusterInfo,
    shreds_receiver: &Receiver<Vec<Shred>>,
    sockets: &[UdpSocket],
    stats: &mut RetransmitStats,
    cluster_nodes_cache: &ClusterNodesCache<RetransmitStage>,
    shred_deduper: &mut ShredDeduper<2>,
    max_slots: &MaxSlots,
    first_shreds_received: &Mutex<BTreeSet<Slot>>,
    rpc_subscriptions: Option<&RpcSubscriptions>,
) -> Result<(), RecvTimeoutError> {
    const RECV_TIMEOUT: Duration = Duration::from_secs(1);
    let mut shreds = shreds_receiver.recv_timeout(RECV_TIMEOUT)?;
    let mut timer_start = Measure::start("retransmit");
    shreds.extend(shreds_receiver.try_iter().flatten());
    stats.num_shreds += shreds.len();
    stats.total_batches += 1;

    let mut epoch_fetch = Measure::start("retransmit_epoch_fetch");
    let (working_bank, root_bank) = {
        let bank_forks = bank_forks.read().unwrap();
        (bank_forks.working_bank(), bank_forks.root_bank())
    };
    epoch_fetch.stop();
    stats.epoch_fetch += epoch_fetch.as_us();

    let mut epoch_cache_update = Measure::start("retransmit_epoch_cach_update");
    shred_deduper.maybe_reset(
        &mut rand::thread_rng(),
        DEDUPER_FALSE_POSITIVE_RATE,
        DEDUPER_RESET_CYCLE,
    );
    epoch_cache_update.stop();
    stats.epoch_cache_update += epoch_cache_update.as_us();

    let socket_addr_space = cluster_info.socket_addr_space();
    let retransmit_shred = |shred: &Shred, socket: &UdpSocket| {
        if shred_deduper.dedup(shred, MAX_DUPLICATE_COUNT) {
            stats.num_shreds_skipped.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let shred_slot = shred.slot();
        max_slots
            .retransmit
            .fetch_max(shred_slot, Ordering::Relaxed);

        if let Some(rpc_subscriptions) = rpc_subscriptions {
            if check_if_first_shred_received(shred_slot, first_shreds_received, &root_bank) {
                rpc_subscriptions.notify_slot_update(SlotUpdate::FirstShredReceived {
                    slot: shred_slot,
                    timestamp: timestamp(),
                });
            }
        }

        let mut compute_turbine_peers = Measure::start("turbine_start");
        // TODO: consider using root-bank here for leader lookup!
        // Shreds' signatures should be verified before they reach here, and if
        // the leader is unknown they should fail signature check. So here we
        // should expect to know the slot leader and otherwise skip the shred.
        let slot_leader =
            match leader_schedule_cache.slot_leader_at(shred_slot, Some(&working_bank)) {
                Some(pubkey) => pubkey,
                None => {
                    stats
                        .unknown_shred_slot_leader
                        .fetch_add(1, Ordering::Relaxed);
                    return None;
                }
            };
        let cluster_nodes =
            cluster_nodes_cache.get(shred_slot, &root_bank, &working_bank, cluster_info);
        let (root_distance, addrs) =
            cluster_nodes.get_retransmit_addrs(slot_leader, shred, &root_bank, DATA_PLANE_FANOUT);
        let addrs: Vec<_> = addrs
            .into_iter()
            .filter(|addr| ContactInfo::is_valid_address(addr, socket_addr_space))
            .collect();
        compute_turbine_peers.stop();
        stats
            .compute_turbine_peers_total
            .fetch_add(compute_turbine_peers.as_us(), Ordering::Relaxed);

        let mut retransmit_time = Measure::start("retransmit_to");
        let num_nodes = match multi_target_send(socket, &shred.payload, &addrs) {
            Ok(()) => addrs.len(),
            Err(SendPktsError::IoError(ioerr, num_failed)) => {
                stats
                    .num_addrs_failed
                    .fetch_add(num_failed, Ordering::Relaxed);
                error!(
                    "retransmit_to multi_target_send error: {:?}, {}/{} packets failed",
                    ioerr,
                    num_failed,
                    addrs.len(),
                );
                addrs.len() - num_failed
            }
        };
        retransmit_time.stop();
        stats.num_nodes.fetch_add(num_nodes, Ordering::Relaxed);
        stats
            .retransmit_total
            .fetch_add(retransmit_time.as_us(), Ordering::Relaxed);
        Some((root_distance, num_nodes))
    };
    let slot_stats = thread_pool.install(|| {
        shreds
            .into_par_iter()
            .with_min_len(4)
            .filter_map(|shred| {
                let index = thread_pool.current_thread_index().unwrap();
                let socket = &sockets[index % sockets.len()];
                Some((shred.slot(), retransmit_shred(&shred, socket)?))
            })
            .fold(
                HashMap::<Slot, RetransmitSlotStats>::new,
                |mut acc, (slot, (root_distance, num_nodes))| {
                    let now = timestamp();
                    let slot_stats = acc.entry(slot).or_default();
                    slot_stats.record(now, root_distance, num_nodes);
                    acc
                },
            )
            .reduce(HashMap::new, RetransmitSlotStats::merge)
    });
    stats.upsert_slot_stats(slot_stats);
    timer_start.stop();
    stats.total_time += timer_start.as_us();
    stats.maybe_submit(&root_bank, &working_bank, cluster_info, cluster_nodes_cache);
    Ok(())
}

/// Service to retransmit messages from the leader or layer 1 to relevant peer nodes.
/// See `cluster_info` for network layer definitions.
/// # Arguments
/// * `sockets` - Sockets to read from.
/// * `bank_forks` - The BankForks structure
/// * `leader_schedule_cache` - The leader schedule to verify shreds
/// * `cluster_info` - This structure needs to be updated and populated by the bank and via gossip.
/// * `r` - Receive channel for shreds to be retransmitted to all the layer 1 nodes.
pub fn retransmitter(
    sockets: Arc<Vec<UdpSocket>>,
    bank_forks: Arc<RwLock<BankForks>>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    cluster_info: Arc<ClusterInfo>,
    shreds_receiver: Receiver<Vec<Shred>>,
    max_slots: Arc<MaxSlots>,
    rpc_subscriptions: Option<Arc<RpcSubscriptions>>,
) -> JoinHandle<()> {
    let cluster_nodes_cache = ClusterNodesCache::<RetransmitStage>::new(
        CLUSTER_NODES_CACHE_NUM_EPOCH_CAP,
        CLUSTER_NODES_CACHE_TTL,
    );
    let mut rng = rand::thread_rng();
    let mut shred_deduper = ShredDeduper::<2>::new(&mut rng, DEDUPER_NUM_BITS);
    let mut stats = RetransmitStats::new(Instant::now());
    let first_shreds_received = Mutex::<BTreeSet<Slot>>::default();
    let num_threads = get_thread_count().min(8).max(sockets.len());
    let thread_pool = ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .thread_name(|i| format!("retransmit-{}", i))
        .build()
        .unwrap();
    Builder::new()
        .name("solana-retransmitter".to_string())
        .spawn(move || {
            trace!("retransmitter started");
            loop {
                match retransmit(
                    &thread_pool,
                    &bank_forks,
                    &leader_schedule_cache,
                    &cluster_info,
                    &shreds_receiver,
                    &sockets,
                    &mut stats,
                    &cluster_nodes_cache,
                    &mut shred_deduper,
                    &max_slots,
                    &first_shreds_received,
                    rpc_subscriptions.as_deref(),
                ) {
                    Ok(()) => (),
                    Err(RecvTimeoutError::Timeout) => (),
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            trace!("exiting retransmitter");
        })
        .unwrap()
}

pub struct RetransmitStage {
    retransmit_thread_handle: JoinHandle<()>,
    window_service: WindowService,
    cluster_slots_service: ClusterSlotsService,
}

impl RetransmitStage {
    #[allow(clippy::new_ret_no_self)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        bank_forks: Arc<RwLock<BankForks>>,
        leader_schedule_cache: Arc<LeaderScheduleCache>,
        blockstore: Arc<Blockstore>,
        cluster_info: Arc<ClusterInfo>,
        retransmit_sockets: Arc<Vec<UdpSocket>>,
        repair_socket: Arc<UdpSocket>,
        ancestor_hashes_socket: Arc<UdpSocket>,
        verified_receiver: Receiver<Vec<PacketBatch>>,
        exit: Arc<AtomicBool>,
        cluster_slots_update_receiver: ClusterSlotsUpdateReceiver,
        epoch_schedule: EpochSchedule,
        cfg: Option<Arc<AtomicBool>>,
        shred_version: u16,
        cluster_slots: Arc<ClusterSlots>,
        duplicate_slots_reset_sender: DuplicateSlotsResetSender,
        verified_vote_receiver: VerifiedVoteReceiver,
        repair_validators: Option<HashSet<Pubkey>>,
        completed_data_sets_sender: CompletedDataSetsSender,
        max_slots: Arc<MaxSlots>,
        rpc_subscriptions: Option<Arc<RpcSubscriptions>>,
        duplicate_slots_sender: Sender<Slot>,
        ancestor_hashes_replay_update_receiver: AncestorHashesReplayUpdateReceiver,
    ) -> Self {
        let (retransmit_sender, retransmit_receiver) = unbounded();

        let retransmit_thread_handle = retransmitter(
            retransmit_sockets,
            bank_forks.clone(),
            leader_schedule_cache.clone(),
            cluster_info.clone(),
            retransmit_receiver,
            max_slots,
            rpc_subscriptions,
        );

        let cluster_slots_service = ClusterSlotsService::new(
            blockstore.clone(),
            cluster_slots.clone(),
            bank_forks.clone(),
            cluster_info.clone(),
            cluster_slots_update_receiver,
            exit.clone(),
        );

        let leader_schedule_cache_clone = leader_schedule_cache.clone();
        let repair_info = RepairInfo {
            bank_forks,
            epoch_schedule,
            duplicate_slots_reset_sender,
            repair_validators,
            cluster_info,
            cluster_slots,
        };
        let window_service = WindowService::new(
            blockstore,
            verified_receiver,
            retransmit_sender,
            repair_socket,
            ancestor_hashes_socket,
            exit,
            repair_info,
            leader_schedule_cache,
            move |id, shred, working_bank, last_root| {
                let is_connected = cfg
                    .as_ref()
                    .map(|x| x.load(Ordering::Relaxed))
                    .unwrap_or(true);
                let rv = should_retransmit_and_persist(
                    shred,
                    working_bank,
                    &leader_schedule_cache_clone,
                    id,
                    last_root,
                    shred_version,
                );
                rv && is_connected
            },
            verified_vote_receiver,
            completed_data_sets_sender,
            duplicate_slots_sender,
            ancestor_hashes_replay_update_receiver,
        );

        Self {
            retransmit_thread_handle,
            window_service,
            cluster_slots_service,
        }
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        self.retransmit_thread_handle.join()?;
        self.window_service.join()?;
        self.cluster_slots_service.join()
    }
}

impl AddAssign for RetransmitSlotStats {
    fn add_assign(&mut self, other: Self) {
        let Self {
            asof,
            outset,
            num_shreds_received,
            num_shreds_sent,
        } = other;
        self.asof = self.asof.max(asof);
        self.outset = if self.outset == 0 {
            outset
        } else {
            self.outset.min(outset)
        };
        for k in 0..3 {
            self.num_shreds_received[k] += num_shreds_received[k];
            self.num_shreds_sent[k] += num_shreds_sent[k];
        }
    }
}

impl RetransmitStats {
    const SLOT_STATS_CACHE_CAPACITY: usize = 750;

    fn new(now: Instant) -> Self {
        Self {
            since: now,
            num_nodes: AtomicUsize::default(),
            num_addrs_failed: AtomicUsize::default(),
            num_shreds: 0usize,
            num_shreds_skipped: AtomicUsize::default(),
            total_batches: 0usize,
            total_time: 0u64,
            epoch_fetch: 0u64,
            epoch_cache_update: 0u64,
            retransmit_total: AtomicU64::default(),
            compute_turbine_peers_total: AtomicU64::default(),
            // Cache capacity is manually enforced.
            slot_stats: LruCache::<Slot, RetransmitSlotStats>::unbounded(),
            unknown_shred_slot_leader: AtomicUsize::default(),
        }
    }

    fn upsert_slot_stats<I>(&mut self, feed: I)
    where
        I: IntoIterator<Item = (Slot, RetransmitSlotStats)>,
    {
        for (slot, slot_stats) in feed {
            match self.slot_stats.get_mut(&slot) {
                None => {
                    self.slot_stats.put(slot, slot_stats);
                }
                Some(entry) => {
                    *entry += slot_stats;
                }
            }
        }
        while self.slot_stats.len() > Self::SLOT_STATS_CACHE_CAPACITY {
            // Pop and submit metrics for the slot which was updated least
            // recently. At this point the node most likely will not receive
            // and retransmit any more shreds for this slot.
            match self.slot_stats.pop_lru() {
                Some((slot, stats)) => stats.submit(slot),
                None => break,
            }
        }
    }
}

impl RetransmitSlotStats {
    fn record(&mut self, now: u64, root_distance: usize, num_nodes: usize) {
        self.outset = if self.outset == 0 {
            now
        } else {
            self.outset.min(now)
        };
        self.asof = self.asof.max(now);
        self.num_shreds_received[root_distance] += 1;
        self.num_shreds_sent[root_distance] += num_nodes;
    }

    fn merge(mut acc: HashMap<Slot, Self>, other: HashMap<Slot, Self>) -> HashMap<Slot, Self> {
        if acc.len() < other.len() {
            return Self::merge(other, acc);
        }
        for (key, value) in other {
            *acc.entry(key).or_default() += value;
        }
        acc
    }

    fn submit(&self, slot: Slot) {
        let num_shreds: usize = self.num_shreds_received.iter().sum();
        let num_nodes: usize = self.num_shreds_sent.iter().sum();
        let elapsed_millis = self.asof.saturating_sub(self.outset);
        datapoint_info!(
            "retransmit-stage-slot-stats",
            ("slot", slot, i64),
            ("outset_timestamp", self.outset, i64),
            ("elapsed_millis", elapsed_millis, i64),
            ("num_shreds", num_shreds, i64),
            ("num_nodes", num_nodes, i64),
            ("num_shreds_received_root", self.num_shreds_received[0], i64),
            (
                "num_shreds_received_1st_layer",
                self.num_shreds_received[1],
                i64
            ),
            (
                "num_shreds_received_2nd_layer",
                self.num_shreds_received[2],
                i64
            ),
            ("num_shreds_sent_root", self.num_shreds_sent[0], i64),
            ("num_shreds_sent_1st_layer", self.num_shreds_sent[1], i64),
            ("num_shreds_sent_2nd_layer", self.num_shreds_sent[2], i64),
        );
    }
}

#[cfg(test)]
mod tests {
    use {super::*, rand::SeedableRng, rand_chacha::ChaChaRng};

    #[test]
    fn test_already_received() {
        let slot = 1;
        let index = 5;
        let version = 0x40;
        let shred = Shred::new_from_data(slot, index, 0, None, true, true, 0, version, 0);
        let mut rng = ChaChaRng::from_seed([0xa5; 32]);
        let shred_deduper = ShredDeduper::<2>::new(&mut rng, /*num_bits:*/ 640_007);
        // unique shred for (1, 5) should pass
        assert!(!shred_deduper.dedup(&shred, MAX_DUPLICATE_COUNT));
        // duplicate shred for (1, 5) blocked
        assert!(shred_deduper.dedup(&shred, MAX_DUPLICATE_COUNT));

        let shred = Shred::new_from_data(slot, index, 2, None, true, true, 0, version, 0);
        // first duplicate shred for (1, 5) passed
        assert!(!shred_deduper.dedup(&shred, MAX_DUPLICATE_COUNT));
        // then blocked
        assert!(shred_deduper.dedup(&shred, MAX_DUPLICATE_COUNT));

        let shred = Shred::new_from_data(slot, index, 8, None, true, true, 0, version, 0);
        // 2nd duplicate shred for (1, 5) blocked
        assert!(shred_deduper.dedup(&shred, MAX_DUPLICATE_COUNT));
        assert!(shred_deduper.dedup(&shred, MAX_DUPLICATE_COUNT));

        let shred = Shred::new_empty_coding(slot, index, 0, 1, 1, 0, version);
        // Coding at (1, 5) passes
        assert!(!shred_deduper.dedup(&shred, MAX_DUPLICATE_COUNT));
        // then blocked
        assert!(shred_deduper.dedup(&shred, MAX_DUPLICATE_COUNT));

        let shred = Shred::new_empty_coding(slot, index, 2, 1, 1, 0, version);
        // 2nd unique coding at (1, 5) passes
        assert!(!shred_deduper.dedup(&shred, MAX_DUPLICATE_COUNT));
        // same again is blocked
        assert!(shred_deduper.dedup(&shred, MAX_DUPLICATE_COUNT));

        let shred = Shred::new_empty_coding(slot, index, 3, 1, 1, 0, version);
        // Another unique coding at (1, 5) always blocked
        assert!(shred_deduper.dedup(&shred, MAX_DUPLICATE_COUNT));
        assert!(shred_deduper.dedup(&shred, MAX_DUPLICATE_COUNT));
    }
}
