mod block_fetcher;
mod block_pool;
mod block_process;
mod get_blocks_process;
mod get_headers_process;
mod header_view;
mod headers_process;
mod peers;

use self::block_fetcher::BlockFetcher;
use self::block_pool::OrphanBlockPool;
use self::block_process::BlockProcess;
use self::get_blocks_process::GetBlocksProcess;
use self::get_headers_process::GetHeadersProcess;
use self::header_view::HeaderView;
use self::headers_process::HeadersProcess;
use self::peers::Peers;
use bigint::H256;
use ckb_chain::chain::{ChainProvider, TipHeader};
use ckb_chain::PowEngine;
use ckb_protocol::{
    GetBlocks, GetBlocksArgs, GetHeaders, GetHeadersArgs, SyncMessage, SyncMessageArgs, SyncPayload,
};
use ckb_time::now_ms;
use ckb_verification::{BlockVerifier, Verifier};
use config::Config;
use core::block::IndexedBlock;
use core::header::{BlockNumber, IndexedHeader};
use flatbuffers::{get_root, FlatBufferBuilder};
use futures::future;
use futures::future::lazy;
use network::PeerId;
use std::cmp;
use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio;
use util::{RwLock, RwLockUpgradableReadGuard};
use AcceptBlockError;
use {
    HEADERS_DOWNLOAD_TIMEOUT_BASE, HEADERS_DOWNLOAD_TIMEOUT_PER_HEADER, MAX_HEADERS_LEN,
    MAX_OUTBOUND_PEERS_TO_PROTECT_FROM_DISCONNECT, MAX_TIP_AGE, POW_SPACE,
};

use network::{NetworkContext, NetworkProtocolHandler, TimerToken};

pub const SEND_GET_HEADERS_TOKEN: TimerToken = 0;
pub const BLOCK_FETCH_TOKEN: TimerToken = 1;

bitflags! {
    pub struct BlockStatus: u32 {
        const UNKNOWN            = 0;
        const VALID_HEADER       = 1;
        const VALID_TREE         = 2;
        const VALID_TRANSACTIONS = 3;
        const VALID_CHAIN        = 4;
        const VALID_SCRIPTS      = 5;

        const VALID_MASK         = Self::VALID_HEADER.bits | Self::VALID_TREE.bits | Self::VALID_TRANSACTIONS.bits |
                                   Self::VALID_CHAIN.bits | Self::VALID_SCRIPTS.bits;
        const BLOCK_HAVE_DATA    = 8;
        const BLOCK_HAVE_UNDO    = 16;
        const BLOCK_HAVE_MASK    = Self::BLOCK_HAVE_DATA.bits | Self::BLOCK_HAVE_UNDO.bits;
        const FAILED_VALID       = 32;
        const FAILED_CHILD       = 64;
        const FAILED_MASK        = Self::FAILED_VALID.bits | Self::FAILED_CHILD.bits;
    }
}

pub type BlockStatusMap = Arc<RwLock<HashMap<H256, BlockStatus>>>;
pub type BlockHeaderMap = Arc<RwLock<HashMap<H256, HeaderView>>>;

pub struct Synchronizer<C, P> {
    pub chain: Arc<C>,
    pub pow: Arc<P>,
    pub status_map: BlockStatusMap,
    pub header_map: BlockHeaderMap,
    pub best_known_header: Arc<RwLock<HeaderView>>,
    pub n_sync: Arc<AtomicUsize>,
    pub peers: Arc<Peers>,
    pub config: Arc<Config>,
    pub orphan_block_pool: Arc<OrphanBlockPool>,
    pub outbound_peers_with_protect: Arc<AtomicUsize>,
}

fn is_outbound(nc: &NetworkContext, peer: PeerId) -> Option<bool> {
    nc.session_info(peer)
        .map(|session_info| session_info.originated)
}

impl<C, P> Clone for Synchronizer<C, P>
where
    C: ChainProvider,
    P: PowEngine,
{
    fn clone(&self) -> Synchronizer<C, P> {
        Synchronizer {
            chain: Arc::clone(&self.chain),
            pow: Arc::clone(&self.pow),
            status_map: Arc::clone(&self.status_map),
            header_map: Arc::clone(&self.header_map),
            best_known_header: Arc::clone(&self.best_known_header),
            n_sync: Arc::clone(&self.n_sync),
            peers: Arc::clone(&self.peers),
            config: Arc::clone(&self.config),
            orphan_block_pool: Arc::clone(&self.orphan_block_pool),
            outbound_peers_with_protect: Arc::clone(&self.outbound_peers_with_protect),
        }
    }
}

