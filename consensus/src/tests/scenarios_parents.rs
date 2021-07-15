// Copyright (c) 2021 MASSA LABS <info@massa.net>

use std::collections::HashMap;

use crate::tests::tools::{self, generate_ledger_file};
use models::Slot;
use serial_test::serial;

#[tokio::test]
#[serial]
async fn test_parent_in_the_future() {
    let ledger_file = generate_ledger_file(&HashMap::new());
    let staking_keys: Vec<crypto::signature::PrivateKey> = (0..1)
        .map(|_| crypto::generate_random_private_key())
        .collect();
    let staking_file = tools::generate_staking_keys_file(&staking_keys);
    let roll_counts_file = tools::generate_default_roll_counts_file(staking_keys.clone());
    let mut cfg = tools::default_consensus_config(
        ledger_file.path(),
        roll_counts_file.path(),
        staking_file.path(),
    );
    cfg.t0 = 1000.into();
    cfg.future_block_processing_max_periods = 50;
    cfg.max_future_processing_blocks = 10;

    tools::consensus_without_pool_test(
        cfg.clone(),
        None,
        async move |mut protocol_controller, consensus_command_sender, consensus_event_receiver| {
            let genesis_hashes = consensus_command_sender
                .get_block_graph_status()
                .await
                .expect("could not get block graph status")
                .genesis_blocks;

            // Parent, in the future.
            let (hasht0s1, _, _) = tools::create_block(
                &cfg,
                Slot::new(4, 0),
                genesis_hashes.clone(),
                staking_keys[0].clone(),
            );

            let _ = tools::create_and_test_block(
                &mut protocol_controller,
                &cfg,
                Slot::new(5, 0),
                vec![hasht0s1],
                false,
                false,
                staking_keys[0].clone(),
            )
            .await;
            (
                protocol_controller,
                consensus_command_sender,
                consensus_event_receiver,
            )
        },
    )
    .await;
}

#[tokio::test]
#[serial]
async fn test_parents() {
    let ledger_file = generate_ledger_file(&HashMap::new());
    let staking_keys: Vec<crypto::signature::PrivateKey> = (0..1)
        .map(|_| crypto::generate_random_private_key())
        .collect();
    let staking_file = tools::generate_staking_keys_file(&staking_keys);
    let roll_counts_file = tools::generate_default_roll_counts_file(staking_keys.clone());
    let mut cfg = tools::default_consensus_config(
        ledger_file.path(),
        roll_counts_file.path(),
        staking_file.path(),
    );
    cfg.t0 = 1000.into();
    cfg.future_block_processing_max_periods = 50;
    cfg.max_future_processing_blocks = 10;

    tools::consensus_without_pool_test(
        cfg.clone(),
        None,
        async move |mut protocol_controller, consensus_command_sender, consensus_event_receiver| {
            let genesis_hashes = consensus_command_sender
                .get_block_graph_status()
                .await
                .expect("could not get block graph status")
                .genesis_blocks;

            // generate two normal blocks in each thread
            let hasht1s1 = tools::create_and_test_block(
                &mut protocol_controller,
                &cfg,
                Slot::new(1, 0),
                genesis_hashes.clone(),
                true,
                false,
                staking_keys[0].clone(),
            )
            .await;

            let _ = tools::create_and_test_block(
                &mut protocol_controller,
                &cfg,
                Slot::new(1, 1),
                genesis_hashes.clone(),
                true,
                false,
                staking_keys[0].clone(),
            )
            .await;

            let _ = tools::create_and_test_block(
                &mut protocol_controller,
                &cfg,
                Slot::new(3, 0),
                vec![hasht1s1, genesis_hashes[0].clone()],
                false,
                false,
                staking_keys[0].clone(),
            )
            .await;
            (
                protocol_controller,
                consensus_command_sender,
                consensus_event_receiver,
            )
        },
    )
    .await;
}

#[tokio::test]
#[serial]
async fn test_parents_in_incompatible_cliques() {
    let ledger_file = generate_ledger_file(&HashMap::new());
    let staking_keys: Vec<crypto::signature::PrivateKey> = (0..1)
        .map(|_| crypto::generate_random_private_key())
        .collect();
    let staking_file = tools::generate_staking_keys_file(&staking_keys);
    let roll_counts_file = tools::generate_default_roll_counts_file(staking_keys.clone());
    let mut cfg = tools::default_consensus_config(
        ledger_file.path(),
        roll_counts_file.path(),
        staking_file.path(),
    );
    cfg.t0 = 1000.into();
    cfg.future_block_processing_max_periods = 50;
    cfg.max_future_processing_blocks = 10;

    tools::consensus_without_pool_test(
        cfg.clone(),
        None,
        async move |mut protocol_controller, consensus_command_sender, consensus_event_receiver| {
            let genesis_hashes = consensus_command_sender
                .get_block_graph_status()
                .await
                .expect("could not get block graph status")
                .genesis_blocks;

            let hasht0s1 = tools::create_and_test_block(
                &mut protocol_controller,
                &cfg,
                Slot::new(1, 0),
                genesis_hashes.clone(),
                true,
                false,
                staking_keys[0].clone(),
            )
            .await;

            let hasht0s2 = tools::create_and_test_block(
                &mut protocol_controller,
                &cfg,
                Slot::new(2, 0),
                genesis_hashes.clone(),
                true,
                false,
                staking_keys[0].clone(),
            )
            .await;

            // from that point we have two incompatible clique

            let _ = tools::create_and_test_block(
                &mut protocol_controller,
                &cfg,
                Slot::new(1, 1),
                vec![hasht0s1, genesis_hashes[1]],
                true,
                false,
                staking_keys[0].clone(),
            )
            .await;

            // Block with incompatible parents.
            let _ = tools::create_and_test_block(
                &mut protocol_controller,
                &cfg,
                Slot::new(2, 1),
                vec![hasht0s1, hasht0s2],
                false,
                false,
                staking_keys[0].clone(),
            )
            .await;
            (
                protocol_controller,
                consensus_command_sender,
                consensus_event_receiver,
            )
        },
    )
    .await;
}
