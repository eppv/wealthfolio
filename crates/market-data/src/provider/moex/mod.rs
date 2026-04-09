//! MOEX (Moscow Exchange) market data provider implementation.
//!
//! This module provides market data from MOEX ISS API:
//! - Russian equities via MOEX ISS API
//! - Bonds, ETFs, and other Russian securities
//! - FX rates for RUB pairs
//!
//! MOEX API documentation: https://iss.moex.com/iss/reference/?lang=en
//! API does not require authentication. Rate limit: ~100 requests/minute.

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Europe;
use log::debug;
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::time::Duration;

use crate::errors::MarketDataError;
use crate::models::{
    AssetProfile, Coverage, InstrumentKind, ProviderInstrument, Quote, QuoteContext, SearchResult,
};
use crate::provider::{MarketDataProvider, ProviderCapabilities, RateLimit};

const PROVIDER_ID: &str = "MOEX";
const BASE_URL: &str = "https://iss.moex.com/iss";
const PRIMARY_BOARD: &str = "TQBR"; // Primary trading board for stocks

// ============================================================================
// API Response Structures
// ============================================================================

/// Generic MOEX ISS table structure.
/// MOEX returns data in a column-based format: columns array + data array of rows.
#[derive(Debug, Deserialize)]
struct MoexDataTable {
    columns: Vec<String>,
    data: Vec<Vec<Option<serde_json::Value>>>,
}

/// Latest quote response structure.
#[derive(Debug, Deserialize)]
struct MoexLatestResponse {
    marketdata: Option<MoexDataTable>,
    securities: Option<MoexDataTable>,
    dataversion: Option<MoexDataTable>,
}

/// Historical quotes response structure.
#[derive(Debug, Deserialize)]
struct MoexHistoryResponse {
    history: MoexDataTable,
    #[serde(rename = "history.cursor")]
    cursor: Option<MoexDataTable>,
}

/// Search response structure.
#[derive(Debug, Deserialize)]
struct MoexSearchResponse {
    securities: MoexDataTable,
}

/// Profile response structure.
#[derive(Debug, Deserialize)]
struct MoexProfileResponse {
    description: Option<MoexDataTable>,
    boards: Option<MoexDataTable>,
}

// ============================================================================
// MOEX Provider Implementation
// ============================================================================

/// MOEX market data provider.
///
/// Supports Russian equities, bonds, ETFs, and RUB FX pairs.
pub struct MoexProvider {
    client: Client,
}

impl MoexProvider {
    /// Create a new MOEX provider.
    ///
    /// MOEX API does not require authentication, but an API key can be
    /// provided for higher rate limits (unused in current implementation).
    pub fn new(_api_key: Option<String>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| Client::new());

