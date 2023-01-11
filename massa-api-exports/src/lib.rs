// Copyright (c) 2022 MASSA LABS <info@massa.net>

use massa_final_state::StateChanges;
use massa_models::address::ExecutionAddressCycleInfo;
use massa_models::api::IndexedSlot;
use massa_models::endorsement::{EndorsementId, SecureShareEndorsement};
use massa_models::ledger_models::LedgerData;
use massa_models::node::NodeId;
use massa_models::operation::{OperationId, SecureShareOperation};
use massa_models::stats::{ConsensusStats, ExecutionStats, NetworkStats};
use massa_models::{
    address::Address, amount::Amount, block::Block, block::BlockId, config::CompactConfig,
    output_event::SCOutputEvent, slot::Slot, version::Version,
};
use massa_signature::{PublicKey, Signature};
use massa_time::MassaTime;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::{collections::VecDeque, fmt::Display};

/// operation input
#[derive(Serialize, Deserialize, Debug)]
pub struct OperationInput {
    /// The public key of the creator of the TX
    pub creator_public_key: PublicKey,
    /// The signature of the operation
    pub signature: Signature,
    /// The serialized version of the content `base58` encoded
    pub serialized_content: Vec<u8>,
}

/// node status
#[derive(Debug, Deserialize, Serialize)]
pub struct NodeStatus {
    /// our node id
    pub node_id: NodeId,
    /// optional node ip
    pub node_ip: Option<IpAddr>,
    /// node version
    pub version: Version,
    /// now
    pub current_time: MassaTime,
    /// current cycle
    pub current_cycle: u64,
    /// connected nodes (node id, ip address, true if the connection is outgoing, false if incoming)
    pub connected_nodes: BTreeMap<NodeId, (IpAddr, bool)>,
    /// latest slot, none if now is before genesis timestamp
    pub last_slot: Option<Slot>,
    /// next slot
    pub next_slot: Slot,
    /// consensus stats
    pub consensus_stats: ConsensusStats,
    /// pool stats (operation count and endorsement count)
    pub pool_stats: (usize, usize),
    /// network stats
    pub network_stats: NetworkStats,
    /// execution stats
    pub execution_stats: ExecutionStats,
    /// compact configuration
    pub config: CompactConfig,
}

impl std::fmt::Display for NodeStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Node's ID: {}", self.node_id)?;
        if self.node_ip.is_some() {
            writeln!(f, "Node's IP: {}", self.node_ip.unwrap())?;
        } else {
            writeln!(f, "No routable IP set")?;
        }
        writeln!(f)?;

        writeln!(f, "Version: {}", self.version)?;
        writeln!(f, "Config:\n{}", self.config)?;
        writeln!(f)?;

        writeln!(f, "Current time: {}", self.current_time.to_utc_string())?;
        writeln!(f, "Current cycle: {}", self.current_cycle)?;
        if self.last_slot.is_some() {
            writeln!(f, "Last slot: {}", self.last_slot.unwrap())?;
        }
        writeln!(f, "Next slot: {}", self.next_slot)?;
        writeln!(f)?;

        writeln!(f, "{}", self.consensus_stats)?;

        writeln!(f, "Pool stats:")?;
        writeln!(f, "\tOperations count: {}", self.pool_stats.0)?;
        writeln!(f, "\tEndorsements count: {}", self.pool_stats.1)?;
        writeln!(f)?;

        writeln!(f, "{}", self.network_stats)?;

        writeln!(f, "{}", self.execution_stats)?;

        writeln!(f, "Connected nodes:")?;
        for (node_id, (ip_addr, is_outgoing)) in &self.connected_nodes {
            writeln!(
                f,
                "Node's ID: {} / IP address: {} / {} connection",
                node_id,
                ip_addr,
                if *is_outgoing { "Out" } else { "In" }
            )?
        }
        Ok(())
    }
}

/// Operation and contextual info about it
#[derive(Debug, Deserialize, Serialize)]
pub struct OperationInfo {
    /// id
    pub id: OperationId,
    /// true if operation is still in pool
    pub in_pool: bool,
    /// the operation appears in `in_blocks`
    /// if it appears in multiple blocks, these blocks are in different cliques
    pub in_blocks: Vec<BlockId>,
    /// true if the operation is final (for example in a final block)
    pub is_final: bool,
    /// the operation itself
    pub operation: SecureShareOperation,
}

