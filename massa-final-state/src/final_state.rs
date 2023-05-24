//! Copyright (c) 2022 MASSA LABS <info@massa.net>

//! This file defines the final state of the node, which includes
//! the final ledger and asynchronous message pool that are kept at
//! the output of a given final slot (the latest executed final slot),
//! and need to be bootstrapped by nodes joining the network.

use crate::{config::FinalStateConfig, error::FinalStateError, state_changes::StateChanges};
use massa_async_pool::AsyncPool;
use massa_db::{DBBatch, MassaDB};
use massa_db::{
    ASYNC_POOL_PREFIX, CYCLE_HISTORY_PREFIX, DEFERRED_CREDITS_PREFIX,
    EXECUTED_DENUNCIATIONS_PREFIX, EXECUTED_OPS_PREFIX, LEDGER_PREFIX, STATE_CF,
};
use massa_executed_ops::ExecutedDenunciations;
use massa_executed_ops::ExecutedOps;
use massa_ledger_exports::LedgerController;
use massa_models::config::PERIODS_BETWEEN_BACKUPS;
use massa_models::slot::Slot;
use massa_pos_exports::{PoSFinalState, SelectorController};
use parking_lot::RwLock;
use rocksdb::IteratorMode;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Represents a final state `(ledger, async pool, executed_ops, executed_de and the state of the PoS)`
pub struct FinalState {
    /// execution state configuration
    pub(crate) config: FinalStateConfig,
    /// slot at the output of which the state is attached
    pub slot: Slot,
    /// final ledger associating addresses to their balance, executable bytecode and data
    pub ledger: Box<dyn LedgerController>,
    /// asynchronous pool containing messages sorted by priority and their data
    pub async_pool: AsyncPool,
    /// proof of stake state containing cycle history and deferred credits
    pub pos_state: PoSFinalState,
    /// executed operations
    pub executed_ops: ExecutedOps,
    /// executed denunciations
    pub executed_denunciations: ExecutedDenunciations,
    /// last_start_period
    /// * If start new network: set to 0
    /// * If from snapshot: retrieve from args
    /// * If from bootstrap: set during bootstrap
    pub last_start_period: u64,
    /// last_slot_before_downtime
    /// * None if start new network
    /// * If from snapshot: retrieve from the slot attached to the snapshot
    /// * If from bootstrap: set during bootstrap
    pub last_slot_before_downtime: Option<Slot>,
    /// the rocksdb instance used to write every final_state struct on disk
    pub db: Arc<RwLock<MassaDB>>,
}

impl FinalState {
    /// Initializes a new `FinalState`
    ///
    /// # Arguments
    /// * `config`: the configuration of the final state to use for initialization
    /// * `ledger`: the instance of the ledger on disk. Used to apply changes to the ledger.
    /// * `selector`: the pos selector. Used to send draw inputs when a new cycle is completed.
    /// * `reset_final_state`: if true, we only keep the ledger, and we reset the other fields of the final state
    pub fn new(
        db: Arc<RwLock<MassaDB>>,
        config: FinalStateConfig,
        ledger: Box<dyn LedgerController>,
        selector: Box<dyn SelectorController>,
        reset_final_state: bool,
    ) -> Result<Self, FinalStateError> {
        let state_slot = db.read().get_change_id();
        let recovered_hash = db.read().get_db_hash();

        match state_slot {
            Ok(slot) => {
                info!(
                    "Recovered ledger. state slot: {}, state hash: {}",
                    slot, recovered_hash
                );
            }
            Err(_e) => {
                info!(
                    "Recovered ledger. Unknown state slot, state hash: {}",
                    recovered_hash
                );
            }
        }

        // create the pos state
        let pos_state = PoSFinalState::new(
            config.pos_config.clone(),
            &config.initial_seed_string,
            &config.initial_rolls_path,
            selector,
            db.read().get_db_hash(),
            db.clone(),
        )
        .map_err(|err| FinalStateError::PosError(format!("PoS final state init error: {}", err)))?;

        // attach at the output of the latest initial final slot, that is the last genesis slot
        let slot = Slot::new(0, config.thread_count.saturating_sub(1));

        // create the async pool
        let async_pool = AsyncPool::new(config.async_pool_config.clone(), db.clone());

        // create a default executed ops
        let executed_ops = ExecutedOps::new(config.executed_ops_config.clone(), db.clone());

        // create a default executed denunciations
        let executed_denunciations =
            ExecutedDenunciations::new(config.executed_denunciations_config.clone(), db.clone());

        let mut final_state = FinalState {
            slot,
            ledger,
            async_pool,
            pos_state,
            config,
            executed_ops,
            executed_denunciations,
            last_start_period: 0,
            last_slot_before_downtime: None,
            db,
        };

        if reset_final_state {
            final_state.async_pool.reset();
            final_state.pos_state.reset();
            final_state.executed_ops.reset();
            final_state.executed_denunciations.reset();
        }

        info!(
            "final_state hash at slot {}: {}",
            slot,
            final_state.db.read().get_db_hash()
        );

        // create the final state
        Ok(final_state)
    }

