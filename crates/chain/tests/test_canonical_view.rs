#![cfg(feature = "miniscript")]

use std::collections::BTreeMap;
use std::collections::HashSet;

use bdk_chain::{
    local_chain::LocalChain, BlockId, CanonicalParams, CanonicalReason, ChainPosition,
    ConfirmationBlockTime, TxGraph,
};
use bdk_testenv::{block_id, hash, local_chain, utils::new_tx};
use bitcoin::{Amount, BlockHash, OutPoint, ScriptBuf, Transaction, TxIn, TxOut, Txid};

mod common;
use common::*;

#[test]
fn test_min_confirmations_parameter() {
    // Create a local chain with several blocks
    let blocks: BTreeMap<u32, BlockHash> = [
        (0, hash!("block0")),
        (1, hash!("block1")),
        (2, hash!("block2")),
        (3, hash!("block3")),
        (4, hash!("block4")),
        (5, hash!("block5")),
        (6, hash!("block6")),
        (7, hash!("block7")),
        (8, hash!("block8")),
        (9, hash!("block9")),
        (10, hash!("block10")),
    ]
    .into_iter()
    .collect();
    let chain = LocalChain::from_blocks(blocks).unwrap();

    let mut tx_graph = TxGraph::default();

    // Create a non-coinbase transaction
    let tx = Transaction {
        input: vec![TxIn {
            previous_output: OutPoint::new(hash!("parent"), 0),
            ..Default::default()
        }],
        output: vec![TxOut {
            value: Amount::from_sat(50_000),
            script_pubkey: ScriptBuf::new(),
        }],
        ..new_tx(1)
    };
    let txid = tx.compute_txid();
    let outpoint = OutPoint::new(txid, 0);

    // Insert transaction into graph
    let _ = tx_graph.insert_tx(tx.clone());

    // Test 1: Transaction confirmed at height 5, tip at height 10 (6 confirmations)
    let anchor_height_5 = ConfirmationBlockTime {
        block_id: chain.get(5).unwrap().block_id(),
        confirmation_time: 123456,
    };
    let _ = tx_graph.insert_anchor(txid, anchor_height_5);

    let chain_tip = chain.tip().block_id();
    let canonical_view = chain.canonical_view(&tx_graph, chain_tip, CanonicalParams::default());

    // Test min_confirmations = 1: Should be confirmed (has 6 confirmations)
    let balance_1_conf = canonical_view.balance(
        [((), outpoint)],
        |_, _| true, // trust all
        1,
    );

    assert_eq!(balance_1_conf.confirmed, Amount::from_sat(50_000));
    assert_eq!(balance_1_conf.trusted_pending, Amount::ZERO);

    // Test min_confirmations = 6: Should be confirmed (has exactly 6 confirmations)
    let balance_6_conf = canonical_view.balance(
        [((), outpoint)],
        |_, _| true, // trust all
        6,
    );
    assert_eq!(balance_6_conf.confirmed, Amount::from_sat(50_000));
    assert_eq!(balance_6_conf.trusted_pending, Amount::ZERO);

    // Test min_confirmations = 7: Should be trusted pending (only has 6 confirmations)
    let balance_7_conf = canonical_view.balance(
        [((), outpoint)],
        |_, _| true, // trust all
        7,
    );
    assert_eq!(balance_7_conf.confirmed, Amount::ZERO);
    assert_eq!(balance_7_conf.trusted_pending, Amount::from_sat(50_000));

    // Test min_confirmations = 0: Should behave same as 1 (confirmed)
    let balance_0_conf = canonical_view.balance(
        [((), outpoint)],
        |_, _| true, // trust all
        0,
    );
    assert_eq!(balance_0_conf.confirmed, Amount::from_sat(50_000));
    assert_eq!(balance_0_conf.trusted_pending, Amount::ZERO);
    assert_eq!(balance_0_conf, balance_1_conf);
}

