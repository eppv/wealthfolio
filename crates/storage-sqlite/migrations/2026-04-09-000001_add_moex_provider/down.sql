-- Remove MOEX (Moscow Exchange) market data provider
DELETE FROM market_data_providers WHERE id = 'MOEX';