impl std::fmt::Display for OperationInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "Operation {}{}{}",
            self.id,
            display_if_true(self.in_pool, " (in pool)"),
            display_if_true(self.is_final, " (final)")
        )?;
        writeln!(f, "In blocks:")?;
        for block_id in &self.in_blocks {
            writeln!(f, "\t- {}", block_id)?;
        }
        writeln!(f, "{}", self.operation)?;
        Ok(())
    }
}

/// Current balance ledger info
#[derive(Debug, Deserialize, Serialize, Clone, Copy)]
pub struct LedgerInfo {
    /// final data
    pub final_ledger_info: LedgerData,
    /// latest data
    pub candidate_ledger_info: LedgerData,
    /// locked balance, for example balance due to a roll sell
    pub locked_balance: Amount,
}

impl std::fmt::Display for LedgerInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "\tFinal balance: {}", self.final_ledger_info.balance)?;
        writeln!(
            f,
            "\tCandidate balance: {}",
            self.candidate_ledger_info.balance
        )?;
        writeln!(f, "\tLocked balance: {}", self.locked_balance)?;
        Ok(())
    }
}

/// Roll counts
#[derive(Debug, Deserialize, Serialize, Clone, Copy)]
pub struct RollsInfo {
    /// count taken into account for the current cycle
    pub active_rolls: u64,
    /// at final blocks
    pub final_rolls: u64,
    /// at latest blocks
    pub candidate_rolls: u64,
}

impl std::fmt::Display for RollsInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "\tActive rolls: {}", self.active_rolls)?;
        writeln!(f, "\tFinal rolls: {}", self.final_rolls)?;
        writeln!(f, "\tCandidate rolls: {}", self.candidate_rolls)?;
        Ok(())
    }
}

/// All you ever dream to know about an address
#[derive(Debug, Deserialize, Serialize)]
pub struct AddressInfo {
    /// the address
    pub address: Address,
    /// the thread the address belongs to
    pub thread: u8,

    /// final balance
    pub final_balance: Amount,
    /// final roll count
    pub final_roll_count: u64,
    /// final datastore keys
    pub final_datastore_keys: Vec<Vec<u8>>,

    /// candidate balance
    pub candidate_balance: Amount,
    /// candidate roll count
    pub candidate_roll_count: u64,
    /// candidate datastore keys
    pub candidate_datastore_keys: Vec<Vec<u8>>,

    /// deferred credits
    pub deferred_credits: Vec<SlotAmount>,

    /// next block draws
    pub next_block_draws: Vec<Slot>,
    /// next endorsement draws
    pub next_endorsement_draws: Vec<IndexedSlot>,

    /// created blocks
    pub created_blocks: Vec<BlockId>,
    /// created operations
    pub created_operations: Vec<OperationId>,
    /// created endorsements
    pub created_endorsements: Vec<EndorsementId>,

    /// cycle information
    pub cycle_infos: Vec<ExecutionAddressCycleInfo>,
}

impl std::fmt::Display for AddressInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Address {} (thread {}):", self.address, self.thread)?;
        writeln!(
            f,
            "\tBalance: final={}, candidate={}",
            self.final_balance, self.candidate_balance
        )?;
        writeln!(
            f,
            "\tRolls: final={}, candidate={}",
            self.final_roll_count, self.candidate_roll_count
        )?;
        write!(f, "\tLocked coins:")?;
        if self.deferred_credits.is_empty() {
            writeln!(f, "0")?;
        } else {
            for slot_amount in &self.deferred_credits {
                writeln!(
                    f,
                    "\t\t{} locked coins will be unlocked at slot {}",
                    slot_amount.amount, slot_amount.slot
                )?;
            }
        }
        writeln!(f, "\tCycle infos:")?;
        for cycle_info in &self.cycle_infos {
            writeln!(
                f,
                "\t\tCycle {} ({}): produced {} and missed {} blocks{}",
                cycle_info.cycle,
                if cycle_info.is_final {
                    "final"
                } else {
                    "candidate"
                },
                cycle_info.ok_count,
                cycle_info.nok_count,
                match cycle_info.active_rolls {
                    Some(rolls) => format!(" with {} active rolls", rolls),
                    None => "".into(),
                },
            )?;
        }
        //writeln!(f, "\tProduced blocks: {}", self.created_blocks.iter().map(|id| id.to_string()).intersperse(", ".into()).collect())?;
        //writeln!(f, "\tProduced operations: {}", self.created_operations.iter().map(|id| id.to_string()).intersperse(", ".into()).collect())?;
        //writeln!(f, "\tProduced endorsements: {}", self.created_endorsements.iter().map(|id| id.to_string()).intersperse(", ".into()).collect())?;
        Ok(())
    }
}