#[test]
fn test_min_confirmations_with_untrusted_tx() {
    // Create a local chain
    let blocks: BTreeMap<u32, BlockHash> = [
        (0, hash!("genesis")),
        (1, hash!("b1")),
        (2, hash!("b2")),
        (3, hash!("b3")),
        (4, hash!("b4")),
        (5, hash!("b5")),
        (6, hash!("b6")),
        (7, hash!("b7")),
        (8, hash!("b8")),
        (9, hash!("b9")),
        (10, hash!("tip")),
    ]
    .into_iter()
    .collect();
    let chain = LocalChain::from_blocks(blocks).unwrap();

    let mut tx_graph = TxGraph::default();

    // Create a transaction
    let tx = Transaction {
        input: vec![TxIn {
            previous_output: OutPoint::new(hash!("parent"), 0),
            ..Default::default()
        }],
        output: vec![TxOut {
            value: Amount::from_sat(25_000),
            script_pubkey: ScriptBuf::new(),
        }],
        ..new_tx(1)
    };
    let txid = tx.compute_txid();
    let outpoint = OutPoint::new(txid, 0);

    let _ = tx_graph.insert_tx(tx.clone());

    // Anchor at height 8, tip at height 10 (3 confirmations)
    let anchor = ConfirmationBlockTime {
        block_id: chain.get(8).unwrap().block_id(),
        confirmation_time: 123456,
    };
    let _ = tx_graph.insert_anchor(txid, anchor);

    let chain_tip = chain.tip().block_id();
    let canonical_view = chain.canonical_view(&tx_graph, chain_tip, CanonicalParams::default());

    // Test with min_confirmations = 5 and untrusted predicate
    let balance = canonical_view.balance(
        [((), outpoint)],
        |_, _| false, // don't trust
        5,
    );

    // Should be untrusted pending (not enough confirmations and not trusted)
    assert_eq!(balance.confirmed, Amount::ZERO);
    assert_eq!(balance.trusted_pending, Amount::ZERO);
    assert_eq!(balance.untrusted_pending, Amount::from_sat(25_000));
}

