use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::Deserialize;

use crate::client::PolymarketClient;

#[derive(Debug, Clone)]
pub struct OrderBook {
    pub bids: Vec<(f64, f64)>, // (price, size)
    pub asks: Vec<(f64, f64)>,
}

impl OrderBook {
    pub fn best_bid(&self) -> Option<(f64, f64)> {
        self.bids.first().cloned()
    }

    pub fn best_ask(&self) -> Option<(f64, f64)> {
        self.asks.first().cloned()
    }
}

/* ===============================
PRICE API RESPONSE
=============================== */

#[derive(Debug, Deserialize)]
struct PriceResponse {
    price: String,
}

/* ===============================
FETCH ORDERBOOK - Using /price endpoint (CORRECT DATA)
=============================== */

pub async fn fetch_orderbook(api: &PolymarketClient, token_id: &str) -> Result<OrderBook> {
    let client = Client::new();

    // Fetch BID price (what we can SELL for)
    let bid_url = format!("{}/price?token_id={}&side=BUY", api.clob_url, token_id);

    let bid_response = client.get(&bid_url).send().await?;

    if !bid_response.status().is_success() {
        return Err(anyhow!(
            "Failed to fetch bid price: {}",
            bid_response.status()
        ));
    }

    let bid_data: PriceResponse = bid_response.json().await?;
    let bid_price: f64 = bid_data
        .price
        .parse()
        .map_err(|e| anyhow!("Failed to parse bid price: {}", e))?;

    // Fetch ASK price (what we must PAY to buy)
    let ask_url = format!("{}/price?token_id={}&side=SELL", api.clob_url, token_id);

    let ask_response = client.get(&ask_url).send().await?;

    if !ask_response.status().is_success() {
        return Err(anyhow!(
            "Failed to fetch ask price: {}",
            ask_response.status()
        ));
    }

    let ask_data: PriceResponse = ask_response.json().await?;
    let ask_price: f64 = ask_data
        .price
        .parse()
        .map_err(|e| anyhow!("Failed to parse ask price: {}", e))?;

    // Create orderbook with single best bid/ask
    Ok(OrderBook {
        bids: vec![(bid_price, 1.0)], // Size doesn't matter for best price
        asks: vec![(ask_price, 1.0)],
    })
}
