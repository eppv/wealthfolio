# Maintaining the MOEX provider branch

This is a fork of wealthfolio. `main` mirrors upstream; `dev/moex-provider`
carries custom MOEX (Moscow Exchange) market-data code. Upstream `main` is
merged into `dev/moex-provider` periodically to keep new upstream features.

## Merge upstream into the custom branch

```sh
git checkout main && git pull           # sync upstream first
git checkout dev/moex-provider
git merge main --no-edit
```

The branch history contains "Update from main" snapshot commits, so the merge
base is old and the merge reports hundreds of commits. That is expected — the
real conflict surface is small (below).

## Expected conflict surface

Files changed on both sides can conflict. In practice only these 7:

- `crates/core/src/quotes/client.rs`
- `crates/market-data/Cargo.toml`
- `crates/market-data/src/lib.rs`
- `crates/market-data/src/resolver/exchanges.json`
- `crates/market-data/src/resolver/exchange_suffixes.rs`
- `Cargo.lock`
- `pnpm-lock.yaml`

Custom-only files never conflict (absent upstream):

- `crates/market-data/src/provider/moex/mod.rs`  (the provider)
- `crates/market-data/src/provider/mod.rs`
- `crates/market-data/src/resolver/exchange_registry.rs`
- `crates/core/src/quotes/{constants,provider_settings,types}.rs`
- `crates/storage-sqlite/migrations/2026-04-09-000001_add_moex_provider/`
- `apps/frontend/public/market-data/moex.png`

Lockfiles: `pnpm-lock.yaml` has no MOEX-specific JS deps — take upstream's
version. `Cargo.lock` must keep MOEX's `chrono-tz 0.9` (from
`crates/market-data/Cargo.toml`); run `cargo check` to regenerate if needed.

## Verify the merge

After merging, the diff vs `main` must contain only MOEX files (~15):

```sh
git diff --name-only main HEAD
```

Then build:

```sh
# Do NOT run `cargo check --workspace` (needs Tauri GTK system libs).
cargo check -p wealthfolio-market-data -p wealthfolio-core -p wealthfolio-storage-sqlite
pnpm install --frozen-lockfile
pnpm type-check
```

## System prerequisites (Ubuntu) for a full Tauri build

`cargo check --workspace` / `pnpm tauri dev` need GTK/WebKit system libraries:

```sh
sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file \
  libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev
```

`libwebkit2gtk-4.1-dev` pulls in `libglib2.0-dev`, which provides `glib-2.0.pc`
and `gobject-2.0.pc` required by the `glib-sys`/`gobject-sys` build scripts.

## Notes

- Do not push to the remote without an explicit request.
- Checking the three MOEX crates above is sufficient to validate MOEX changes;
  the Tauri app crates are unrelated to MOEX.