impl AddressInfo {
    /// Only essential info about an address
    pub fn compact(&self) -> CompactAddressInfo {
        CompactAddressInfo {
            address: self.address,
            thread: self.thread,
            active_rolls: self
                .cycle_infos
                .last()
                .and_then(|c| c.active_rolls)
                .unwrap_or_default(),
            final_rolls: self.final_roll_count,
            candidate_rolls: self.candidate_roll_count,
            final_balance: self.final_balance,
            candidate_balance: self.candidate_balance,
        }
    }
}

/// Less information about an address
#[derive(Debug, Serialize, Deserialize)]
pub struct CompactAddressInfo {
    /// the address
    pub address: Address,
    /// the thread it is
    pub thread: u8,
    /// candidate rolls
    pub candidate_rolls: u64,
    /// final rolls
    pub final_rolls: u64,
    /// active rolls
    pub active_rolls: u64,
    /// final balance
    pub final_balance: Amount,
    /// candidate balance
    pub candidate_balance: Amount,
}

impl std::fmt::Display for CompactAddressInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Address: {} (thread {}):", self.address, self.thread)?;
        writeln!(
            f,
            "\tBalance: final={}, candidate={}",
            self.final_balance, self.candidate_balance
        )?;
        writeln!(
            f,
            "\tRolls: active={}, final={}, candidate={}",
            self.active_rolls, self.final_rolls, self.candidate_rolls
        )?;
        Ok(())
    }
}

/// All you wanna know about an endorsement
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct EndorsementInfo {
    /// id
    pub id: EndorsementId,
    /// true if endorsement is still in pool
    pub in_pool: bool,
    /// the endorsement appears in `in_blocks`
    /// if it appears in multiple blocks, these blocks are in different cliques
    pub in_blocks: Vec<BlockId>,
    /// true if the endorsement is final (for example in a final block)
    pub is_final: bool,
    /// the endorsement itself
    pub endorsement: SecureShareEndorsement,
}

impl std::fmt::Display for EndorsementInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "Endorsement {}{}{}",
            self.id,
            display_if_true(self.in_pool, " (in pool)"),
            display_if_true(self.is_final, " (final)")
        )?;
        writeln!(f, "In blocks:")?;
        for block_id in &self.in_blocks {
            writeln!(f, "\t- {}", block_id)?;
        }
        writeln!(f, "{}", self.endorsement)?;
        Ok(())
    }
}

/// slot / amount pair
#[derive(Debug, Deserialize, Serialize)]
pub struct SlotAmount {
    /// slot
    pub slot: Slot,
    /// amount
    pub amount: Amount,
}

/// refactor to delete
#[derive(Debug, Deserialize, Serialize)]
pub struct BlockInfo {
    /// block id
    pub id: BlockId,
    /// optional block info content
    pub content: Option<BlockInfoContent>,
}

/// Block content
#[derive(Debug, Deserialize, Serialize)]
pub struct BlockInfoContent {
    /// true if final
    pub is_final: bool,
    /// true if in the greatest clique (and not final)
    pub is_in_blockclique: bool,
    /// true if candidate (active any clique but not final)
    pub is_candidate: bool,
    /// true if discarded
    pub is_discarded: bool,
    /// block
    pub block: Block,
}

impl std::fmt::Display for BlockInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(content) = &self.content {
            writeln!(
                f,
                "Block ID: {}{}{}{}{}",
                self.id,
                display_if_true(content.is_final, " (final)"),
                display_if_true(content.is_candidate, " (candidate)"),
                display_if_true(content.is_in_blockclique, " (blockclique)"),
                display_if_true(content.is_discarded, " (discarded)"),
            )?;
            writeln!(f, "Block: {}", content.block)?;
        } else {
            writeln!(f, "Block {} not found", self.id)?;
        }
        Ok(())
    }
}

/// A block resume (without the block itself)
#[derive(Debug, Deserialize, Serialize)]
pub struct BlockSummary {
    /// id
    pub id: BlockId,
    /// true if in a final block
    pub is_final: bool,
    /// true if incompatible with a final block
    pub is_stale: bool,
    /// true if in the greatest block clique
    pub is_in_blockclique: bool,
    /// the slot the block is in
    pub slot: Slot,
    /// the block creator
    pub creator: Address,
    /// the block parents
    pub parents: Vec<BlockId>,
}

