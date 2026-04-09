-- Add MOEX (Moscow Exchange) market data provider
INSERT OR IGNORE INTO market_data_providers (id, name, description, url, priority, enabled, logo_filename, last_synced_at, last_sync_status, last_sync_error)
VALUES
    ('MOEX', 'MOEX', 'Moscow Exchange (MOEX) provides data for Russian equities, bonds, ETFs, and RUB FX pairs. No API key required.', 'https://iss.moex.com/iss/', 4, FALSE, 'moex.png', NULL, NULL, NULL);
