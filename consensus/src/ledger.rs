// Copyright (c) 2021 MASSA LABS <info@massa.net>

use crate::error::InternalError;
use sled::{Transactional, Tree};
use std::{
    collections::{hash_map, HashMap, HashSet},
    convert::{TryFrom, TryInto},
    usize,
};

use crate::{ConsensusConfig, ConsensusError};
use models::{
    array_from_slice, u8_from_slice, Address, Amount, DeserializeCompact, DeserializeVarInt,
    ModelsError, Operation, SerializeCompact, SerializeVarInt, ADDRESS_SIZE_BYTES,
};
use serde::{Deserialize, Serialize};

pub struct Ledger {
    ledger_per_thread: Vec<Tree>, // containing (Address, LedgerData)
    latest_final_periods: Tree,   // containing (thread_number: u8, latest_final_period: u64)
    cfg: ConsensusConfig,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LedgerData {
    pub balance: Amount,
}

impl Default for LedgerData {
    fn default() -> Self {
        LedgerData {
            balance: Amount::default(),
        }
    }
}

impl SerializeCompact for LedgerData {
    fn to_bytes_compact(&self) -> Result<Vec<u8>, models::ModelsError> {
        let mut res: Vec<u8> = Vec::new();
        res.extend(&self.balance.to_bytes_compact()?);
        Ok(res)
    }
}

impl DeserializeCompact for LedgerData {
    fn from_bytes_compact(buffer: &[u8]) -> Result<(Self, usize), models::ModelsError> {
        let mut cursor = 0usize;
        let (balance, delta) = Amount::from_bytes_compact(&buffer[cursor..])?;
        cursor += delta;
        Ok((LedgerData { balance }, cursor))
    }
}

impl LedgerData {
    pub fn new(starting_balance: Amount) -> LedgerData {
        LedgerData {
            balance: starting_balance,
        }
    }

    fn apply_change(&mut self, change: &LedgerChange) -> Result<(), ConsensusError> {
        if change.balance_increment {
            self.balance = self.balance.checked_add(change.balance_delta).ok_or(
                ConsensusError::InvalidLedgerChange(
                    "balance overflow in LedgerData::apply_change".into(),
                ),
            )?;
        } else {
            self.balance = self.balance.checked_sub(change.balance_delta).ok_or(
                ConsensusError::InvalidLedgerChange(
                    "balance underflow in LedgerData::apply_change".into(),
                ),
            )?;
        }
        Ok(())
    }