    /// Initializes a `FinalState` from a snapshot. Currently, we do not use the final_state from the ledger,
    /// we just create a new one. This will be changed in the follow-up.
    ///
    /// # Arguments
    /// * `config`: the configuration of the final state to use for initialization
    /// * `ledger`: the instance of the ledger on disk. Used to apply changes to the ledger.
    /// * `selector`: the pos selector. Used to send draw inputs when a new cycle is completed.
    /// * `last_start_period`: at what period we should attach the final_state
    pub fn new_derived_from_snapshot(
        db: Arc<RwLock<MassaDB>>,
        config: FinalStateConfig,
        ledger: Box<dyn LedgerController>,
        selector: Box<dyn SelectorController>,
        last_start_period: u64,
    ) -> Result<Self, FinalStateError> {
        info!("Restarting from snapshot");

        // FIRST, we recover the last known final_state
        let mut final_state = FinalState::new(db, config, ledger, selector, false)?;

        final_state.slot = final_state.db.read().get_change_id().map_err(|_| {
            FinalStateError::InvalidSlot(String::from("Could not recover Slot in Ledger"))
        })?;

        // This is needed for `test_bootstrap_server` to work
        if cfg!(feature = "testing") {
            let mut batch = DBBatch::new();
            final_state.pos_state.create_initial_cycle(&mut batch);
            final_state
                .db
                .write()
                .write_batch(batch, Some(final_state.slot));
        }

        final_state.last_slot_before_downtime = Some(final_state.slot);

        debug!(
            "Latest consistent slot found in snapshot data: {}",
            final_state.slot
        );

        info!(
            "final_state hash at slot {}: {}",
            final_state.slot,
            final_state.db.read().get_db_hash()
        );

        // Then, interpolate the downtime, to attach at end_slot;
        final_state.last_start_period = last_start_period;

        final_state.init_ledger_hash();

        // We compute the draws here because we need to feed_cycles when interpolating
        final_state.compute_initial_draws()?;

        final_state.interpolate_downtime()?;

        Ok(final_state)
    }

    /// Used after bootstrap, to set the initial ledger hash (used in initial draws)
    pub fn init_ledger_hash(&mut self) {
        self.pos_state.initial_ledger_hash = self.db.read().get_db_hash();

        info!(
            "Set initial ledger hash to {}",
            self.db.read().get_db_hash().to_string()
        )
    }

