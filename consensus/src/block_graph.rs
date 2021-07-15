// Copyright (c) 2021 MASSA LABS <info@massa.net>

//! All information concerning blocks, the block graph and cliques is managed here.
use super::config::ConsensusConfig;
use crate::{
    error::ConsensusError,
    ledger::{
        Ledger, LedgerChange, LedgerData, LedgerExport, LedgerSubset, OperationLedgerInterface,
    },
    pos::{OperationRollInterface, ProofOfStake, RollCounts, RollUpdate, RollUpdates},
};
use crypto::hash::Hash;
use crypto::signature::derive_public_key;
use models::{
    array_from_slice, u8_from_slice, with_serialization_context, Address, Block, BlockHeader,
    BlockHeaderContent, BlockId, DeserializeCompact, DeserializeVarInt, ModelsError, OperationId,
    OperationSearchResult, OperationSearchResultBlockStatus, OperationSearchResultStatus,
    SerializeCompact, SerializeVarInt, Slot, ADDRESS_SIZE_BYTES, BLOCK_ID_SIZE_BYTES,
};
use serde::{Deserialize, Serialize};
use std::mem;
use std::{
    collections::{hash_map, BTreeSet, HashMap, HashSet, VecDeque},
    convert::TryFrom,
};
use std::{convert::TryInto, usize};

#[derive(Debug, Clone)]
enum HeaderOrBlock {
    Header(BlockHeader),
    Block(Block, HashMap<OperationId, (usize, u64)>), // (index, validity end period)
}