    /// returns true if the balance is zero
    fn is_nil(&self) -> bool {
        self.balance == Amount::default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerChange {
    pub balance_delta: Amount,
    pub balance_increment: bool, // wether to increment or decrement balance of delta
}

impl Default for LedgerChange {
    fn default() -> Self {
        LedgerChange {
            balance_delta: Amount::default(),
            balance_increment: true,
        }
    }
}

impl LedgerChange {
    /// Applies another ledger change on top of self
    pub fn chain(&mut self, change: &LedgerChange) -> Result<(), ConsensusError> {
        if self.balance_increment == change.balance_increment {
            self.balance_delta = self.balance_delta.checked_add(change.balance_delta).ok_or(
                ConsensusError::InvalidLedgerChange("overflow in LedgerChange::chain".into()),
            )?;
        } else if change.balance_delta > self.balance_delta {
            self.balance_delta = change.balance_delta.checked_sub(self.balance_delta).ok_or(
                ConsensusError::InvalidLedgerChange("underflow in LedgerChange::chain".into()),
            )?;
            self.balance_increment = !self.balance_increment;
        } else {
            self.balance_delta = self.balance_delta.checked_sub(change.balance_delta).ok_or(
                ConsensusError::InvalidLedgerChange("underflow in LedgerChange::chain".into()),
            )?;
        }
        if self.balance_delta == Amount::default() {
            self.balance_increment = true;
        }
        Ok(())
    }

    pub fn is_nil(&self) -> bool {
        self.balance_delta == Amount::default()
    }
}

impl SerializeCompact for LedgerChange {
    fn to_bytes_compact(&self) -> Result<Vec<u8>, models::ModelsError> {
        let mut res: Vec<u8> = Vec::new();
        res.push(if self.balance_increment { 1u8 } else { 0u8 });
        res.extend(&self.balance_delta.to_bytes_compact()?);
        Ok(res)
    }
}

impl DeserializeCompact for LedgerChange {
    fn from_bytes_compact(buffer: &[u8]) -> Result<(Self, usize), models::ModelsError> {
        let mut cursor = 0usize;

        let balance_increment = match u8_from_slice(&buffer[cursor..])? {
            0u8 => false,
            1u8 => true,
            _ => {
                return Err(ModelsError::DeserializeError(
                    "wrong boolean balance_increment encoding in LedgerChange deserialization"
                        .into(),
                ))
            }
        };
        cursor += 1;

        let (balance_delta, delta) = Amount::from_bytes_compact(&buffer[cursor..])?;
        cursor += delta;

        Ok((
            LedgerChange {
                balance_increment,
                balance_delta,
            },
            cursor,
        ))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LedgerChanges(pub HashMap<Address, LedgerChange>);

impl LedgerChanges {
    pub fn get_involved_addresses(&self) -> HashSet<Address> {
        self.0.keys().copied().collect()
    }

    /// applies a LedgerChange
    pub fn apply(&mut self, addr: &Address, change: &LedgerChange) -> Result<(), ConsensusError> {
        match self.0.entry(*addr) {
            hash_map::Entry::Occupied(mut occ) => {
                occ.get_mut().chain(change)?;
                if occ.get().is_nil() {
                    occ.remove();
                }
            }
            hash_map::Entry::Vacant(vac) => {
                let mut res = LedgerChange::default();
                res.chain(change)?;
                if !res.is_nil() {
                    vac.insert(res);
                }
            }
        }
        Ok(())
    }

    /// chain with another LedgerChange
    pub fn chain(&mut self, other: &LedgerChanges) -> Result<(), ConsensusError> {
        for (addr, change) in other.0.iter() {
            self.apply(addr, change)?;
        }
        Ok(())
    }

    /// merge another ledger changes into self, overwriting existing data
    /// addrs that are in not other are removed from self
    pub fn sync_from(&mut self, addrs: &HashSet<Address>, mut other: LedgerChanges) {
        for addr in addrs.iter() {
            if let Some(new_val) = other.0.remove(addr) {
                self.0.insert(*addr, new_val);
            } else {
                self.0.remove(addr);
            }
        }
    }

    /// clone subset
    pub fn clone_subset(&self, addrs: &HashSet<Address>) -> Self {
        LedgerChanges(
            self.0
                .iter()
                .filter_map(|(a, dta)| {
                    if addrs.contains(a) {
                        Some((*a, dta.clone()))
                    } else {
                        None
                    }
                })
                .collect(),
        )
    }

    /// add reward related changes
    pub fn add_reward(
        &mut self,
        creator: Address,
        endorsers: Vec<Address>,
        parent_creator: Address,
        reward: Amount,
        endorsement_count: u32,
    ) -> Result<(), ConsensusError> {
        let endorsers_count = endorsers.len() as u64;
        let third = reward
            .checked_div_u64(3 * (1 + (endorsement_count as u64)))
            .ok_or(ConsensusError::AmountOverflowError)?;
        for ed in endorsers {
            self.apply(
                &parent_creator,
                &LedgerChange {
                    balance_delta: third,
                    balance_increment: true,
                },
            )?;
            self.apply(
                &ed,
                &LedgerChange {
                    balance_delta: third,
                    balance_increment: true,
                },
            )?;
        }
        let total_credited = third
            .checked_mul_u64(2 * endorsers_count)
            .ok_or(ConsensusError::AmountOverflowError)?;
        // here we credited only parent_creator and ed for every endorsement
        // total_credited now contains the total amount already credited

        let expected_credit = reward
            .checked_mul_u64(1 + endorsers_count)
            .ok_or(ConsensusError::AmountOverflowError)?
            .checked_div_u64(1 + (endorsement_count as u64))
            .ok_or(ConsensusError::AmountOverflowError)?;
        // here expected_credit contains the expected amount that should be credited in total
        // the difference between expected_credit and total_credited is sent to the block creator
        self.apply(
            &creator,
            &LedgerChange {
                balance_delta: expected_credit.saturating_sub(total_credited),
                balance_increment: true,
            },
        )
    }
}

pub trait OperationLedgerInterface {
    fn get_ledger_changes(
        &self,
        fee_target: &Address,
        thread_count: u8,
        roll_price: Amount,
    ) -> Result<LedgerChanges, ConsensusError>;
}

impl OperationLedgerInterface for Operation {
    fn get_ledger_changes(
        &self,
        fee_target: &Address,
        _thread_count: u8,
        roll_price: Amount,
    ) -> Result<LedgerChanges, ConsensusError> {
        let mut res = LedgerChanges::default();

        // sender fee
        let sender_address = Address::from_public_key(&self.content.sender_public_key)?;
        res.apply(
            &sender_address,
            &LedgerChange {
                balance_delta: self.content.fee.clone().into(),
                balance_increment: false,
            },
        )?;

        // fee target
        res.apply(
            &fee_target,
            &LedgerChange {
                balance_delta: self.content.fee.clone().into(),
                balance_increment: true,
            },
        )?;

        // operation type specific
        match &self.content.op {
            models::OperationType::Transaction {
                recipient_address,
                amount,
            } => {
                res.apply(
                    &sender_address,
                    &LedgerChange {
                        balance_delta: amount.clone().into(),
                        balance_increment: false,
                    },
                )?;
                res.apply(
                    &recipient_address,
                    &LedgerChange {
                        balance_delta: amount.clone().into(),
                        balance_increment: true,
                    },
                )?;
            }
            models::OperationType::RollBuy { roll_count } => {
                res.apply(
                    &sender_address,
                    &LedgerChange {
                        balance_delta: roll_price
                            .checked_mul_u64(*roll_count)
                            .ok_or(ConsensusError::RollOverflowError)?,
                        balance_increment: false,
                    },
                )?;
            }
            // roll sale is handled separately with a delay
            models::OperationType::RollSell { .. } => {}
        }

        Ok(res)
    }
}

impl Ledger {
    /// if no latest_final_periods in file, they are initialized at 0u64
    /// if there is a ledger in the given file, it is loaded
    pub fn new(
        cfg: ConsensusConfig,
        opt_init_data: Option<LedgerSubset>,
    ) -> Result<Ledger, ConsensusError> {
        let sled_config = sled::Config::default()
            .path(&cfg.ledger_path)
            .cache_capacity(cfg.ledger_cache_capacity)
            .flush_every_ms(cfg.ledger_flush_interval.map(|v| v.to_millis()));
        let db = sled_config.open()?;

        let mut ledger_per_thread = Vec::new();
        for thread in 0..cfg.thread_count {
            db.drop_tree(format!("ledger_thread_{:?}", thread))?;
            let current_tree = db.open_tree(format!("ledger_thread_{:?}", thread))?;
            ledger_per_thread.push(current_tree);
        }
        db.drop_tree("latest_final_periods".to_string())?;
        let latest_final_periods = db.open_tree("latest_final_periods")?;
        if latest_final_periods.is_empty() {
            for thread in 0..cfg.thread_count {
                let zero: u64 = 0;
                latest_final_periods.insert([thread], &zero.to_be_bytes())?;
            }
        }

        if let Some(init_ledger) = opt_init_data {
            ledger_per_thread.transaction(|ledger| {
                for (address, data) in init_ledger.0.iter() {
                    let thread = address.get_thread(cfg.thread_count);
                    ledger[thread as usize].insert(
                        &address.to_bytes(),
                        data.to_bytes_compact().map_err(|err| {
                            sled::transaction::ConflictableTransactionError::Abort(
                                InternalError::TransactionError(format!(
                                    "error serializing ledger data: {:?}",
                                    err
                                )),
                            )
                        })?,
                    )?;
                }
                Ok(())
            })?;
        }
        Ok(Ledger {
            ledger_per_thread,
            latest_final_periods,
            cfg,
        })
    }

    /// Returns the final ledger data of a list of unique addresses belonging to any thread.
    pub fn get_final_data(
        &self,
        addresses: HashSet<&Address>,
    ) -> Result<LedgerSubset, ConsensusError> {
        self.ledger_per_thread
            .transaction(|ledger_per_thread| {
                let mut result = LedgerSubset::default();
                for address in addresses.iter() {
                    let thread = address.get_thread(self.cfg.thread_count);
                    let ledger = ledger_per_thread.get(thread as usize).ok_or_else(|| {
                        sled::transaction::ConflictableTransactionError::Abort(
                            InternalError::TransactionError(format!(
                                "Could not get ledger for thread {:?}",
                                thread
                            )),
                        )
                    })?;
                    let data = if let Some(res) = ledger.get(address.to_bytes())? {
                        LedgerData::from_bytes_compact(&res)
                            .map_err(|err| {
                                sled::transaction::ConflictableTransactionError::Abort(
                                    InternalError::TransactionError(format!(
                                        "error deserializing ledger data: {:?}",
                                        err
                                    )),
                                )
                            })?
                            .0
                    } else {
                        LedgerData::default()
                    };

                    // Should never panic since we are operating on a set of addresses.
                    assert!(result.0.insert(**address, data).is_none());
                }
                Ok(result)
            })
            .map_err(|_| {
                ConsensusError::LedgerInconsistency(format!(
                    "Unable to fetch data for addresses {:?}",
                    addresses
                ))
            })
    }

    /// If there is something in the ledger file, it is overwritten
    pub fn from_export(
        export: LedgerExport,
        latest_final_periods: Vec<u64>,
        cfg: ConsensusConfig,
    ) -> Result<Ledger, ConsensusError> {
        let ledger = Ledger::new(cfg.clone(), None)?;
        ledger.clear()?;

        // fill ledger per thread
        for (address, addr_data) in export.ledger_subset.iter() {
            let thread = address.get_thread(cfg.thread_count);
            if ledger.ledger_per_thread[thread as usize]
                .insert(address.into_bytes(), addr_data.to_bytes_compact()?)?
                .is_some()
            {
                return Err(ConsensusError::LedgerInconsistency(format!(
                    "adress {:?} already in ledger while bootsrapping",
                    address
                )));
            };
        }

        // initilize final periods
        ledger.latest_final_periods.transaction(|tree| {
            for (thread, period) in latest_final_periods.iter().enumerate() {
                tree.insert(&[thread as u8], &period.to_be_bytes())?;
            }
            Ok(())
        })?;
        Ok(ledger)
    }

    /// Returns the final balance of an address. 0 if the address does not exist.
    pub fn get_final_balance(&self, address: &Address) -> Result<Amount, ConsensusError> {
        let thread = address.get_thread(self.cfg.thread_count);
        if let Some(res) = self.ledger_per_thread[thread as usize].get(address.to_bytes())? {
            Ok(LedgerData::from_bytes_compact(&res)?.0.balance)
        } else {
            Ok(Amount::default())
        }
    }

    /// Atomically apply a batch of changes to the ledger.
    /// All changes should occure in one thread.
    /// Update last final period.
    ///
    /// * If the balance of an address falls exactly to 0, it is removed from the ledger.
    /// * If the balance of a non-existing address increases, the address is added to the ledger.
    /// * If we attempt to substract more than the balance of an address, the transaction is cancelled and the function returns an error.
    pub fn apply_final_changes(
        &self,
        thread: u8,
        changes: &LedgerChanges,
        latest_final_period: u64,
    ) -> Result<(), ConsensusError> {
        let ledger = self.ledger_per_thread.get(thread as usize).ok_or_else(|| {
            ConsensusError::LedgerInconsistency(format!("missing ledger for thread {:?}", thread))
        })?;

        (ledger, &self.latest_final_periods).transaction(|(db, latest_final_periods_db)| {
            for (address, change) in changes.0.iter() {
                if address.get_thread(self.cfg.thread_count) != thread {
                    continue;
                }
                let address_bytes = address.to_bytes();
                let mut data = if let Some(old_bytes) = &db.get(address_bytes)? {
                    let (old, _) = LedgerData::from_bytes_compact(old_bytes).map_err(|err| {
                        sled::transaction::ConflictableTransactionError::Abort(
                            InternalError::TransactionError(format!(
                                "error deserializing ledger data: {:?}",
                                err
                            )),
                        )
                    })?;
                    old
                } else {
                    // creating new entry
                    LedgerData::default()
                };
                data.apply_change(change).map_err(|err| {
                    sled::transaction::ConflictableTransactionError::Abort(
                        InternalError::TransactionError(format!(
                            "error applying change: {:?}",
                            err
                        )),
                    )
                })?;
                // remove entry if nil
                if data.is_nil() {
                    db.remove(&address_bytes)?;
                } else {
                    db.insert(
                        &address_bytes,
                        data.to_bytes_compact().map_err(|err| {
                            sled::transaction::ConflictableTransactionError::Abort(
                                InternalError::TransactionError(format!(
                                    "error serializing ledger data: {:?}",
                                    err
                                )),
                            )
                        })?,
                    )?;
                }
            }
            latest_final_periods_db
                .insert(&[thread], &latest_final_period.to_be_bytes())
                .map_err(|err| {
                    sled::transaction::ConflictableTransactionError::Abort(
                        InternalError::TransactionError(format!(
                            "error inserting transaction: {:?}",
                            err
                        )),
                    )
                })?;
            Ok(())
        })?;
        Ok(())
    }

    /// returns the final periods.
    pub fn get_latest_final_periods(&self) -> Result<Vec<u64>, ConsensusError> {
        self.latest_final_periods
            .transaction(|db| {
                let mut res = Vec::with_capacity(self.cfg.thread_count as usize);
                for thread in 0..self.cfg.thread_count {
                    if let Some(val) = db.get([thread])? {
                        let latest = array_from_slice(&val).map_err(|err| {
                            sled::transaction::ConflictableTransactionError::Abort(
                                InternalError::TransactionError(format!(
                                    "error getting latest final period for thread: {:?} {:?}",
                                    thread, err
                                )),
                            )
                        })?;
                        res.push(u64::from_be_bytes(latest));
                    } else {
                        // Note: this should never happen,
                        // since they are initialized in ::new().
                        return Err(sled::transaction::ConflictableTransactionError::Abort(
                            InternalError::TransactionError(format!(
                                "error getting latest final period for thread: {:?}",
                                thread
                            )),
                        ));
                    }
                }
                Ok(res)
            })
            .map_err(|_| {
                ConsensusError::LedgerInconsistency("Unable to fetch latest final periods.".into())
            })
    }

    /// To empty the db.
    pub fn clear(&self) -> Result<(), ConsensusError> {
        // Note: this cannot be done transactionally.
        for db in self.ledger_per_thread.iter() {
            db.clear()?;
        }
        self.latest_final_periods.clear()?;
        Ok(())
    }

    /// Used for bootstrap.
    // Note: this cannot be done transactionally.
    pub fn read_whole(&self) -> Result<LedgerSubset, ConsensusError> {
        let mut res = LedgerSubset::default();
        for tree in self.ledger_per_thread.iter() {
            for element in tree.iter() {
                let (addr, data) = element?;
                let address = Address::from_bytes(addr.as_ref().try_into()?)?;
                let (ledger_data, _) = LedgerData::from_bytes_compact(&data)?;
                if let Some(val) = res.0.insert(address, ledger_data) {
                    return Err(ConsensusError::LedgerInconsistency(format!(
                        "address {:?} twice in ledger",
                        val
                    )));
                }
            }
        }
        Ok(res)
    }

    /// Gets ledger at latest final blocks for query_addrs
    pub fn get_final_ledger_subset(
        &self,
        query_addrs: &HashSet<Address>,
    ) -> Result<LedgerSubset, ConsensusError> {
        let res = self.ledger_per_thread.transaction(|ledger_per_thread| {
            let mut data = LedgerSubset::default();
            for addr in query_addrs {
                let thread = addr.get_thread(self.cfg.thread_count);
                if let Some(data_bytes) = ledger_per_thread[thread as usize].get(addr.to_bytes())? {
                    let (ledger_data, _) =
                        LedgerData::from_bytes_compact(&data_bytes).map_err(|err| {
                            sled::transaction::ConflictableTransactionError::Abort(
                                InternalError::TransactionError(format!(
                                    "error deserializing ledger data: {:?}",
                                    err
                                )),
                            )
                        })?;
                    data.0.insert(*addr, ledger_data);
                } else {
                    data.0.insert(*addr, LedgerData::default());
                }
            }
            Ok(data)
        })?;
        Ok(res)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct LedgerSubset(pub HashMap<Address, LedgerData>);

impl LedgerSubset {
    /// If subset contains given address
    pub fn contains(&self, address: &Address) -> bool {
        self.0.contains_key(address)
    }

    /// Get the data for given address
    pub fn get_data(&self, address: &Address) -> LedgerData {
        self.0.get(address).cloned().unwrap_or(LedgerData {
            balance: Amount::default(),
        })
    }

    /// List involved addresses
    pub fn get_involved_addresses(&self) -> HashSet<Address> {
        self.0.keys().copied().collect()
    }

    /// Applies given change to that ledger subset
    /// note: a failure may still leave the entry modified
    pub fn apply_change(
        &mut self,
        addr: &Address,
        change: &LedgerChange,
    ) -> Result<(), ConsensusError> {
        match self.0.entry(*addr) {
            hash_map::Entry::Occupied(mut occ) => {
                occ.get_mut().apply_change(change)?;
                if occ.get().is_nil() {
                    occ.remove();
                }
            }
            hash_map::Entry::Vacant(vac) => {
                let mut res = LedgerData::default();
                res.apply_change(change)?;
                if !res.is_nil() {
                    vac.insert(res);
                }
            }
        }
        Ok(())
    }

    /// apply ledger changes
    ///  note: a failure may still leave the entry modified
    pub fn apply_changes(&mut self, changes: &LedgerChanges) -> Result<(), ConsensusError> {
        for (addr, change) in changes.0.iter() {
            self.apply_change(addr, change)?;
        }
        Ok(())
    }

    /// Applies thread changes change to that ledger subset
    /// note: a failure may still leave the entry modified
    pub fn chain(&mut self, changes: &LedgerChanges) -> Result<(), ConsensusError> {
        for (addr, change) in changes.0.iter() {
            self.apply_change(addr, change)?;
        }
        Ok(())
    }

    /// merge another ledger subset into self, overwriting existing data
    /// addrs that are in not other are removed from self
    pub fn sync_from(&mut self, addrs: &HashSet<Address>, mut other: LedgerSubset) {
        for addr in addrs.iter() {
            if let Some(new_val) = other.0.remove(addr) {
                self.0.insert(*addr, new_val);
            } else {
                self.0.remove(addr);
            }
        }
    }

    /// clone subset
    pub fn clone_subset(&self, addrs: &HashSet<Address>) -> Self {
        LedgerSubset(
            self.0
                .iter()
                .filter_map(|(a, dta)| {
                    if addrs.contains(a) {
                        Some((*a, dta.clone()))
                    } else {
                        None
                    }
                })
                .collect(),
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LedgerExport {
    pub ledger_subset: Vec<(Address, LedgerData)>,
}

impl<'a> TryFrom<&'a Ledger> for LedgerExport {
    type Error = ConsensusError;

    fn try_from(value: &'a Ledger) -> Result<Self, Self::Error> {
        Ok(LedgerExport {
            ledger_subset: value
                .read_whole()?
                .0
                .iter()
                .map(|(k, v)| (*k, v.clone()))
                .collect(),
        })
    }
}

impl SerializeCompact for LedgerExport {
    /// ## Example
    /// ```rust
    /// # use models::{SerializeCompact, DeserializeCompact, SerializationContext, Address, Amount};
    /// # use std::str::FromStr;
    /// # use consensus::{LedgerExport, LedgerData};
    /// # let mut ledger = LedgerExport::default();
    /// # ledger.ledger_subset = vec![
    /// #   (Address::from_bs58_check("2oxLZc6g6EHfc5VtywyPttEeGDxWq3xjvTNziayWGDfxETZVTi".into()).unwrap(), LedgerData::new(Amount::from_str("1022").unwrap())),
    /// #   (Address::from_bs58_check("2mvD6zEvo8gGaZbcs6AYTyWKFonZaKvKzDGRsiXhZ9zbxPD11q".into()).unwrap(), LedgerData::new(Amount::from_str("1020").unwrap())),
    /// # ];
    /// # models::init_serialization_context(models::SerializationContext {
    /// #     max_block_operations: 1024,
    /// #     parent_count: 2,
    /// #     max_peer_list_length: 128,
    /// #     max_message_size: 3 * 1024 * 1024,
    /// #     max_block_size: 3 * 1024 * 1024,
    /// #     max_bootstrap_blocks: 100,
    /// #     max_bootstrap_cliques: 100,
    /// #     max_bootstrap_deps: 100,
    /// #     max_bootstrap_children: 100,
    /// #     max_ask_blocks_per_message: 10,
    /// #     max_operations_per_message: 1024,
    /// #     max_endorsements_per_message: 1024,
    /// #     max_bootstrap_message_size: 100000000,
    /// #     max_bootstrap_pos_cycles: 10000,
    /// #     max_bootstrap_pos_entries: 10000,
    /// #     max_block_endorsments: 8,
    /// # });
    /// let bytes = ledger.clone().to_bytes_compact().unwrap();
    /// let (res, _) = LedgerExport::from_bytes_compact(&bytes).unwrap();
    /// for (address, data) in &ledger.ledger_subset {
    ///    assert!(res.ledger_subset.iter().filter(|(addr, dta)| address == addr && dta.to_bytes_compact().unwrap() == data.to_bytes_compact().unwrap()).count() == 1)
    /// }
    /// assert_eq!(ledger.ledger_subset.len(), res.ledger_subset.len());
    /// ```
    fn to_bytes_compact(&self) -> Result<Vec<u8>, models::ModelsError> {
        let mut res: Vec<u8> = Vec::new();

        let entry_count: u64 = self.ledger_subset.len().try_into().map_err(|err| {
            models::ModelsError::SerializeError(format!(
                "too many entries in LedgerExport: {:?}",
                err
            ))
        })?;
        res.extend(entry_count.to_varint_bytes());
        for (address, data) in self.ledger_subset.iter() {
            res.extend(&address.to_bytes());
            res.extend(&data.to_bytes_compact()?);
        }

        Ok(res)
    }
}

impl DeserializeCompact for LedgerExport {
    fn from_bytes_compact(buffer: &[u8]) -> Result<(Self, usize), models::ModelsError> {
        let mut cursor = 0usize;

        let (entry_count, delta) = u64::from_varint_bytes(&buffer[cursor..])?;
        //TODO add entry_count checks
        cursor += delta;

        let mut ledger_subset = Vec::with_capacity(entry_count as usize);
        for _ in 0..entry_count {
            let address = Address::from_bytes(&array_from_slice(&buffer[cursor..])?)?;
            cursor += ADDRESS_SIZE_BYTES;

            let (data, delta) = LedgerData::from_bytes_compact(&buffer[cursor..])?;
            cursor += delta;

            ledger_subset.push((address, data));
        }

        Ok((LedgerExport { ledger_subset }, cursor))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::str::FromStr;

    #[test]
    #[serial]
    fn test_ledger_change_chain() {
        for &v1 in &[-100i32, -10, 0, 10, 100] {
            for &v2 in &[-100i32, -10, 0, 10, 100] {
                let mut res = LedgerChange {
                    balance_increment: (v1 >= 0),
                    balance_delta: Amount::from_str(&v1.abs().to_string()).unwrap(),
                };
                res.chain(&LedgerChange {
                    balance_increment: (v2 >= 0),
                    balance_delta: Amount::from_str(&v2.abs().to_string()).unwrap(),
                })
                .unwrap();
                let expect: i32 = v1 + v2;
                assert_eq!(res.balance_increment, (expect >= 0));
                assert_eq!(
                    res.balance_delta,
                    Amount::from_str(&expect.abs().to_string()).unwrap()
                );
            }
        }
    }
}