    /// Once we created a FinalState from a snapshot, we need to edit it to attach at the end_slot and handle the downtime.
    /// This basically recreates the history of the final_state, without executing the slots.
    fn interpolate_downtime(&mut self) -> Result<(), FinalStateError> {
        // TODO: Change the current_slot when we deserialize the final state from RocksDB. Until then, final_state slot and the ledger slot are not consistent!
        // let current_slot = self.slot;
        let current_slot = Slot::new(0, self.config.thread_count.saturating_sub(1));
        let current_slot_cycle = current_slot.get_cycle(self.config.periods_per_cycle);

        let end_slot = Slot::new(
            self.last_start_period,
            self.config.thread_count.saturating_sub(1),
        );
        let end_slot_cycle = end_slot.get_cycle(self.config.periods_per_cycle);

        if current_slot_cycle == end_slot_cycle {
            // In that case, we just complete the gap in the same cycle
            self.interpolate_single_cycle(current_slot, end_slot)?;
        } else {
            // Here, we we also complete the cycle_infos in between
            self.interpolate_multiple_cycles(
                current_slot,
                end_slot,
                current_slot_cycle,
                end_slot_cycle,
            )?;
        }

        self.slot = end_slot;

        // Recompute the hash with the updated data and feed it to POS_state.

        let final_state_hash = self.db.read().get_db_hash();

        info!(
            "final_state hash at slot {}: {}",
            self.slot, final_state_hash
        );

        // feed final_state_hash to the last cycle
        let cycle = self.slot.get_cycle(self.config.periods_per_cycle);
        self.pos_state
            .feed_cycle_state_hash(cycle, final_state_hash);

        Ok(())
    }

    /// This helper function is to be called if the downtime does not span over multiple cycles
    fn interpolate_single_cycle(
        &mut self,
        current_slot: Slot,
        end_slot: Slot,
    ) -> Result<(), FinalStateError> {
        let latest_snapshot_cycle =
            self.pos_state
                .cycle_history_cache
                .pop_back()
                .ok_or(FinalStateError::SnapshotError(String::from(
                    "Invalid cycle_history",
                )))?;

        let latest_snapshot_cycle_info = self.pos_state.get_cycle_info(latest_snapshot_cycle.0);

        let mut batch = DBBatch::new();

        self.pos_state
            .delete_cycle_info(latest_snapshot_cycle.0, &mut batch);

        self.pos_state.db.write().write_batch(batch, None);

        let mut batch = DBBatch::new();

        self.pos_state
            .create_new_cycle_from_last(
                &latest_snapshot_cycle_info,
                current_slot
                    .get_next_slot(self.config.thread_count)
                    .expect("Cannot get next slot"),
                end_slot,
                &mut batch,
            )
            .map_err(|err| FinalStateError::PosError(format!("{}", err)))?;

        self.pos_state.db.write().write_batch(batch, None);

        Ok(())
    }