#[test]
fn test_min_confirmations_multiple_transactions() {
    // Create a local chain
    let blocks: BTreeMap<u32, BlockHash> = [
        (0, hash!("genesis")),
        (1, hash!("b1")),
        (2, hash!("b2")),
        (3, hash!("b3")),
        (4, hash!("b4")),
        (5, hash!("b5")),
        (6, hash!("b6")),
        (7, hash!("b7")),
        (8, hash!("b8")),
        (9, hash!("b9")),
        (10, hash!("b10")),
        (11, hash!("b11")),
        (12, hash!("b12")),
        (13, hash!("b13")),
        (14, hash!("b14")),
        (15, hash!("tip")),
    ]
    .into_iter()
    .collect();
    let chain = LocalChain::from_blocks(blocks).unwrap();

    let mut tx_graph = TxGraph::default();

    // Create multiple transactions at different heights
    let mut outpoints = vec![];

    // Transaction 0: anchored at height 5, has 11 confirmations (tip-5+1 = 15-5+1 = 11)
    let tx0 = Transaction {
        input: vec![TxIn {
            previous_output: OutPoint::new(hash!("parent0"), 0),
            ..Default::default()
        }],
        output: vec![TxOut {
            value: Amount::from_sat(10_000),
            script_pubkey: ScriptBuf::new(),
        }],
        ..new_tx(1)
    };
    let txid0 = tx0.compute_txid();
    let outpoint0 = OutPoint::new(txid0, 0);
    let _ = tx_graph.insert_tx(tx0);
    let _ = tx_graph.insert_anchor(
        txid0,
        ConfirmationBlockTime {
            block_id: chain.get(5).unwrap().block_id(),
            confirmation_time: 123456,
        },
    );
    outpoints.push(((), outpoint0));

    // Transaction 1: anchored at height 10, has 6 confirmations (15-10+1 = 6)
    let tx1 = Transaction {
        input: vec![TxIn {
            previous_output: OutPoint::new(hash!("parent1"), 0),
            ..Default::default()
        }],
        output: vec![TxOut {
            value: Amount::from_sat(20_000),
            script_pubkey: ScriptBuf::new(),
        }],
        ..new_tx(2)
    };
    let txid1 = tx1.compute_txid();
    let outpoint1 = OutPoint::new(txid1, 0);
    let _ = tx_graph.insert_tx(tx1);
    let _ = tx_graph.insert_anchor(
        txid1,
        ConfirmationBlockTime {
            block_id: chain.get(10).unwrap().block_id(),
            confirmation_time: 123457,
        },
    );
    outpoints.push(((), outpoint1));

    // Transaction 2: anchored at height 13, has 3 confirmations (15-13+1 = 3)
    let tx2 = Transaction {
        input: vec![TxIn {
            previous_output: OutPoint::new(hash!("parent2"), 0),
            ..Default::default()
        }],
        output: vec![TxOut {
            value: Amount::from_sat(30_000),
            script_pubkey: ScriptBuf::new(),
        }],
        ..new_tx(3)
    };
    let txid2 = tx2.compute_txid();
    let outpoint2 = OutPoint::new(txid2, 0);
    let _ = tx_graph.insert_tx(tx2);
    let _ = tx_graph.insert_anchor(
        txid2,
        ConfirmationBlockTime {
            block_id: chain.get(13).unwrap().block_id(),
            confirmation_time: 123458,
        },
    );
    outpoints.push(((), outpoint2));

    let chain_tip = chain.tip().block_id();
    let canonical_view = chain.canonical_view(&tx_graph, chain_tip, CanonicalParams::default());

    // Test with min_confirmations = 5
    // tx0: 11 confirmations -> confirmed
    // tx1: 6 confirmations -> confirmed
    // tx2: 3 confirmations -> trusted pending
    let balance = canonical_view.balance(outpoints.clone(), |_, _| true, 5);

    assert_eq!(
        balance.confirmed,
        Amount::from_sat(10_000 + 20_000) // tx0 + tx1
    );
    assert_eq!(
        balance.trusted_pending,
        Amount::from_sat(30_000) // tx2
    );
    assert_eq!(balance.untrusted_pending, Amount::ZERO);

    // Test with min_confirmations = 10
    // tx0: 11 confirmations -> confirmed
    // tx1: 6 confirmations -> trusted pending
    // tx2: 3 confirmations -> trusted pending
    let balance_high = canonical_view.balance(outpoints, |_, _| true, 10);

    assert_eq!(
        balance_high.confirmed,
        Amount::from_sat(10_000) // only tx0
    );
    assert_eq!(
        balance_high.trusted_pending,
        Amount::from_sat(20_000 + 30_000) // tx1 + tx2
    );
    assert_eq!(balance_high.untrusted_pending, Amount::ZERO);
}

struct Scenario<'a> {
    name: &'a str,
    tx_templates: &'a [TxTemplate<'a, BlockId>],
    exp_canonical_txs: HashSet<&'a str>,
}

