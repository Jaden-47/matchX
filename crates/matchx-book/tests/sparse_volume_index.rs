use matchx_arena::Arena;
use matchx_book::OrderBook;
use matchx_types::*;

fn sparse_config() -> InstrumentConfig {
    // Small dense window to force most prices into sparse storage
    InstrumentConfig {
        id: 1,
        tick_size: 1,
        lot_size: 1,
        base_price: 500,
        max_ticks: 100,
        stp_mode: StpMode::CancelNewest,
    }
}

#[test]
fn sparse_ask_volume_query_is_correct() {
    let mut arena = Arena::new(256);
    let mut book = OrderBook::new(sparse_config());

    // Insert asks in sparse region (outside dense window 500..600)
    book.insert_order(
        OrderId(1),
        Side::Ask,
        200,
        10,
        OrderType::Limit,
        None,
        None,
        &mut arena,
    );
    book.insert_order(
        OrderId(2),
        Side::Ask,
        300,
        20,
        OrderType::Limit,
        None,
        None,
        &mut arena,
    );
    book.insert_order(
        OrderId(3),
        Side::Ask,
        400,
        30,
        OrderType::Limit,
        None,
        None,
        &mut arena,
    );

    // Query: asks available at or below 350
    let avail = book.ask_available_at_or_below(350);
    assert_eq!(avail, 30); // 200@10 + 300@20
}

#[test]
fn sparse_bid_volume_query_is_correct() {
    let mut arena = Arena::new(256);
    let mut book = OrderBook::new(sparse_config());

    // Insert bids in sparse region (outside dense window 500..600)
    book.insert_order(
        OrderId(1),
        Side::Bid,
        700,
        10,
        OrderType::Limit,
        None,
        None,
        &mut arena,
    );
    book.insert_order(
        OrderId(2),
        Side::Bid,
        800,
        20,
        OrderType::Limit,
        None,
        None,
        &mut arena,
    );
    book.insert_order(
        OrderId(3),
        Side::Bid,
        900,
        30,
        OrderType::Limit,
        None,
        None,
        &mut arena,
    );

    // Query: bids available at or above 750
    let avail = book.bid_available_at_or_above(750);
    assert_eq!(avail, 50); // 800@20 + 900@30
}

#[test]
fn mixed_dense_sparse_volume_query() {
    let mut arena = Arena::new(256);
    let mut book = OrderBook::new(sparse_config());

    // Dense region ask (within 500..600)
    book.insert_order(
        OrderId(1),
        Side::Ask,
        520,
        15,
        OrderType::Limit,
        None,
        None,
        &mut arena,
    );
    // Sparse region ask (below window)
    book.insert_order(
        OrderId(2),
        Side::Ask,
        300,
        25,
        OrderType::Limit,
        None,
        None,
        &mut arena,
    );

    let avail = book.ask_available_at_or_below(550);
    assert_eq!(avail, 40); // 300@25 + 520@15
}

#[test]
fn fragmented_sparse_fok_precheck() {
    let mut arena = Arena::new(1024);
    let mut book = OrderBook::new(sparse_config());

    // 100 sparse ask levels, 1 lot each
    for i in 0..100 {
        book.insert_order(
            OrderId(i + 1),
            Side::Ask,
            200 + i,
            1,
            OrderType::Limit,
            None,
            None,
            &mut arena,
        );
    }

    // FOK pre-check: need 50 lots at or below 250
    let avail = book.ask_available_at_or_below(250);
    assert_eq!(avail, 51); // prices 200..=250
}