impl HeaderOrBlock {
    /// Gets slot for that header or block
    pub fn get_slot(&self) -> Slot {
        match self {
            HeaderOrBlock::Header(header) => header.content.slot,
            HeaderOrBlock::Block(block, _) => block.header.content.slot,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveBlock {
    pub block: Block,
    pub parents: Vec<(BlockId, u64)>, // one (hash, period) per thread ( if not genesis )
    pub children: Vec<HashMap<BlockId, u64>>, // one HashMap<hash, period> per thread (blocks that need to be kept)
    pub dependencies: HashSet<BlockId>,       // dependencies required for validity check
    pub descendants: HashSet<BlockId>,
    pub is_final: bool,
    pub block_ledger_change: Vec<HashMap<Address, LedgerChange>>,
    pub operation_set: HashMap<OperationId, (usize, u64)>, // index in the block, end of validity period
    pub addresses_to_operations: HashMap<Address, HashSet<OperationId>>,
    pub roll_updates: RollUpdates, // Address -> RollUpdate
}

impl ActiveBlock {
    /// Computes the fitness of the block
    fn fitness(&self) -> u64 {
        /*
        self.block
            .header
            .endorsements
            .iter()
            .fold(1, |acc, endorsement| match endorsement {
                Some(_) => acc + 1,
                None => acc,
            })
        */
        1
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportActiveBlock {
    pub block: Block,
    pub parents: Vec<(BlockId, u64)>, // one (hash, period) per thread ( if not genesis )
    pub children: Vec<Vec<(BlockId, u64)>>, // one HashMap<hash, period> per thread (blocks that need to be kept)
    pub dependencies: Vec<BlockId>,         // dependencies required for validity check
    pub is_final: bool,
    pub block_ledger_change: Vec<Vec<(Address, LedgerChange)>>,
    pub roll_updates: Vec<(Address, RollUpdate)>,
}

impl From<ActiveBlock> for ExportActiveBlock {
    fn from(block: ActiveBlock) -> Self {
        ExportActiveBlock {
            block: block.block,
            parents: block.parents,
            children: block
                .children
                .into_iter()
                .map(|map| map.into_iter().collect())
                .collect(),
            dependencies: block.dependencies.into_iter().collect(),
            is_final: block.is_final,
            block_ledger_change: block
                .block_ledger_change
                .into_iter()
                .map(|map| map.into_iter().collect())
                .collect(),
            roll_updates: block.roll_updates.0.into_iter().collect(),
        }
    }
}

impl<'a> TryFrom<ExportActiveBlock> for ActiveBlock {
    fn try_from(block: ExportActiveBlock) -> Result<ActiveBlock, ConsensusError> {
        let operation_set = block
            .block
            .operations
            .iter()
            .enumerate()
            .map(|(idx, op)| match op.get_operation_id() {
                Ok(id) => Ok((id, (idx, op.content.expire_period))),
                Err(e) => Err(e),
            })
            .collect::<Result<_, _>>()?;

        let addresses_to_operations = block.block.involved_addresses()?;
        Ok(ActiveBlock {
            block: block.block,
            parents: block.parents,
            children: block
                .children
                .into_iter()
                .map(|map| map.into_iter().collect())
                .collect(),
            dependencies: block.dependencies.into_iter().collect(),
            descendants: HashSet::new(),
            is_final: block.is_final,
            block_ledger_change: block
                .block_ledger_change
                .into_iter()
                .map(|map| map.into_iter().collect())
                .collect(),
            operation_set,
            addresses_to_operations,
            roll_updates: RollUpdates(block.roll_updates.into_iter().collect()),
        })
    }

    type Error = ConsensusError;
}

impl SerializeCompact for ExportActiveBlock {
    fn to_bytes_compact(&self) -> Result<Vec<u8>, models::ModelsError> {
        let mut res: Vec<u8> = Vec::new();

        //is_final
        if self.is_final {
            res.push(1);
        } else {
            res.push(0);
        }

        //block
        res.extend(self.block.to_bytes_compact()?);

        //parents
        // parents (note: there should be none if slot period=0)
        if self.parents.is_empty() {
            res.push(0);
        } else {
            res.push(1);
        }
        for (hash, period) in self.parents.iter() {
            res.extend(&hash.to_bytes());
            res.extend(period.to_varint_bytes());
        }

        //children
        let children_count: u32 = self.children.len().try_into().map_err(|err| {
            ModelsError::SerializeError(format!("too many children in ActiveBlock: {:?}", err))
        })?;
        res.extend(children_count.to_varint_bytes());
        for map in self.children.iter() {
            let map_count: u32 = map.len().try_into().map_err(|err| {
                ModelsError::SerializeError(format!(
                    "too many entry in children map in ActiveBlock: {:?}",
                    err
                ))
            })?;
            res.extend(map_count.to_varint_bytes());
            for (hash, period) in map {
                res.extend(&hash.to_bytes());
                res.extend(period.to_varint_bytes());
            }
        }

        //dependencies
        let dependencies_count: u32 = self.dependencies.len().try_into().map_err(|err| {
            ModelsError::SerializeError(format!("too many dependencies in ActiveBlock: {:?}", err))
        })?;
        res.extend(dependencies_count.to_varint_bytes());
        for dep in self.dependencies.iter() {
            res.extend(&dep.to_bytes());
        }

        //block_ledger_change
        let block_ledger_change_count: u32 =
            self.block_ledger_change.len().try_into().map_err(|err| {
                ModelsError::SerializeError(format!(
                    "too many block_ledger_change in ActiveBlock: {:?}",
                    err
                ))
            })?;
        res.extend(block_ledger_change_count.to_varint_bytes());
        for map in self.block_ledger_change.iter() {
            let map_count: u32 = map.len().try_into().map_err(|err| {
                ModelsError::SerializeError(format!(
                    "too many entry in block_ledger_change map in ActiveBlock: {:?}",
                    err
                ))
            })?;
            res.extend(map_count.to_varint_bytes());
            for (address, ledger) in map {
                res.extend(&address.to_bytes());
                res.extend(ledger.to_bytes_compact()?);
            }
        }

        // roll updates
        let roll_updates_count: u32 = self.roll_updates.len().try_into().map_err(|err| {
            ModelsError::SerializeError(format!("too many roll updates in ActiveBlock: {:?}", err))
        })?;
        res.extend(roll_updates_count.to_varint_bytes());
        for (addr, roll_update) in self.roll_updates.iter() {
            res.extend(addr.to_bytes());
            res.extend(roll_update.to_bytes_compact()?);
        }

        Ok(res)
    }
}

impl DeserializeCompact for ExportActiveBlock {
    fn from_bytes_compact(buffer: &[u8]) -> Result<(Self, usize), models::ModelsError> {
        let mut cursor = 0usize;
        let (parent_count, max_bootstrap_children, max_bootstrap_deps, max_bootstrap_pos_entries) =
            with_serialization_context(|context| {
                (
                    context.parent_count,
                    context.max_bootstrap_children,
                    context.max_bootstrap_deps,
                    context.max_bootstrap_pos_entries,
                )
            });

        //is_final
        let is_final_u8 = u8_from_slice(buffer)?;
        cursor += 1;
        let is_final = !(is_final_u8 == 0);

        //block
        let (block, delta) = Block::from_bytes_compact(&buffer[cursor..])?;
        cursor += delta;

        // parents
        let has_parents = u8_from_slice(&buffer[cursor..])?;
        cursor += 1;
        let parents = if has_parents == 1 {
            let mut parents: Vec<(BlockId, u64)> = Vec::with_capacity(parent_count as usize);
            for _ in 0..parent_count {
                let parent_h = BlockId::from_bytes(&array_from_slice(&buffer[cursor..])?)?;
                cursor += BLOCK_ID_SIZE_BYTES;
                let (period, delta) = u64::from_varint_bytes(&buffer[cursor..])?;
                cursor += delta;

                parents.push((parent_h, period));
            }
            parents
        } else if has_parents == 0 {
            Vec::new()
        } else {
            return Err(ModelsError::SerializeError(
                "ActiveBlock from_bytes_compact bad has parents flags.".into(),
            ));
        };

        //children
        let (children_count, delta) = u32::from_varint_bytes(&buffer[cursor..])?;
        if children_count > parent_count.into() {
            return Err(ModelsError::DeserializeError(
                "too many threads with children to deserialize".to_string(),
            ));
        }
        cursor += delta;
        let mut children: Vec<Vec<(BlockId, u64)>> = Vec::with_capacity(children_count as usize);
        for _ in 0..(children_count as usize) {
            let (map_count, delta) = u32::from_varint_bytes(&buffer[cursor..])?;
            if map_count > max_bootstrap_children {
                return Err(ModelsError::DeserializeError(
                    "too many children to deserialize".to_string(),
                ));
            }
            cursor += delta;
            let mut map: Vec<(BlockId, u64)> = Vec::with_capacity(map_count as usize);
            for _ in 0..(map_count as usize) {
                let hash = BlockId::from_bytes(&array_from_slice(&buffer[cursor..])?)?;
                cursor += BLOCK_ID_SIZE_BYTES;
                let (period, delta) = u64::from_varint_bytes(&buffer[cursor..])?;
                cursor += delta;
                map.push((hash, period));
            }
            children.push(map);
        }

        //dependencies
        let (dependencies_count, delta) = u32::from_varint_bytes(&buffer[cursor..])?;
        if dependencies_count > max_bootstrap_deps {
            return Err(ModelsError::DeserializeError(
                "too many dependencies to deserialize".to_string(),
            ));
        }
        cursor += delta;
        let mut dependencies: Vec<BlockId> = Vec::with_capacity(dependencies_count as usize);
        for _ in 0..(dependencies_count as usize) {
            let dep = BlockId::from_bytes(&array_from_slice(&buffer[cursor..])?)?;
            cursor += BLOCK_ID_SIZE_BYTES;
            dependencies.push(dep);
        }

        //block_ledger_change
        let (block_ledger_change_count, delta) = u32::from_varint_bytes(&buffer[cursor..])?;
        if block_ledger_change_count != parent_count as u32 {
            return Err(ModelsError::DeserializeError(
                "wrong number of threads to deserialize in block_ledger_change".to_string(),
            ));
        }
        cursor += delta;
        let mut block_ledger_change: Vec<Vec<(Address, LedgerChange)>> =
            Vec::with_capacity(block_ledger_change_count as usize);
        for _ in 0..(block_ledger_change_count as usize) {
            let (map_count, delta) = u32::from_varint_bytes(&buffer[cursor..])?;
            if map_count > max_bootstrap_children {
                return Err(ModelsError::DeserializeError(
                    "too many block_ledger_change to deserialize".to_string(),
                ));
            }
            cursor += delta;
            let mut map: Vec<(Address, LedgerChange)> = Vec::with_capacity(map_count as usize);
            for _ in 0..(map_count as usize) {
                let address = Address::from_bytes(&array_from_slice(&buffer[cursor..])?)?;
                cursor += ADDRESS_SIZE_BYTES;
                let (ledger, delta) = LedgerChange::from_bytes_compact(&buffer[cursor..])?;
                cursor += delta;
                map.push((address, ledger));
            }
            block_ledger_change.push(map);
        }

        // roll_updates
        let (roll_updates_count, delta) = u32::from_varint_bytes(&buffer[cursor..])?;
        if roll_updates_count > max_bootstrap_pos_entries {
            return Err(ModelsError::DeserializeError(
                "too many roll updates to deserialize".to_string(),
            ));
        }
        cursor += delta;
        let mut roll_updates: Vec<(Address, RollUpdate)> =
            Vec::with_capacity(roll_updates_count as usize);
        for _ in 0..roll_updates_count {
            let address = Address::from_bytes(&array_from_slice(&buffer[cursor..])?)?;
            cursor += ADDRESS_SIZE_BYTES;
            let (roll_update, delta) = RollUpdate::from_bytes_compact(&buffer[cursor..])?;
            cursor += delta;
            roll_updates.push((address, roll_update));
        }

        Ok((
            ExportActiveBlock {
                is_final,
                block,
                parents,
                children,
                dependencies,
                block_ledger_change,
                roll_updates,
            },
            cursor,
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiscardReason {
    /// Block is invalid, either structurally, or because of some incompatibility. The String contains the reason for info or debugging.
    Invalid(String),
    /// Block is incompatible with a final block.
    Stale,
    /// Block has enough fitness.
    Final,
}

#[derive(Debug, Clone)]
enum BlockStatus {
    Incoming(HeaderOrBlock),
    WaitingForSlot(HeaderOrBlock),
    WaitingForDependencies {
        header_or_block: HeaderOrBlock,
        unsatisfied_dependencies: HashSet<BlockId>, // includes self if it's only a header
        sequence_number: u64,
    },
    Active(ActiveBlock),
    Discarded {
        header: BlockHeader,
        reason: DiscardReason,
        sequence_number: u64,
    },
}

/// Block status in the graph that can be exported.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExportBlockStatus {
    Incoming,
    WaitingForSlot,
    WaitingForDependencies,
    Active(Block),
    Discarded(DiscardReason),
    Stored(Block),
}

impl<'a> From<&'a BlockStatus> for ExportBlockStatus {
    fn from(block: &BlockStatus) -> Self {
        match block {
            BlockStatus::Incoming(_) => ExportBlockStatus::Incoming,
            BlockStatus::WaitingForSlot(_) => ExportBlockStatus::WaitingForSlot,
            BlockStatus::WaitingForDependencies { .. } => ExportBlockStatus::WaitingForDependencies,
            BlockStatus::Active(active_block) => {
                ExportBlockStatus::Active(active_block.block.clone())
            }
            BlockStatus::Discarded { reason, .. } => ExportBlockStatus::Discarded(reason.clone()),
        }
    }
}

/// The block version that can be exported.
/// Note that the detailed list of operation is not exported
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportCompiledBlock {
    /// Header of the corresponding block.
    pub block: BlockHeader,
    /// For (i, set) in children,
    /// set contains the headers' hashes
    /// of blocks referencing exported block as a parent,
    /// in thread i.
    pub children: Vec<HashSet<BlockId>>,
    /// Active or final
    pub status: Status,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Status {
    Active,
    Final,
}

#[derive(Debug, Default, Clone)]
pub struct ExportDiscardedBlocks {
    pub map: HashMap<BlockId, (DiscardReason, BlockHeader)>,
}

#[derive(Debug, Clone)]
pub struct BlockGraphExport {
    /// Genesis blocks.
    pub genesis_blocks: Vec<BlockId>,
    /// Map of active blocks, were blocks are in their exported version.
    pub active_blocks: HashMap<BlockId, ExportCompiledBlock>,
    /// Finite cache of discarded blocks, in exported version.
    pub discarded_blocks: ExportDiscardedBlocks,
    /// Best parents hashe in each thread.
    pub best_parents: Vec<BlockId>,
    /// Latest final period and block hash in each thread.
    pub latest_final_blocks_periods: Vec<(BlockId, u64)>,
    /// Head of the incompatibility graph.
    pub gi_head: HashMap<BlockId, HashSet<BlockId>>,
    /// List of maximal cliques of compatible blocks.
    pub max_cliques: Vec<HashSet<BlockId>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerDataExport {
    /// Candidate data
    pub candidate_data: LedgerSubset,
    /// Final data
    pub final_data: LedgerSubset,
}

impl LedgerDataExport {
    pub fn new(thread_count: u8) -> LedgerDataExport {
        LedgerDataExport {
            candidate_data: LedgerSubset::new(thread_count),
            final_data: LedgerSubset::new(thread_count),
        }
    }
}

impl<'a> From<&'a BlockGraph> for BlockGraphExport {
    /// Conversion from blockgraph.
    fn from(block_graph: &'a BlockGraph) -> Self {
        let mut export = BlockGraphExport {
            genesis_blocks: block_graph.genesis_hashes.clone(),
            active_blocks: Default::default(),
            discarded_blocks: Default::default(),
            best_parents: block_graph.best_parents.clone(),
            latest_final_blocks_periods: block_graph.latest_final_blocks_periods.clone(),
            gi_head: block_graph.gi_head.clone(),
            max_cliques: block_graph.max_cliques.clone(),
        };

        for (hash, block) in block_graph.block_statuses.iter() {
            match block {
                BlockStatus::Discarded { header, reason, .. } => {
                    export
                        .discarded_blocks
                        .map
                        .insert(*hash, (reason.clone(), header.clone()));
                }
                BlockStatus::Active(block) => {
                    export.active_blocks.insert(
                        *hash,
                        ExportCompiledBlock {
                            block: block.block.header.clone(),
                            children: block
                                .children
                                .iter()
                                .map(|thread| thread.keys().copied().collect::<HashSet<BlockId>>())
                                .collect(),
                            status: if block.is_final {
                                Status::Final
                            } else {
                                Status::Active
                            },
                        },
                    );
                }
                _ => continue,
            }
        }

        export
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootsrapableGraph {
    /// Map of active blocks, were blocks are in their exported version.
    pub active_blocks: Vec<(BlockId, ExportActiveBlock)>,
    /// Best parents hashe in each thread.
    pub best_parents: Vec<BlockId>,
    /// Latest final period and block hash in each thread.
    pub latest_final_blocks_periods: Vec<(BlockId, u64)>,
    /// Head of the incompatibility graph.
    pub gi_head: Vec<(BlockId, Vec<BlockId>)>,
    /// List of maximal cliques of compatible blocks.
    pub max_cliques: Vec<Vec<BlockId>>,
    /// Ledger at last final blocks
    pub ledger: LedgerExport,
}

impl<'a> TryFrom<&'a BlockGraph> for BootsrapableGraph {
    type Error = ConsensusError;
    fn try_from(block_graph: &'a BlockGraph) -> Result<Self, Self::Error> {
        let mut active_blocks = HashMap::new();
        for (hash, status) in block_graph.block_statuses.iter() {
            match status {
                BlockStatus::Active(block) => {
                    active_blocks.insert(*hash, block.clone());
                }
                _ => continue,
            }
        }

        Ok(BootsrapableGraph {
            active_blocks: active_blocks
                .into_iter()
                .map(|(hash, block)| (hash, block.into()))
                .collect(),
            best_parents: block_graph.best_parents.clone(),
            latest_final_blocks_periods: block_graph.latest_final_blocks_periods.clone(),
            gi_head: block_graph
                .gi_head
                .clone()
                .into_iter()
                .map(|(hash, incomp)| (hash, incomp.into_iter().collect()))
                .collect(),
            max_cliques: block_graph
                .max_cliques
                .clone()
                .into_iter()
                .map(|clique| clique.into_iter().collect())
                .collect(),
            ledger: LedgerExport::try_from(&block_graph.ledger)?,
        })
    }
}

impl SerializeCompact for BootsrapableGraph {
    fn to_bytes_compact(&self) -> Result<Vec<u8>, models::ModelsError> {
        let mut res: Vec<u8> = Vec::new();
        let (max_bootstrap_blocks, max_bootstrap_cliques) = with_serialization_context(|context| {
            (context.max_bootstrap_blocks, context.max_bootstrap_cliques)
        });

        //active_blocks
        let blocks_count: u32 = self.active_blocks.len().try_into().map_err(|err| {
            ModelsError::SerializeError(format!(
                "too many active blocks in BootsrapableGraph: {:?}",
                err
            ))
        })?;
        if blocks_count > max_bootstrap_blocks {
            return Err(ModelsError::SerializeError(format!("too many blocks in active_blocks for serialization context in BootstrapableGraph: {:?}", blocks_count)));
        }
        res.extend(blocks_count.to_varint_bytes());
        for (hash, block) in self.active_blocks.iter() {
            res.extend(&hash.to_bytes());
            res.extend(block.to_bytes_compact()?);
        }

        //best_parents
        for parent_h in self.best_parents.iter() {
            res.extend(&parent_h.to_bytes());
        }

        //latest_final_blocks_periods
        for (hash, period) in self.latest_final_blocks_periods.iter() {
            res.extend(&hash.to_bytes());
            res.extend(period.to_varint_bytes());
        }

        //gi_head
        let gi_head_count: u32 = self.gi_head.len().try_into().map_err(|err| {
            ModelsError::SerializeError(format!("too many gi_head in BootsrapableGraph: {:?}", err))
        })?;
        res.extend(gi_head_count.to_varint_bytes());
        for (gihash, set) in self.gi_head.iter() {
            res.extend(&gihash.to_bytes());
            let set_count: u32 = set.len().try_into().map_err(|err| {
                ModelsError::SerializeError(format!(
                    "too many entry in gi_head set in BootsrapableGraph: {:?}",
                    err
                ))
            })?;
            res.extend(set_count.to_varint_bytes());
            for hash in set {
                res.extend(&hash.to_bytes());
            }
        }

        //max_cliques
        let max_cliques_count: u32 = self.max_cliques.len().try_into().map_err(|err| {
            ModelsError::SerializeError(format!(
                "too many max_cliques in BootsrapableGraph: {:?}",
                err
            ))
        })?;
        if max_cliques_count > max_bootstrap_cliques {
            return Err(ModelsError::SerializeError(format!("too many blocks in max_cliques for serialization context in BootstrapableGraph: {:?}", max_cliques_count)));
        }
        res.extend(max_cliques_count.to_varint_bytes());
        for set in self.max_cliques.iter() {
            let set_count: u32 = set.len().try_into().map_err(|err| {
                ModelsError::SerializeError(format!(
                    "too many entry in max_cliques set in BootsrapableGraph: {:?}",
                    err
                ))
            })?;
            res.extend(set_count.to_varint_bytes());
            for hash in set {
                res.extend(&hash.to_bytes());
            }
        }

        // ledger
        res.extend(self.ledger.to_bytes_compact()?);

        Ok(res)
    }
}

impl DeserializeCompact for BootsrapableGraph {
    fn from_bytes_compact(buffer: &[u8]) -> Result<(Self, usize), models::ModelsError> {
        let mut cursor = 0usize;
        let (max_bootstrap_blocks, parent_count, max_bootstrap_cliques) =
            with_serialization_context(|context| {
                (
                    context.max_bootstrap_blocks,
                    context.parent_count,
                    context.max_bootstrap_cliques,
                )
            });

        //active_blocks
        let (active_blocks_count, delta) = u32::from_varint_bytes(buffer)?;
        if active_blocks_count > max_bootstrap_blocks {
            return Err(ModelsError::DeserializeError(format!("too many blocks in active_blocks for deserialization context in BootstrapableGraph: {:?}", active_blocks_count)));
        }
        cursor += delta;
        let mut active_blocks: Vec<(BlockId, ExportActiveBlock)> =
            Vec::with_capacity(active_blocks_count as usize);
        for _ in 0..(active_blocks_count as usize) {
            let hash = BlockId::from_bytes(&array_from_slice(&buffer[cursor..])?)?;
            cursor += BLOCK_ID_SIZE_BYTES;
            let (block, delta) = ExportActiveBlock::from_bytes_compact(&buffer[cursor..])?;
            cursor += delta;
            active_blocks.push((hash, block));
        }

        //best_parents
        let mut best_parents: Vec<BlockId> = Vec::with_capacity(parent_count as usize);
        for _ in 0..parent_count {
            let parent_h = BlockId::from_bytes(&array_from_slice(&buffer[cursor..])?)?;
            cursor += BLOCK_ID_SIZE_BYTES;
            best_parents.push(parent_h);
        }

        //latest_final_blocks_periods
        let mut latest_final_blocks_periods: Vec<(BlockId, u64)> =
            Vec::with_capacity(parent_count as usize);
        for _ in 0..parent_count {
            let hash = BlockId::from_bytes(&array_from_slice(&buffer[cursor..])?)?;
            cursor += BLOCK_ID_SIZE_BYTES;
            let (period, delta) = u64::from_varint_bytes(&buffer[cursor..])?;
            cursor += delta;
            latest_final_blocks_periods.push((hash, period));
        }

        //gi_head
        let (gi_head_count, delta) = u32::from_varint_bytes(&buffer[cursor..])?;
        if gi_head_count > max_bootstrap_blocks {
            return Err(ModelsError::DeserializeError(format!("too many blocks in gi_head for deserialization context in BootstrapableGraph: {:?}", gi_head_count)));
        }
        cursor += delta;
        let mut gi_head: Vec<(BlockId, Vec<BlockId>)> = Vec::with_capacity(gi_head_count as usize);
        for _ in 0..(gi_head_count as usize) {
            let gihash = BlockId::from_bytes(&array_from_slice(&buffer[cursor..])?)?;
            cursor += BLOCK_ID_SIZE_BYTES;
            let (set_count, delta) = u32::from_varint_bytes(&buffer[cursor..])?;
            if set_count > max_bootstrap_blocks {
                return Err(ModelsError::DeserializeError(format!("too many blocks in a set in gi_head for deserialization context in BootstrapableGraph: {:?}", set_count)));
            }
            cursor += delta;
            let mut set: Vec<BlockId> = Vec::with_capacity(set_count as usize);
            for _ in 0..(set_count as usize) {
                let hash = BlockId::from_bytes(&array_from_slice(&buffer[cursor..])?)?;
                cursor += BLOCK_ID_SIZE_BYTES;
                set.push(hash);
            }
            gi_head.push((gihash, set));
        }

        //max_cliques: Vec<HashSet<BlockId>>
        let (max_cliques_count, delta) = u32::from_varint_bytes(&buffer[cursor..])?;
        if max_cliques_count > max_bootstrap_cliques {
            return Err(ModelsError::DeserializeError(format!("too many blocks in max_cliques for deserialization context in BootstrapableGraph: {:?}", max_cliques_count)));
        }
        cursor += delta;
        let mut max_cliques: Vec<Vec<BlockId>> = Vec::with_capacity(max_cliques_count as usize);
        for _ in 0..(max_cliques_count as usize) {
            let (set_count, delta) = u32::from_varint_bytes(&buffer[cursor..])?;
            if set_count > max_bootstrap_blocks {
                return Err(ModelsError::DeserializeError(format!("too many blocks in a clique for deserialization context in BootstrapableGraph: {:?}", set_count)));
            }
            cursor += delta;
            let mut set: Vec<BlockId> = Vec::with_capacity(set_count as usize);
            for _ in 0..(set_count as usize) {
                let hash = BlockId::from_bytes(&array_from_slice(&buffer[cursor..])?)?;
                cursor += BLOCK_ID_SIZE_BYTES;
                set.push(hash);
            }
            max_cliques.push(set);
        }

        let (ledger, delta) = LedgerExport::from_bytes_compact(&buffer[cursor..])?;
        cursor += delta;

        Ok((
            BootsrapableGraph {
                active_blocks,
                best_parents,
                latest_final_blocks_periods,
                gi_head,
                max_cliques,
                ledger,
            },
            cursor,
        ))
    }
}

pub struct BlockGraph {
    cfg: ConsensusConfig,
    genesis_hashes: Vec<BlockId>,
    sequence_counter: u64,
    block_statuses: HashMap<BlockId, BlockStatus>,
    latest_final_blocks_periods: Vec<(BlockId, u64)>,
    best_parents: Vec<BlockId>,
    gi_head: HashMap<BlockId, HashSet<BlockId>>,
    max_cliques: Vec<HashSet<BlockId>>,
    to_propagate: HashMap<BlockId, Block>,
    attack_attempts: Vec<BlockId>,
    new_final_blocks: HashSet<BlockId>,
    new_stale_blocks: HashMap<BlockId, Slot>,
    ledger: Ledger,
}

#[derive(Debug)]
enum HeaderCheckOutcome {
    Proceed {
        parents_hash_period: Vec<(BlockId, u64)>,
        dependencies: HashSet<BlockId>,
        incompatibilities: HashSet<BlockId>,
        inherited_incompatibilities_count: usize,
    },
    Discard(DiscardReason),
    WaitForSlot,
    WaitForDependencies(HashSet<BlockId>),
}

#[derive(Debug)]
enum BlockCheckOutcome {
    Proceed {
        parents_hash_period: Vec<(BlockId, u64)>,
        dependencies: HashSet<BlockId>,
        incompatibilities: HashSet<BlockId>,
        inherited_incompatibilities_count: usize,
        block_ledger_changes: Vec<HashMap<Address, LedgerChange>>,
        roll_updates: RollUpdates,
    },
    Discard(DiscardReason),
    WaitForSlot,
    WaitForDependencies(HashSet<BlockId>),
}

#[derive(Debug)]
enum BlockOperationsCheckOutcome {
    Proceed {
        dependencies: HashSet<BlockId>,
        block_ledger_changes: Vec<HashMap<Address, LedgerChange>>,
        roll_updates: RollUpdates,
    },
    Discard(DiscardReason),
    WaitForDependencies(HashSet<BlockId>),
}

async fn read_genesis_ledger(cfg: &ConsensusConfig) -> Result<Ledger, ConsensusError> {
    // load ledger from file
    let ledger = serde_json::from_str::<HashMap<Address, LedgerData>>(
        &tokio::fs::read_to_string(&cfg.initial_ledger_path).await?,
    )?;
    massa_trace!("read_genesis_ledger", { "ledger": ledger });
    Ledger::new(cfg.clone(), Some(ledger))
}

/// Creates genesis block in given thread.
///
/// # Arguments
/// * cfg: consensus configuration
/// * serialization_context: ref to a SerializationContext instance
/// * thread_number: thread in wich we want a genesis block
fn create_genesis_block(
    cfg: &ConsensusConfig,
    thread_number: u8,
) -> Result<(BlockId, Block), ConsensusError> {
    let private_key = cfg.genesis_key;
    let public_key = derive_public_key(&private_key);
    let (header_hash, header) = BlockHeader::new_signed(
        &private_key,
        BlockHeaderContent {
            creator: public_key,
            slot: Slot::new(0, thread_number),
            parents: Vec::new(),
            operation_merkle_root: Hash::hash(&Vec::new()),
        },
    )?;

    Ok((
        header_hash,
        Block {
            header,
            operations: Vec::new(),
        },
    ))
}

impl BlockGraph {
    /// Creates a new block_graph.
    ///
    /// # Argument
    /// * cfg : consensus configuration.
    /// * serialization_context: SerializationContext instance
    pub async fn new(
        cfg: ConsensusConfig,
        init: Option<BootsrapableGraph>,
    ) -> Result<Self, ConsensusError> {
        // load genesis blocks

        let mut block_statuses = HashMap::new();
        let mut block_hashes = Vec::with_capacity(cfg.thread_count as usize);
        for thread in 0u8..cfg.thread_count {
            let (hash, block) = create_genesis_block(&cfg, thread).map_err(|err| {
                ConsensusError::GenesisCreationError(format!("genesis error {:?}", err))
            })?;

            block_hashes.push(hash);
            block_statuses.insert(
                hash,
                BlockStatus::Active(ActiveBlock {
                    block,
                    parents: Vec::new(),
                    children: vec![HashMap::new(); cfg.thread_count as usize],
                    dependencies: HashSet::new(),
                    descendants: HashSet::new(),
                    is_final: true,
                    block_ledger_change: vec![HashMap::new(); cfg.thread_count as usize], // no changes in genesis blocks
                    operation_set: HashMap::with_capacity(0),
                    addresses_to_operations: HashMap::with_capacity(0),
                    roll_updates: RollUpdates::new(),
                }),
            );
        }

        massa_trace!("consensus.block_graph.new", {});
        if let Some(boot_graph) = init {
            let ledger = Ledger::from_export(
                boot_graph.ledger,
                boot_graph
                    .latest_final_blocks_periods
                    .iter()
                    .map(|(_id, period)| *period)
                    .collect(),
                cfg.clone(),
            )?;
            let mut res_graph = BlockGraph {
                cfg,
                sequence_counter: 0,
                genesis_hashes: block_hashes.clone(),
                block_statuses: boot_graph
                    .active_blocks
                    .iter()
                    .map(|(hash, block)| {
                        Ok((*hash, BlockStatus::Active(block.clone().try_into()?)))
                    })
                    .collect::<Result<_, ConsensusError>>()?,
                latest_final_blocks_periods: boot_graph.latest_final_blocks_periods,
                best_parents: boot_graph.best_parents,
                gi_head: boot_graph
                    .gi_head
                    .into_iter()
                    .map(|(h, v)| (h, v.into_iter().collect()))
                    .collect(),
                max_cliques: boot_graph
                    .max_cliques
                    .into_iter()
                    .map(|v| v.into_iter().collect())
                    .collect(),
                to_propagate: Default::default(),
                attack_attempts: Default::default(),
                ledger,
                new_final_blocks: Default::default(),
                new_stale_blocks: Default::default(),
            };
            // compute block descendants
            let active_blocks_map: HashMap<BlockId, Vec<BlockId>> = res_graph
                .block_statuses
                .iter()
                .filter_map(|(h, s)| {
                    if let BlockStatus::Active(a) = s {
                        Some((*h, a.parents.iter().map(|(ph, _)| *ph).collect()))
                    } else {
                        None
                    }
                })
                .collect();
            for (b_hash, b_parents) in active_blocks_map.into_iter() {
                let mut ancestors: VecDeque<BlockId> = b_parents.into_iter().collect();
                let mut visited: HashSet<BlockId> = HashSet::new();
                while let Some(ancestor_h) = ancestors.pop_back() {
                    if !visited.insert(ancestor_h) {
                        continue;
                    }
                    if let Some(BlockStatus::Active(ab)) =
                        res_graph.block_statuses.get_mut(&ancestor_h)
                    {
                        ab.descendants.insert(b_hash);
                        for (ancestor_parent_h, _) in ab.parents.iter() {
                            ancestors.push_front(*ancestor_parent_h);
                        }
                    }
                }
            }
            Ok(res_graph)
        } else {
            let ledger = read_genesis_ledger(&cfg).await?;
            Ok(BlockGraph {
                cfg,
                sequence_counter: 0,
                genesis_hashes: block_hashes.clone(),
                block_statuses,
                latest_final_blocks_periods: block_hashes.iter().map(|h| (*h, 0)).collect(),
                best_parents: block_hashes,
                gi_head: HashMap::new(),
                max_cliques: vec![HashSet::new()],
                to_propagate: Default::default(),
                attack_attempts: Default::default(),
                ledger,
                new_final_blocks: Default::default(),
                new_stale_blocks: Default::default(),
            })
        }
    }
    /// Gets lastest final blocks (hash, period) for each thread.
    pub fn get_latest_final_blocks_periods(&self) -> &Vec<(BlockId, u64)> {
        &self.latest_final_blocks_periods
    }

    /// Gets best parents.
    pub fn get_best_parents(&self) -> &Vec<BlockId> {
        &self.best_parents
    }

    ///for algo see pos.md
    // if addrs_opt is Some(addrs), restrict to addrs. If None, return all addresses.
    // returns (roll_counts, cycle_roll_updates)
    pub fn get_roll_data_at_parent(
        &self,
        block_id: BlockId,
        addrs_opt: Option<&HashSet<Address>>,
        pos: &ProofOfStake,
    ) -> Result<(RollCounts, RollUpdates), ConsensusError> {
        // get target block and its cycle/thread
        let (target_cycle, target_thread) = match self.block_statuses.get(&block_id) {
            Some(BlockStatus::Active(a_block)) => (
                a_block
                    .block
                    .header
                    .content
                    .slot
                    .get_cycle(self.cfg.periods_per_cycle),
                a_block.block.header.content.slot.thread,
            ),
            _ => {
                return Err(ConsensusError::ContainerInconsistency(format!(
                    "block missing or non-active: {:?}",
                    block_id
                )));
            }
        };

        // stack back to latest final slot
        // (step 1 in pos.md)
        let mut stack = Vec::new();
        let mut cur_block_id = block_id;
        let final_cycle;
        loop {
            // get block
            let cur_a_block = match self.block_statuses.get(&cur_block_id) {
                Some(BlockStatus::Active(a_block)) => a_block,
                _ => {
                    return Err(ConsensusError::ContainerInconsistency(format!(
                        "block missing or non-active: {:?}",
                        cur_block_id
                    )));
                }
            };
            if cur_a_block.is_final {
                // filters out genesis and final blocks
                // (step 1.1 in pos.md)
                final_cycle = cur_a_block
                    .block
                    .header
                    .content
                    .slot
                    .get_cycle(self.cfg.periods_per_cycle);
                break;
            }
            // (step 1.2 in pos.md)
            stack.push(cur_block_id);
            cur_block_id = cur_a_block.parents[target_thread as usize].0;
        }

        // get latest final PoS state for addresses
        // (step 2 a,d 3 in pos.md)
        let (mut cur_rolls, mut cur_cycle_roll_updates) = {
            // (step 2 in pos.md)
            let cycle_state = pos
                .get_final_roll_data(final_cycle, target_thread)
                .ok_or_else(|| {
                    ConsensusError::ContainerInconsistency(format!(
                        "final PoS cycle not available: {:?}",
                        final_cycle
                    ))
                })?;
            // (step 3 in pos.md)
            let cur_cycle_roll_updates = if final_cycle == target_cycle {
                cycle_state.cycle_updates.clone_subset(addrs_opt)
            } else {
                RollUpdates::new()
            };
            (
                cycle_state.roll_count.clone_subset(addrs_opt),
                cur_cycle_roll_updates,
            )
        };

        // unstack blocks and apply their roll changes
        // (step 4 in pos.md)
        while let Some(cur_block_id) = stack.pop() {
            // get block and apply its roll updates to cur_rolls and cur_cycle_roll_updates if in the same cycle as the target block
            match self.block_statuses.get(&cur_block_id) {
                Some(BlockStatus::Active(a_block)) => {
                    // (step 4.1 in pos.md)
                    cur_rolls.apply_subset(&a_block.roll_updates, addrs_opt)?;
                    // (step 4.2 in pos.md)
                    if a_block
                        .block
                        .header
                        .content
                        .slot
                        .get_cycle(self.cfg.periods_per_cycle)
                        == target_cycle
                    {
                        // if the block is in the target cycle, accumulate the roll updates
                        // applies compensations but ignores their amount
                        cur_cycle_roll_updates.chain_subset(&a_block.roll_updates, addrs_opt)?;
                    }
                }
                _ => {
                    return Err(ConsensusError::ContainerInconsistency(format!(
                        "block missing or non-active: {:?}",
                        cur_block_id
                    )));
                }
            };
        }

        // (step 5 in pos.md)
        Ok((cur_rolls, cur_cycle_roll_updates))
    }

    /// gets Ledger data export for given Addressses
    pub fn get_ledger_data_export(
        &self,
        addresses: &HashSet<Address>,
    ) -> Result<LedgerDataExport, ConsensusError> {
        let best_parents = self.get_best_parents();
        Ok(LedgerDataExport {
            candidate_data: self.get_ledger_at_parents(best_parents, addresses)?,
            final_data: self.ledger.get_final_ledger_subset(addresses)?,
        })
    }

    pub fn get_operations_involving_address(
        &self,
        address: &Address,
    ) -> Result<HashMap<OperationId, OperationSearchResult>, ConsensusError> {
        let mut res: HashMap<OperationId, OperationSearchResult> = HashMap::new();
        for (_, block_status) in self.block_statuses.iter() {
            if let BlockStatus::Active(ActiveBlock {
                addresses_to_operations,
                is_final,
                operation_set,
                block,
                ..
            }) = block_status
            {
                if let Some(ops) = addresses_to_operations.get(address) {
                    for op in ops.iter() {
                        let (idx, _) = operation_set.get(op).ok_or_else(|| {
                            ConsensusError::ContainerInconsistency(format!(
                                "op {:?} should be here",
                                op
                            ))
                        })?;
                        let search = OperationSearchResult {
                            op: block.operations[*idx].clone(),
                            in_pool: false,
                            in_blocks: vec![(block.header.compute_block_id()?, (*idx, *is_final))]
                                .into_iter()
                                .collect(),
                            status: OperationSearchResultStatus::InBlock(
                                OperationSearchResultBlockStatus::Active,
                            ),
                        };
                        if let Some(old_search) = res.get_mut(op) {
                            old_search.extend(&search);
                        } else {
                            res.insert(*op, search);
                        }
                    }
                }
            }
        }
        Ok(res)
    }

    /// Gets whole compiled block corresponding to given hash, if it is active.
    ///
    /// # Argument
    /// * block_id : block ID
    pub fn get_active_block(&self, block_id: &BlockId) -> Option<&ActiveBlock> {
        BlockGraph::get_full_active_block(&self.block_statuses, *block_id)
    }

    pub fn get_export_block_status(&self, block_id: &BlockId) -> Option<ExportBlockStatus> {
        self.block_statuses
            .get(block_id)
            .map(|block_status| block_status.into())
    }

    /// Retrieves operations from operation Ids
    pub fn get_operations(
        &self,
        operation_ids: &HashSet<OperationId>,
    ) -> HashMap<OperationId, OperationSearchResult> {
        let mut res: HashMap<OperationId, OperationSearchResult> = HashMap::new();
        // for each active block
        for (block_id, block_status) in self.block_statuses.iter() {
            if let BlockStatus::Active(ActiveBlock {
                block,
                operation_set,
                is_final,
                ..
            }) = block_status
            {
                // check the intersection with the wanted operation ids, and update/insert into results
                operation_ids
                    .iter()
                    .filter_map(|op_id| {
                        operation_set
                            .get(op_id)
                            .map(|(idx, _)| (op_id, idx, &block.operations[*idx]))
                    })
                    .for_each(|(op_id, idx, op)| {
                        let search_new = OperationSearchResult {
                            op: op.clone(),
                            in_pool: false,
                            in_blocks: vec![(*block_id, (*idx, *is_final))].into_iter().collect(),
                            status: OperationSearchResultStatus::InBlock(
                                OperationSearchResultBlockStatus::Active,
                            ),
                        };
                        res.entry(*op_id)
                            .and_modify(|search_old| search_old.extend(&search_new))
                            .or_insert(search_new);
                    });
            }
        }
        res
    }

    // signal new slot
    pub fn slot_tick(
        &mut self,
        pos: &mut ProofOfStake,
        current_slot: Option<Slot>,
    ) -> Result<(), ConsensusError> {
        // list all elements for which the time has come
        let to_process: BTreeSet<(Slot, BlockId)> = self
            .block_statuses
            .iter()
            .filter_map(|(hash, block_status)| match block_status {
                BlockStatus::WaitingForSlot(header_or_block) => {
                    let slot = header_or_block.get_slot();
                    if Some(slot) <= current_slot {
                        Some((slot, *hash))
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .collect();

        massa_trace!("consensus.block_graph.slot_tick", {});
        // process those elements
        self.rec_process(to_process, pos, current_slot)?;

        Ok(())
    }

    /// A new header has come !
    pub fn incoming_header(
        &mut self,
        hash: BlockId,
        header: BlockHeader,
        pos: &mut ProofOfStake,
        current_slot: Option<Slot>,
    ) -> Result<(), ConsensusError> {
        // ignore genesis blocks
        if self.genesis_hashes.contains(&hash) {
            return Ok(());
        }

        massa_trace!("consensus.block_graph.incoming_header", {"hash": hash, "header": header});
        let mut to_ack: BTreeSet<(Slot, BlockId)> = BTreeSet::new();
        match self.block_statuses.entry(hash) {
            // if absent => add as Incoming, call rec_ack on it
            hash_map::Entry::Vacant(vac) => {
                to_ack.insert((header.content.slot, hash));
                vac.insert(BlockStatus::Incoming(HeaderOrBlock::Header(header)));
            }
            hash_map::Entry::Occupied(mut occ) => match occ.get_mut() {
                BlockStatus::Discarded {
                    sequence_number, ..
                } => {
                    // promote if discarded
                    *sequence_number = BlockGraph::new_sequence_number(&mut self.sequence_counter);
                }
                BlockStatus::WaitingForDependencies { .. } => {
                    // promote in dependencies
                    self.promote_dep_tree(hash)?;
                }
                _ => {}
            },
        }

        // process
        self.rec_process(to_ack, pos, current_slot)?;

        Ok(())
    }

    /// A new block has come
    pub fn incoming_block(
        &mut self,
        block_id: BlockId,
        block: Block,
        operation_set: HashMap<OperationId, (usize, u64)>,
        pos: &mut ProofOfStake,
        current_slot: Option<Slot>,
    ) -> Result<(), ConsensusError> {
        // ignore genesis blocks
        if self.genesis_hashes.contains(&block_id) {
            return Ok(());
        }

        massa_trace!("consensus.block_graph.incoming_block", {"block_id": block_id, "block": block});
        let mut to_ack: BTreeSet<(Slot, BlockId)> = BTreeSet::new();
        match self.block_statuses.entry(block_id) {
            // if absent => add as Incoming, call rec_ack on it
            hash_map::Entry::Vacant(vac) => {
                to_ack.insert((block.header.content.slot, block_id));
                vac.insert(BlockStatus::Incoming(HeaderOrBlock::Block(
                    block,
                    operation_set,
                )));
            }
            hash_map::Entry::Occupied(mut occ) => match occ.get_mut() {
                BlockStatus::Discarded {
                    sequence_number, ..
                } => {
                    // promote if discarded
                    *sequence_number = BlockGraph::new_sequence_number(&mut self.sequence_counter);
                }
                BlockStatus::WaitingForSlot(header_or_block) => {
                    // promote to full block
                    *header_or_block = HeaderOrBlock::Block(block, operation_set);
                }
                BlockStatus::WaitingForDependencies {
                    header_or_block,
                    unsatisfied_dependencies,
                    ..
                } => {
                    // promote to full block and satisfy self-dependency
                    if unsatisfied_dependencies.remove(&block_id) {
                        // a dependency was satisfied: process
                        to_ack.insert((block.header.content.slot, block_id));
                    }
                    *header_or_block = HeaderOrBlock::Block(block, operation_set);
                    // promote in dependencies
                    self.promote_dep_tree(block_id)?;
                }
                _ => return Ok(()),
            },
        }

        // process
        self.rec_process(to_ack, pos, current_slot)?;

        Ok(())
    }

    fn new_sequence_number(sequence_counter: &mut u64) -> u64 {
        let res = *sequence_counter;
        *sequence_counter += 1;
        res
    }

    // acknowledge a set of items recursively
    fn rec_process(
        &mut self,
        mut to_ack: BTreeSet<(Slot, BlockId)>,
        pos: &mut ProofOfStake,
        current_slot: Option<Slot>,
    ) -> Result<(), ConsensusError> {
        // order processing by (slot, hash)
        while let Some((_slot, hash)) = to_ack.pop_first() {
            to_ack.extend(self.process(hash, pos, current_slot)?)
        }
        Ok(())
    }

    // ack a single item, return a set of items to re-ack
    fn process(
        &mut self,
        block_id: BlockId,
        pos: &mut ProofOfStake,
        current_slot: Option<Slot>,
    ) -> Result<BTreeSet<(Slot, BlockId)>, ConsensusError> {
        // list items to reprocess
        let mut reprocess = BTreeSet::new();

        massa_trace!("consensus.block_graph.process", { "block_id": block_id });
        // control all the waiting states and try to get a valid block
        let (
            valid_block,
            valid_block_parents_hash_period,
            valid_block_deps,
            valid_block_incomp,
            valid_block_inherited_incomp_count,
            valid_block_changes,
            valid_block_operation_set,
            valid_block_roll_updates,
        ) = match self.block_statuses.get(&block_id) {
            None => return Ok(BTreeSet::new()), // disappeared before being processed: do nothing

            // discarded: do nothing
            Some(BlockStatus::Discarded { .. }) => {
                massa_trace!("consensus.block_graph.process.discarded", {
                    "block_id": block_id
                });
                return Ok(BTreeSet::new());
            }

            // already active: do nothing
            Some(BlockStatus::Active(_)) => {
                massa_trace!("consensus.block_graph.process.active", {
                    "block_id": block_id
                });
                return Ok(BTreeSet::new());
            }

            // incoming header
            Some(BlockStatus::Incoming(HeaderOrBlock::Header(_))) => {
                massa_trace!("consensus.block_graph.process.incoming_header", {
                    "block_id": block_id
                });
                // remove header
                let header = if let Some(BlockStatus::Incoming(HeaderOrBlock::Header(header))) =
                    self.block_statuses.remove(&block_id)
                {
                    header
                } else {
                    return Err(ConsensusError::ContainerInconsistency(format!(
                        "inconsistency inside block statuses removing incoming header {:?}",
                        block_id
                    )));
                };
                match self.check_header(&block_id, &header, pos, current_slot)? {
                    HeaderCheckOutcome::Proceed { .. } => {
                        // set as waiting dependencies
                        let mut dependencies = HashSet::new();
                        dependencies.insert(block_id); // add self as unsatisfied
                        self.block_statuses.insert(
                            block_id,
                            BlockStatus::WaitingForDependencies {
                                header_or_block: HeaderOrBlock::Header(header),
                                unsatisfied_dependencies: dependencies,
                                sequence_number: BlockGraph::new_sequence_number(
                                    &mut self.sequence_counter,
                                ),
                            },
                        );
                        self.promote_dep_tree(block_id)?;

                        massa_trace!(
                            "consensus.block_graph.process.incoming_header.waiting_for_self",
                            { "block_id": block_id }
                        );
                        return Ok(BTreeSet::new());
                    }
                    HeaderCheckOutcome::WaitForDependencies(mut dependencies) => {
                        // set as waiting dependencies
                        dependencies.insert(block_id); // add self as unsatisfied
                        massa_trace!("consensus.block_graph.process.incoming_header.waiting_for_dependencies", {"block_id": block_id, "dependencies": dependencies});

                        self.block_statuses.insert(
                            block_id,
                            BlockStatus::WaitingForDependencies {
                                header_or_block: HeaderOrBlock::Header(header),
                                unsatisfied_dependencies: dependencies,
                                sequence_number: BlockGraph::new_sequence_number(
                                    &mut self.sequence_counter,
                                ),
                            },
                        );
                        self.promote_dep_tree(block_id)?;

                        return Ok(BTreeSet::new());
                    }
                    HeaderCheckOutcome::WaitForSlot => {
                        // make it wait for slot
                        self.block_statuses.insert(
                            block_id,
                            BlockStatus::WaitingForSlot(HeaderOrBlock::Header(header)),
                        );

                        massa_trace!(
                            "consensus.block_graph.process.incoming_header.waiting_for_slot",
                            { "block_id": block_id }
                        );
                        return Ok(BTreeSet::new());
                    }
                    HeaderCheckOutcome::Discard(reason) => {
                        self.maybe_note_attack_attempt(&reason, &block_id);
                        massa_trace!("consensus.block_graph.process.incoming_header.discarded", {"block_id": block_id, "reason": reason});
                        // count stales
                        if reason == DiscardReason::Stale {
                            self.new_stale_blocks.insert(block_id, header.content.slot);
                        }
                        // discard
                        self.block_statuses.insert(
                            block_id,
                            BlockStatus::Discarded {
                                header,
                                reason,
                                sequence_number: BlockGraph::new_sequence_number(
                                    &mut self.sequence_counter,
                                ),
                            },
                        );

                        return Ok(BTreeSet::new());
                    }
                }
            }

            // incoming block
            Some(BlockStatus::Incoming(HeaderOrBlock::Block(_, _))) => {
                massa_trace!("consensus.block_graph.process.incoming_block", {
                    "block_id": block_id
                });
                let (block, operation_set) = if let Some(BlockStatus::Incoming(
                    HeaderOrBlock::Block(block, operation_set),
                )) = self.block_statuses.remove(&block_id)
                {
                    (block, operation_set)
                } else {
                    return Err(ConsensusError::ContainerInconsistency(format!(
                        "inconsistency inside block statuses removing incoming block {:?}",
                        block_id
                    )));
                };
                match self.check_block(&block_id, &block, &operation_set, pos, current_slot)? {
                    BlockCheckOutcome::Proceed {
                        parents_hash_period,
                        dependencies,
                        incompatibilities,
                        inherited_incompatibilities_count,
                        block_ledger_changes,
                        roll_updates,
                    } => {
                        // block is valid: remove it from Incoming and return it
                        massa_trace!("consensus.block_graph.process.incoming_block.valid", {
                            "block_id": block_id
                        });
                        (
                            block,
                            parents_hash_period,
                            dependencies,
                            incompatibilities,
                            inherited_incompatibilities_count,
                            block_ledger_changes,
                            operation_set,
                            roll_updates,
                        )
                    }
                    BlockCheckOutcome::WaitForDependencies(dependencies) => {
                        // set as waiting dependencies
                        self.block_statuses.insert(
                            block_id,
                            BlockStatus::WaitingForDependencies {
                                header_or_block: HeaderOrBlock::Block(block, operation_set),
                                unsatisfied_dependencies: dependencies,
                                sequence_number: BlockGraph::new_sequence_number(
                                    &mut self.sequence_counter,
                                ),
                            },
                        );
                        self.promote_dep_tree(block_id)?;
                        massa_trace!(
                            "consensus.block_graph.process.incoming_block.waiting_for_dependencies",
                            { "block_id": block_id }
                        );
                        return Ok(BTreeSet::new());
                    }
                    BlockCheckOutcome::WaitForSlot => {
                        // set as waiting for slot
                        self.block_statuses.insert(
                            block_id,
                            BlockStatus::WaitingForSlot(HeaderOrBlock::Block(block, operation_set)),
                        );

                        massa_trace!(
                            "consensus.block_graph.process.incoming_block.waiting_for_slot",
                            { "block_id": block_id }
                        );
                        return Ok(BTreeSet::new());
                    }
                    BlockCheckOutcome::Discard(reason) => {
                        self.maybe_note_attack_attempt(&reason, &block_id);
                        massa_trace!("consensus.block_graph.process.incoming_block.discarded", {"block_id": block_id, "reason": reason});
                        // count stales
                        if reason == DiscardReason::Stale {
                            self.new_stale_blocks
                                .insert(block_id, block.header.content.slot);
                        }
                        // add to discard
                        self.block_statuses.insert(
                            block_id,
                            BlockStatus::Discarded {
                                header: block.header,
                                reason,
                                sequence_number: BlockGraph::new_sequence_number(
                                    &mut self.sequence_counter,
                                ),
                            },
                        );

                        return Ok(BTreeSet::new());
                    }
                }
            }

            Some(BlockStatus::WaitingForSlot(header_or_block)) => {
                massa_trace!("consensus.block_graph.process.waiting_for_slot", {
                    "block_id": block_id
                });
                let slot = header_or_block.get_slot();
                if Some(slot) > current_slot {
                    massa_trace!(
                        "consensus.block_graph.process.waiting_for_slot.in_the_future",
                        { "block_id": block_id }
                    );
                    // in the future: ignore
                    return Ok(BTreeSet::new());
                }
                // send back as incoming and ask for reprocess
                if let Some(BlockStatus::WaitingForSlot(header_or_block)) =
                    self.block_statuses.remove(&block_id)
                {
                    self.block_statuses
                        .insert(block_id, BlockStatus::Incoming(header_or_block));
                    reprocess.insert((slot, block_id));
                    massa_trace!(
                        "consensus.block_graph.process.waiting_for_slot.reprocess",
                        { "block_id": block_id }
                    );
                    return Ok(reprocess);
                } else {
                    return Err(ConsensusError::ContainerInconsistency(format!("inconsistency inside block statuses removing waiting for slot block or header {:?}", block_id)));
                };
            }

            Some(BlockStatus::WaitingForDependencies {
                unsatisfied_dependencies,
                ..
            }) => {
                massa_trace!("consensus.block_graph.process.waiting_for_dependencies", {
                    "block_id": block_id
                });
                if !unsatisfied_dependencies.is_empty() {
                    // still has unsatisfied dependencies: ignore
                    return Ok(BTreeSet::new());
                }
                // send back as incoming and ask for reprocess
                if let Some(BlockStatus::WaitingForDependencies {
                    header_or_block, ..
                }) = self.block_statuses.remove(&block_id)
                {
                    reprocess.insert((header_or_block.get_slot(), block_id));
                    self.block_statuses
                        .insert(block_id, BlockStatus::Incoming(header_or_block));
                    massa_trace!(
                        "consensus.block_graph.process.waiting_for_dependencies.reprocess",
                        { "block_id": block_id }
                    );
                    return Ok(reprocess);
                } else {
                    return Err(ConsensusError::ContainerInconsistency(format!("inconsistency inside block statuses removing waiting for slot header or block {:?}", block_id)));
                }
            }
        };

        let valid_block_addresses_to_operations = valid_block.involved_addresses()?;

        // add block to graph
        self.add_block_to_graph(
            block_id,
            valid_block_parents_hash_period,
            valid_block,
            valid_block_deps,
            valid_block_incomp,
            valid_block_inherited_incomp_count,
            valid_block_changes,
            valid_block_operation_set,
            valid_block_addresses_to_operations,
            valid_block_roll_updates,
        )?;

        // if the block was added, update linked dependencies and mark satisfied ones for recheck
        if let Some(BlockStatus::Active(active)) = self.block_statuses.get(&block_id) {
            massa_trace!("consensus.block_graph.process.is_active", {
                "block_id": block_id
            });
            self.to_propagate.insert(block_id, active.block.clone());
            for (itm_block_id, itm_status) in self.block_statuses.iter_mut() {
                if let BlockStatus::WaitingForDependencies {
                    header_or_block,
                    unsatisfied_dependencies,
                    ..
                } = itm_status
                {
                    if unsatisfied_dependencies.remove(&block_id) {
                        // a dependency was satisfied: retry
                        reprocess.insert((header_or_block.get_slot(), *itm_block_id));
                    }
                }
            }
        }

        Ok(reprocess)
    }

    /// Note an attack attempt if the discard reason indicates one.
    fn maybe_note_attack_attempt(&mut self, reason: &DiscardReason, hash: &BlockId) {
        massa_trace!("consensus.block_graph.maybe_note_attack_attempt", {"hash": hash, "reason": reason});
        // If invalid, note the attack attempt.
        if let DiscardReason::Invalid(reason) = reason {
            info!(
                "consensus.block_graph.maybe_note_attack_attempt DiscardReason::Invalid:{}",
                reason
            );
            self.attack_attempts.push(*hash);
        }
    }

    /// Gets whole ActiveBlock corresponding to given block_id
    ///
    /// # Argument
    /// * block_id : block ID
    fn get_full_active_block(
        block_statuses: &HashMap<BlockId, BlockStatus>,
        block_id: BlockId,
    ) -> Option<&ActiveBlock> {
        match block_statuses.get(&block_id) {
            Some(BlockStatus::Active(active_block)) => Some(active_block),
            _ => None,
        }
    }

    /// Gets a block and all its descendants
    ///
    /// # Argument
    /// * hash : hash of the given block
    fn get_active_block_and_descendants(
        &self,
        block_id: &BlockId,
    ) -> Result<HashSet<BlockId>, ConsensusError> {
        let mut to_visit = vec![*block_id];
        let mut result: HashSet<BlockId> = HashSet::new();
        while let Some(visit_h) = to_visit.pop() {
            if !result.insert(visit_h) {
                continue; // already visited
            }
            BlockGraph::get_full_active_block(&self.block_statuses, visit_h)
                .ok_or_else(|| ConsensusError::ContainerInconsistency(format!("inconsistency inside block statuses iterating through descendants of {:?} - missing {:?}", block_id, visit_h)))?
                .children
                .iter()
                .for_each(|thread_children| to_visit.extend(thread_children.keys()));
        }
        Ok(result)
    }

    fn check_header(
        &self,
        block_id: &BlockId,
        header: &BlockHeader,
        pos: &mut ProofOfStake,
        current_slot: Option<Slot>,
    ) -> Result<HeaderCheckOutcome, ConsensusError> {
        massa_trace!("consensus.block_graph.check_header", {
            "block_id": block_id
        });
        let mut parents: Vec<(BlockId, u64)> = Vec::with_capacity(self.cfg.thread_count as usize);
        let mut deps = HashSet::new();
        let mut incomp = HashSet::new();
        let mut missing_deps = HashSet::new();

        // basic structural checks
        if header.content.parents.len() != (self.cfg.thread_count as usize)
            || header.content.slot.period == 0
            || header.content.slot.thread >= self.cfg.thread_count
        {
            return Ok(HeaderCheckOutcome::Discard(DiscardReason::Invalid(
                "Basic structural header checks failed".to_string(),
            )));
        }

        // check that is older than the latest final block in that thread
        // Note: this excludes genesis blocks
        if header.content.slot.period
            <= self.latest_final_blocks_periods[header.content.slot.thread as usize].1
        {
            return Ok(HeaderCheckOutcome::Discard(DiscardReason::Stale));
        }

        // check if block slot is too much in the future
        if let Some(cur_slot) = current_slot {
            if header.content.slot.period
                > cur_slot
                    .period
                    .saturating_add(self.cfg.future_block_processing_max_periods)
            {
                return Ok(HeaderCheckOutcome::WaitForSlot);
            }
        }

        // check if it was the creator's turn to create this block
        // (step 1 in consensus/pos.md)
        // note: do this AFTER TooMuchInTheFuture checks
        //       to avoid doing too many draws to check blocks in the distant future
        let slot_draw_address = match pos.draw(header.content.slot) {
            Ok(addr) => addr,
            Err(ConsensusError::PosCycleUnavailable(_)) => {
                // slot is not available yet
                return Ok(HeaderCheckOutcome::WaitForSlot);
            }
            Err(err) => return Err(err),
        };
        if Address::from_public_key(&header.content.creator)? != slot_draw_address {
            // it was not the creator's turn to create a block for this slot
            return Ok(HeaderCheckOutcome::Discard(DiscardReason::Invalid(
                format!("Bad creator turn for the slot:{}", header.content.slot),
            )));
        }

        // check if block is in the future: queue it
        // note: do it after testing signature + draw to prevent queue flooding/DoS
        // note: Some(x) > None
        if Some(header.content.slot) > current_slot {
            return Ok(HeaderCheckOutcome::WaitForSlot);
        }

        // Note: here we will check if we already have a block for that slot
        // and if someone double staked, they will be denounced

        // list parents and ensure they are present
        let parent_set: HashSet<BlockId> = header.content.parents.iter().copied().collect();
        deps.extend(&parent_set);
        for parent_thread in 0u8..self.cfg.thread_count {
            let parent_hash = header.content.parents[parent_thread as usize];
            match self.block_statuses.get(&parent_hash) {
                Some(BlockStatus::Discarded { reason, .. }) => {
                    // parent is discarded
                    return Ok(HeaderCheckOutcome::Discard(match reason {
                        DiscardReason::Invalid(invalid_reason) => DiscardReason::Invalid(format!("discarded because a parent was discarded for the following reason: {:?}", invalid_reason)),
                        r => r.clone()
                    }));
                }
                Some(BlockStatus::Active(parent)) => {
                    // parent is active

                    // check that the parent is from an earlier slot in the right thread
                    if parent.block.header.content.slot.thread != parent_thread
                        || parent.block.header.content.slot >= header.content.slot
                    {
                        return Ok(HeaderCheckOutcome::Discard(DiscardReason::Invalid(
                            format!(
                                "Bad parent {} in thread:{} or slot:{} for {}.",
                                parent_hash,
                                parent_thread,
                                parent.block.header.content.slot,
                                header.content.slot
                            ),
                        )));
                    }

                    // inherit parent incompatibilities
                    // and ensure parents are mutually compatible
                    if let Some(p_incomp) = self.gi_head.get(&parent_hash) {
                        if !p_incomp.is_disjoint(&parent_set) {
                            return Ok(HeaderCheckOutcome::Discard(DiscardReason::Invalid(
                                "Parent not mutually compatible".to_string(),
                            )));
                        }
                        incomp.extend(p_incomp);
                    }

                    parents.push((parent_hash, parent.block.header.content.slot.period));
                }
                _ => {
                    // parent is missing or queued
                    if self.genesis_hashes.contains(&parent_hash) {
                        // forbid depending on discarded genesis block
                        return Ok(HeaderCheckOutcome::Discard(DiscardReason::Stale));
                    }
                    missing_deps.insert(parent_hash);
                }
            }
        }
        if !missing_deps.is_empty() {
            return Ok(HeaderCheckOutcome::WaitForDependencies(missing_deps));
        }
        let inherited_incomp_count = incomp.len();

        // check the topological consistency of the parents
        {
            let mut gp_max_slots = vec![0u64; self.cfg.thread_count as usize];
            for parent_i in 0..self.cfg.thread_count {
                let (parent_h, parent_period) = parents[parent_i as usize];
                let parent = self.get_active_block(&parent_h).ok_or_else(|| {
                    ConsensusError::ContainerInconsistency(format!(
                        "inconsistency inside block statuses searching parent {:?} of block {:?}",
                        parent_h, block_id
                    ))
                })?;
                if parent_period < gp_max_slots[parent_i as usize] {
                    // a parent is earlier than a block known by another parent in that thread
                    return Ok(HeaderCheckOutcome::Discard(DiscardReason::Invalid(
                        "a parent is earlier than a block known by another parent in that thread"
                            .to_string(),
                    )));
                }
                gp_max_slots[parent_i as usize] = parent_period;
                if parent_period == 0 {
                    // genesis
                    continue;
                }
                for gp_i in 0..self.cfg.thread_count {
                    if gp_i == parent_i {
                        continue;
                    }
                    let gp_h = parent.parents[gp_i as usize].0;
                    deps.insert(gp_h);
                    match self.block_statuses.get(&gp_h) {
                        // this grandpa is discarded
                        Some(BlockStatus::Discarded { reason, .. }) => {
                            return Ok(HeaderCheckOutcome::Discard(reason.clone()));
                        }
                        // this grandpa is active
                        Some(BlockStatus::Active(gp)) => {
                            if gp.block.header.content.slot.period > gp_max_slots[gp_i as usize] {
                                if gp_i < parent_i {
                                    return Ok(HeaderCheckOutcome::Discard(
                                        DiscardReason::Invalid(
                                            "grandpa error: gp_i < parent_i".to_string(),
                                        ),
                                    ));
                                }
                                gp_max_slots[gp_i as usize] = gp.block.header.content.slot.period;
                            }
                        }
                        // this grandpa is missing or queued
                        _ => {
                            if self.genesis_hashes.contains(&gp_h) {
                                // forbid depending on discarded genesis block
                                return Ok(HeaderCheckOutcome::Discard(DiscardReason::Stale));
                            }
                            missing_deps.insert(gp_h);
                        }
                    }
                }
            }
        }
        if !missing_deps.is_empty() {
            return Ok(HeaderCheckOutcome::WaitForDependencies(missing_deps));
        }

        // get parent in own thread
        let parent_in_own_thread = BlockGraph::get_full_active_block(
            &self.block_statuses,
            parents[header.content.slot.thread as usize].0,
        )
        .ok_or_else(|| {
            ConsensusError::ContainerInconsistency(format!(
            "inconsistency inside block statuses searching parent {:?} in own thread of block {:?}",
            parents[header.content.slot.thread as usize].0, block_id
        ))
        })?;

        // thread incompatibility test
        parent_in_own_thread.children[header.content.slot.thread as usize]
            .keys()
            .filter(|&sibling_h| sibling_h != block_id)
            .try_for_each(|&sibling_h| {
                incomp.extend(self.get_active_block_and_descendants(&sibling_h)?);
                Result::<(), ConsensusError>::Ok(())
            })?;

        // grandpa incompatibility test
        for tau in (0u8..self.cfg.thread_count).filter(|&t| t != header.content.slot.thread) {
            // for each parent in a different thread tau
            // traverse parent's descendants in tau
            let mut to_explore = vec![(0usize, header.content.parents[tau as usize])];
            while let Some((cur_gen, cur_h)) = to_explore.pop() {
                let cur_b = BlockGraph::get_full_active_block(&self.block_statuses, cur_h)
                    .ok_or_else(|| ConsensusError::ContainerInconsistency(format!("inconsistency inside block statuses searching {:?} while checking grandpa incompatibility of block {:?}",cur_h,  block_id)))?;

                // traverse but do not check up to generation 1
                if cur_gen <= 1 {
                    to_explore.extend(
                        cur_b.children[tau as usize]
                            .keys()
                            .map(|&c_h| (cur_gen + 1, c_h)),
                    );
                    continue;
                }

                // check if the parent in tauB has a strictly lower period number than B's parent in tauB
                // note: cur_b cannot be genesis at gen > 1
                if BlockGraph::get_full_active_block(
                    &self.block_statuses,
                    cur_b.block.header.content.parents[header.content.slot.thread as usize],
                )
                .ok_or_else(||
                    ConsensusError::ContainerInconsistency(
                        format!("inconsistency inside block statuses searching {:?} check if the parent in tauB has a strictly lower period number than B's parent in tauB while checking grandpa incompatibility of block {:?}",
                        cur_b.block.header.content.parents[header.content.slot.thread as usize],
                        block_id)
                    ))?
                .block
                .header
                .content
                .slot
                .period
                    < parent_in_own_thread.block.header.content.slot.period
                {
                    // GPI detected
                    incomp.extend(self.get_active_block_and_descendants(&cur_h)?);
                } // otherwise, cur_b and its descendants cannot be GPI with the block: don't traverse
            }
        }

        // check if the block is incompatible with a parent
        if !incomp.is_disjoint(&parents.iter().map(|(h, _p)| *h).collect()) {
            return Ok(HeaderCheckOutcome::Discard(DiscardReason::Invalid(
                "Block incompatible with a parent".to_string(),
            )));
        }

        // check if the block is incompatible with a final block
        if !incomp.is_disjoint(
            &self
                .block_statuses
                .iter()
                .filter_map(|(h, s)| {
                    if let BlockStatus::Active(a) = s {
                        if a.is_final {
                            return Some(*h);
                        }
                    }
                    None
                })
                .collect(),
        ) {
            return Ok(HeaderCheckOutcome::Discard(DiscardReason::Stale));
        }
        massa_trace!("consensus.block_graph.check_header.ok", {
            "block_id": block_id
        });

        Ok(HeaderCheckOutcome::Proceed {
            parents_hash_period: parents,
            dependencies: deps,
            incompatibilities: incomp,
            inherited_incompatibilities_count: inherited_incomp_count,
        })
    }

    fn check_block(
        &self,
        block_id: &BlockId,
        block: &Block,
        operation_set: &HashMap<OperationId, (usize, u64)>,
        pos: &mut ProofOfStake,
        current_slot: Option<Slot>,
    ) -> Result<BlockCheckOutcome, ConsensusError> {
        massa_trace!("consensus.block_graph.check_block", {
            "block_id": block_id
        });
        let mut deps;
        let incomp;
        let parents;
        let inherited_incomp_count;

        // check header
        match self.check_header(block_id, &block.header, pos, current_slot)? {
            HeaderCheckOutcome::Proceed {
                parents_hash_period,
                dependencies,
                incompatibilities,
                inherited_incompatibilities_count,
            } => {
                // block_changes can be ignored as it is empty, (maybe add an error if not)
                parents = parents_hash_period;
                deps = dependencies;
                incomp = incompatibilities;
                inherited_incomp_count = inherited_incompatibilities_count;
            }
            HeaderCheckOutcome::Discard(reason) => return Ok(BlockCheckOutcome::Discard(reason)),
            HeaderCheckOutcome::WaitForDependencies(deps) => {
                return Ok(BlockCheckOutcome::WaitForDependencies(deps))
            }
            HeaderCheckOutcome::WaitForSlot => return Ok(BlockCheckOutcome::WaitForSlot),
        }

        // check operations
        let (operations_deps, block_ledger_changes, roll_updates) =
            match self.check_operations(block, operation_set, pos)? {
                BlockOperationsCheckOutcome::Proceed {
                    dependencies,
                    block_ledger_changes,
                    roll_updates,
                } => (dependencies, block_ledger_changes, roll_updates),
                BlockOperationsCheckOutcome::Discard(reason) => {
                    return Ok(BlockCheckOutcome::Discard(reason))
                }
                BlockOperationsCheckOutcome::WaitForDependencies(deps) => {
                    return Ok(BlockCheckOutcome::WaitForDependencies(deps))
                }
            };
        deps.extend(operations_deps);

        massa_trace!("consensus.block_graph.check_block.ok", {
            "block_id": block_id
        });

        Ok(BlockCheckOutcome::Proceed {
            parents_hash_period: parents,
            dependencies: deps,
            incompatibilities: incomp,
            inherited_incompatibilities_count: inherited_incomp_count,
            block_ledger_changes,
            roll_updates,
        })
    }

    /// Check if operations are consistent.
    ///
    /// Returns changes done by that block to the ledger (one hashmap per thread) and rolls
    /// consensus/pos.md#block-reception-process
    fn check_operations(
        &self,
        block_to_check: &Block,
        operation_set: &HashMap<OperationId, (usize, u64)>,
        pos: &ProofOfStake,
    ) -> Result<BlockOperationsCheckOutcome, ConsensusError> {
        // check that ops are not reused in previous blocks. Note that in-block reuse was checked in protocol.
        let mut dependencies: HashSet<BlockId> = HashSet::new();
        for operation in block_to_check.operations.iter() {
            // get thread
            let op_thread = match Address::from_public_key(&operation.content.sender_public_key) {
                Ok(addr) => addr.get_thread(self.cfg.thread_count),
                Err(err) => {
                    warn!(
                        "block graph check_operations error, bad operation sender_public_key address :{}",
                        err
                    );
                    return Ok(BlockOperationsCheckOutcome::Discard(
                        DiscardReason::Invalid(format!(
                            "bad operation sender_public_key address :{}",
                            err
                        )),
                    ));
                }
            };

            let op_start_validity_period = *operation
                .get_validity_range(self.cfg.operation_validity_periods)
                .start();

            let mut current_block_id = block_to_check.header.content.parents[op_thread as usize]; // non-genesis => has parents
            loop {
                //get block to process.
                let current_block = match self.block_statuses.get(&current_block_id) {
                    Some(block) => match block {
                        BlockStatus::Active(block) => block,
                        _ => return Err(ConsensusError::ContainerInconsistency(format!("block {:?} is not active but is an ancestor of a potentially active block", current_block_id))),
                    },
                    None => {
                        let mut missing_deps = HashSet::with_capacity(1);
                        missing_deps.insert(current_block_id);
                        return Ok(BlockOperationsCheckOutcome::WaitForDependencies(missing_deps));
                    }
                };

                // stop at op validity start
                if current_block.block.header.content.slot.period < op_start_validity_period {
                    break; // next op.
                }

                // check if present
                if current_block
                    .operation_set
                    .keys()
                    .any(|k| operation_set.contains_key(k))
                {
                    error!("block graph check_operations error, block operation already integrated in another block");
                    return Ok(BlockOperationsCheckOutcome::Discard(
                        DiscardReason::Invalid(
                            "Block operation already integrated in another block".to_string(),
                        ),
                    ));
                }
                dependencies.insert(current_block_id);

                if current_block.parents.is_empty() {
                    //genesis block found
                    break;
                }

                current_block_id = current_block.parents[op_thread as usize].0;
            }
        }

        //union for operation in block { operation.get_involved_addresses(block.creator.address) } union block.creator.address;
        //with only the addresses in the block's thread retained
        let block_creator_address =
            match Address::from_public_key(&block_to_check.header.content.creator) {
                Ok(addr) => addr,
                Err(err) => {
                    warn!(
                        "block graph check_operations error, bad block creator address :{}",
                        err
                    );
                    return Ok(BlockOperationsCheckOutcome::Discard(
                        DiscardReason::Invalid(format!("bad block creator address :{}", err)),
                    ));
                }
            };
        let mut ledger_involved_addresses: HashSet<Address> = HashSet::new();
        let mut roll_involved_addresses: HashSet<Address> = HashSet::new();
        ledger_involved_addresses.insert(block_creator_address);
        for op in block_to_check.operations.iter() {
            match op.get_ledger_involved_addresses(Some(block_creator_address)) {
                Ok(addrs) => {
                    ledger_involved_addresses.extend(addrs);
                }
                Err(err) => {
                    warn!(
                        "block graph check_operations error, error during get_ledger_involved_addresses :{}",
                        err
                    );
                    return Ok(BlockOperationsCheckOutcome::Discard(
                        DiscardReason::Invalid(format!(
                            "error during get_ledger_involved_addresses :{}",
                            err
                        )),
                    ));
                }
            };

            // (step 2 in consensus/pos.md)
            match op.get_roll_involved_addresses() {
                Ok(addrs) => {
                    roll_involved_addresses.extend(addrs);
                }
                Err(err) => {
                    warn!(
                        "block graph check_operations error, error during get_roll_involved_addresses :{}",
                        err
                    );
                    return Ok(BlockOperationsCheckOutcome::Discard(
                        DiscardReason::Invalid(format!(
                            "error during get_roll_involved_addresses :{}",
                            err
                        )),
                    ));
                }
            };
        }
        ledger_involved_addresses.retain(|addr| {
            addr.get_thread(self.cfg.thread_count) == block_to_check.header.content.slot.thread
        });
        roll_involved_addresses.retain(|addr| {
            addr.get_thread(self.cfg.thread_count) == block_to_check.header.content.slot.thread
        });
        let mut current_ledger = match self.get_ledger_at_parents(
            &block_to_check.header.content.parents,
            &ledger_involved_addresses,
        ) {
            Ok(ledger) => ledger,
            Err(err) => {
                warn!(
                    "block graph check_operations error, error retrieving ledger at parents :{}",
                    err
                );
                return Ok(BlockOperationsCheckOutcome::Discard(
                    DiscardReason::Invalid(format!("error retrieving ledger at parents :{}", err)),
                ));
            }
        };

        // (step 3 in consensus/pos.md)
        let parent_id = block_to_check.header.content.parents
            [block_to_check.header.content.slot.thread as usize];
        // will not panic: tested before
        let parent_cycle = self
            .get_active_block(&parent_id)
            .unwrap()
            .block
            .header
            .content
            .slot
            .get_cycle(self.cfg.periods_per_cycle);

        let (mut roll_counts, mut cycle_roll_updates) =
            self.get_roll_data_at_parent(parent_id, Some(&roll_involved_addresses), pos)?;

        if block_to_check
            .header
            .content
            .slot
            .get_cycle(self.cfg.periods_per_cycle)
            != parent_cycle
        {
            // if parent is from a different cycle: reset roll updates
            // (step 3.1 in consensus/pos.md)
            cycle_roll_updates = RollUpdates::new();
        }
        // here, cycle_roll_updates is compensated

        // block roll updates
        // (step 4 in consensus/pos.md)
        let mut roll_updates = RollUpdates::new();

        let mut block_ledger_changes: Vec<HashMap<Address, LedgerChange>> =
            vec![HashMap::new(); self.cfg.thread_count as usize];

        // block constant reward
        let creator_thread = block_creator_address.get_thread(self.cfg.thread_count);
        let reward_ledger_change = LedgerChange {
            balance_delta: self.cfg.block_reward,
            balance_increment: true,
        };
        let reward_change = (&block_creator_address, &reward_ledger_change);
        if creator_thread == block_to_check.header.content.slot.thread {
            if let Err(err) = current_ledger.apply_change(reward_change) {
                warn!("block graph check_operations error, can't apply reward_change to block current_ledger: {}", err);
                return Ok(BlockOperationsCheckOutcome::Discard(
                    DiscardReason::Invalid(format!(
                        "can't apply reward_change to block current_ledger: {}",
                        err
                    )),
                ));
            }
        }
        match block_ledger_changes[creator_thread as usize].entry(block_creator_address) {
            hash_map::Entry::Occupied(mut occ) => {
                if let Err(err) = occ.get_mut().chain(&reward_ledger_change) {
                    warn!(
                        "block graph check_operations error, can't chain reward change :{}",
                        err
                    );
                    return Ok(BlockOperationsCheckOutcome::Discard(
                        DiscardReason::Invalid(format!("Can't chain reward change :{}", err)),
                    ));
                }
            }
            hash_map::Entry::Vacant(vac) => {
                vac.insert(reward_ledger_change);
            }
        }

        // crediting roll sales after a lock cycle
        // (step 5 in consensus/pos.md)
        if parent_cycle
            != block_to_check
                .header
                .content
                .slot
                .get_cycle(self.cfg.periods_per_cycle)
        {
            // We credit addresses that sold a roll after a lock cycle.
            // (step 5.1 in consensus/pos.md)
            let thread = block_to_check.header.content.slot.thread;
            let credits = pos
                .get_roll_sell_credit(block_to_check.header.content.slot)?
                .into_iter()
                .filter_map(|(addr, amount)| {
                    if addr.get_thread(self.cfg.thread_count) == thread {
                        Some((
                            addr,
                            LedgerChange {
                                balance_delta: amount,
                                balance_increment: true,
                            },
                        ))
                    } else {
                        None
                    }
                });

            for (addr, change) in credits {
                if let Err(err) = current_ledger.apply_change((&addr, &change)) {
                    warn!("block graph check_operations error, can't apply cycle sell credit to block current_ledger :{}", err);
                    return Ok(BlockOperationsCheckOutcome::Discard(
                        DiscardReason::Invalid(format!(
                            "can't apply cycle sell credit to block current_ledger :{}",
                            err
                        )),
                    ));
                }
                // add ledger change to block ledger changes
                match block_ledger_changes[thread as usize].entry(addr) {
                    hash_map::Entry::Occupied(mut occ) => {
                        if let Err(err) = occ.get_mut().chain(&change) {
                            warn!(
                                "block graph check_operations error, can't chain cycle sell credit :{}",
                                err
                            );
                            return Ok(BlockOperationsCheckOutcome::Discard(
                                DiscardReason::Invalid(format!(
                                    "can't chain cycle sell credit :{}",
                                    err
                                )),
                            ));
                        }
                    }
                    hash_map::Entry::Vacant(vac) => {
                        vac.insert(change);
                    }
                }
            }
        }

        // all operations
        // (including step 6 in consensus/pos.md)
        for operation in block_to_check.operations.iter() {
            let op_roll_updates = operation.get_roll_updates()?;

            // (step 6.1 in consensus/pos.md)
            if let Err(err) = roll_counts.apply_subset(&op_roll_updates, None) {
                warn!("could not apply roll update in block: {}", err);
                return Ok(BlockOperationsCheckOutcome::Discard(
                    DiscardReason::Invalid(format!(
                        "could not apply roll update in block: {}",
                        err
                    )),
                ));
            }

            // chain block roll updates, ignore compensations (step 6.2 in consensus/pos.md)
            if let Err(err) = roll_updates.chain_subset(&op_roll_updates, None) {
                warn!("could not chain roll update in block: {}", err);
                return Ok(BlockOperationsCheckOutcome::Discard(
                    DiscardReason::Invalid(format!(
                        "could not chain roll update in block: {}",
                        err
                    )),
                ));
            }

            // chain cycle roll updates, apply compensation reimbursements (step 6.3.1 in consensus/pos.md)
            match cycle_roll_updates.chain_subset(&op_roll_updates, None) {
                Ok(compensations) => {
                    for (compensation_addr, compensation_count) in compensations.into_iter() {
                        let balance_delta =
                            match compensation_count.0.checked_mul(self.cfg.roll_price) {
                                Some(v) => v,
                                None => {
                                    return Ok(BlockOperationsCheckOutcome::Discard(
                                        DiscardReason::Invalid(
                                            "overflow on roll compensation sale price".into(),
                                        ),
                                    ));
                                }
                            };
                        let compensation_ledger_change = LedgerChange {
                            balance_delta,
                            balance_increment: true,
                        };

                        // try apply compensations to current_ledger
                        // (step 6.3.1 in consensus/pos.md)
                        let compensation_change = (&compensation_addr, &compensation_ledger_change);
                        if let Err(err) = current_ledger.apply_change(compensation_change) {
                            warn!("block graph check_operations error, can't apply compensation_change to block current_ledger: {}", err);
                            return Ok(BlockOperationsCheckOutcome::Discard(
                                DiscardReason::Invalid(format!(
                                    "can't apply compensation_change to block current_ledger: {}",
                                    err
                                )),
                            ));
                        }

                        // try apply compensations to block_ledger_changes
                        // (step 6.3.2 in consensus/pos.md)
                        match block_ledger_changes
                            [compensation_addr.get_thread(self.cfg.thread_count) as usize]
                            .entry(compensation_addr)
                        {
                            hash_map::Entry::Occupied(mut occ) => {
                                if let Err(err) = occ.get_mut().chain(&compensation_ledger_change) {
                                    warn!(
                                        "block graph check_operations error, can't chain roll compensation change :{}",
                                        err
                                    );
                                    return Ok(BlockOperationsCheckOutcome::Discard(
                                        DiscardReason::Invalid(format!(
                                            "Can't chain roll compensation change :{}",
                                            err
                                        )),
                                    ));
                                }
                            }
                            hash_map::Entry::Vacant(vac) => {
                                vac.insert(compensation_ledger_change);
                            }
                        }
                    }
                }
                Err(err) => {
                    warn!("could not chain roll update in block: {}", err);
                    return Ok(BlockOperationsCheckOutcome::Discard(
                        DiscardReason::Invalid(format!(
                            "could not chain roll update in block: {}",
                            err
                        )),
                    ));
                }
            }

            let op_ledger_changes = match operation.get_ledger_changes(
                &block_creator_address,
                self.cfg.thread_count,
                self.cfg.roll_price,
            ) {
                Err(err) => {
                    warn!(
                        "block graph check_operations error, can't get changes :{}",
                        err
                    );
                    return Ok(BlockOperationsCheckOutcome::Discard(
                        DiscardReason::Invalid(format!("can't get changes :{}", err)),
                    ));
                }
                Ok(op_ledger_changes) => op_ledger_changes,
            };

            for (thread, op_thread_changes) in op_ledger_changes.into_iter().enumerate() {
                for (change_addr, op_change) in op_thread_changes.into_iter() {
                    // apply change to ledger and check if ok
                    if thread == (block_to_check.header.content.slot.thread as usize) {
                        if let Err(err) = current_ledger.apply_change((&change_addr, &op_change)) {
                            warn!("block graph check_operations error, can't apply change to block current_ledger :{}", err);
                            return Ok(BlockOperationsCheckOutcome::Discard(
                                DiscardReason::Invalid(format!(
                                    "can't apply change to block current_ledger :{}",
                                    err
                                )),
                            ));
                        }
                    }
                    // add change to block changes
                    match block_ledger_changes[thread].entry(change_addr) {
                        hash_map::Entry::Occupied(mut occ) => {
                            if let Err(err) = occ.get_mut().chain(&op_change) {
                                warn!(
                                    "block graph check_operations error, can't chain change :{}",
                                    err
                                );
                                return Ok(BlockOperationsCheckOutcome::Discard(
                                    DiscardReason::Invalid(format!("can't chain change :{}", err)),
                                ));
                            }
                        }
                        hash_map::Entry::Vacant(vac) => {
                            vac.insert(op_change);
                        }
                    }
                }
            }
        }

        Ok(BlockOperationsCheckOutcome::Proceed {
            dependencies,
            block_ledger_changes,
            roll_updates,
        })
    }

    pub fn get_genesis_block_ids(&self) -> &Vec<BlockId> {
        &self.genesis_hashes
    }

    /// Compute ledger subset after given parents for given addresses
    pub fn get_ledger_at_parents(
        &self,
        parents: &[BlockId],
        query_addrs: &HashSet<Address>,
    ) -> Result<LedgerSubset, ConsensusError> {
        // check that all addresses belong to threads with parents later or equal to the latest_final_block of that thread
        let involved_threads: HashSet<u8> = query_addrs
            .iter()
            .map(|addr| addr.get_thread(self.cfg.thread_count))
            .collect();
        for thread in involved_threads.into_iter() {
            match self.block_statuses.get(&parents[thread as usize]) {
                Some(BlockStatus::Active(b)) => {
                    if b.block.header.content.slot.period
                        < self.latest_final_blocks_periods[thread as usize].1
                    {
                        return Err(ConsensusError::ContainerInconsistency(format!(
                            "asking for operations in thread {:?}, for which the given parent is older than the latest final block of that thread",
                            thread
                        )));
                    }
                }
                _ => {
                    return Err(ConsensusError::ContainerInconsistency(format!(
                        "parent block missing or in non-active state: {:?}",
                        parents[thread as usize]
                    )));
                }
            }
        }

        // compute backtrack ending slots for each thread
        let mut stop_periods =
            vec![vec![0u64; self.cfg.thread_count as usize]; self.cfg.thread_count as usize];
        for target_thread in 0u8..self.cfg.thread_count {
            let (target_last_final_id, target_last_final_period) =
                self.latest_final_blocks_periods[target_thread as usize];
            match self.block_statuses.get(&target_last_final_id) {
                Some(BlockStatus::Active(b)) => {
                    if !b.parents.is_empty() {
                        stop_periods[target_thread as usize] =
                            b.parents.iter().map(|(_id, period)| period + 1).collect();
                    }
                }
                _ => {
                    return Err(ConsensusError::ContainerInconsistency(format!(
                        "last final block missing or in non-active state: {:?}",
                        target_last_final_id
                    )));
                }
            }
            stop_periods[target_thread as usize][target_thread as usize] =
                target_last_final_period + 1;
        }

        // backtrack blocks starting from parents
        let mut ancestry: HashSet<BlockId> = HashSet::new();
        let mut to_scan: Vec<BlockId> = parents.to_vec();
        let mut accumulated_changes: Vec<HashMap<Address, LedgerChange>> =
            vec![HashMap::new(); self.cfg.thread_count as usize];
        while let Some(scan_b_id) = to_scan.pop() {
            // insert into ancestry, ignore if already scanned
            if !ancestry.insert(scan_b_id) {
                continue;
            }

            // get block, quit if not found or not active
            let scan_b = match self.block_statuses.get(&scan_b_id) {
                Some(BlockStatus::Active(b)) => b,
                _ => {
                    return Err(ConsensusError::ContainerInconsistency(format!(
                        "missing or not active block during ancestry traversal: {:?}",
                        scan_b_id
                    )));
                }
            };

            // accumulate ledger changes
            // Warning 1: this uses ledger change commutativity and associativity, may not work with smart contracts
            // Warning 2: we assume that overflows cannot happen here (they won't be deterministic)
            let mut explore_parents = false;
            for thread in 0u8..self.cfg.thread_count {
                if scan_b.block.header.content.slot.period
                    < stop_periods[thread as usize]
                        [scan_b.block.header.content.slot.thread as usize]
                {
                    continue;
                }
                explore_parents = true;

                for (addr, changes) in scan_b.block_ledger_change[thread as usize].iter() {
                    if !query_addrs.contains(addr) {
                        continue;
                    }
                    match accumulated_changes[thread as usize].entry(*addr) {
                        hash_map::Entry::Occupied(mut occ) => {
                            occ.get_mut().chain(changes)?;
                        }
                        hash_map::Entry::Vacant(vac) => {
                            vac.insert(changes.clone());
                        }
                    }
                }
            }

            // if this ancestor is still useful for the ledger of some thread, explore its parents
            if explore_parents {
                to_scan.extend(scan_b.parents.iter().map(|(id, _period)| id));
            }
        }

        // get final ledger and apply changes to it
        let mut res_ledger = self.ledger.get_final_ledger_subset(query_addrs)?;
        for thread in 0u8..self.cfg.thread_count {
            for (addr, change) in accumulated_changes[thread as usize].iter() {
                res_ledger.apply_change((addr, change))?;
            }
        }

        Ok(res_ledger)
    }

    /// Computes max cliques of compatible blocks
    fn compute_max_cliques(&self) -> Vec<HashSet<BlockId>> {
        let mut max_cliques: Vec<HashSet<BlockId>> = Vec::new();

        // algorithm adapted from IK_GPX as summarized in:
        //   Cazals et al., "A note on the problem of reporting maximal cliques"
        //   Theoretical Computer Science, 2008
        //   https://doi.org/10.1016/j.tcs.2008.05.010

        // stack: r, p, x
        let mut stack: Vec<(HashSet<BlockId>, HashSet<BlockId>, HashSet<BlockId>)> = vec![(
            HashSet::new(),
            self.gi_head.keys().cloned().collect(),
            HashSet::new(),
        )];
        while let Some((r, mut p, mut x)) = stack.pop() {
            if p.is_empty() && x.is_empty() {
                max_cliques.push(r);
                continue;
            }
            // choose the pivot vertex following the GPX scheme:
            // u_p = node from (p \/ x) that maximizes the cardinality of (P \ Neighbors(u_p, GI))
            let &u_p = p
                .union(&x)
                .max_by_key(|&u| {
                    p.difference(&(&self.gi_head[u] | &vec![*u].into_iter().collect()))
                        .count()
                })
                .unwrap(); // p was checked to be non-empty before

            // iterate over u_set = (p /\ Neighbors(u_p, GI))
            let u_set: HashSet<BlockId> =
                &p & &(&self.gi_head[&u_p] | &vec![u_p].into_iter().collect());
            for u_i in u_set.into_iter() {
                p.remove(&u_i);
                let u_i_set: HashSet<BlockId> = vec![u_i].into_iter().collect();
                let comp_n_u_i: HashSet<BlockId> = &self.gi_head[&u_i] | &u_i_set;
                stack.push((&r | &u_i_set, &p - &comp_n_u_i, &x - &comp_n_u_i));
                x.insert(u_i);
            }
        }
        if max_cliques.is_empty() {
            // make sure at least one clique remains
            max_cliques = vec![HashSet::new()];
        }
        max_cliques
    }

    fn add_block_to_graph(
        &mut self,
        hash: BlockId,
        parents_hash_period: Vec<(BlockId, u64)>,
        block: Block,
        deps: HashSet<BlockId>,
        incomp: HashSet<BlockId>,
        inherited_incomp_count: usize,
        block_ledger_change: Vec<HashMap<Address, LedgerChange>>,
        operation_set: HashMap<OperationId, (usize, u64)>,
        addresses_to_operations: HashMap<Address, HashSet<OperationId>>,
        roll_updates: RollUpdates,
    ) -> Result<(), ConsensusError> {
        massa_trace!("consensus.block_graph.add_block_to_graph", { "hash": hash });
        // add block to status structure
        self.block_statuses.insert(
            hash,
            BlockStatus::Active(ActiveBlock {
                parents: parents_hash_period.clone(),
                dependencies: deps,
                descendants: HashSet::new(),
                block: block.clone(),
                children: vec![HashMap::new(); self.cfg.thread_count as usize],
                is_final: false,
                block_ledger_change,
                operation_set,
                addresses_to_operations,
                roll_updates,
            }),
        );

        // add as child to parents
        for (parent_h, _parent_period) in parents_hash_period.iter() {
            if let Some(BlockStatus::Active(a_parent)) = self.block_statuses.get_mut(parent_h) {
                a_parent.children[block.header.content.slot.thread as usize]
                    .insert(hash, block.header.content.slot.period);
            } else {
                return Err(ConsensusError::ContainerInconsistency(format!(
                    "inconsistency inside block statuses adding child {:?} of block {:?}",
                    hash, parent_h
                )));
            }
        }

        // add as descendant to ancestors. Note: descendants are never removed.
        {
            let mut ancestors: VecDeque<BlockId> =
                parents_hash_period.iter().map(|(h, _)| *h).collect();
            let mut visited: HashSet<BlockId> = HashSet::new();
            while let Some(ancestor_h) = ancestors.pop_back() {
                if !visited.insert(ancestor_h) {
                    continue;
                }
                if let Some(BlockStatus::Active(ab)) = self.block_statuses.get_mut(&ancestor_h) {
                    ab.descendants.insert(hash);
                    for (ancestor_parent_h, _) in ab.parents.iter() {
                        ancestors.push_front(*ancestor_parent_h);
                    }
                }
            }
        }

        // add incompatibilities to gi_head
        massa_trace!(
            "consensus.block_graph.add_block_to_graph.add_incompatibilities",
            {}
        );
        for incomp_h in incomp.iter() {
            self.gi_head
                .get_mut(incomp_h)
                .ok_or(ConsensusError::MissingBlock)?
                .insert(hash);
        }
        self.gi_head.insert(hash, incomp.clone());

        // max cliques update
        massa_trace!(
            "consensus.block_graph.add_block_to_graph.max_cliques_update",
            {}
        );
        if incomp.len() == inherited_incomp_count {
            // clique optimization routine:
            //   the block only has incompatibilities inherited from its parents
            //   therefore it is not forking and can simply be added to the cliques it is compatible with
            self.max_cliques
                .iter_mut()
                .filter(|c| incomp.is_disjoint(c))
                .for_each(|c| {
                    c.insert(hash);
                });
        } else {
            // fully recompute max cliques
            massa_trace!(
                "consensus.block_graph.add_block_to_graph.clique_full_computing",
                { "hash": hash }
            );
            let before = self.max_cliques.len();
            self.max_cliques = self.compute_max_cliques();
            let after = self.max_cliques.len();
            if before != after {
                massa_trace!(
                    "consensus.block_graph.add_block_to_graph.clique_full_computing more than one clique",
                    { "cliques": self.max_cliques, "gi_head": self.gi_head }
                );
                //gi_head
                warn!(
                    "clique number went from {:?} to {:?} after adding {:?}",
                    before, after, hash
                );
            }
        }

        // compute clique fitnesses and find blockclique
        massa_trace!("consensus.block_graph.add_block_to_graph.compute_clique_fitnesses_and_find_blockclique", {});
        // note: clique_fitnesses is pair (fitness, -hash_sum) where the second parameter is negative for sorting
        let mut clique_fitnesses = vec![(0u64, num::BigInt::default()); self.max_cliques.len()];
        let mut blockclique_i = 0usize;
        for (clique_i, clique) in self.max_cliques.iter().enumerate() {
            let mut sum_fit: u64 = 0;
            let mut sum_hash = num::BigInt::default();
            for block_h in clique.iter() {
                sum_fit = sum_fit
                    .checked_add(
                        BlockGraph::get_full_active_block(&self.block_statuses, *block_h)
                            .ok_or_else(|| ConsensusError::ContainerInconsistency(format!("inconsistency inside block statuses computing fitness while adding {:?} - missing {:?}", hash, block_h)))?
                            .fitness(),
                    )
                    .ok_or(ConsensusError::FitnessOverflow)?;
                sum_hash -=
                    num::BigInt::from_bytes_be(num::bigint::Sign::Plus, &block_h.to_bytes());
            }
            clique_fitnesses[clique_i] = (sum_fit, sum_hash);
            if clique_fitnesses[clique_i] > clique_fitnesses[blockclique_i] {
                blockclique_i = clique_i;
            }
        }

        // update best parents
        massa_trace!(
            "consensus.block_graph.add_block_to_graph.update_best_parents",
            {}
        );
        {
            let blockclique = &self.max_cliques[blockclique_i];
            let mut parents_updated = 0u8;
            for block_h in blockclique.iter() {
                let block_a = BlockGraph::get_full_active_block(&self.block_statuses, *block_h)
                    .ok_or_else(|| ConsensusError::ContainerInconsistency(format!("inconsistency inside block statuses updating best parents while adding {:?} - missing {:?}", hash, block_h)))?;
                if blockclique.is_disjoint(
                    &block_a.children[block_a.block.header.content.slot.thread as usize]
                        .keys()
                        .copied()
                        .collect(),
                ) {
                    self.best_parents[block_a.block.header.content.slot.thread as usize] = *block_h;
                    parents_updated += 1;
                    if parents_updated == self.cfg.thread_count {
                        break;
                    }
                }
            }
        }

        // list stale blocks
        massa_trace!(
            "consensus.block_graph.add_block_to_graph.list_stale_blocks",
            {}
        );
        let stale_blocks = {
            let fitness_threshold = clique_fitnesses[blockclique_i]
                .0
                .saturating_sub(self.cfg.delta_f0);
            // iterate from largest to smallest to minimize reallocations
            let mut indices: Vec<usize> = (0..self.max_cliques.len()).collect();
            indices.sort_unstable_by_key(|&i| std::cmp::Reverse(self.max_cliques[i].len()));
            let mut high_set: HashSet<BlockId> = HashSet::new();
            let mut low_set: HashSet<BlockId> = HashSet::new();
            let mut keep_mask = vec![true; self.max_cliques.len()];
            for clique_i in indices.into_iter() {
                if clique_fitnesses[clique_i].0 >= fitness_threshold {
                    high_set.extend(&self.max_cliques[clique_i]);
                } else {
                    low_set.extend(&self.max_cliques[clique_i]);
                    keep_mask[clique_i] = false;
                }
            }
            let mut clique_i = 0;
            self.max_cliques.retain(|_| {
                clique_i += 1;
                keep_mask[clique_i - 1]
            });
            clique_i = 0;
            clique_fitnesses.retain(|_| {
                clique_i += 1;
                if keep_mask[clique_i - 1] {
                    true
                } else {
                    if blockclique_i > clique_i - 1 {
                        blockclique_i -= 1;
                    }
                    false
                }
            });
            &low_set - &high_set
        };
        // mark stale blocks
        massa_trace!(
            "consensus.block_graph.add_block_to_graph.mark_stale_blocks",
            {}
        );
        for stale_block_hash in stale_blocks.into_iter() {
            if let Some(BlockStatus::Active(active_block)) =
                self.block_statuses.remove(&stale_block_hash)
            {
                if active_block.is_final {
                    return Err(ConsensusError::ContainerInconsistency(format!("inconsistency inside block statuses removing stale blocks adding {:?} - block {:?} was already final", hash, stale_block_hash)));
                }

                // remove from gi_head
                if let Some(other_incomps) = self.gi_head.remove(&stale_block_hash) {
                    for other_incomp in other_incomps.into_iter() {
                        if let Some(other_incomp_lst) = self.gi_head.get_mut(&other_incomp) {
                            other_incomp_lst.remove(&stale_block_hash);
                        }
                    }
                }

                // remove from cliques
                self.max_cliques.iter_mut().for_each(|c| {
                    c.remove(&stale_block_hash);
                });
                self.max_cliques.retain(|c| !c.is_empty()); // remove empty cliques
                if self.max_cliques.is_empty() {
                    // make sure at least one clique remains
                    self.max_cliques = vec![HashSet::new()];
                }

                // remove from parent's children
                for (parent_h, _parent_period) in active_block.parents.iter() {
                    if let Some(BlockStatus::Active(ActiveBlock { children, .. })) =
                        self.block_statuses.get_mut(parent_h)
                    {
                        children[active_block.block.header.content.slot.thread as usize]
                            .remove(&stale_block_hash);
                    }
                }

                massa_trace!("consensus.block_graph.add_block_to_graph.stale", {
                    "hash": stale_block_hash
                });
                // mark as stale
                self.new_stale_blocks
                    .insert(stale_block_hash, active_block.block.header.content.slot);
                self.block_statuses.insert(
                    stale_block_hash,
                    BlockStatus::Discarded {
                        header: active_block.block.header,
                        reason: DiscardReason::Stale,
                        sequence_number: BlockGraph::new_sequence_number(
                            &mut self.sequence_counter,
                        ),
                    },
                );
            } else {
                return Err(ConsensusError::ContainerInconsistency(format!("inconsistency inside block statuses removing stale blocks adding {:?} - block {:?} is missing", hash, stale_block_hash)));
            }
        }

        // list final blocks
        massa_trace!(
            "consensus.block_graph.add_block_to_graph.list_final_blocks",
            {}
        );
        let final_blocks = {
            // short-circuiting intersection of cliques from smallest to largest
            let mut indices: Vec<usize> = (0..self.max_cliques.len()).collect();
            indices.sort_unstable_by_key(|&i| self.max_cliques[i].len());
            let mut final_candidates = self.max_cliques[indices[0]].clone();
            for i in 1..indices.len() {
                final_candidates.retain(|v| self.max_cliques[i].contains(v));
                if final_candidates.is_empty() {
                    break;
                }
            }

            // restrict search to cliques with high enough fitness, sort cliques by fitness (highest to lowest)
            massa_trace!(
                "consensus.block_graph.add_block_to_graph.list_final_blocks.restrict",
                {}
            );
            indices.retain(|&i| clique_fitnesses[i].0 > self.cfg.delta_f0);
            indices.sort_unstable_by_key(|&i| std::cmp::Reverse(clique_fitnesses[i].0));

            let mut final_blocks: HashSet<BlockId> = HashSet::new();
            for clique_i in indices.into_iter() {
                massa_trace!(
                    "consensus.block_graph.add_block_to_graph.list_final_blocks.loop",
                    { "clique_i": clique_i }
                );
                // check in cliques from highest to lowest fitness
                if final_candidates.is_empty() {
                    // no more final candidates
                    break;
                }
                let clique = &self.max_cliques[clique_i];

                // compute the total fitness of all the descendants of the candidate within the clique
                let loc_candidates = final_candidates.clone();
                for candidate_h in loc_candidates.into_iter() {
                    let desc_fit: u64 =
                        BlockGraph::get_full_active_block(&self.block_statuses, candidate_h)
                            .ok_or(ConsensusError::MissingBlock)?
                            .descendants
                            .intersection(clique)
                            .map(|h| {
                                if let Some(BlockStatus::Active(ab)) = self.block_statuses.get(h) {
                                    return ab.fitness();
                                }
                                0
                            })
                            .sum();
                    if desc_fit > self.cfg.delta_f0 {
                        // candidate is final
                        final_candidates.remove(&candidate_h);
                        final_blocks.insert(candidate_h);
                    }
                }
            }
            final_blocks
        };

        // Save latest_final_blocks_periods for later use when updating the ledger.
        let old_latest_final_blocks_periods = self.latest_final_blocks_periods.clone();

        // mark final blocks and update latest_final_blocks_periods
        massa_trace!(
            "consensus.block_graph.add_block_to_graph.mark_final_blocks",
            {}
        );
        for final_block_hash in final_blocks.into_iter() {
            // remove from gi_head
            if let Some(other_incomps) = self.gi_head.remove(&final_block_hash) {
                for other_incomp in other_incomps.into_iter() {
                    if let Some(other_incomp_lst) = self.gi_head.get_mut(&other_incomp) {
                        other_incomp_lst.remove(&final_block_hash);
                    }
                }
            }

            // remove from cliques
            self.max_cliques.iter_mut().for_each(|c| {
                c.remove(&final_block_hash);
            });
            self.max_cliques.retain(|c| !c.is_empty()); // remove empty cliques
            if self.max_cliques.is_empty() {
                // make sure at least one clique remains
                self.max_cliques = vec![HashSet::new()];
            }

            // mark as final and update latest_final_blocks_periods
            if let Some(BlockStatus::Active(ActiveBlock {
                block: final_block,
                is_final,
                ..
            })) = self.block_statuses.get_mut(&final_block_hash)
            {
                massa_trace!("consensus.block_graph.add_block_to_graph.final", {
                    "hash": final_block_hash
                });
                *is_final = true;
                // update latest final blocks
                if final_block.header.content.slot.period
                    > self.latest_final_blocks_periods
                        [final_block.header.content.slot.thread as usize]
                        .1
                {
                    self.latest_final_blocks_periods
                        [final_block.header.content.slot.thread as usize] =
                        (final_block_hash, final_block.header.content.slot.period);
                }
                // update new final blocks list
                self.new_final_blocks.insert(final_block_hash);
            } else {
                return Err(ConsensusError::ContainerInconsistency(format!("inconsistency inside block statuses updating final blocks adding {:?} - block {:?} is missing", hash, final_block_hash)));
            }
        }

        // list threads where latest final block changed
        let changed_threads_old_block_thread_id_period = self
            .latest_final_blocks_periods
            .iter()
            .enumerate()
            .filter_map(|(thread, (b_id, _b_period))| {
                let (old_b_id, old_period) = &old_latest_final_blocks_periods[thread];
                if b_id != old_b_id {
                    return Some((thread as u8, old_b_id, old_period));
                }
                None
            });

        // Update ledger with changes from final blocks, "B2".
        for (changed_thread, old_block_id, old_period) in changed_threads_old_block_thread_id_period
        {
            // Get the old block
            let old_block = match self.block_statuses.get(old_block_id) {
                Some(BlockStatus::Active(latest)) => latest,
                _ => return Err(ConsensusError::ContainerInconsistency(format!("inconsistency inside block statuses updating final blocks - active old latest final block {:?} is missing in thread {:?}", old_block_id, changed_thread))),
            };

            // Get the latest final in the same thread.
            let latest_final_in_thread_id =
                self.latest_final_blocks_periods[changed_thread as usize].0;

            // Init the stop backtrack stop periods
            let mut stop_backtrack_periods = vec![0u64; self.cfg.thread_count as usize];
            for limit_thread in 0u8..self.cfg.thread_count {
                if limit_thread == changed_thread {
                    // in the same thread, set the stop backtrack period to B1.period + 1
                    stop_backtrack_periods[limit_thread as usize] = old_period + 1;
                } else if !old_block.parents.is_empty() {
                    // In every other thread, set it to B1.parents[tau*].period + 1
                    stop_backtrack_periods[limit_thread as usize] =
                        old_block.parents[limit_thread as usize].1 + 1;
                }
            }

            // Backtrack blocks starting from B2.
            let mut ancestry: HashSet<BlockId> = HashSet::new();
            let mut to_scan: Vec<BlockId> = vec![latest_final_in_thread_id]; // B2
            let mut accumulated_changes: HashMap<Address, LedgerChange> = HashMap::new();
            while let Some(scan_b_id) = to_scan.pop() {
                // insert into ancestry, ignore if already scanned
                if !ancestry.insert(scan_b_id) {
                    continue;
                }

                // get block, quit if not found or not active
                let scan_b = match self.block_statuses.get(&scan_b_id) {
                    Some(BlockStatus::Active(b)) => b,
                    _ => return Err(ConsensusError::ContainerInconsistency(format!("inconsistency inside block statuses updating final blocks - block {:?} is missing", scan_b_id)))
                };

                // accumulate ledger changes
                // Warning 1: this uses ledger change commutativity and associativity, may not work with smart contracts
                // Warning 2: we assume that overflows cannot happen here (they won't be deterministic)
                if scan_b.block.header.content.slot.period
                    < stop_backtrack_periods[scan_b.block.header.content.slot.thread as usize]
                {
                    continue;
                }
                for (addr, changes) in scan_b.block_ledger_change[changed_thread as usize].iter() {
                    match accumulated_changes.entry(*addr) {
                        hash_map::Entry::Occupied(mut occ) => {
                            occ.get_mut().chain(changes)?;
                        }
                        hash_map::Entry::Vacant(vac) => {
                            vac.insert(changes.clone());
                        }
                    }
                }

                // Explore parents
                to_scan.extend(
                    scan_b
                        .parents
                        .iter()
                        .map(|(b_id, _period)| *b_id)
                        .collect::<Vec<BlockId>>(),
                );
            }

            // update ledger
            self.ledger.apply_final_changes(
                changed_thread,
                accumulated_changes.into_iter().collect(),
                self.latest_final_blocks_periods[changed_thread as usize].1,
            )?;
        }

        massa_trace!("consensus.block_graph.add_block_to_graph.end", {});
        Ok(())
    }

    // prune active blocks and return final blocks, return discarded final blocks
    fn prune_active(&mut self) -> Result<HashMap<BlockId, Block>, ConsensusError> {
        // list all active blocks
        let active_blocks: HashSet<BlockId> = self
            .block_statuses
            .iter()
            .filter_map(|(h, bs)| match bs {
                BlockStatus::Active(_) => Some(*h),
                _ => None,
            })
            .collect();

        let mut retain_active: HashSet<BlockId> = HashSet::new();

        let latest_final_blocks: Vec<BlockId> = self
            .latest_final_blocks_periods
            .iter()
            .map(|(hash, _)| *hash)
            .collect();

        // retain all non-final active blocks,
        // the current "best parents",
        // and the dependencies for both.
        for (hash, block_status) in self.block_statuses.iter() {
            if let BlockStatus::Active(ActiveBlock {
                is_final,
                dependencies,
                ..
            }) = block_status
            {
                if !*is_final
                    || self.best_parents.contains(hash)
                    || latest_final_blocks.contains(hash)
                {
                    retain_active.extend(dependencies);
                    retain_active.insert(*hash);
                }
            }
        }

        // retain best parents
        retain_active.extend(&self.best_parents);

        // retain last final blocks
        retain_active.extend(self.latest_final_blocks_periods.iter().map(|(h, _)| *h));

        for (thread, id) in latest_final_blocks.iter().enumerate() {
            let mut current_block_id = *id;
            while let Some(current_block) = self.get_active_block(&current_block_id) {
                // retain block
                retain_active.insert(current_block_id);

                // stop traversing when reaching a block with period number low enough
                // so that any of its operations will have their validity period expired at the latest final block in thread
                // note: one more is kept because of the way we iterate
                if current_block.block.header.content.slot.period
                    < self.latest_final_blocks_periods[thread]
                        .1
                        .saturating_sub(self.cfg.operation_validity_periods)
                {
                    break;
                }

                // if not genesis, traverse parent
                if current_block.block.header.content.parents.is_empty() {
                    break;
                }

                current_block_id = current_block.block.header.content.parents[thread as usize];
            }
        }

        // grow with parents & fill thread holes twice
        for _ in 0..2 {
            // retain the parents of the selected blocks
            let retain_clone = retain_active.clone();

            for retain_h in retain_clone.into_iter() {
                retain_active.extend(
                    self.get_active_block(&retain_h)
                        .ok_or_else(|| ConsensusError::ContainerInconsistency(format!("inconsistency inside block statuses pruning and retaining the parents of the selected blocks - {:?} is missing", retain_h)))?
                        .parents
                        .iter()
                        .map(|(b_id, _p)| *b_id),
                )
            }

            // find earliest kept slots in each thread
            let mut earliest_retained_periods: Vec<u64> = self
                .latest_final_blocks_periods
                .iter()
                .map(|(_, p)| *p)
                .collect();
            for retain_h in retain_active.iter() {
                let retain_slot = &self
                    .get_active_block(retain_h)
                    .ok_or_else(|| ConsensusError::ContainerInconsistency(format!("inconsistency inside block statuses pruning and finding earliest kept slots in each thread - {:?} is missing", retain_h)))?
                    .block.header
                    .content
                    .slot;
                earliest_retained_periods[retain_slot.thread as usize] = std::cmp::min(
                    earliest_retained_periods[retain_slot.thread as usize],
                    retain_slot.period,
                );
            }

            // fill up from the latest final block back to the earliest for each thread
            for thread in 0..self.cfg.thread_count {
                let mut cursor = self.latest_final_blocks_periods[thread as usize].0; // hash of tha latest final in that thread
                while let Some(c_block) = self.get_active_block(&cursor) {
                    if c_block.block.header.content.slot.period
                        < earliest_retained_periods[thread as usize]
                    {
                        break;
                    }
                    retain_active.insert(cursor);
                    if c_block.parents.is_empty() {
                        // genesis
                        break;
                    }
                    cursor = c_block.parents[thread as usize].0;
                }
            }
        }

        // remove unused final active blocks
        let mut discarded_finals: HashMap<BlockId, Block> = HashMap::new();
        for discard_active_h in active_blocks.difference(&retain_active) {
            let discarded_active = if let Some(BlockStatus::Active(discarded_active)) =
                self.block_statuses.remove(discard_active_h)
            {
                discarded_active
            } else {
                return Err(ConsensusError::ContainerInconsistency(format!("inconsistency inside block statuses pruning and removing unused final active blocks - {:?} is missing", discard_active_h)));
            };

            // remove from parent's children
            for (parent_h, _parent_period) in discarded_active.parents.iter() {
                if let Some(BlockStatus::Active(ActiveBlock { children, .. })) =
                    self.block_statuses.get_mut(parent_h)
                {
                    children[discarded_active.block.header.content.slot.thread as usize]
                        .remove(discard_active_h);
                }
            }

            massa_trace!("consensus.block_graph.prune_active", {"hash": discard_active_h, "reason": DiscardReason::Final});
            // mark as final
            self.block_statuses.insert(
                *discard_active_h,
                BlockStatus::Discarded {
                    header: discarded_active.block.header.clone(),
                    reason: DiscardReason::Final,
                    sequence_number: BlockGraph::new_sequence_number(&mut self.sequence_counter),
                },
            );

            discarded_finals.insert(*discard_active_h, discarded_active.block);
        }

        Ok(discarded_finals)
    }

    fn promote_dep_tree(&mut self, hash: BlockId) -> Result<(), ConsensusError> {
        let mut to_explore = vec![hash];
        let mut to_promote: HashMap<BlockId, (Slot, u64)> = HashMap::new();
        while let Some(h) = to_explore.pop() {
            if to_promote.contains_key(&h) {
                continue;
            }
            if let Some(BlockStatus::WaitingForDependencies {
                header_or_block,
                unsatisfied_dependencies,
                sequence_number,
                ..
            }) = self.block_statuses.get(&h)
            {
                // promote current block
                to_promote.insert(h, (header_or_block.get_slot(), *sequence_number));
                // register dependencies for exploration
                to_explore.extend(unsatisfied_dependencies);
            }
        }

        let mut to_promote: Vec<(Slot, u64, BlockId)> = to_promote
            .into_iter()
            .map(|(h, (slot, seq))| (slot, seq, h))
            .collect();
        to_promote.sort_unstable(); // last ones should have the highest seq number
        for (_slot, _seq, h) in to_promote.into_iter() {
            if let Some(BlockStatus::WaitingForDependencies {
                sequence_number, ..
            }) = self.block_statuses.get_mut(&h)
            {
                *sequence_number = BlockGraph::new_sequence_number(&mut self.sequence_counter);
            }
        }
        Ok(())
    }

    fn prune_waiting_for_dependencies(&mut self) -> Result<(), ConsensusError> {
        let mut to_discard: HashMap<BlockId, Option<DiscardReason>> = HashMap::new();
        let mut to_keep: HashMap<BlockId, (u64, Slot)> = HashMap::new();

        // list items that are older than the latest final blocks in their threads or have deps that are discarded
        {
            for (hash, block_status) in self.block_statuses.iter() {
                if let BlockStatus::WaitingForDependencies {
                    header_or_block,
                    unsatisfied_dependencies,
                    sequence_number,
                } = block_status
                {
                    // has already discarded dependencies => discard (choose worst reason)
                    let mut discard_reason = None;
                    let mut discarded_dep_found = false;
                    for dep in unsatisfied_dependencies.iter() {
                        if let Some(BlockStatus::Discarded { reason, .. }) =
                            self.block_statuses.get(dep)
                        {
                            discarded_dep_found = true;
                            match reason {
                                DiscardReason::Invalid(reason) => {
                                    discard_reason = Some(DiscardReason::Invalid(format!("discarded because depend on block:{} that has discard reason:{}", hash, reason)));
                                    break;
                                }
                                DiscardReason::Stale => discard_reason = Some(DiscardReason::Stale),
                                DiscardReason::Final => discard_reason = Some(DiscardReason::Stale),
                            }
                        }
                    }
                    if discarded_dep_found {
                        to_discard.insert(*hash, discard_reason);
                        continue;
                    }

                    // is at least as old as the latest final block in its thread => discard as stale
                    let slot = header_or_block.get_slot();
                    if slot.period <= self.latest_final_blocks_periods[slot.thread as usize].1 {
                        to_discard.insert(*hash, Some(DiscardReason::Stale));
                        continue;
                    }

                    // otherwise, mark as to_keep
                    to_keep.insert(*hash, (*sequence_number, header_or_block.get_slot()));
                }
            }
        }

        // discard in chain and because of limited size
        while !to_keep.is_empty() {
            // mark entries as to_discard and remove them from to_keep
            for (hash, _old_order) in to_keep.clone().into_iter() {
                if let Some(BlockStatus::WaitingForDependencies {
                    unsatisfied_dependencies,
                    ..
                }) = self.block_statuses.get(&hash)
                {
                    // has dependencies that will be discarded => discard (choose worst reason)
                    let mut discard_reason = None;
                    let mut dep_to_discard_found = false;
                    for dep in unsatisfied_dependencies.iter() {
                        if let Some(reason) = to_discard.get(dep) {
                            dep_to_discard_found = true;
                            match reason {
                                Some(DiscardReason::Invalid(reason)) => {
                                    discard_reason = Some(DiscardReason::Invalid(format!("discarded because depend on block:{} that has discard reason:{}", hash, reason)));
                                    break;
                                }
                                Some(DiscardReason::Stale) => {
                                    discard_reason = Some(DiscardReason::Stale)
                                }
                                Some(DiscardReason::Final) => {
                                    discard_reason = Some(DiscardReason::Stale)
                                }
                                None => {} // leave as None
                            }
                        }
                    }
                    if dep_to_discard_found {
                        to_keep.remove(&hash);
                        to_discard.insert(hash, discard_reason);
                        continue;
                    }
                }
            }

            // remove worst excess element
            if to_keep.len() > self.cfg.max_dependency_blocks {
                let remove_elt = to_keep
                    .iter()
                    .filter_map(|(hash, _old_order)| {
                        if let Some(BlockStatus::WaitingForDependencies {
                            header_or_block,
                            sequence_number,
                            ..
                        }) = self.block_statuses.get(hash)
                        {
                            return Some((sequence_number, header_or_block.get_slot(), *hash));
                        }
                        None
                    })
                    .min();
                if let Some((_seq_num, _slot, hash)) = remove_elt {
                    to_keep.remove(&hash);
                    to_discard.insert(hash, None);
                    continue;
                }
            }

            // nothing happened: stop loop
            break;
        }

        // transition states to Discarded if there is a reason, otherwise just drop
        for (hash, reason_opt) in to_discard.drain() {
            if let Some(BlockStatus::WaitingForDependencies {
                header_or_block, ..
            }) = self.block_statuses.remove(&hash)
            {
                let header = match header_or_block {
                    HeaderOrBlock::Header(h) => h,
                    HeaderOrBlock::Block(b, _) => b.header,
                };
                massa_trace!("consensus.block_graph.prune_waiting_for_dependencies", {"hash": hash, "reason": reason_opt});

                if let Some(reason) = reason_opt {
                    // add to stats if reason is Stale
                    if reason == DiscardReason::Stale {
                        self.new_stale_blocks.insert(hash, header.content.slot);
                    }
                    // transition to Discarded only if there is a reason
                    self.block_statuses.insert(
                        hash,
                        BlockStatus::Discarded {
                            header,
                            reason,
                            sequence_number: BlockGraph::new_sequence_number(
                                &mut self.sequence_counter,
                            ),
                        },
                    );
                }
            }
        }

        Ok(())
    }

    fn prune_slot_waiting(&mut self) {
        let mut slot_waiting: Vec<(Slot, BlockId)> = self
            .block_statuses
            .iter()
            .filter_map(|(hash, block_status)| {
                if let BlockStatus::WaitingForSlot(header_or_block) = block_status {
                    return Some((header_or_block.get_slot(), *hash));
                }
                None
            })
            .collect();
        slot_waiting.sort_unstable();
        let retained: HashSet<BlockId> = slot_waiting
            .into_iter()
            .take(self.cfg.max_future_processing_blocks)
            .map(|(_slot, hash)| hash)
            .collect();
        self.block_statuses.retain(|hash, block_status| {
            if let BlockStatus::WaitingForSlot(_) = block_status {
                return retained.contains(hash);
            }
            true
        });
    }

    fn prune_discarded(&mut self) -> Result<(), ConsensusError> {
        let mut discard_hashes: Vec<(u64, BlockId)> = self
            .block_statuses
            .iter()
            .filter_map(|(hash, block_status)| {
                if let BlockStatus::Discarded {
                    sequence_number, ..
                } = block_status
                {
                    return Some((*sequence_number, *hash));
                }
                None
            })
            .collect();
        if discard_hashes.len() <= self.cfg.max_discarded_blocks {
            return Ok(());
        }
        discard_hashes.sort_unstable();
        discard_hashes.truncate(self.cfg.max_discarded_blocks);
        discard_hashes
            .into_iter()
            .take(self.cfg.max_discarded_blocks)
            .for_each(|(_period, hash)| {
                self.block_statuses.remove(&hash);
            });
        Ok(())
    }

    // prune and return final blocks, return discarded final blocks
    pub fn prune(&mut self) -> Result<HashMap<BlockId, Block>, ConsensusError> {
        let before = self.max_cliques.len();
        // Step 1: discard final blocks that are not useful to the graph anymore and return them
        let discarded_finals = self.prune_active()?;

        // Step 2: prune slot waiting blocks
        self.prune_slot_waiting();

        // Step 3: prune dependency waiting blocks
        self.prune_waiting_for_dependencies()?;

        // Step 4: prune discarded
        self.prune_discarded()?;

        let after = self.max_cliques.len();
        if before != after {
            warn!(
                "clique number went from {:?} to {:?} after pruning",
                before, after
            );
        }

        Ok(discarded_finals)
    }

    // get the current block wishlist
    pub fn get_block_wishlist(&self) -> Result<HashSet<BlockId>, ConsensusError> {
        let mut wishlist = HashSet::new();
        for block_status in self.block_statuses.values() {
            if let BlockStatus::WaitingForDependencies {
                unsatisfied_dependencies,
                ..
            } = block_status
            {
                for unsatisfied_h in unsatisfied_dependencies.iter() {
                    if let Some(BlockStatus::WaitingForDependencies {
                        header_or_block: HeaderOrBlock::Block(_, _),
                        ..
                    }) = self.block_statuses.get(unsatisfied_h)
                    {
                        // the full block is already available
                        continue;
                    }
                    wishlist.insert(*unsatisfied_h);
                }
            }
        }

        Ok(wishlist)
    }

    pub fn get_clique_count(&self) -> usize {
        self.max_cliques.len()
    }

    // Get the headers to be propagated.
    // Must be called by the consensus worker within `block_db_changed`.
    pub fn get_blocks_to_propagate(&mut self) -> HashMap<BlockId, Block> {
        mem::take(&mut self.to_propagate)
    }

    // Get the hashes of objects that were attack attempts.
    // Must be called by the consensus worker within `block_db_changed`.
    pub fn get_attack_attempts(&mut self) -> Vec<BlockId> {
        mem::take(&mut self.attack_attempts)
    }

    // Get the ids of blocks that became final.
    // Must be called by the consensus worker within `block_db_changed`.
    pub fn get_new_final_blocks(&mut self) -> HashSet<BlockId> {
        mem::take(&mut self.new_final_blocks)
    }

    // Get the ids of blocks that became stale.
    // Must be called by the consensus worker within `block_db_changed`.
    pub fn get_new_stale_blocks(&mut self) -> HashMap<BlockId, Slot> {
        mem::take(&mut self.new_stale_blocks)
    }
}

#[cfg(test)]
mod tests {
    use crypto::signature::{PrivateKey, PublicKey};
    use serial_test::serial;
    use std::{path::Path, usize};

    use super::*;
    use crate::tests::tools::get_dummy_block_id;
    use tempfile::NamedTempFile;
    use time::UTime;

    fn get_export_active_test_block() -> ExportActiveBlock {
        let block = Block {
            header: BlockHeader {
                content: BlockHeaderContent{
                    creator: crypto::signature::PublicKey::from_bs58_check("4vYrPNzUM8PKg2rYPW3ZnXPzy67j9fn5WsGCbnwAnk2Lf7jNHb").unwrap(),
                    operation_merkle_root: Hash::hash(&Vec::new()),
                    parents: vec![
                        get_dummy_block_id("parent1"),
                        get_dummy_block_id("parent2"),
                    ],
                    slot: Slot::new(1, 0),
                },
                signature: crypto::signature::Signature::from_bs58_check(
                    "5f4E3opXPWc3A1gvRVV7DJufvabDfaLkT1GMterpJXqRZ5B7bxPe5LoNzGDQp9LkphQuChBN1R5yEvVJqanbjx7mgLEae"
                ).unwrap()
            },
            operations: vec![]
        };

        ExportActiveBlock {
            parents: vec![
                (get_dummy_block_id("parent11"), 23),
                (get_dummy_block_id("parent12"), 24),
            ],
            dependencies: vec![
                get_dummy_block_id("dep11"),
                get_dummy_block_id("dep12"),
                get_dummy_block_id("dep13"),
            ]
            .into_iter()
            .collect(),
            block,
            children: vec![vec![
                (get_dummy_block_id("child11"), 31),
                (get_dummy_block_id("child11"), 31),
            ]
            .into_iter()
            .collect()],
            is_final: true,
            block_ledger_change: vec![
                vec![
                    (
                        Address::from_bytes(&Hash::hash("addr01".as_bytes()).into_bytes()).unwrap(),
                        LedgerChange {
                            balance_delta: 1,
                            balance_increment: true, // whether to increment or decrement balance of delta
                        },
                    ),
                    (
                        Address::from_bytes(&Hash::hash("addr02".as_bytes()).into_bytes()).unwrap(),
                        LedgerChange {
                            balance_delta: 2,
                            balance_increment: false, // whether to increment or decrement balance of delta
                        },
                    ),
                ],
                vec![(
                    Address::from_bytes(&Hash::hash("addr11".as_bytes()).into_bytes()).unwrap(),
                    LedgerChange {
                        balance_delta: 3,
                        balance_increment: false, // whether to increment or decrement balance of delta
                    },
                )],
            ],
            roll_updates: vec![],
        }
    }

    #[tokio::test]
    #[serial]
    pub async fn test_get_ledger_at_parents() {
        //     stderrlog::new()
        // .verbosity(4)
        // .timestamp(stderrlog::Timestamp::Millisecond)
        // .init()
        // .unwrap();
        let thread_count: u8 = 2;
        let active_block: ActiveBlock = get_export_active_test_block().try_into().unwrap();
        let ledger_file = generate_ledger_file(&HashMap::new());
        let mut cfg = example_consensus_config(ledger_file.path());

        cfg.block_reward = 1;
        //to generate address and public keys
        /*        let private_key = generate_random_private_key();
        let public_key = derive_public_key(&private_key);

        let add = Address::from_public_key(&public_key).unwrap();

        println!(
            "public key:{}, address:{}, th:{}",
            public_key.to_bs58_check(),
            add.to_bs58_check(),
            add.get_thread(thread_count)
        ); */

        //define addresses use for the test
        let pubkey_a =
            PublicKey::from_bs58_check("5UvFn66yoQerrEmikCxDVvhkLvCo9R2hJAYFMh2pZfYUQDMuCE")
                .unwrap();
        let address_a = Address::from_public_key(&pubkey_a).unwrap();
        assert_eq!(0, address_a.get_thread(thread_count));

        let pubkey_b =
            PublicKey::from_bs58_check("4uRbkzUvQwW19dD6cxQ9WiYo8BZTPQsmsCbBrFLxMiUYTSbo2p")
                .unwrap();
        let address_b = Address::from_public_key(&pubkey_b).unwrap();
        assert_eq!(1, address_b.get_thread(thread_count));

        let address_c =
            Address::from_bs58_check("2cABaQpb4fgYjGE7z2TnbQ2DePsyh9KwwPbodS7fD9Pft9uS1p").unwrap();
        assert_eq!(1, address_c.get_thread(thread_count));
        let address_d =
            Address::from_bs58_check("21bU2xruH7bFzfcUhJ6SGjnLmC9cMt1kxzqFr11eV58uj7Ui8h").unwrap();
        assert_eq!(1, address_d.get_thread(thread_count));

        let (hash_genesist0, block_genesist0) = create_genesis_block(&cfg, 0).unwrap();
        let (hash_genesist1, block_genesist1) = create_genesis_block(&cfg, 1).unwrap();
        let export_genesist0 = ExportActiveBlock {
            block: block_genesist0,
            parents: vec![],      // one (hash, period) per thread ( if not genesis )
            children: vec![], // one HashMap<hash, period> per thread (blocks that need to be kept)
            dependencies: vec![], // dependencies required for validity check
            is_final: true,
            block_ledger_change: vec![Vec::new(); thread_count as usize],
            roll_updates: vec![],
        };
        let export_genesist1 = ExportActiveBlock {
            block: block_genesist1,
            parents: vec![],      // one (hash, period) per thread ( if not genesis )
            children: vec![], // one HashMap<hash, period> per thread (blocks that need to be kept)
            dependencies: vec![], // dependencies required for validity check
            is_final: true,
            block_ledger_change: vec![Vec::new(); thread_count as usize],
            roll_updates: vec![],
        };
        //update ledger with initial content.
        //   Thread 0  [at the output of block p0t0]:
        //   A 1000000000
        // Thread 1 [at the output of block p2t1]:
        //   B: 2000000000

        //block reward: 1

        //create block p1t0
        // block p1t0 [NON-FINAL]: creator A, parents [p0t0, p0t1] operations:
        //   A -> B : 2, fee 4
        //   => counted as [A += +1 - 2 - 4 + 4, B += +2]
        let mut blockp1t0 = active_block.clone();
        blockp1t0.parents = vec![(hash_genesist0, 0), (hash_genesist1, 0)];
        blockp1t0.is_final = true;
        blockp1t0.block.header.content.creator = pubkey_a.clone();
        blockp1t0.block_ledger_change = vec![
            vec![(address_a, LedgerChange::new(1, false))]
                .into_iter()
                .collect(),
            vec![(address_b, LedgerChange::new(2, true))]
                .into_iter()
                .collect(),
        ];
        blockp1t0.block.header.content.slot = Slot::new(1, 0);

        // block p1t1 [FINAL]: creator B, parents [p0t0, p0t1], operations:
        //   B -> A : 128, fee 64
        //   B -> A : 32, fee 16
        // => counted as [A += 128 + 32] (B: -128 -32 + 16 + 64 -16 -64 +1=-159 not counted !!)
        let mut blockp1t1 = active_block.clone();
        blockp1t1.parents = vec![(hash_genesist0, 0), (hash_genesist1, 0)];
        blockp1t1.is_final = true;
        blockp1t1.block.header.content.creator = pubkey_b.clone();
        blockp1t1.block_ledger_change = vec![
            vec![(address_a, LedgerChange::new(160, true))] //(A->B: -2 Fee: +4)
                .into_iter()
                .collect(),
            vec![(address_b, LedgerChange::new(159, false))]
                .into_iter()
                .collect(),
        ];

        blockp1t1.block.header.content.slot = Slot::new(1, 1);

        // block p2t0 [NON-FINAL]: creator A, parents [p1t0, p0t1], operations:
        //   A -> A : 512, fee 1024
        // => counted as [A += 1]
        let mut blockp2t0 = active_block.clone();
        blockp2t0.parents = vec![(get_dummy_block_id("blockp1t0"), 1), (hash_genesist1, 0)];
        blockp2t0.is_final = false;
        blockp2t0.block.header.content.creator = pubkey_a.clone();
        blockp2t0.block_ledger_change = vec![
            vec![(address_a, LedgerChange::new(1, true))] //A: 512 - 512 + 1024 -1024 + 1
                .into_iter()
                .collect(),
            HashMap::new(),
        ];
        blockp2t0.block.header.content.slot = Slot::new(2, 0);

        // block p2t1 [FINAL]: creator B, parents [p1t0, p1t1] operations:
        //   B -> A : 10, fee 1
        // => counted as [A += 10] (B not counted !)
        let mut blockp2t1 = active_block.clone();
        blockp2t1.parents = vec![
            (get_dummy_block_id("blockp1t0"), 1),
            (get_dummy_block_id("blockp1t1"), 1),
        ];
        blockp2t1.is_final = true;
        blockp2t1.block.header.content.creator = pubkey_b.clone();
        blockp2t1.block_ledger_change = vec![
            vec![(address_a, LedgerChange::new(10, true))]
                .into_iter()
                .collect(),
            vec![(address_b, LedgerChange::new(9, false))] //B: -10 + 1 -1 +1=-9 not counted
                .into_iter()
                .collect(),
        ];

        blockp2t1.block.header.content.slot = Slot::new(2, 1);

        // block p3t0 [NON-FINAL]: creator A, parents [p2t0, p1t1] operations:
        //   A -> C : 2048, fee 4096
        // => counted as [A += 1 - 2048 - 4096 (+4096) ; C created to 2048]
        let mut blockp3t0 = active_block.clone();
        blockp3t0.parents = vec![
            (get_dummy_block_id("blockp2t0"), 2),
            (get_dummy_block_id("blockp1t1"), 1),
        ];
        blockp3t0.is_final = false;
        blockp3t0.block.header.content.creator = pubkey_a.clone();
        blockp3t0.block_ledger_change = vec![
            vec![(address_a, LedgerChange::new(2047, false))]
                .into_iter() //A: -2048 -4096 + 4096 + 1
                .collect(),
            vec![(address_c, LedgerChange::new(2048, true))] //C: 2048
                .into_iter()
                .collect(),
        ];
        blockp3t0.block.header.content.slot = Slot::new(3, 0);

        // block p3t1 [NON-FINAL]: creator B, parents [p2t0, p2t1] operations:
        //   B -> A : 100, fee 10
        // => counted as [B += 1 - 100 - 10 + 10 ; A += 100]
        let mut blockp3t1 = active_block.clone();
        blockp3t1.parents = vec![
            (get_dummy_block_id("blockp2t0"), 2),
            (get_dummy_block_id("blockp2t1"), 2),
        ];
        blockp3t1.is_final = false;
        blockp3t1.block.header.content.creator = pubkey_b.clone();
        blockp3t1.block_ledger_change = vec![
            vec![(address_a, LedgerChange::new(100, true))]
                .into_iter()
                .collect(),
            vec![(address_b, LedgerChange::new(99, false))]
                .into_iter()
                .collect(),
        ];

        blockp3t1.block.header.content.slot = Slot::new(3, 1);

        let export_graph = BootsrapableGraph {
            /// Map of active blocks, were blocks are in their exported version.
            active_blocks: vec![
                (hash_genesist0, export_genesist0),
                (hash_genesist1, export_genesist1),
                (get_dummy_block_id("blockp1t0"), blockp1t0.into()),
                (get_dummy_block_id("blockp1t1"), blockp1t1.into()),
                (get_dummy_block_id("blockp2t0"), blockp2t0.into()),
                (get_dummy_block_id("blockp2t1"), blockp2t1.into()),
                (get_dummy_block_id("blockp3t0"), blockp3t0.into()),
                (get_dummy_block_id("blockp3t1"), blockp3t1.into()),
            ],
            /// Best parents hash in each thread.
            best_parents: vec![
                get_dummy_block_id("blockp3t0"),
                get_dummy_block_id("blockp3t1"),
            ],
            /// Latest final period and block hash in each thread.
            latest_final_blocks_periods: vec![
                (get_dummy_block_id("blockp1t0"), 1),
                (get_dummy_block_id("blockp2t1"), 2),
            ],
            /// Head of the incompatibility graph.
            gi_head: vec![],
            /// List of maximal cliques of compatible blocks.
            max_cliques: vec![],
            /// Ledger at last final blocks
            ledger: LedgerExport {
                ledger_per_thread: vec![
                    (vec![(
                        address_a,
                        LedgerData {
                            balance: 1_000_000_000,
                        },
                    )]),
                    (vec![(
                        address_b,
                        LedgerData {
                            balance: 2_000_000_000,
                        },
                    )]),
                ], // containing (Address, LedgerData)
            },
        };

        let block_graph = BlockGraph::new(cfg, Some(export_graph)).await.unwrap();

        //Ledger at parents (p3t0, p3t1) for addresses A, B, C, D:
        let res = block_graph
            .get_ledger_at_parents(
                &vec![
                    get_dummy_block_id("blockp3t0"),
                    get_dummy_block_id("blockp3t1"),
                ],
                &vec![address_a, address_b, address_c, address_d]
                    .into_iter()
                    .collect(),
            )
            .unwrap();
        println!("res: {:#?}", res.data);
        // Result ledger:
        // A: 999994127
        // B: 1999999901 = 2000_000_000 - 99
        // C: 2048
        // D: 0
        assert_eq!(res.data[0][&address_a].balance, 999998224);
        assert_eq!(res.data[1][&address_b].balance, 1999999901);
        assert_eq!(res.data[1][&address_c].balance, 2048);
        assert_eq!(res.data[1][&address_d].balance, 0);

        //ask_ledger_at_parents for parents [p1t0, p1t1] for address A  => balance A = 1000000159
        let res = block_graph
            .get_ledger_at_parents(
                &vec![
                    get_dummy_block_id("blockp1t0"),
                    get_dummy_block_id("blockp1t1"),
                ],
                &vec![address_a].into_iter().collect(),
            )
            .unwrap();
        println!("res: {:#?}", res.data);
        // Result ledger:
        // A: 999994127
        // B: 1999999903
        // C: 2048
        // D: 0
        assert_eq!(res.data[0][&address_a].balance, 1000000160);

        //ask_ledger_at_parents for parents [p1t0, p1t1] for addresses A, B => ERROR
        let res = block_graph.get_ledger_at_parents(
            &vec![
                get_dummy_block_id("blockp1t0"),
                get_dummy_block_id("blockp1t1"),
            ],
            &vec![address_a, address_b].into_iter().collect(),
        );
        println!("res: {:#?}", res);
        if let Ok(_) = res {
            panic!("get_ledger_at_parents should return an error");
        }
    }

    #[test]
    #[serial]
    fn test_bootsrapable_graph_serialize_compact() {
        //test with 2 thread
        models::init_serialization_context(models::SerializationContext {
            max_block_operations: 1024,
            parent_count: 2,
            max_peer_list_length: 128,
            max_message_size: 3 * 1024 * 1024,
            max_block_size: 3 * 1024 * 1024,
            max_bootstrap_blocks: 100,
            max_bootstrap_cliques: 100,
            max_bootstrap_deps: 100,
            max_bootstrap_children: 100,
            max_ask_blocks_per_message: 10,
            max_operations_per_message: 1024,
            max_bootstrap_message_size: 100000000,
            max_bootstrap_pos_entries: 1000,
            max_bootstrap_pos_cycles: 5,
        });

        let active_block = get_export_active_test_block();

        let bytes = active_block.block.to_bytes_compact().unwrap();
        let new_block = Block::from_bytes_compact(&bytes).unwrap();

        println!("{:?}", new_block);

        let graph = BootsrapableGraph {
            /// Map of active blocks, were blocks are in their exported version.
            active_blocks: vec![
                (get_dummy_block_id("active11"), active_block.clone()),
                (get_dummy_block_id("active12"), active_block.clone()),
                (get_dummy_block_id("active13"), active_block.clone()),
            ]
            .into_iter()
            .collect(),
            /// Best parents hash in each thread.
            best_parents: vec![
                get_dummy_block_id("parent11"),
                get_dummy_block_id("parent12"),
            ],
            /// Latest final period and block hash in each thread.
            latest_final_blocks_periods: vec![
                (get_dummy_block_id("lfinal11"), 23),
                (get_dummy_block_id("lfinal12"), 24),
            ],
            /// Head of the incompatibility graph.
            gi_head: vec![
                (
                    get_dummy_block_id("gi_head11"),
                    vec![get_dummy_block_id("set11"), get_dummy_block_id("set12")],
                ),
                (
                    get_dummy_block_id("gi_head12"),
                    vec![get_dummy_block_id("set21"), get_dummy_block_id("set22")],
                ),
                (
                    get_dummy_block_id("gi_head13"),
                    vec![get_dummy_block_id("set31"), get_dummy_block_id("set32")],
                ),
            ]
            .into_iter()
            .collect(),

            /// List of maximal cliques of compatible blocks.
            max_cliques: vec![vec![
                get_dummy_block_id("max_cliques11"),
                get_dummy_block_id("max_cliques12"),
            ]
            .into_iter()
            .collect()],
            ledger: LedgerExport {
                ledger_per_thread: Vec::new(),
            },
        };

        let bytes = graph.to_bytes_compact().unwrap();
        let (new_graph, cursor) = BootsrapableGraph::from_bytes_compact(&bytes).unwrap();

        assert_eq!(bytes.len(), cursor);
        assert_eq!(
            graph.active_blocks[0].1.block.header.signature,
            new_graph.active_blocks[0].1.block.header.signature
        );
        assert_eq!(graph.best_parents[0], new_graph.best_parents[0]);
        assert_eq!(graph.best_parents[1], new_graph.best_parents[1]);
        assert_eq!(
            graph.latest_final_blocks_periods[0],
            new_graph.latest_final_blocks_periods[0]
        );
        assert_eq!(
            graph.latest_final_blocks_periods[1],
            new_graph.latest_final_blocks_periods[1]
        );
    }

    #[tokio::test]
    #[serial]
    async fn test_clique_calculation() {
        let ledger_file = generate_ledger_file(&HashMap::new());
        let cfg = example_consensus_config(ledger_file.path());
        let mut block_graph = BlockGraph::new(cfg, None).await.unwrap();
        let hashes: Vec<BlockId> = vec![
            "VzCRpnoZVYY1yQZTXtVQbbxwzdu6hYtdCUZB5BXWSabsiXyfP",
            "JnWwNHRR1tUD7UJfnEFgDB4S4gfDTX2ezLadr7pcwuZnxTvn1",
            "xtvLedxC7CigAPytS5qh9nbTuYyLbQKCfbX8finiHsKMWH6SG",
            "2Qs9sSbc5sGpVv5GnTeDkTKdDpKhp4AgCVT4XFcMaf55msdvJN",
            "2VNc8pR4tNnZpEPudJr97iNHxXbHiubNDmuaSzrxaBVwKXxV6w",
            "2bsrYpfLdvVWAJkwXoJz1kn4LWshdJ6QjwTrA7suKg8AY3ecH1",
            "kfUeGj3ZgBprqFRiAQpE47dW5tcKTAueVaWXZquJW6SaPBd4G",
        ]
        .into_iter()
        .map(|h| BlockId::from_bs58_check(h).unwrap())
        .collect();
        block_graph.gi_head = vec![
            (0, vec![1, 2, 3, 4]),
            (1, vec![0]),
            (2, vec![0]),
            (3, vec![0]),
            (4, vec![0]),
            (5, vec![6]),
            (6, vec![5]),
        ]
        .into_iter()
        .map(|(idx, lst)| (hashes[idx], lst.into_iter().map(|i| hashes[i]).collect()))
        .collect();
        let computed_sets = block_graph.compute_max_cliques();

        let expected_sets: Vec<HashSet<BlockId>> = vec![
            vec![1, 2, 3, 4, 5],
            vec![1, 2, 3, 4, 6],
            vec![0, 5],
            vec![0, 6],
        ]
        .into_iter()
        .map(|lst| lst.into_iter().map(|i| hashes[i]).collect())
        .collect();

        assert_eq!(computed_sets.len(), expected_sets.len());
        for expected in expected_sets.into_iter() {
            assert!(computed_sets.iter().any(|v| v == &expected));
        }
    }

    /// generate a named temporary JSON ledger file
    fn generate_ledger_file(ledger_vec: &HashMap<Address, LedgerData>) -> NamedTempFile {
        use std::io::prelude::*;
        let ledger_file_named = NamedTempFile::new().expect("cannot create temp file");
        serde_json::to_writer_pretty(ledger_file_named.as_file(), &ledger_vec)
            .expect("unable to write ledger file");
        ledger_file_named
            .as_file()
            .seek(std::io::SeekFrom::Start(0))
            .expect("could not seek file");
        ledger_file_named
    }

    pub fn generate_staking_keys_file(staking_keys: &Vec<PrivateKey>) -> NamedTempFile {
        use std::io::prelude::*;
        let file_named = NamedTempFile::new().expect("cannot create temp file");
        serde_json::to_writer_pretty(file_named.as_file(), &staking_keys)
            .expect("unable to write ledger file");
        file_named
            .as_file()
            .seek(std::io::SeekFrom::Start(0))
            .expect("could not seek file");
        file_named
    }

    fn example_consensus_config(initial_ledger_path: &Path) -> ConsensusConfig {
        let genesis_key = crypto::generate_random_private_key();
        let mut staking_keys = Vec::new();
        for _ in 0..2 {
            staking_keys.push(crypto::generate_random_private_key());
        }
        let staking_file = generate_staking_keys_file(&staking_keys);

        let thread_count: u8 = 2;
        let max_block_size = 1024 * 1024;
        let max_operations_per_block = 1024;
        let tempdir = tempfile::tempdir().expect("cannot create temp dir");
        let tempdir3 = tempfile::tempdir().expect("cannot create temp dir");

        models::init_serialization_context(models::SerializationContext {
            max_block_operations: 1024,
            parent_count: 2,
            max_peer_list_length: 128,
            max_message_size: 3 * 1024 * 1024,
            max_block_size: 3 * 1024 * 1024,
            max_bootstrap_blocks: 100,
            max_bootstrap_cliques: 100,
            max_bootstrap_deps: 100,
            max_bootstrap_children: 100,
            max_ask_blocks_per_message: 10,
            max_operations_per_message: 1024,
            max_bootstrap_message_size: 100000000,
            max_bootstrap_pos_entries: 1000,
            max_bootstrap_pos_cycles: 5,
        });

        ConsensusConfig {
            genesis_timestamp: UTime::now(0).unwrap(),
            thread_count,
            t0: 32.into(),
            genesis_key,
            max_discarded_blocks: 10,
            future_block_processing_max_periods: 3,
            max_future_processing_blocks: 10,
            max_dependency_blocks: 10,
            delta_f0: 5,
            disable_block_creation: true,
            max_block_size,
            max_operations_per_block,
            operation_validity_periods: 3,
            ledger_path: tempdir.path().to_path_buf(),
            ledger_cache_capacity: 1000000,
            ledger_flush_interval: Some(200.into()),
            ledger_reset_at_startup: true,
            block_reward: 1,
            initial_ledger_path: initial_ledger_path.to_path_buf(),
            operation_batch_size: 100,
            initial_rolls_path: tempdir3.path().to_path_buf(),
            initial_draw_seed: "genesis".into(),
            periods_per_cycle: 100,
            pos_lookback_cycles: 2,
            pos_lock_cycles: 1,
            pos_draw_cached_cycles: 2,
            roll_price: 10,
            stats_timespan: 60000.into(),
            staking_keys_path: staking_file.path().to_path_buf(),
        }
    }
}
