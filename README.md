# herdr-pane-status

A [herdr](https://github.com/ogulcancelik/herdr) plugin that marks the panes
whose agent finished or needs you, until you look at them. herdr's sidebar
tells you which *agent* is done; this puts the same marker on the pane itself.

## What it shows

On the pane's top border, using herdr's own state icons
(`ui.status_indicators = "symbols"`):

```
┌ ✓ reviewer · Fix the login redirect ─────────────────┐
  │ └──┬───┘   └─────────┬──────────┘
  │ pane label  the agent's terminal title
  state icon
```

| Icon | When |
| --- | --- |
| `×` | The agent is blocked on a question or an approval. |
| `✓` | The agent finished and you haven't focused this pane since. |
| `◐` | The agent is working (turn off with `show_working`). |

The pane's own label stays in the title because herdr shows the plugin's
title in its place. Once you have seen a pane (and its agent is idle), it
shows its own label alone, where herdr's sidebar shows `○`.

Tabs are left alone: herdr's sidebar already shows each agent's state, and
herdr 0.9.5 can't clear a tab name once a plugin sets one.

### When a pane counts as done

herdr's own `done` status can't say which pane finished: switching to a tab
marks every pane in it seen, and a pane that finishes in the tab you are
looking at goes straight to `idle`. So the plugin keeps its own flag per pane:

- **Set** when the agent goes from working or blocked to idle or done, unless
  that pane was focused and herdr counted it as seen.
- **Cleared** when you focus the pane, or when its agent starts again or exits.
- **Kept** across a daemon restart (`finished.json`).

## Install

Requires herdr **0.9.5 or later** and a Rust toolchain (`cargo`).

```sh
herdr plugin install seankao31/herdr-pane-status   # builds with cargo
```

Or from a checkout (`plugin link` doesn't build):

```sh
cargo build --release
herdr plugin link "$PWD"
target/release/herdr-pane-status ensure   # start now, rather than on the next focus change
```

### Compatibility

- **Other plugins that set pane titles.** herdr shows the newest title any
  plugin reports for a pane, so two plugins that both report titles keep
  replacing each other's. Run only one of them.
- **Hooks on `pane.agent_status_changed`.** herdr fires that event whenever a
  pane's reported title changes, not only its status, so other plugins'
  hooks on it also run each time a marker appears or clears.
- **Scripts that read pane titles.** While a marker shows, the `title` field
  in `herdr pane list` and `herdr pane get` holds this plugin's text; `label`
  and `terminal_title` are untouched.

On Windows the GNU toolchain works without Visual Studio (`windows-sys` is
kept at 0.59, whose import libraries need no `dlltool`).

## Configure

`config.json` in the plugin's config directory (see `config.default.json`):

| Key | Default | |
| --- | --- | --- |
| `show_working` | `true` | Show `◐` on working panes, not only `×` and `✓`. |

The config directory is `~/.config/herdr/plugins/config/seankao31.herdr-pane-status/`,
or `%APPDATA%\herdr\plugins\config\seankao31.herdr-pane-status\` on Windows.
The daemon reads it when it starts.

## How it works

One binary. `herdr-pane-status ensure` is the plugin's hook, run on startup
and on every `pane.created` and `pane.focused`: if no daemon holds the lock in
the state directory, it starts `herdr-pane-status daemon` detached and exits.
A daemon that died is back the next time you move between panes.

The daemon subscribes to herdr's events (each pane's status, pane lifecycle,
focus), reads `pane.list` once a second for task text, labels and anything an
event missed, and reports titles with `pane.report_metadata`. Titles carry a
4-second TTL, so a dead daemon's titles clear on their own; an unchanged title
is re-sent every 2 seconds. herdr fires `pane.agent_status_changed` for every
new title (and runs every plugin hook on that event), so titles only change
when what they say changes.

When herdr drops a subscription that fell behind (`events_lost`) while it
keeps running, the daemon resyncs and resubscribes; it shuts down once herdr
stops answering.

Measured on Windows with herdr 0.9.5, about 30 panes in 14 tabs: the daemon uses
about 0.1% of one core and 4.6 MB; herdr's own CPU showed no difference with
it on or off; the hook takes about 21 ms; a status change reaches the border
as soon as herdr sends the event.

## Limitations

- **Border titles need a border.** With herdr's default
  `ui.pane_borders = "auto"`, only panes in a tab with two or more panes have
  one, so a pane alone in its tab shows nothing; herdr's sidebar still does.
  `pane_borders = "always"` frames lone panes too.
- **Clean shutdown** needs `herdr-pane-status stop` (or creating `stop` in
  the state directory); a plain kill or `SIGTERM` skips it. Titles still
  clear on their own within 4 seconds.

## Troubleshooting

The daemon logs to `daemon.log` in its state directory,
`~/.local/state/herdr/plugins/seankao31.herdr-pane-status/` or
`%LOCALAPPDATA%\herdr\plugins\seankao31.herdr-pane-status\` on Windows.
herdr's record of each hook run is in
`herdr plugin log list --plugin seankao31.herdr-pane-status`.

To stop it: `herdr plugin disable seankao31.herdr-pane-status`, then
`target/release/herdr-pane-status stop` from the plugin folder (otherwise the
next focus change starts it again).

## License

[MIT](LICENSE)