        Self { client }
    }

    // ========================================================================
    // HTTP Request Helpers
    // ========================================================================

    async fn make_request(&self, url: &str) -> Result<reqwest::Response, MarketDataError> {
        debug!("MOEX request: {}", url);

        self.client
            .get(url)
            .send()
            .await
            .map_err(|e| MarketDataError::Network(e.into()))
    }

    // ========================================================================
    // Column/Value Extraction Helpers
    // ========================================================================

    /// Find column index by name in MOEX column-data format.
    fn find_column_index(columns: &[String], name: &str) -> Option<usize> {
        columns.iter().position(|c| c == name)
    }

    /// Safely extract a value from a MOEX response row.
    fn extract_string_value(
        row: impl AsRef<[Option<serde_json::Value>]>,
        col_index: Option<usize>,
    ) -> Option<String> {
        col_index
            .and_then(|idx| row.as_ref().get(idx))
            .and_then(|v| v.as_ref())
            .and_then(|v| v.as_str())
            .map(String::from)
    }

    fn extract_f64_value(
        row: impl AsRef<[Option<serde_json::Value>]>,
        col_index: Option<usize>,
    ) -> Option<f64> {
        col_index
            .and_then(|idx| row.as_ref().get(idx))
            .and_then(|v| v.as_ref())
            .and_then(|v| v.as_f64())
    }

    // ========================================================================
    // Board Data Filtering
    // ========================================================================

    /// Find rows matching a specific board ID.
    fn filter_by_board<'a>(
        data: &'a MoexDataTable,
        board_id: &str,
    ) -> Vec<&'a Vec<Option<serde_json::Value>>> {
        let board_col = Self::find_column_index(&data.columns, "BOARDID");

        data.data
            .iter()
            .filter(|row| {
                Self::extract_string_value(row, board_col)
                    .map(|b| b == board_id)
                    .unwrap_or(false)
            })
            .collect()
    }

    /// Get the first row matching the primary board (TQBR), or fallback to first available row.
    fn get_primary_board_data(data: &MoexDataTable) -> Option<Vec<Option<serde_json::Value>>> {
        // Try primary board first
        let filtered = Self::filter_by_board(data, PRIMARY_BOARD);
        if !filtered.is_empty() {
            return filtered.into_iter().next().cloned();
        }

        // Fallback to first row
        data.data.first().cloned()
    }

    // ========================================================================
    // Timestamp Parsing
    // ========================================================================

    /// Parse MOEX historical date (YYYY-MM-DD) to UTC DateTime.
    fn parse_historical_date(date_str: &str) -> Option<DateTime<Utc>> {
        NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
            .ok()
            .map(|date| {
                // Historical dates are end-of-day in Moscow time
                let moscow_datetime = date.and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap());

                Europe::Moscow
                    .from_local_datetime(&moscow_datetime)
                    .single()
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(Utc::now)
            })
    }

    /// Parse MOEX market data timestamp (UPDATETIME as HH:MM:SS) combined with trade date.
    fn parse_market_timestamp(trade_date: &str, time_str: &str) -> Option<DateTime<Utc>> {
        NaiveDateTime::parse_from_str(&format!("{} {}", trade_date, time_str), "%Y-%m-%d %H:%M:%S")
            .ok()
            .map(|naive_dt| {
                Europe::Moscow
                    .from_local_datetime(&naive_dt)
                    .single()
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(Utc::now)
            })
    }

    // ========================================================================
    // Currency Normalization
    // ========================================================================

    /// Normalize MOEX currency codes (SUR → RUB).
    fn normalize_currency(currency: &str) -> String {
        if currency == "SUR" {
            "RUB".to_string()
        } else {
            currency.to_string()
        }
    }
}

// ============================================================================
// MarketDataProvider Implementation
// ============================================================================

