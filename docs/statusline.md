# Cached Usage in Terminal Status Bars

`codexctl statusline` prints one short line for this machine's active account:

```text
p2 62% wk · 6d22h
```

The name is the account label, or the alias suffix after `+` and before `@`.
Names contain at most 20 letters, digits, spaces, dots, underscores, or hyphens.
Percentages show the weekly allowance left, rounded down.
The time shows the weekly reset countdown.
A known five-hour window adds, for example, `· 88% 5h`.
An account without a weekly window produces no output.

The command reads a private local cache with a 120-second lifetime from the original usage sample.
Server sample age is preserved, and stale or failed server usage is hidden.
It makes no network requests, takes no store lock, and creates no files.
Local reads have a 150-millisecond wait budget.
Process startup and operating-system scheduling can add time.
Missing, stale, invalid, or unreadable data produces no output and exit code 0.
An account change hides a cache entry for the previous account.
A passed weekly reset also hides the entry until fresh usage arrives.
The command shows the machine's selected account. Running Codex sessions can retain an earlier account.

## Refresh the Cache

Run this command once before using a status bar:

```sh
codexctl status >/dev/null
```

`status` updates `~/.codexctl/statusline.json` from the usage it already fetches.
The token helper also updates this file when the account server supplies usage in its token response.
Server accounts need an account server with statusline support for both update paths.
Older servers do not prove window durations, so their accounts produce no statusline.
Their normal `status` and `status --json` output stays available.
[Status JSON](status-json.md) reports each declared window duration and reset time;
window durations from older servers remain `null`.
The cache contains account identity metadata and usage, with no credentials.
Cache write errors do not fail the original status or token operation.

For continuous updates, run this loop in a separate terminal or a dedicated HerdR pane.
Press Ctrl+C to stop it. Start only one loop per machine.

```sh
while :; do
  codexctl status >/dev/null 2>&1
  sleep 60
done
```

Keep this network refresh outside prompt hooks and status-bar commands.
`statusline` never starts a background refresh itself.

## tmux

Add to `~/.tmux.conf`. This example replaces the right-hand status area.

```tmux
set -g status-interval 15
set -g status-right '#(codexctl statusline)'
set -g status-right-length 80
```

## Starship

Add to `~/.config/starship.toml`.
If you use a custom top-level `format`, include `$custom` in it.

```toml
[custom.codexctl]
command = "codexctl statusline"
when = true
format = '[$output]($style) '
style = "dimmed cyan"
```

## zsh

Add to `~/.zshrc`. This example replaces `RPROMPT` and escapes percent signs for zsh.

```zsh
autoload -Uz add-zsh-hook
_codexctl_usage_prompt() {
  local quota
  quota=$(codexctl statusline)
  RPROMPT=${quota//\%/%%}
}
add-zsh-hook precmd _codexctl_usage_prompt
```

## HerdR

Set `tab_bar_right` in the existing `[ui]` section of `~/.config/herdr/config.toml`:

```toml
[ui]
tab_bar_right = [
  { type = "command", command = "codexctl statusline", interval_seconds = 15, timeout_seconds = 1 },
]
```

HerdR runs this command on its server machine.
Install codexctl and refresh the cache on that machine.
Use an absolute executable path if the server's `PATH` does not include codexctl.
Run `herdr server reload-config` to apply the configuration.
This status area is separate from Codex's built-in footer.
