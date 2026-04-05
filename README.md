# codexctl

Manage multiple OpenAI Codex CLI accounts. Switch profiles, check rate limits across all accounts, and tab-complete profile names.

## Install

```bash
cargo install --git https://github.com/Sawmills/codexctl
```

## Usage

### Save accounts

Log into each Codex account and save it:

```bash
codex --login          # log into amir@example.com
codexctl save          # auto-detects email, saves as profile

codex --login          # log into amir+2@example.com
codexctl save
```

Or provide a custom alias:

```bash
codexctl save work-main
```

### Check rate limits

```bash
codexctl status
```

```
┌────────────────────┬──────┬─────────┬───────────┬─────────┬───────────┬────────┐
│ Account            ┆ Plan ┆ 5h Used ┆ 5h Reset  ┆ 7d Used ┆ 7d Reset  ┆ Active │
╞════════════════════╪══════╪═════════╪═══════════╪═════════╪═══════════╪════════╡
│ amir+5@sawmills.ai ┆ team ┆ 0%      ┆ in 5h 00m ┆ 25%     ┆ in 4d 18h ┆        │
│ amir+6@sawmills.ai ┆ team ┆ 0%      ┆ in 5h 00m ┆ 25%     ┆ in 4d 16h ┆        │
│ amir@sawmills.ai   ┆ team ┆ 78%     ┆ in 3h 54m ┆ 92%     ┆ in 4d 15h ┆ *      │
│ amir+2@sawmills.ai ┆ team ┆ 100%    ┆ in 3h 12m ┆ 80%     ┆ in 4d 15h ┆        │
└────────────────────┴──────┴─────────┴───────────┴─────────┴───────────┴────────┘
```

Sorted by availability — most available accounts first. All accounts fetched in parallel.

### Switch accounts

Direct:

```bash
codexctl use amir+5@sawmills.ai
```

Interactive fuzzy picker:

```bash
codexctl switch
```

### Other commands

```bash
codexctl list          # list saved profiles
codexctl whoami        # show active account
codexctl remove <alias>
```

## Shell completions

```bash
# zsh (source-based)
codexctl completions zsh > ~/.cache/zsh/completions/_codexctl

# bash
codexctl completions bash >> ~/.bashrc

# fish
codexctl completions fish > ~/.config/fish/completions/codexctl.fish
```

Completions dynamically list profile names for `use` and `remove`.

## How it works

Profiles are stored in `~/.codexctl/profiles/<alias>/` — each containing a copy of `auth.json` and `meta.json`. Switching copies the profile's `auth.json` into `~/.codex/auth.json`.

Rate limits are fetched from `chatgpt.com/backend-api/wham/usage` using the stored access tokens.

Supports both Codex CLI auth formats:

- Nested: `{"auth_mode": "chatgpt", "tokens": {"access_token": "..."}}`
- Flat: `{"access_token": "..."}`

## License

MIT