impl<C, P> Synchronizer<C, P>
where
    C: ChainProvider,
    P: PowEngine,
{
    pub fn new(chain: &Arc<C>, pow: &Arc<P>, config: Config) -> Synchronizer<C, P> {
        let TipHeader {
            header,
            total_difficulty,
            ..
        } = chain.tip_header().read().clone();
        let best_known_header = HeaderView::new(header, total_difficulty);
        let orphan_block_limit = config.orphan_block_limit;

        Synchronizer {
            config: Arc::new(config),
            chain: Arc::clone(chain),
            pow: Arc::clone(pow),
            peers: Arc::new(Peers::default()),
            orphan_block_pool: Arc::new(OrphanBlockPool::with_capacity(orphan_block_limit)),
            best_known_header: Arc::new(RwLock::new(best_known_header)),
            status_map: Arc::new(RwLock::new(HashMap::new())),
            header_map: Arc::new(RwLock::new(HashMap::new())),
            n_sync: Arc::new(AtomicUsize::new(0)),
            outbound_peers_with_protect: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn process(&self, nc: &NetworkContext, peer: PeerId, message: SyncMessage) {
        match message.payload_type() {
            SyncPayload::GetHeaders => {
                GetHeadersProcess::new(&message.payload_as_get_headers().unwrap(), self, peer, nc)
                    .execute()
            }
            SyncPayload::Headers => {
                HeadersProcess::new(&message.payload_as_headers().unwrap(), self, peer, nc)
                    .execute()
            }
            SyncPayload::GetBlocks => {
                GetBlocksProcess::new(&message.payload_as_get_blocks().unwrap(), self, peer, nc)
                    .execute()
            }
            SyncPayload::Block => {
                BlockProcess::new(&message.payload_as_block().unwrap(), self, peer, nc).execute()
            }
            SyncPayload::NONE => {}
        }
    }

    pub fn get_block_status(&self, hash: &H256) -> BlockStatus {
        let guard = self.status_map.upgradable_read();
        match guard.get(hash).cloned() {
            Some(s) => s,
            None => if self.chain.block_header(hash).is_some() {
                let mut write_guard = RwLockUpgradableReadGuard::upgrade(guard);
                write_guard.insert(*hash, BlockStatus::BLOCK_HAVE_MASK);
                BlockStatus::BLOCK_HAVE_MASK
            } else {
                BlockStatus::UNKNOWN
            },
        }
    }

    pub fn peers(&self) -> Arc<Peers> {
        Arc::clone(&self.peers)
    }

    pub fn insert_block_status(&self, hash: H256, status: BlockStatus) {
        self.status_map.write().insert(hash, status);
    }

    pub fn best_known_header(&self) -> HeaderView {
        self.best_known_header.read().clone()
    }

    pub fn is_initial_block_download(&self) -> bool {
        now_ms().saturating_sub(self.chain.tip_header().read().header.timestamp) > MAX_TIP_AGE
    }

    pub fn get_headers_sync_timeout(&self, tip: &IndexedHeader) -> u64 {
        HEADERS_DOWNLOAD_TIMEOUT_BASE
            + HEADERS_DOWNLOAD_TIMEOUT_PER_HEADER
                * (now_ms().saturating_sub(tip.header.timestamp) / POW_SPACE)
    }

    pub fn mark_block_stored(&self, hash: H256) {
        self.status_map
            .write()
            .entry(hash)
            .and_modify(|status| *status = BlockStatus::BLOCK_HAVE_MASK)
            .or_insert_with(|| BlockStatus::BLOCK_HAVE_MASK);
    }

    pub fn tip_header(&self) -> IndexedHeader {
        self.chain.tip_header().read().header.clone()
    }

    pub fn get_locator(&self, start: &IndexedHeader) -> Vec<H256> {
        let mut step = 1;
        let mut locator = Vec::with_capacity(32);
        let mut index = start.number;
        let base = start.hash();
        loop {
            let header = self
                .get_ancestor(&base, index)
                .expect("index calculated in get_locator");
            locator.push(header.hash());

            if locator.len() >= 10 {
                step <<= 1;
            }

            if index < step {
                // always include genesis hash
                if index != 0 {
                    locator.push(self.chain.genesis_hash());
                }
                break;
            }
            index -= step;
        }
        locator
    }

    pub fn locate_latest_common_block(
        &self,
        _hash_stop: &H256,
        locator: &[H256],
    ) -> Option<BlockNumber> {
        if locator.is_empty() {
            return None;
        }

        if locator.last().expect("empty checked") != &self.chain.genesis_hash() {
            return None;
        }

        // iterator are lazy
        let (index, latest_common) = locator
            .iter()
            .enumerate()
            .map(|(index, hash)| (index, self.chain.block_number(hash)))
            .find(|(_index, number)| number.is_some())
            .expect("locator last checked");

        if index == 0 || latest_common == Some(0) {
            return latest_common;
        }

        if let Some(header) = locator
            .get(index - 1)
            .and_then(|hash| self.chain.block_header(&hash))
        {
            let mut block_hash = header.parent_hash;
            loop {
                let block_header = match self.chain.block_header(&block_hash) {
                    None => break latest_common,
                    Some(block_header) => block_header,
                };

                if let Some(block_number) = self.chain.block_number(&block_hash) {
                    return Some(block_number);
                }

                block_hash = block_header.parent_hash;
            }
        } else {
            latest_common
        }
    }

    pub fn get_header_view(&self, hash: &H256) -> Option<HeaderView> {
        self.header_map.read().get(hash).cloned().or_else(|| {
            self.chain.block_header(&hash).and_then(|header| {
                self.chain
                    .block_ext(&hash)
                    .map(|block_ext| HeaderView::new(header, block_ext.total_difficulty))
            })
        })
    }

    pub fn get_header(&self, hash: &H256) -> Option<IndexedHeader> {
        self.header_map
            .read()
            .get(hash)
            .map(|view| &view.header)
            .cloned()
            .or_else(|| self.chain.block_header(&hash))
    }

    pub fn get_block(&self, hash: &H256) -> Option<IndexedBlock> {
        self.chain.block(hash)
    }

    pub fn get_ancestor(&self, base: &H256, number: BlockNumber) -> Option<IndexedHeader> {
        if let Some(header) = self.get_header(base) {
            let mut n_number = header.number;
            let mut index_walk = header;
            if number > n_number {
                return None;
            }

            while n_number > number {
                if let Some(header) = self.get_header(&index_walk.parent_hash) {
                    index_walk = header;
                    n_number -= 1;
                } else {
                    return None;
                }
            }
            return Some(index_walk);
        }
        None
    }

    pub fn get_locator_response(
        &self,
        block_number: BlockNumber,
        hash_stop: &H256,
    ) -> Vec<IndexedHeader> {
        let tip_number = self.tip_header().number;
        let max_height = cmp::min(
            block_number + 1 + MAX_HEADERS_LEN as BlockNumber,
            tip_number + 1,
        );
        (block_number + 1..max_height)
            .filter_map(|block_number| self.chain.block_hash(block_number))
            .take_while(|block_hash| block_hash != hash_stop)
            .filter_map(|block_hash| self.chain.block_header(&block_hash))
            .collect()
    }

    pub fn insert_header_view(&self, header: &IndexedHeader, peer: PeerId) {
        if let Some(parent_view) = self.get_header_view(&header.parent_hash) {
            let total_difficulty = parent_view.total_difficulty + header.difficulty;
            let header_view = {
                let best_known_header = self.best_known_header.upgradable_read();
                let header_view = HeaderView::new(header.clone(), total_difficulty);

                if total_difficulty > best_known_header.total_difficulty
                    || (total_difficulty == best_known_header.total_difficulty
                        && header.hash() < best_known_header.header.hash())
                {
                    let mut best_known_header =
                        RwLockUpgradableReadGuard::upgrade(best_known_header);
                    *best_known_header = header_view.clone();
                }
                header_view
            };

            self.peers.new_header_received(peer, &header_view);

            let mut header_map = self.header_map.write();
            header_map.insert(header.hash(), header_view);
        }
    }

    // If the peer reorganized, our previous last_common_header may not be an ancestor
    // of its current best_known_header. Go back enough to fix that.
    pub fn last_common_ancestor(
        &self,
        last_common_header: &IndexedHeader,
        best_known_header: &IndexedHeader,
    ) -> Option<IndexedHeader> {
        debug_assert!(best_known_header.number >= last_common_header.number);

        let mut m_right =
            try_option!(self.get_ancestor(&best_known_header.hash(), last_common_header.number));

        if &m_right == last_common_header {
            return Some(m_right);
        }

        let mut m_left = try_option!(self.get_header(&last_common_header.hash()));
        debug_assert!(m_right.header.number == m_left.header.number);

        while m_left != m_right {
            m_left =
                try_option!(self.get_ancestor(&m_left.header.hash(), m_left.header.number - 1));
            m_right =
                try_option!(self.get_ancestor(&m_right.header.hash(), m_right.header.number - 1));
        }
        Some(m_left)
    }

    //TODO: process block which we don't request
    #[cfg_attr(feature = "cargo-clippy", allow(single_match))]
    pub fn process_new_block(&self, peer: PeerId, block: IndexedBlock) {
        match self.get_block_status(&block.hash()) {
            BlockStatus::VALID_MASK => {
                self.insert_new_block(peer, block);
            }
            status => {
                debug!(target: "sync", "[Synchronizer] process_new_block unexpect status {:?}", status);
            }
        }
    }

    fn accept_block(&self, peer: PeerId, block: &IndexedBlock) -> Result<(), AcceptBlockError> {
        BlockVerifier::new(block, &self.chain, &self.pow).verify()?;
        self.chain.process_block(&block)?;
        self.mark_block_stored(block.hash());
        self.peers.set_last_common_header(peer, &block.header);
        Ok(())
    }

    //FIXME: guarantee concurrent block process
    fn insert_new_block(&self, peer: PeerId, block: IndexedBlock) {
        if self.chain.output_root(&block.header.parent_hash).is_some() {
            let accept_ret = self.accept_block(peer, &block);
            if accept_ret.is_ok() {
                let pre_orphan_block = self
                    .orphan_block_pool
                    .remove_blocks_by_parent(&block.hash());
                for block in pre_orphan_block {
                    if self.chain.output_root(&block.header.parent_hash).is_some() {
                        let ret = self.accept_block(peer, &block);
                        if ret.is_err() {
                            debug!(
                                target: "sync", "[Synchronizer] accept_block {:#?} error {:?}",
                                block,
                                ret.unwrap_err()
                            );
                        }
                    } else {
                        debug!(
                            target: "sync", "[Synchronizer] insert_orphan_block {:#?}------------{:?}",
                            block.number(),
                            block.hash()
                        );
                        self.orphan_block_pool.insert(block);
                    }
                }
            } else {
                debug!(
                    target: "sync", "[Synchronizer] accept_block {:#?} error {:?}",
                    block,
                    accept_ret.unwrap_err()
                )
            }
        } else {
            debug!(
                target: "sync", "[Synchronizer] insert_orphan_block {:#?}------------{:?}",
                block.number(),
                block.hash()
            );
            self.orphan_block_pool.insert(block);
        }

        debug!(target: "sync", "[Synchronizer] insert_new_block finish");
    }

    pub fn get_blocks_to_fetch(&self, peer: PeerId) -> Option<Vec<H256>> {
        BlockFetcher::new(&self, peer).fetch()
    }

    fn on_connected(&self, nc: &NetworkContext, peer: PeerId) {
        let tip = self.tip_header();
        let timeout = self.get_headers_sync_timeout(&tip);

        let protect_outbound = is_outbound(nc, peer).unwrap_or_else(|| false)
            && self.outbound_peers_with_protect.load(Ordering::Acquire)
                < MAX_OUTBOUND_PEERS_TO_PROTECT_FROM_DISCONNECT;

        if protect_outbound {
            self.outbound_peers_with_protect
                .fetch_add(1, Ordering::Release);
        }

        self.peers.on_connected(peer, timeout, protect_outbound);
        self.n_sync.fetch_add(1, Ordering::Release);
        self.send_getheaders_to_peer(nc, peer, &tip);
    }

    pub fn send_getheaders_to_peer(&self, nc: &NetworkContext, peer: PeerId, tip: &IndexedHeader) {
        let locator_hash = self.get_locator(tip);

        let builder = &mut FlatBufferBuilder::new();
        {
            let block_locator_hashes = Some(
                builder.create_vector(
                    &locator_hash
                        .iter()
                        .flat_map(|hash| hash.iter().cloned())
                        .collect::<Vec<_>>(),
                ),
            );
            let payload = Some(
                GetHeaders::create(
                    builder,
                    &GetHeadersArgs {
                        version: 0,
                        hash_stop: None, // TODO PENDING hash_stop
                        block_locator_hashes,
                    },
                ).as_union_value(),
            );
            let payload_type = SyncPayload::GetHeaders;
            let message = SyncMessage::create(
                builder,
                &SyncMessageArgs {
                    payload_type,
                    payload,
                },
            );
            builder.finish(message, None);
        }

        nc.send(peer, 0, builder.finished_data().to_vec());
    }

    fn send_getheaders_to_all(&self, nc: &NetworkContext) {
        let peers: Vec<PeerId> = self
            .peers
            .state
            .read()
            .iter()
            .filter(|(_, state)| state.sync_started)
            .map(|(peer_id, _)| peer_id)
            .cloned()
            .collect();
        debug!(target: "sync", "send_getheaders to peers= {:?}", &peers);
        let tip = self.tip_header();
        for peer in peers {
            self.send_getheaders_to_peer(nc, peer, &tip);
        }
    }

    fn find_blocks_to_fetch(&self, nc: &NetworkContext) {
        let peers: Vec<PeerId> = self
            .peers
            .state
            .read()
            .iter()
            .filter(|(_, state)| state.sync_started)
            .map(|(peer_id, _)| peer_id)
            .cloned()
            .collect();

        debug!(target: "sync", "poll find_blocks_to_fetch select peers");
        for peer in peers {
            if let Some(v_fetch) = self.get_blocks_to_fetch(peer) {
                self.send_getblocks(&v_fetch, nc, peer);
            }
        }
    }

    fn send_getblocks(&self, v_fetch: &[H256], nc: &NetworkContext, peer: PeerId) {
        let builder = &mut FlatBufferBuilder::new();
        {
            let block_hashes = Some(
                builder.create_vector(
                    &v_fetch
                        .iter()
                        .flat_map(|hash| hash.iter().cloned())
                        .collect::<Vec<_>>(),
                ),
            );
            let payload =
                Some(GetBlocks::create(builder, &GetBlocksArgs { block_hashes }).as_union_value());
            let payload_type = SyncPayload::GetBlocks;
            let message = SyncMessage::create(
                builder,
                &SyncMessageArgs {
                    payload_type,
                    payload,
                },
            );
            builder.finish(message, None);
        }

        nc.send(peer, 0, builder.finished_data().to_vec());

        debug!(target: "sync", "send_getblocks len={:?} to peer={:?}", v_fetch.len() , peer);
    }
}

impl<C, P> NetworkProtocolHandler for Synchronizer<C, P>
where
    C: ChainProvider + 'static,
    P: PowEngine + 'static,
{
    fn initialize(&self, nc: Box<NetworkContext>) {
        // NOTE: 100ms is what bitcoin use.
        let _ = nc.register_timer(SEND_GET_HEADERS_TOKEN, Duration::from_millis(100));
        let _ = nc.register_timer(BLOCK_FETCH_TOKEN, Duration::from_millis(100));
    }

    fn read(&self, nc: Box<NetworkContext>, peer: &PeerId, _packet_id: u8, data: &[u8]) {
        let data = data.to_owned();
        let synchronizer = self.clone();
        let peer = *peer;
        tokio::spawn(lazy(move || {
            // TODO use flatbuffers verifier
            let msg = get_root::<SyncMessage>(&data);
            debug!(target: "sync", "msg {:?}", msg.payload_type());
            synchronizer.process(&nc, peer, msg);
            future::ok(())
        }));
    }

    fn connected(&self, nc: Box<NetworkContext>, peer: &PeerId) {
        let synchronizer = self.clone();
        let peer = *peer;
        tokio::spawn(lazy(move || {
            if synchronizer.n_sync.load(Ordering::Acquire) == 0
                || !synchronizer.is_initial_block_download()
            {
                debug!(target: "sync", "init_getheaders peer={:?} connected", peer);
                synchronizer.on_connected(nc.as_ref(), peer);
            }
            future::ok(())
        }));
    }

    fn disconnected(&self, _nc: Box<NetworkContext>, peer: &PeerId) {
        let synchronizer = self.clone();
        let peer = *peer;
        tokio::spawn(lazy(move || {
            info!(target: "sync", "peer={} SyncProtocol.disconnected", peer);
            synchronizer.peers.disconnected(peer);
            future::ok(())
        }));
    }

    fn timeout(&self, nc: Box<NetworkContext>, token: TimerToken) {
        let synchronizer = self.clone();
        tokio::spawn(lazy(move || {
            if !synchronizer.peers.state.read().is_empty() {
                match token as usize {
                    SEND_GET_HEADERS_TOKEN => {
                        synchronizer.send_getheaders_to_all(&nc);
                    }
                    BLOCK_FETCH_TOKEN => {
                        synchronizer.find_blocks_to_fetch(&nc);
                    }
                    _ => unreachable!(),
                }
            } else {
                debug!(target: "sync", "no peers connected");
            }
            future::ok(())
        }));
    }
}

#[cfg(test)]
mod tests {
    extern crate env_logger;

    use self::block_process::BlockProcess;
    use self::headers_process::HeadersProcess;
    use super::*;
    use bigint::U256;
    use ckb_chain::chain::Chain;
    use ckb_chain::consensus::Consensus;
    use ckb_chain::index::ChainIndex;
    use ckb_chain::store::ChainKVStore;
    use ckb_chain::{DummyPowEngine, COLUMNS};
    use ckb_notify::{Event, Notify, MINER_SUBSCRIBER};
    use ckb_protocol::{
        build_block_args, build_header_args, get_root_as_sync_message, Block, Header as FbsHeader,
        Headers, HeadersArgs, SyncMessage, SyncMessageArgs, SyncPayload,
    };
    use core::header::{Header, RawHeader, Seal};
    use core::transaction::{CellInput, CellOutput, IndexedTransaction, Transaction, VERSION};
    use core::uncle::uncles_hash;
    use crossbeam_channel;
    use crossbeam_channel::Receiver;
    use db::memorydb::MemoryKeyValueDB;
    use flatbuffers::FlatBufferBuilder;
    use merkle_root::merkle_root;
    use network::{
        Error as NetworkError, NetworkContext, PacketId, PeerId, ProtocolId, SessionInfo, Severity,
        TimerToken,
    };
    use std::time::Duration;

    #[test]
    fn test_block_status() {
        let status1 = BlockStatus::FAILED_VALID;
        let status2 = BlockStatus::FAILED_CHILD;
        assert!((status1 & BlockStatus::FAILED_MASK) == status1);
        assert!((status2 & BlockStatus::FAILED_MASK) == status2);
    }

    fn gen_chain(consensus: &Consensus, notify: Notify) -> Chain<ChainKVStore<MemoryKeyValueDB>> {
        let db = MemoryKeyValueDB::open(COLUMNS as usize);
        let store = ChainKVStore::new(db);
        let chain = Chain::init(store, consensus.clone(), notify).unwrap();
        chain
    }

    fn create_cellbase(number: BlockNumber) -> IndexedTransaction {
        let inputs = vec![CellInput::new_cellbase_input(number)];
        let outputs = vec![CellOutput::new(0, vec![], H256::from(0))];
        Transaction::new(VERSION, Vec::new(), inputs, outputs).into()
    }

    fn gen_block(parent_header: IndexedHeader, difficulty: U256, nonce: u64) -> IndexedBlock {
        let now = 1 + parent_header.timestamp;
        let number = parent_header.number + 1;
        let cellbase = create_cellbase(number);
        let cellbase_id = cellbase.hash();
        let txs = vec![cellbase];
        let txs_hash = vec![cellbase_id];
        let txs_commit = merkle_root(txs_hash.as_slice());
        let uncles = vec![];
        let uncles_hash = uncles_hash(&uncles);
        let header = Header {
            raw: RawHeader {
                number,
                cellbase_id,
                uncles_hash,
                txs_commit,
                txs_proposal: H256::zero(),
                version: 0,
                parent_hash: parent_header.hash(),
                timestamp: now,
                difficulty: difficulty,
            },
            seal: Seal {
                nonce,
                proof: Default::default(),
            },
        };

        IndexedBlock {
            uncles,
            header: header.into(),
            commit_transactions: txs,
            proposal_transactions: vec![],
        }
    }

    fn insert_block<CS: ChainIndex>(chain: &Chain<CS>, nonce: u64, number: BlockNumber) {
        let parent = chain
            .block_header(&chain.block_hash(number - 1).unwrap())
            .unwrap();
        let now = 1 + parent.timestamp;
        let difficulty = chain.calculate_difficulty(&parent).unwrap();
        let cellbase = create_cellbase(number);
        let cellbase_id = cellbase.hash();
        let txs = vec![cellbase];
        let txs_hash: Vec<H256> = txs.iter().map(|t| t.hash()).collect();
        let txs_commit = merkle_root(txs_hash.as_slice());

        let uncles = vec![];
        let uncles_hash = uncles_hash(&uncles);
        let header = Header {
            raw: RawHeader {
                number,
                txs_commit,
                cellbase_id,
                uncles_hash,
                version: 0,
                txs_proposal: H256::zero(),
                parent_hash: parent.hash(),
                timestamp: now,
                difficulty: difficulty,
            },
            seal: Seal {
                nonce,
                proof: Default::default(),
            },
        };

        let block = IndexedBlock {
            header: header.into(),
            uncles: vec![],
            commit_transactions: txs,
            proposal_transactions: vec![],
        };
        chain.process_block(&block).expect("process block ok");
    }

    #[test]
    fn test_locator() {
        let config = Consensus::default();
        let chain = Arc::new(gen_chain(&config, Notify::default()));

        let num = 200;
        let index = [
            199, 198, 197, 196, 195, 194, 193, 192, 191, 190, 188, 184, 176, 160, 128, 64,
        ];

        for i in 1..num {
            insert_block(&chain, i, i);
        }

        let synchronizer =
            Synchronizer::new(&chain, &Arc::new(DummyPowEngine::new()), Config::default());

        let locator = synchronizer.get_locator(&chain.tip_header().read().header);

        let mut expect = Vec::new();

        for i in index.iter() {
            expect.push(chain.block_hash(*i).unwrap());
        }
        //genesis_hash must be the last one
        expect.push(chain.genesis_hash());

        assert_eq!(expect, locator);
    }

    #[test]
    fn test_locate_latest_common_block() {
        let config = Consensus::default();
        let chain1 = Arc::new(gen_chain(&config, Notify::default()));
        let chain2 = Arc::new(gen_chain(&config, Notify::default()));
        let num = 200;

        for i in 1..num {
            insert_block(&chain1, i, i);
        }

        for i in 1..num {
            insert_block(&chain2, i + 1, i);
        }

        let pow_engine = Arc::new(DummyPowEngine::new());

        let synchronizer1 = Synchronizer::new(&chain1, &pow_engine, Config::default());

        let synchronizer2 = Synchronizer::new(&chain2, &pow_engine, Config::default());

        let locator1 = synchronizer1.get_locator(&chain1.tip_header().read().header);

        let latest_common = synchronizer2.locate_latest_common_block(&H256::zero(), &locator1[..]);

        assert_eq!(latest_common, Some(0));

        let chain3 = Arc::new(gen_chain(&config, Notify::default()));

        for i in 1..num {
            let j = if i > 192 { i + 1 } else { i };
            insert_block(&chain3, j, i);
        }

        let synchronizer3 = Synchronizer::new(&chain3, &pow_engine, Config::default());

        let latest_common3 = synchronizer3.locate_latest_common_block(&H256::zero(), &locator1[..]);
        assert_eq!(latest_common3, Some(192));
    }

    #[test]
    fn test_locate_latest_common_block2() {
        let config = Consensus::default();
        let chain1 = Arc::new(gen_chain(&config, Notify::default()));
        let chain2 = Arc::new(gen_chain(&config, Notify::default()));
        let block_number = 200;

        let mut blocks: Vec<IndexedBlock> = Vec::new();
        let mut parent = config.genesis_block().header.clone();
        for i in 1..block_number {
            let difficulty = chain1.calculate_difficulty(&parent).unwrap();
            let new_block = gen_block(parent, difficulty, i);
            blocks.push(new_block.clone());

            chain1.process_block(&new_block).expect("process block ok");

            chain2.process_block(&new_block).expect("process block ok");
            parent = new_block.header;
        }

        parent = blocks[150].header.clone();
        let fork = parent.number;
        for i in 1..block_number + 1 {
            let difficulty = chain2.calculate_difficulty(&parent).unwrap();
            let new_block = gen_block(parent, difficulty, i + 100);
            chain2.process_block(&new_block).expect("process block ok");
            parent = new_block.header;
        }

        let pow_engine = Arc::new(DummyPowEngine::new());

        let synchronizer1 = Synchronizer::new(&chain1, &pow_engine, Config::default());

        let synchronizer2 = Synchronizer::new(&chain2, &pow_engine, Config::default());

        let locator1 = synchronizer1.get_locator(&chain1.tip_header().read().header);

        let latest_common = synchronizer2
            .locate_latest_common_block(&H256::zero(), &locator1[..])
            .unwrap();

        assert_eq!(
            chain1.block_hash(fork).unwrap(),
            chain2.block_hash(fork).unwrap()
        );
        assert!(chain1.block_hash(fork + 1).unwrap() != chain2.block_hash(fork + 1).unwrap());
        assert_eq!(
            chain1.block_hash(latest_common).unwrap(),
            chain1.block_hash(fork).unwrap()
        );
    }

    #[test]
    fn test_get_ancestor() {
        let config = Consensus::default();
        let chain = Arc::new(gen_chain(&config, Notify::default()));
        let num = 200;

        for i in 1..num {
            insert_block(&chain, i, i);
        }

        let synchronizer =
            Synchronizer::new(&chain, &Arc::new(DummyPowEngine::new()), Config::default());

        let header = synchronizer.get_ancestor(&chain.tip_header().read().header.hash(), 100);
        let tip = synchronizer.get_ancestor(&chain.tip_header().read().header.hash(), 199);
        let noop = synchronizer.get_ancestor(&chain.tip_header().read().header.hash(), 200);
        assert!(tip.is_some());
        assert!(header.is_some());
        assert!(noop.is_none());
        assert_eq!(tip.unwrap(), chain.tip_header().read().header.clone());
        assert_eq!(
            header.unwrap(),
            chain.block_header(&chain.block_hash(100).unwrap()).unwrap()
        );
    }

    #[test]
    fn test_process_new_block() {
        let config = Consensus::default();
        let chain1 = Arc::new(gen_chain(&config, Notify::default()));
        let chain2 = Arc::new(gen_chain(&config, Notify::default()));
        let block_number = 2000;

        let mut blocks: Vec<IndexedBlock> = Vec::new();
        let mut parent = chain1.block_header(&chain1.block_hash(0).unwrap()).unwrap();
        for i in 1..block_number {
            let difficulty = chain1.calculate_difficulty(&parent).unwrap();
            let new_block = gen_block(parent, difficulty, i + 100);
            chain1.process_block(&new_block).expect("process block ok");
            blocks.push(new_block.clone());
            parent = new_block.header;
        }

        let synchronizer =
            Synchronizer::new(&chain2, &Arc::new(DummyPowEngine::new()), Config::default());

        blocks.clone().into_iter().for_each(|block| {
            synchronizer.insert_new_block(0, block);
        });

        assert_eq!(
            blocks.last().unwrap().header,
            chain2.tip_header().read().header
        );
    }

    #[test]
    fn test_get_locator_response() {
        let config = Consensus::default();
        let chain = Arc::new(gen_chain(&config, Notify::default()));
        let block_number = 200;

        let mut blocks: Vec<IndexedBlock> = Vec::new();
        let mut parent = chain.block_header(&chain.block_hash(0).unwrap()).unwrap();
        for i in 1..block_number + 1 {
            let difficulty = chain.calculate_difficulty(&parent).unwrap();
            let new_block = gen_block(parent, difficulty, i + 100);
            blocks.push(new_block.clone());
            chain.process_block(&new_block).expect("process block ok");
            parent = new_block.header;
        }

        let synchronizer =
            Synchronizer::new(&chain, &Arc::new(DummyPowEngine::new()), Config::default());

        let headers = synchronizer.get_locator_response(180, &H256::zero());

        assert_eq!(headers.first().unwrap(), &blocks[180].header);
        assert_eq!(headers.last().unwrap(), &blocks[199].header);

        for window in headers.windows(2) {
            if let [parent, header] = &window {
                assert_eq!(header.parent_hash, parent.hash());
            }
        }
    }

    #[derive(Clone)]
    struct DummyNetworkContext {}

    impl NetworkContext for DummyNetworkContext {
        /// Send a packet over the network to another peer.
        fn send(&self, _peer: PeerId, _packet_id: PacketId, _data: Vec<u8>) {}

        /// Send a packet over the network to another peer using specified protocol.
        fn send_protocol(
            &self,
            _protocol: ProtocolId,
            _peer: PeerId,
            _packet_id: PacketId,
            _data: Vec<u8>,
        ) {
        }

        /// Respond to a current network message. Panics if no there is no packet in the context. If the session is expired returns nothing.
        fn respond(&self, _packet_id: PacketId, _data: Vec<u8>) {}

        /// Report peer. Depending on the report, peer may be disconnected and possibly banned.
        fn report_peer(&self, _peer: PeerId, _reason: Severity) {}

        /// Check if the session is still active.
        fn is_expired(&self) -> bool {
            false
        }

        /// Register a new IO timer. 'IoHandler::timeout' will be called with the token.
        fn register_timer(&self, _token: TimerToken, _delay: Duration) -> Result<(), NetworkError> {
            Ok(())
        }

        /// Returns peer identification string
        fn peer_client_version(&self, _peer: PeerId) -> String {
            "unknown".to_string()
        }

        /// Returns information on p2p session
        fn session_info(&self, _peer: PeerId) -> Option<SessionInfo> {
            None
        }

        /// Returns max version for a given protocol.
        fn protocol_version(&self, _protocol: ProtocolId, _peer: PeerId) -> Option<u8> {
            None
        }

        /// Returns this object's subprotocol name.
        fn subprotocol_name(&self) -> ProtocolId {
            [1, 1, 1]
        }
    }

    #[test]
    fn test_sync_process() {
        let _ = env_logger::try_init();
        let config = Consensus::default();
        let notify = Notify::default();
        let chain1 = Arc::new(gen_chain(&config, notify.clone()));
        let chain2 = Arc::new(gen_chain(&config, notify.clone()));
        let num = 200;

        for i in 1..num {
            insert_block(&chain1, i, i);
        }
        let synchronizer1 =
            Synchronizer::new(&chain1, &Arc::new(DummyPowEngine::new()), Config::default());

        let locator1 = synchronizer1.get_locator(&chain1.tip_header().read().header);

        for i in 1..num + 1 {
            let j = if i > 192 { i + 1 } else { i };
            insert_block(&chain2, j, i);
        }

        let synchronizer2 =
            Synchronizer::new(&chain2, &Arc::new(DummyPowEngine::new()), Config::default());
        let latest_common = synchronizer2.locate_latest_common_block(&H256::zero(), &locator1[..]);
        assert_eq!(latest_common, Some(192));

        let headers = synchronizer2.get_locator_response(192, &H256::zero());

        assert_eq!(
            headers.first().unwrap().hash(),
            chain2.block_hash(193).unwrap()
        );
        assert_eq!(
            headers.last().unwrap().hash(),
            chain2.block_hash(200).unwrap()
        );

        let builder = &mut FlatBufferBuilder::new();

        {
            let vec = headers
                .iter()
                .map(|header| {
                    let header_args = build_header_args(builder, header);
                    FbsHeader::create(builder, &header_args)
                }).collect::<Vec<_>>();

            let headers = Some(builder.create_vector(&vec));
            let payload = Headers::create(builder, &HeadersArgs { headers });
            let message = SyncMessage::create(
                builder,
                &SyncMessageArgs {
                    payload_type: SyncPayload::Headers,
                    payload: Some(payload.as_union_value()),
                },
            );
            builder.finish(message, None);
        }

        let peer = 1usize;
        HeadersProcess::new(
            &get_root_as_sync_message(builder.finished_data())
                .payload_as_headers()
                .unwrap(),
            &synchronizer1,
            peer,
            &DummyNetworkContext {},
        ).execute();

        let best_known_header = synchronizer1.peers.best_known_header(peer);

        assert_eq!(
            best_known_header.clone().map(|h| h.header),
            headers.last().cloned()
        );

        let blocks_to_fetch = synchronizer1.get_blocks_to_fetch(peer).unwrap();

        assert_eq!(
            blocks_to_fetch.first().unwrap(),
            &chain2.block_hash(193).unwrap()
        );
        assert_eq!(
            blocks_to_fetch.last().unwrap(),
            &chain2.block_hash(200).unwrap()
        );

        let mut fetched_blocks = Vec::new();
        for block_hash in &blocks_to_fetch {
            fetched_blocks.push(chain2.block(block_hash).unwrap());
        }

        let (tx, rx) = crossbeam_channel::unbounded();
        notify.register_transaction_subscriber(MINER_SUBSCRIBER, tx.clone());
        notify.register_tip_subscriber(MINER_SUBSCRIBER, tx.clone());

        pub struct TryIter<'a, T: 'a> {
            pub inner: &'a Receiver<T>,
        }

        impl<'a, T> Iterator for TryIter<'a, T> {
            type Item = T;

            fn next(&mut self) -> Option<T> {
                self.inner.try_recv()
            }
        }

        for block in &fetched_blocks {
            let builder = &mut FlatBufferBuilder::new();
            let block_args = build_block_args(builder, &block);
            let payload = Block::create(builder, &block_args);
            let message = SyncMessage::create(
                builder,
                &SyncMessageArgs {
                    payload_type: SyncPayload::Block,
                    payload: Some(payload.as_union_value()),
                },
            );
            builder.finish(message, None);

            BlockProcess::new(
                &get_root_as_sync_message(builder.finished_data())
                    .payload_as_block()
                    .unwrap(),
                &synchronizer1,
                peer,
                &DummyNetworkContext {},
            ).execute();
        }

        let mut iter = TryIter { inner: &rx };
        assert_eq!(
            &synchronizer1
                .peers
                .last_common_headers
                .read()
                .get(&peer)
                .unwrap()
                .hash(),
            blocks_to_fetch.last().unwrap()
        );

        assert_eq!(
            iter.next(),
            Some(Event::NewTip(Arc::new(fetched_blocks[7].clone())))
        );
    }
}