    /// This helper function is to be called if the downtime spans over multiple cycles
    fn interpolate_multiple_cycles(
        &mut self,
        current_slot: Slot,
        end_slot: Slot,
        current_slot_cycle: u64,
        end_slot_cycle: u64,
    ) -> Result<(), FinalStateError> {
        let latest_snapshot_cycle =
            self.pos_state
                .cycle_history_cache
                .pop_back()
                .ok_or(FinalStateError::SnapshotError(String::from(
                    "Invalid cycle_history",
                )))?;

        let latest_snapshot_cycle_info = self.pos_state.get_cycle_info(latest_snapshot_cycle.0);

        let mut batch = DBBatch::new();

        self.pos_state
            .delete_cycle_info(latest_snapshot_cycle.0, &mut batch);

        self.pos_state.db.write().write_batch(batch, None);

        // Firstly, complete the first cycle
        let last_slot = Slot::new_last_of_cycle(
            current_slot_cycle,
            self.config.periods_per_cycle,
            self.config.thread_count,
        )
        .map_err(|err| {
            FinalStateError::InvalidSlot(format!(
                "Cannot create slot for interpolating downtime: {}",
                err
            ))
        })?;

        let mut batch = DBBatch::new();

        self.pos_state
            .create_new_cycle_from_last(
                &latest_snapshot_cycle_info,
                current_slot
                    .get_next_slot(self.config.thread_count)
                    .expect("Cannot get next slot"),
                last_slot,
                &mut batch,
            )
            .map_err(|err| FinalStateError::PosError(format!("{}", err)))?;

        self.pos_state.db.write().write_batch(batch, None);

        // Feed final_state_hash to the completed cycle
        self.feed_cycle_hash_and_selector_for_interpolation(current_slot_cycle)?;

        // Then, build all the completed cycles in betweens. If we have to build more cycles than the cycle_history_length, we only build the last ones.
        let current_slot_cycle = (current_slot_cycle + 1)
            .max(end_slot_cycle.saturating_sub(self.config.pos_config.cycle_history_length as u64));

        for cycle in current_slot_cycle..end_slot_cycle {
            let first_slot = Slot::new_first_of_cycle(cycle, self.config.periods_per_cycle)
                .map_err(|err| {
                    FinalStateError::InvalidSlot(format!(
                        "Cannot create slot for interpolating downtime: {}",
                        err
                    ))
                })?;

            let last_slot = Slot::new_last_of_cycle(
                cycle,
                self.config.periods_per_cycle,
                self.config.thread_count,
            )
            .map_err(|err| {
                FinalStateError::InvalidSlot(format!(
                    "Cannot create slot for interpolating downtime: {}",
                    err
                ))
            })?;

            let mut batch = DBBatch::new();

            self.pos_state
                .create_new_cycle_from_last(
                    &latest_snapshot_cycle_info,
                    first_slot,
                    last_slot,
                    &mut batch,
                )
                .map_err(|err| FinalStateError::PosError(format!("{}", err)))?;

            self.pos_state.db.write().write_batch(batch, None);

            // Feed final_state_hash to the completed cycle
            self.feed_cycle_hash_and_selector_for_interpolation(cycle)?;
        }

        // Then, build the last cycle
        let first_slot = Slot::new_first_of_cycle(end_slot_cycle, self.config.periods_per_cycle)
            .map_err(|err| {
                FinalStateError::InvalidSlot(format!(
                    "Cannot create slot for interpolating downtime: {}",
                    err
                ))
            })?;

        let mut batch = DBBatch::new();

        self.pos_state
            .create_new_cycle_from_last(
                &latest_snapshot_cycle_info,
                first_slot,
                end_slot,
                &mut batch,
            )
            .map_err(|err| FinalStateError::PosError(format!("{}", err)))?;

        // If the end_slot_cycle is completed
        if end_slot.is_last_of_cycle(self.config.periods_per_cycle, self.config.thread_count) {
            // Feed final_state_hash to the completed cycle
            self.feed_cycle_hash_and_selector_for_interpolation(end_slot_cycle)?;
        }

        // We reduce the cycle_history len as needed
        while self.pos_state.cycle_history_cache.len() > self.pos_state.config.cycle_history_length
        {
            if let Some((cycle, _)) = self.pos_state.cycle_history_cache.pop_front() {
                self.pos_state.delete_cycle_info(cycle, &mut batch);
            }
        }

        self.db.write().write_batch(batch, None);

        Ok(())
    }

    /// Used during interpolation, when a new cycle is set as completed
    fn feed_cycle_hash_and_selector_for_interpolation(
        &mut self,
        cycle: u64,
    ) -> Result<(), FinalStateError> {
        self.pos_state
            .feed_cycle_state_hash(cycle, self.db.read().get_db_hash());

        self.pos_state
            .feed_selector(cycle.checked_add(2).ok_or_else(|| {
                FinalStateError::PosError("cycle overflow when feeding selector".into())
            })?)
            .map_err(|_| {
                FinalStateError::PosError("cycle overflow when feeding selector".into())
            })?;
        Ok(())
    }

    /// Reset the final state to the initial state.
    ///
    /// USED ONLY FOR BOOTSTRAP
    pub fn reset(&mut self) {
        self.slot = Slot::new(0, self.config.thread_count.saturating_sub(1));
        self.db.write().reset(self.slot);
        self.ledger.reset();
        self.async_pool.reset();
        self.pos_state.reset();
        self.executed_ops.reset();
        self.executed_denunciations.reset();
    }