#[async_trait]
impl MarketDataProvider for MoexProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn priority(&self) -> u8 {
        // Medium priority - specialized for Russian market
        2
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            instrument_kinds: &[
                InstrumentKind::Equity,
                InstrumentKind::Fx, // RUB pairs
            ],
            coverage: Coverage {
                equity_mic_allow: Some(&["XMOS"]), // Moscow Exchange MIC
                equity_mic_deny: None,
                allow_unknown_mic: false,
                metal_quote_ccy_allow: None,
            },
            supports_latest: true,
            supports_historical: true,
            supports_search: true,  // MOEX has search capabilities
            supports_profile: true, // MOEX has security information
        }
    }

    fn rate_limit(&self) -> RateLimit {
        RateLimit {
            requests_per_minute: 100, // MOEX limit
            max_concurrency: 5,
            min_delay: Duration::from_millis(600), // 100 requests/min = ~600ms between requests
        }
    }

    async fn get_latest_quote(
        &self,
        context: &QuoteContext,
        instrument: ProviderInstrument,
    ) -> Result<Quote, MarketDataError> {
        let symbol = instrument.to_symbol_string();
        debug!("MOEX: Fetching latest quote for {}", symbol);

        // Build URL for latest quote
        let url = format!(
            "{}/engines/stock/markets/shares/securities/{}.json",
            BASE_URL, symbol
        );

        let resp = self.make_request(&url).await?;

        if !resp.status().is_success() {
            return Err(MarketDataError::SymbolNotFound(symbol));
        }

        let body: MoexLatestResponse =
            resp.json()
                .await
                .map_err(|e| MarketDataError::ProviderError {
                    provider: PROVIDER_ID.to_string(),
                    message: format!("Failed to parse response: {}", e),
                })?;

        let marketdata = body
            .marketdata
            .ok_or_else(|| MarketDataError::NoDataForRange)?;

        // Find primary board data
        let row = Self::get_primary_board_data(&marketdata)
            .ok_or_else(|| MarketDataError::SymbolNotFound(symbol.clone()))?;

        // Extract trade date from dataversion (if available)
        let trade_date = body.dataversion.as_ref().and_then(|dv| {
            let trade_date_col = Self::find_column_index(&dv.columns, "trade_date");
            dv.data
                .first()
                .and_then(|r| Self::extract_string_value(r, trade_date_col))
        });

        debug!(
            "MOEX latest quote: trade_date={:?}, symbol={}",
            trade_date, symbol
        );

        // Extract column indices
        let last_col = Self::find_column_index(&marketdata.columns, "LAST");
        let open_col = Self::find_column_index(&marketdata.columns, "OPEN");
        let high_col = Self::find_column_index(&marketdata.columns, "HIGH");
        let low_col = Self::find_column_index(&marketdata.columns, "LOW");
        let volume_col = Self::find_column_index(&marketdata.columns, "VOLTODAY");
        let time_col = Self::find_column_index(&marketdata.columns, "UPDATETIME");
        let currency_col = Self::find_column_index(&marketdata.columns, "CURRENCYID");

        // Extract values
        let close = Self::extract_f64_value(&row, last_col)
            .ok_or_else(|| MarketDataError::NoDataForRange)?;

        let close = Decimal::try_from(close).map_err(|_| MarketDataError::ValidationFailed {
            message: "Failed to convert price to decimal".to_string(),
        })?;

        let open = Self::extract_f64_value(&row, open_col).and_then(|v| Decimal::try_from(v).ok());
        let high = Self::extract_f64_value(&row, high_col).and_then(|v| Decimal::try_from(v).ok());
        let low = Self::extract_f64_value(&row, low_col).and_then(|v| Decimal::try_from(v).ok());
        let volume =
            Self::extract_f64_value(&row, volume_col).and_then(|v| Decimal::try_from(v).ok());

        // Parse timestamp
        // marketdata has UPDATETIME (time only), trade date comes from dataversion
        let update_time = Self::extract_string_value(&row, time_col);

        let timestamp = match (&trade_date, &update_time) {
            (Some(date), Some(time)) => Self::parse_market_timestamp(date, time),
            (Some(date), None) => Self::parse_historical_date(date),
            _ => None,
        }
        .unwrap_or_else(Utc::now);

        // Determine currency
        let currency = Self::extract_string_value(&row, currency_col)
            .map(|c| Self::normalize_currency(&c))
            .or_else(|| {
                context
                    .currency_hint
                    .as_ref()
                    .map(|c| Self::normalize_currency(c))
            })
            .unwrap_or_else(|| "RUB".to_string());

        Ok(Quote::ohlcv(
            timestamp,
            open.unwrap_or(close),
            high.unwrap_or(close),
            low.unwrap_or(close),
            close,
            volume.unwrap_or_default(),
            currency,
            PROVIDER_ID.to_string(),
        ))
    }

    async fn get_historical_quotes(
        &self,
        context: &QuoteContext,
        instrument: ProviderInstrument,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<Quote>, MarketDataError> {
        let symbol = instrument.to_symbol_string();
        debug!(
            "MOEX: Fetching historical quotes for {} from {} to {}",
            symbol,
            start.format("%Y-%m-%d"),
            end.format("%Y-%m-%d")
        );

        // Build URL for historical data
        let url = format!(
            "{}/history/engines/stock/markets/shares/securities/{}.json?from={}&till={}",
            BASE_URL,
            symbol,
            start.format("%Y-%m-%d"),
            end.format("%Y-%m-%d")
        );

        let resp = self.make_request(&url).await?;

        if !resp.status().is_success() {
            return Err(MarketDataError::SymbolNotFound(symbol));
        }

        let body: MoexHistoryResponse =
            resp.json()
                .await
                .map_err(|e| MarketDataError::ProviderError {
                    provider: PROVIDER_ID.to_string(),
                    message: format!("Failed to parse response: {}", e),
                })?;

        let history = body.history;

        // History endpoint returns one row per trading date, all with BOARDID = TQBR.
        // Filter all rows to get all trading dates.
        let rows = Self::filter_by_board(&history, PRIMARY_BOARD);

        debug!(
            "MOEX history response: {} columns, {} total rows, {} TQBR rows",
            history.columns.len(),
            history.data.len(),
            rows.len()
        );

        if rows.is_empty() {
            return Err(MarketDataError::NoDataForRange);
        }

        // Extract column indices
        let close_col = Self::find_column_index(&history.columns, "CLOSE");
        let open_col = Self::find_column_index(&history.columns, "OPEN");
        let high_col = Self::find_column_index(&history.columns, "HIGH");
        let low_col = Self::find_column_index(&history.columns, "LOW");
        let volume_col = Self::find_column_index(&history.columns, "VOLUME");
        let tradedate_col = Self::find_column_index(&history.columns, "TRADEDATE");
        let currency_col = Self::find_column_index(&history.columns, "CURRENCYID");

        // Determine currency (from first row or context)
        let currency = rows
            .first()
            .and_then(|row| Self::extract_string_value(row, currency_col))
            .map(|c| Self::normalize_currency(c.as_str()))
            .or_else(|| {
                context
                    .currency_hint
                    .as_ref()
                    .map(|c| Self::normalize_currency(c.as_ref()))
            })
            .unwrap_or_else(|| "RUB".to_string());

        // Parse rows into quotes
        let mut quotes = Vec::new();

        for row in &rows {
            let close =
                Self::extract_f64_value(row, close_col).and_then(|v| Decimal::try_from(v).ok());

            let close = match close {
                Some(c) => c,
                None => continue, // Skip rows without close price
            };

            let open =
                Self::extract_f64_value(row, open_col).and_then(|v| Decimal::try_from(v).ok());
            let high =
                Self::extract_f64_value(row, high_col).and_then(|v| Decimal::try_from(v).ok());
            let low = Self::extract_f64_value(row, low_col).and_then(|v| Decimal::try_from(v).ok());
            let volume =
                Self::extract_f64_value(row, volume_col).and_then(|v| Decimal::try_from(v).ok());

            // Parse timestamp
            let timestamp = Self::extract_string_value(row, tradedate_col)
                .and_then(|d| Self::parse_historical_date(&d));

            if let Some(ts) = timestamp {
                quotes.push(Quote::ohlcv(
                    ts,
                    open.unwrap_or(close),
                    high.unwrap_or(close),
                    low.unwrap_or(close),
                    close,
                    volume.unwrap_or_default(),
                    currency.clone(),
                    PROVIDER_ID.to_string(),
                ));
            }
        }

        // Sort by timestamp ascending
        quotes.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

        if quotes.is_empty() {
            return Err(MarketDataError::NoDataForRange);
        }

        // History endpoint only has finalized end-of-day data.
        // If the requested range includes today, also fetch the marketdata endpoint
        // to get the current day's intraday quote.
        let today_utc = Utc::now().date_naive();
        let end_date_naive = end.date_naive();

        if end_date_naive >= today_utc {
            // Reuse get_latest_quote via the trait method
            if let Ok(latest_quote) = self.get_latest_quote(context, instrument.clone()).await {
                debug!(
                    "MOEX: Appending latest quote for historical range: {}",
                    symbol
                );
                quotes.push(latest_quote);
                // Re-sort after appending
                quotes.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
            }
        }

        Ok(quotes)
    }

    async fn search(&self, query: &str) -> Result<Vec<SearchResult>, MarketDataError> {
        debug!("MOEX: Searching for '{}'", query);

        // Build search URL
        let url = format!("{}/securities.json?q={}", BASE_URL, query);

        let resp = self.make_request(&url).await?;

        if !resp.status().is_success() {
            return Ok(vec![]);
        }

        let body: MoexSearchResponse =
            resp.json()
                .await
                .map_err(|e| MarketDataError::ProviderError {
                    provider: PROVIDER_ID.to_string(),
                    message: format!("Failed to parse response: {}", e),
                })?;

        let securities = body.securities;

        debug!(
            "MOEX search response: {} columns, {} rows",
            securities.columns.len(),
            securities.data.len()
        );

        // Extract column indices (MOEX uses lowercase column names)
        let secid_col = Self::find_column_index(&securities.columns, "secid");
        let shortname_col = Self::find_column_index(&securities.columns, "shortname");
        let name_col = Self::find_column_index(&securities.columns, "name");
        let type_col = Self::find_column_index(&securities.columns, "type");
        let is_traded_col = Self::find_column_index(&securities.columns, "is_traded");

        debug!(
            "MOEX column indices: secid={:?}, shortname={:?}, type={:?}, is_traded={:?}",
            secid_col, shortname_col, type_col, is_traded_col
        );

        // Filter and map securities
        let results: Vec<SearchResult> = securities
            .data
            .iter()
            .filter(|row| {
                // Only include traded securities
                Self::extract_f64_value(row, is_traded_col)
                    .map(|v| v == 1.0)
                    .unwrap_or(false)
            })
            .filter(|row| {
                // Filter out index funds and other non-tradeable types
                Self::extract_string_value(row, type_col)
                    .map(|t| should_include_security_type(&t))
                    .unwrap_or(false)
            })
            .filter_map(|row| {
                let symbol = Self::extract_string_value(row, secid_col)?;
                let name = Self::extract_string_value(row, shortname_col)
                    .or_else(|| Self::extract_string_value(row, name_col))
                    .unwrap_or_else(|| symbol.clone());

                let asset_type = Self::extract_string_value(row, type_col)
                    .map(|t| map_moex_type_to_asset_type(&t))
                    .unwrap_or_else(|| "EQUITY".to_string());

                Some(
                    SearchResult::new(&symbol, &name, "MOEX", &asset_type)
                        .with_exchange_mic("XMOS")
                        .with_currency("RUB")
                        .with_data_source(PROVIDER_ID),
                )
            })
            .take(20) // Limit results
            .collect();

        Ok(results)
    }

    async fn get_profile(&self, symbol: &str) -> Result<AssetProfile, MarketDataError> {
        debug!("MOEX: Fetching profile for '{}'", symbol);

        // Build profile URL
        let url = format!("{}/securities/{}.json", BASE_URL, symbol);

        let resp = self.make_request(&url).await?;

        if !resp.status().is_success() {
            return Err(MarketDataError::SymbolNotFound(symbol.to_string()));
        }

        let body: MoexProfileResponse =
            resp.json()
                .await
                .map_err(|e| MarketDataError::ProviderError {
                    provider: PROVIDER_ID.to_string(),
                    message: format!("Failed to parse response: {}", e),
                })?;

        let description = body
            .description
            .ok_or_else(|| MarketDataError::NoDataForRange)?;

        // Extract column indices
        let name_col = Self::find_column_index(&description.columns, "NAME");
        let issuename_col = Self::find_column_index(&description.columns, "ISSUENAME");
        let isin_col = Self::find_column_index(&description.columns, "ISIN");
        let facevalue_col = Self::find_column_index(&description.columns, "FACEVALUE");
        let issuesize_col = Self::find_column_index(&description.columns, "ISSUESIZE");
        let typename_col = Self::find_column_index(&description.columns, "TYPENAME");
        let currency_col = Self::find_column_index(&description.columns, "CURRENCYID");

        // Get first row
        let row = description
            .data
            .first()
            .ok_or_else(|| MarketDataError::SymbolNotFound(symbol.to_string()))?;

        // Extract values
        let name = Self::extract_string_value(row, name_col);
        let description_text = Self::extract_string_value(row, issuename_col);
        let isin = Self::extract_string_value(row, isin_col);
        let asset_type =
            Self::extract_string_value(row, typename_col).map(|t| map_moex_type_to_asset_type(&t));
        let _currency =
            Self::extract_string_value(row, currency_col).map(|c| Self::normalize_currency(&c));

        let mut profile = AssetProfile::new();
        profile.source = Some(PROVIDER_ID.to_string());
        profile.name = name;
        profile.description = description_text;
        profile.isin = isin;
        profile.quote_type = asset_type;

        // Market cap approximation (if issuesize and price available)
        if let Some(issuesize) = Self::extract_f64_value(row, issuesize_col) {
            profile.market_cap = Some(issuesize);
        }

        // Face value for bonds
        if let Some(facevalue) = Self::extract_f64_value(row, facevalue_col) {
            // Store in description as string for reference
            if let Some(desc) = &profile.description {
                profile.description = Some(format!("{}\nFace Value: {}", desc, facevalue));
            } else {
                profile.description = Some(format!("Face Value: {}", facevalue));
            }
        }

        Ok(profile)
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Map MOEX security type to standard asset type.
fn map_moex_type_to_asset_type(moex_type: &str) -> String {
    match moex_type.to_lowercase().as_str() {
        "common_share" | "preferred_share" => "EQUITY".to_string(),
        "exchange_bond" | "corporate_bond" | "government_bond" => "BOND".to_string(),
        "etf_share" | "exchange_fund_share" => "ETF".to_string(),
        "currency_pair" | "fx" => "FX".to_string(),
        "stock_index" => "INDEX".to_string(),
        _ => "EQUITY".to_string(), // Default to equity
    }
}

/// Check if we should include this security type in search results.
/// Filters out index funds, structural products, and other non-tradeable types.
fn should_include_security_type(moex_type: &str) -> bool {
    match moex_type.to_lowercase().as_str() {
        // Include these tradeable types
        "common_share"
        | "preferred_share"
        | "exchange_bond"
        | "corporate_bond"
        | "government_bond"
        | "etf_share"
        | "exchange_fund_share"
        | "currency_pair"
        | "fx" => true,
        // Exclude index funds, structural products, etc.
        _ => false,
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;
    use chrono::Timelike;
    use std::borrow::Cow;
    use std::sync::Arc;

    fn create_test_provider() -> MoexProvider {
        MoexProvider::new(None)
    }

    fn create_test_equity_context() -> QuoteContext {
        QuoteContext {
            instrument: crate::models::InstrumentId::Equity {
                ticker: Arc::from("GAZP"),
                mic: Some(Cow::Borrowed("XMOS")),
            },
            overrides: None,
            currency_hint: Some(Cow::Borrowed("RUB")),
            preferred_provider: None,
            bond_metadata: None,
            custom_provider_code: None,
        }
    }

    #[test]
    fn test_provider_id() {
        let provider = create_test_provider();
        assert_eq!(provider.id(), "MOEX");
    }

    #[test]
    fn test_provider_priority() {
        let provider = create_test_provider();
        assert_eq!(provider.priority(), 2);
    }

    #[test]
    fn test_provider_capabilities() {
        let provider = create_test_provider();
        let caps = provider.capabilities();

        assert!(caps.instrument_kinds.contains(&InstrumentKind::Equity));
        assert!(caps.instrument_kinds.contains(&InstrumentKind::Fx));
        assert!(!caps.instrument_kinds.contains(&InstrumentKind::Crypto));
        assert!(!caps.instrument_kinds.contains(&InstrumentKind::Metal));

        assert!(caps.supports_latest);
        assert!(caps.supports_historical);
        assert!(caps.supports_search);
        assert!(caps.supports_profile);

        // Test coverage
        let rub_equity = crate::models::InstrumentId::Equity {
            ticker: Arc::from("SBER"),
            mic: Some(Cow::Borrowed("XMOS")),
        };
        assert!(caps.coverage.supports(&rub_equity));

        let us_equity = crate::models::InstrumentId::Equity {
            ticker: Arc::from("AAPL"),
            mic: Some(Cow::Borrowed("XNAS")),
        };
        assert!(!caps.coverage.supports(&us_equity));
    }

    #[test]
    fn test_rate_limit() {
        let provider = create_test_provider();
        let limit = provider.rate_limit();

        assert_eq!(limit.requests_per_minute, 100);
        assert_eq!(limit.max_concurrency, 5);
        assert_eq!(limit.min_delay, Duration::from_millis(600));
    }

    #[test]
    fn test_find_column_index() {
        let columns = vec![
            "SECID".to_string(),
            "BOARDID".to_string(),
            "LAST".to_string(),
            "OPEN".to_string(),
        ];

        assert_eq!(MoexProvider::find_column_index(&columns, "LAST"), Some(2));
        assert_eq!(MoexProvider::find_column_index(&columns, "CLOSE"), None);
        assert_eq!(MoexProvider::find_column_index(&columns, "SECID"), Some(0));
    }

    #[test]
    fn test_extract_value() {
        let row = vec![
            Some(serde_json::Value::String("SBER".to_string())),
            Some(serde_json::Value::Number(
                serde_json::Number::from_f64(250.5).unwrap(),
            )),
            None,
            Some(serde_json::Value::Number(
                serde_json::Number::from_f64(1000.0).unwrap(),
            )),
        ];

        assert_eq!(
            MoexProvider::extract_string_value(&row, Some(0)),
            Some("SBER".to_string())
        );
        assert_eq!(MoexProvider::extract_f64_value(&row, Some(1)), Some(250.5));
        assert_eq!(MoexProvider::extract_f64_value(&row, Some(2)), None);
        assert_eq!(MoexProvider::extract_f64_value(&row, Some(3)), Some(1000.0));
    }

    #[test]
    fn test_normalize_currency() {
        assert_eq!(MoexProvider::normalize_currency("SUR"), "RUB");
        assert_eq!(MoexProvider::normalize_currency("RUB"), "RUB");
        assert_eq!(MoexProvider::normalize_currency("USD"), "USD");
    }

    #[test]
    fn test_parse_historical_date() {
        let result = MoexProvider::parse_historical_date("2024-01-15");
        assert!(result.is_some());

        let dt = result.unwrap();
        // Moscow time is UTC+3, so 00:00 Moscow = 21:00 UTC previous day
        assert_eq!(dt.day(), 14);
        assert_eq!(dt.hour(), 21);
    }

    #[test]
    fn test_parse_market_timestamp() {
        let result = MoexProvider::parse_market_timestamp("2024-01-15", "14:30:00");
        assert!(result.is_some());

        let dt = result.unwrap();
        // 14:30 Moscow (UTC+3) = 11:30 UTC
        assert_eq!(dt.hour(), 11);
        assert_eq!(dt.minute(), 30);
    }

    #[test]
    fn test_map_moex_type_to_asset_type() {
        assert_eq!(map_moex_type_to_asset_type("common_share"), "EQUITY");
        assert_eq!(map_moex_type_to_asset_type("preferred_share"), "EQUITY");
        assert_eq!(map_moex_type_to_asset_type("exchange_bond"), "BOND");
        assert_eq!(map_moex_type_to_asset_type("etf_share"), "ETF");
        assert_eq!(map_moex_type_to_asset_type("currency_pair"), "FX");
        assert_eq!(map_moex_type_to_asset_type("unknown_type"), "EQUITY");
    }

    #[test]
    fn test_should_include_security_type() {
        // Include tradeable types
        assert!(should_include_security_type("common_share"));
        assert!(should_include_security_type("preferred_share"));
        assert!(should_include_security_type("exchange_bond"));
        assert!(should_include_security_type("etf_share"));
        assert!(should_include_security_type("currency_pair"));

        // Exclude index funds and other non-tradeable types
        assert!(!should_include_security_type("stock_index_pf"));
        assert!(!should_include_security_type("stock_index"));
        assert!(!should_include_security_type("unknown_type"));
    }

    #[tokio::test]
    async fn test_get_latest_quote_not_implemented() {
        let provider = create_test_provider();
        let context = create_test_equity_context();
        let instrument = ProviderInstrument::EquitySymbol {
            symbol: Arc::from("GAZP"),
        };

        // This will now make a real API call (may succeed or fail depending on network)
        let result = provider.get_latest_quote(&context, instrument).await;
        // We just verify it doesn't panic - actual result depends on network
        debug!("get_latest_quote result: {:?}", result.is_ok());
    }

    #[tokio::test]
    async fn test_search_returns_results() {
        let provider = create_test_provider();
        let result = provider.search("SBER").await;

        // May succeed or fail depending on network availability
        if let Ok(results) = result {
            // If we got results, verify structure
            for r in &results {
                assert!(!r.symbol.is_empty());
                assert!(!r.name.is_empty());
                assert_eq!(r.exchange, "MOEX");
            }
        }
    }
}
