use anyhow::{Context, Result};
use log::{info, warn};
use std::env;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ethers::types::{H256, U256};
use ::rand::Rng;
use std::str::FromStr;
use ethers::types::Address;
use rust_decimal::prelude::ToPrimitive;

use crate::client::PolymarketClient;
use crate::config::{TradingConfig, WalletConfig};
use crate::domain::order::{PricedOrder, Side};
use crate::domain::ArbitrageOpportunity;
use crate::execution::clob_client::ClobClient;
use crate::wallet::signer::{ClobOrder, WalletSigner};

const MIN_ORDER_USDC: f64 = 1.0;

pub struct Trader {
    api: Arc<PolymarketClient>,
    clob: Arc<ClobClient>,
    config: TradingConfig,
    wallet_config: WalletConfig,
    signer: WalletSigner,
}

impl Trader {
    pub fn new(
        api: Arc<PolymarketClient>,
        clob: Arc<ClobClient>,
        config: TradingConfig,
        wallet_config: WalletConfig,
        signer: WalletSigner,
    ) -> Self {
        Self {
            api,
            clob,
            config,
            wallet_config,
            signer,
        }
    }

    fn max_sum_threshold() -> f64 {
        env::var("ARBITRAGE_MAX_SUM")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.99)
    }

    fn calculate_position_size(&self, balance: f64, opportunity: &ArbitrageOpportunity) -> f64 {
        use crate::config::TradeMode;

        let sizing = &self.config.position_sizing;

        let raw_size = match sizing.mode {
            TradeMode::Fixed => sizing.fixed_usdc.unwrap_or(5.0),
            TradeMode::Percentage => {
                let pct = sizing.percentage.unwrap_or(10.0);
                balance * (pct / 100.0)
            }
            TradeMode::Dynamic => {
                let max_risk = sizing.max_risk_percent.unwrap_or(1.0);
                let profit_margin = opportunity.expected_profit.to_f64().unwrap_or(0.0);

                if profit_margin > 0.0 {
                    (balance * (max_risk / 100.0)) / profit_margin
                } else {
                    MIN_ORDER_USDC
                }
            }
            TradeMode::Free => balance.min(100.0),
        };

        raw_size.max(MIN_ORDER_USDC)
    }

    pub async fn execute_arbitrage(&self, opp: &ArbitrageOpportunity) -> Result<()> {
        info!("🎯 Arbitrage opportunity detected!");
        info!(
            "   ETH UP @ {:.4} + BTC DOWN @ {:.4} = {:.4}",
            opp.eth_up_price.to_f64().unwrap(),
            opp.btc_down_price.to_f64().unwrap(),
            opp.total_cost.to_f64().unwrap()
        );
        info!(
            "   Expected profit: {:.2}%",
            opp.expected_profit.to_f64().unwrap() * 100.0
        );

        let balance = match self.api.get_usdc_balance().await {
            Ok(b) => b.to_f64().unwrap_or(0.0),
            Err(e) => {
                warn!("Failed to fetch balance: {}", e);
                return Ok(());
            }
        };

        info!("💰 Current balance: ${:.2}", balance);

        if balance < MIN_ORDER_USDC {
            warn!(
                "⚠️  Insufficient balance for trading (min ${:.2})",
                MIN_ORDER_USDC
            );
            return Ok(());
        }

        // ╔═══════════════════════════════════════════════════════════╗
        // ║  REMOVED SECTION - Lines 109-115 (OLD CODE)              ║
        // ║  What: Duplicate max_sum check removed                   ║
        // ║  Why: Strategy already validates this                    ║
        // ║                                                           ║
        // ║  OLD CODE (REMOVED):                                      ║
        // ║  if opp.total_cost.to_f64().unwrap_or(1.0) >= Self::max_sum_threshold() {
        // ║      warn!("⚠️  Total cost {:.4} exceeds threshold {:.4}",
        // ║          opp.total_cost.to_f64().unwrap(),               ║
        // ║          Self::max_sum_threshold()                       ║
        // ║      );                                                   ║
        // ║      return Ok(());                                       ║
        // ║  }                                                        ║
        // ╚═══════════════════════════════════════════════════════════╝

        let size = self.calculate_position_size(balance, opp);
        let size = size.min(balance);

        info!("📊 Position size: ${:.2}", size);

        let required = size * 2.0;
        if balance < required {
            let adjusted_size = balance / 2.0;
            info!("⚠️  Adjusting size to ${:.2} to fit balance", adjusted_size);
            if adjusted_size < MIN_ORDER_USDC {
                warn!("⚠️  Cannot execute - insufficient balance for both legs");
                return Ok(());
            }
        }

        let required_usdc = (size * 2.0 * 1_000_000.0) as u128;
        if let Err(e) = self.clob.ensure_trading_ready(required_usdc).await {
            warn!("⚠️  Trading readiness check failed: {}", e);
            return Err(e);
        }

        let orders = vec![
            PricedOrder {
                token_id: opp.eth_up_token_id.clone(),
                side: Side::Buy,
                price: opp.eth_up_price.to_f64().unwrap(),
                size_usdc: size,
            },
            PricedOrder {
                token_id: opp.btc_down_token_id.clone(),
                side: Side::Buy,
                price: opp.btc_down_price.to_f64().unwrap(),
                size_usdc: size,
            },
        ];

        info!("📝 Submitting {} orders...", orders.len());

        for (i, order) in orders.iter().enumerate() {
            info!(
                "   Order {}: {} {} @ ${:.4}",
                i + 1,
                if order.side == Side::Buy {
                    "BUY"
                } else {
                    "SELL"
                },
                &order.token_id[..16],
                order.price
            );

            match self.execute_order(order).await {
                Ok(_) => {
                    info!("   ✅ Order {} executed successfully", i + 1);
                }
                Err(e) => {
                    warn!("   ❌ Order {} failed: {}", i + 1, e);
                }
            }
        }

        info!("✅ Arbitrage execution complete!");
        Ok(())
    }

    async fn execute_order(&self, priced: &PricedOrder) -> Result<()> {
        let token_id = self.parse_token_id(&priced.token_id)?;

        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

        let nonce = U256::from(now * 1000 + (rand::random::<u64>() % 1000));
        let expiration = U256::from(now + 3600);

        let price_u256 = U256::from((priced.price * 1_000_000.0) as u64);
        let size_u256 = U256::from((priced.size_usdc * 1_000_000.0) as u64);

        let side = match priced.side {
            Side::Buy => 0,
            Side::Sell => 1,
        };
        
        // Calculate maker/taker amounts based on side
        let (maker_amount, taker_amount) = if side == 0 {
            // BUY: makerAmount = price × size, takerAmount = size
            (price_u256 * size_u256 / U256::from(1_000_000), size_u256)
        } else {
            // SELL: makerAmount = size, takerAmount = price × size
            (size_u256, price_u256 * size_u256 / U256::from(1_000_000))
        };
        
        let order = ClobOrder {
            salt: U256::from(::rand::random::<u64>()),
            maker: Address::from_str(&self.wallet_config.proxy_wallet)?,
            signer: self.signer.address(),
            taker: Address::zero(),
            token_id,
            maker_amount,
            taker_amount,
            side,
            fee_rate_bps: U256::zero(),
            nonce,
            expiration,
        };

        let signature = self
            .signer
            .sign_order(&order)
            .await
            .context("Failed to sign order")?;

        self.clob
            .submit_order(order, signature, &self.wallet_config.proxy_wallet)
            .await
            .context("Failed to submit order to CLOB")?;

        Ok(())
    }

    fn parse_token_id(&self, token_id_hex: &str) -> Result<H256> {
        let hex = token_id_hex.strip_prefix("0x").unwrap_or(token_id_hex);
        let bytes = hex::decode(hex).context("Invalid token ID hex")?;

        if bytes.len() != 32 {
            anyhow::bail!("Token ID must be 32 bytes, got {}", bytes.len());
        }

        Ok(H256::from_slice(&bytes))
    }
}