    /// Performs the initial draws.
    pub fn compute_initial_draws(&mut self) -> Result<(), FinalStateError> {
        self.pos_state
            .compute_initial_draws()
            .map_err(|err| FinalStateError::PosError(err.to_string()))
    }

    /// Applies changes to the execution state at a given slot, and settles that slot forever.
    /// Once this is called, the state is attached at the output of the provided slot.
    ///
    /// Panics if the new slot is not the one coming just after the current one.
    pub fn finalize(&mut self, slot: Slot, changes: StateChanges) {
        // check slot consistency
        let next_slot = self
            .slot
            .get_next_slot(self.config.thread_count)
            .expect("overflow in execution state slot");
        if slot != next_slot {
            panic!("attempting to apply execution state changes at slot {} while the current slot is {}", slot, self.slot);
        }

        // update current slot
        self.slot = slot;

        let mut db_batch = DBBatch::new();

        // apply the state changes to the batch

        self.async_pool
            .apply_changes_to_batch(&changes.async_pool_changes, &mut db_batch);

        self.pos_state
            .apply_changes_to_batch(changes.pos_changes.clone(), self.slot, true, &mut db_batch)
            .expect("could not settle slot in final state proof-of-stake");
        // TODO:
        // do not panic above, it might just mean that the lookback cycle is not available
        // bootstrap again instead

        self.ledger
            .apply_changes_to_batch(changes.ledger_changes.clone(), &mut db_batch);

        self.executed_ops.apply_changes_to_batch(
            changes.executed_ops_changes.clone(),
            self.slot,
            &mut db_batch,
        );

        self.executed_denunciations.apply_changes_to_batch(
            changes.executed_denunciations_changes.clone(),
            self.slot,
            &mut db_batch,
        );

        self.db.write().write_batch(db_batch, Some(self.slot));

        let final_state_hash = self.db.read().get_db_hash();

        // compute the final state hash
        info!(
            "final_state hash at slot {}: {}",
            self.slot, final_state_hash
        );

        // Backup DB if needed
        if self.slot.period % PERIODS_BETWEEN_BACKUPS == 0 && self.slot.period != 0 {
            let state_slot = self.db.read().get_change_id();
            match state_slot {
                Ok(slot) => {
                    info!(
                        "Backuping db for slot {}, state slot: {}, state hash: {}",
                        self.slot, slot, final_state_hash
                    );
                }
                Err(e) => {
                    info!("{}", e);
                    info!(
                        "Backuping db for unknown state slot, state hash: {}",
                        final_state_hash
                    );
                }
            }

            self.db.read().backup_db(self.slot);
        }

        // feed final_state_hash to the last cycle
        let cycle = slot.get_cycle(self.config.periods_per_cycle);
        self.pos_state
            .feed_cycle_state_hash(cycle, final_state_hash);
    }

    /// After bootstrap or load from disk, recompute all the caches.
    pub fn recompute_caches(&mut self) {
        self.async_pool.recompute_message_info_cache();
        self.executed_ops.recompute_sorted_ops_and_op_exec_status();
        self.executed_denunciations.recompute_sorted_denunciations();
        self.pos_state.recompute_pos_state_caches();
    }