impl std::fmt::Display for BlockSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "Block's ID: {}{}{}{}",
            self.id,
            display_if_true(self.is_final, "final"),
            display_if_true(self.is_stale, "stale"),
            display_if_true(self.is_in_blockclique, "in blockclique"),
        )?;
        writeln!(f, "Slot: {}", self.slot)?;
        writeln!(f, "Creator: {}", self.creator)?;
        writeln!(f, "Parents' IDs:")?;
        for parent in &self.parents {
            writeln!(f, "\t- {}", parent)?;
        }
        Ok(())
    }
}

/// The result of the read-only execution.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ReadOnlyResult {
    /// An error occurred during execution.
    Error(String),
    /// The result of a successful execution.
    Ok(Vec<u8>),
}

/// The response to a request for a read-only execution.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExecuteReadOnlyResponse {
    /// The slot at which the read-only execution occurred.
    pub executed_at: Slot,
    /// The result of the read-only execution.
    pub result: ReadOnlyResult,
    /// The output events generated by the read-only execution.
    pub output_events: VecDeque<SCOutputEvent>,
    /// The gas cost for the execution
    pub gas_cost: u64,
    /// state changes caused by the execution step
    pub state_changes: StateChanges,
}

impl Display for ExecuteReadOnlyResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Executed at slot: {}", self.executed_at)?;
        writeln!(
            f,
            "Result: {}",
            match &self.result {
                ReadOnlyResult::Error(e) =>
                    format!("an error occurred during the execution: {}", e),
                ReadOnlyResult::Ok(ret) => format!("success, returned value: {:?}", ret),
            }
        )?;
        writeln!(f, "Gas cost: {}", self.gas_cost)?;
        if !self.output_events.is_empty() {
            writeln!(f, "Generated events:",)?;
            for event in self.output_events.iter() {
                writeln!(f, "{}", event)?; // id already displayed in event
            }
        }
        Ok(())
    }
}

/// Dumb utils function to display nicely boolean value
fn display_if_true(value: bool, text: &str) -> String {
    if value {
        format!("[{}]", text)
    } else {
        String::from("")
    }
}

/// Just a wrapper with a optional beginning and end
#[derive(Debug, Deserialize, Clone, Copy, Serialize)]
pub struct TimeInterval {
    /// optional start slot
    pub start: Option<MassaTime>,
    /// optional end slot
    pub end: Option<MassaTime>,
}

/// Datastore entry query input structure
#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct DatastoreEntryInput {
    /// associated address of the entry
    pub address: Address,
    /// datastore key
    pub key: Vec<u8>,
}

/// Datastore entry query output structure
#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct DatastoreEntryOutput {
    /// final datastore entry value
    pub final_value: Option<Vec<u8>>,
    /// candidate datastore entry value
    pub candidate_value: Option<Vec<u8>>,
}

impl std::fmt::Display for DatastoreEntryOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "final value: {:?}", self.final_value)?;
        writeln!(f, "candidate value: {:?}", self.candidate_value)?;
        Ok(())
    }
}

/// read only bytecode execution request
#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct ReadOnlyBytecodeExecution {
    /// max available gas
    pub max_gas: u64,
    /// byte code
    pub bytecode: Vec<u8>,
    /// caller's address, optional
    pub address: Option<Address>,
    /// Operation datastore, optional
    pub operation_datastore: Option<Vec<u8>>,
    /// (optional) execution start state
    ///
    /// Some(true) means start execution from final state
    /// Some(false) or None means start execution from active state
    pub is_final: Option<bool>,
}

/// read SC call request
#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct ReadOnlyCall {
    /// max available gas
    pub max_gas: u64,
    /// target address
    pub target_address: Address,
    /// target function
    pub target_function: String,
    /// function parameter
    pub parameter: Vec<u8>,
    /// caller's address, optional
    pub caller_address: Option<Address>,
    /// (optional) execution start state
    ///
    /// Some(true) means start execution from final state
    /// Some(false) or None means start execution from active state
    pub is_final: Option<bool>,
}

/// SCRUD operations
#[derive(strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum ScrudOperation {
    /// search operation
    Search,
    /// create operation
    Create,
    /// read operation
    Read,
    /// update operation
    Update,
    /// delete operation
    Delete,
}

/// Bootsrap lists types
#[derive(strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum ListType {
    /// contains banned entry
    Blacklist,
    /// contains allowed entry
    Whitelist,
}