#[test]
fn test_assumed_canonical_scenarios() {
    // Create a local chain
    let local_chain: LocalChain<BlockHash> = local_chain![
        (0, hash!("genesis")),
        (1, hash!("block1")),
        (2, hash!("block2")),
        (3, hash!("block3")),
        (4, hash!("block4")),
        (5, hash!("block5")),
        (6, hash!("block6")),
        (7, hash!("block7")),
        (8, hash!("block8")),
        (9, hash!("block9")),
        (10, hash!("block10"))
    ];
    let chain_tip = local_chain.chain_tip();

    // Create arrays before scenario to avoid lifetime issues
    let tx_templates = [
        TxTemplate {
            tx_name: "txA",
            inputs: &[TxInTemplate::Bogus],
            outputs: &[TxOutTemplate::new(100000, Some(0))],
            anchors: &[],
            last_seen: None,
            assume_canonical: false,
        },
        TxTemplate {
            tx_name: "txB",
            inputs: &[TxInTemplate::PrevTx("txA", 0)],
            outputs: &[TxOutTemplate::new(50000, Some(0))],
            anchors: &[block_id!(5, "block5")],
            last_seen: None,
            assume_canonical: false,
        },
        TxTemplate {
            tx_name: "txC",
            inputs: &[TxInTemplate::PrevTx("txB", 0)],
            outputs: &[TxOutTemplate::new(25000, Some(0))],
            anchors: &[],
            last_seen: None,
            assume_canonical: true,
        },
    ];

    let scenarios = vec![Scenario {
        name: "txC spends txB; txB spends txA; txB is anchored; txC is assumed canonical",
        tx_templates: &tx_templates,
        exp_canonical_txs: HashSet::from(["txA", "txB", "txC"]),
    }];

    for scenario in scenarios {
        let env = init_graph(scenario.tx_templates);

        // get the actual txid from given tx_name.
        let txid_c = *env.txid_to_name.get("txC").unwrap();

        // build the expected `CanonicalReason` with specific descendant txid's
        //
        // in this scenario: txC is assumed canonical, and it's descendant of txB and txA
        // therefore the whole chain should become assumed canonical.
        //
        // the descendant txid field refers to the directly **assumed canonical txC**.
        // Since txB is found to have a direct anchor, its descendant must be cleared.
        let exp_reasons = vec![
            (
                "txA",
                CanonicalReason::Assumed {
                    anchor: None,
                    descendant: Some(txid_c),
                },
            ),
            (
                "txB",
                CanonicalReason::Assumed {
                    anchor: Some(block_id!(5, "block5")),
                    descendant: None,
                },
            ),
            (
                "txC",
                CanonicalReason::Assumed {
                    anchor: None,
                    descendant: None,
                },
            ),
        ];

        // build task & canonicalize
        let canonical_params = env.canonicalization_params;
        let canonical_task = env.tx_graph.canonical_task(chain_tip, canonical_params);
        let canonical_txs = local_chain.canonicalize(canonical_task);

        // assert canonical transactions
        let exp_canonical_txids: HashSet<Txid> = scenario
            .exp_canonical_txs
            .iter()
            .map(|tx_name| {
                *env.txid_to_name
                    .get(tx_name)
                    .expect("txid should exist for tx_name")
            })
            .collect::<HashSet<Txid>>();

        let canonical_txids = canonical_txs
            .txs()
            .map(|canonical_tx| canonical_tx.txid)
            .collect::<HashSet<Txid>>();

        assert_eq!(
            canonical_txids, exp_canonical_txids,
            "[{}] canonical transactions mismatch",
            scenario.name
        );

        // assert canonical reasons
        for (tx_name, exp_reason) in exp_reasons {
            let txid = env
                .txid_to_name
                .get(tx_name)
                .expect("txid should exist for tx_name");

            let canonical_reason = canonical_txs
                .txs()
                .find(|ctx| &ctx.txid == txid)
                .expect("expected txid should exist in canonical txs")
                .pos;

            assert_eq!(
                canonical_reason, exp_reason,
                "[{}] canonical reason mismatch for {}",
                scenario.name, tx_name
            )
        }

        let txid_b = *env.txid_to_name.get("txB").unwrap();

        // build the expected `ChainPosition` with specific txid's for transitively confirmed txs.
        //
        // in this scenario:
        //
        // txA: should be confirmed transitively by txB.
        // txB: should be confirmed, has a direct anchor(block5).
        // txC: should be unconfirmed, has been assumed canonical though has no direct anchors.
        let exp_positions = vec![
            (
                "txA",
                ChainPosition::Confirmed {
                    anchor: block_id!(5, "block5"),
                    transitively: Some(txid_b),
                },
            ),
            (
                "txB",
                ChainPosition::Confirmed {
                    anchor: block_id!(5, "block5"),
                    transitively: None,
                },
            ),
            (
                "txC",
                ChainPosition::Unconfirmed {
                    first_seen: None,
                    last_seen: None,
                },
            ),
        ];

        // build task & resolve positions
        let canonical_view = canonical_txs.view();

        // assert final positions
        for (tx_name, exp_position) in exp_positions {
            let txid = *env
                .txid_to_name
                .get(tx_name)
                .expect("txid should exist for tx_name");

            let canonical_position = canonical_view
                .txs()
                .find(|ctx| ctx.txid == txid)
                .expect("expected txid should exist in canonical view")
                .pos;

            assert_eq!(
                canonical_position, exp_position,
                "[{}] canonical position mismatch for {}",
                scenario.name, tx_name
            );
        }
    }
}