    /// Deserialize the entire DB and check the data. Useful to check after bootstrap.
    pub fn is_db_valid(&self) -> bool {
        let db = self.db.read();
        let handle = db.db.cf_handle(STATE_CF).unwrap();

        for (serialized_key, serialized_value) in
            db.db.iterator_cf(handle, IteratorMode::Start).flatten()
        {
            if !serialized_key.starts_with(CYCLE_HISTORY_PREFIX.as_bytes())
                && !serialized_key.starts_with(DEFERRED_CREDITS_PREFIX.as_bytes())
                && !serialized_key.starts_with(ASYNC_POOL_PREFIX.as_bytes())
                && !serialized_key.starts_with(EXECUTED_OPS_PREFIX.as_bytes())
                && !serialized_key.starts_with(EXECUTED_DENUNCIATIONS_PREFIX.as_bytes())
                && !serialized_key.starts_with(LEDGER_PREFIX.as_bytes())
            {
                warn!(
                    "Key/value does not correspond to any prefix: serialized_key: {:?}, serialized_value: {:?}",
                    serialized_key, serialized_value
                );
                println!(
                    "Key/value does not correspond to any prefix: serialized_key: {:?}, serialized_value: {:?}",
                    serialized_key, serialized_value
                );
                return false;
            }

            if serialized_key.starts_with(CYCLE_HISTORY_PREFIX.as_bytes()) {
                if !self
                    .pos_state
                    .is_cycle_history_key_value_valid(&serialized_key, &serialized_value)
                {
                    warn!(
                        "Wrong key/value for CYCLE_HISTORY_KEY PREFIX serialized_key: {:?}, serialized_value: {:?}",
                        serialized_key, serialized_value
                    );
                    println!(
                        "Wrong key/value for CYCLE_HISTORY_KEY PREFIX serialized_key: {:?}, serialized_value: {:?}",
                        serialized_key, serialized_value
                    );
                    return false;
                }
            } else if serialized_key.starts_with(DEFERRED_CREDITS_PREFIX.as_bytes()) {
                if !self
                    .pos_state
                    .is_deferred_credits_key_value_valid(&serialized_key, &serialized_value)
                {
                    warn!(
                        "Wrong key/value for DEFERRED_CREDITS PREFIX serialized_key: {:?}, serialized_value: {:?}",
                        serialized_key, serialized_value
                    );
                    println!(
                        "Wrong key/value for DEFERRED_CREDITS PREFIX serialized_key: {:?}, serialized_value: {:?}",
                        serialized_key, serialized_value
                    );
                    return false;
                }
            } else if serialized_key.starts_with(ASYNC_POOL_PREFIX.as_bytes()) {
                if !self
                    .async_pool
                    .is_key_value_valid(&serialized_key, &serialized_value)
                {
                    warn!(
                        "Wrong key/value for ASYNC_POOL PREFIX serialized_key: {:?}, serialized_value: {:?}",
                        serialized_key, serialized_value
                    );
                    println!(
                        "Wrong key/value for ASYNC_POOL PREFIX serialized_key: {:?}, serialized_value: {:?}",
                        serialized_key, serialized_value
                    );
                    return false;
                }
            } else if serialized_key.starts_with(EXECUTED_OPS_PREFIX.as_bytes()) {
                if !self
                    .executed_ops
                    .is_key_value_valid(&serialized_key, &serialized_value)
                {
                    warn!(
                        "Wrong key/value for EXECUTED_OPS PREFIX serialized_key: {:?}, serialized_value: {:?}",
                        serialized_key, serialized_value
                    );
                    println!(
                        "Wrong key/value for EXECUTED_OPS PREFIX serialized_key: {:?}, serialized_value: {:?}",
                        serialized_key, serialized_value
                    );
                    return false;
                }
            } else if serialized_key.starts_with(EXECUTED_DENUNCIATIONS_PREFIX.as_bytes()) {
                if !self
                    .executed_denunciations
                    .is_key_value_valid(&serialized_key, &serialized_value)
                {
                    warn!("Wrong key/value for EXECUTED_DENUNCIATIONS PREFIX serialized_key: {:?}, serialized_value: {:?}", serialized_key, serialized_value);
                    println!("Wrong key/value for EXECUTED_DENUNCIATIONS PREFIX serialized_key: {:?}, serialized_value: {:?}", serialized_key, serialized_value);
                    return false;
                }
            } else if serialized_key.starts_with(LEDGER_PREFIX.as_bytes())
                && !self
                    .ledger
                    .is_key_value_valid(&serialized_key, &serialized_value)
            {
                warn!(
                    "Wrong key/value for LEDGER PREFIX serialized_key: {:?}, serialized_value: {:?}",
                    serialized_key, serialized_value
                );
                println!(
                    "Wrong key/value for LEDGER PREFIX serialized_key: {:?}, serialized_value: {:?}",
                    serialized_key, serialized_value
                );
                return false;
            }
        }

        true
    }
}
