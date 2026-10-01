# OTA live walk — hardware validation

The one ChipTUI validation that needs a person at a terminal with the board
powered and on the network: every other layer of the OTA feature is pinned by
fixtures (`AGENTS.md` §Testing), and an agent session has no tty, so the modal
itself has never been walked end to end against real hardware.

The mechanism was proven against a XIAO ESP32-C3 on 2026-09-07 (full
upload → mark-pending → reset → swap → verify → confirm cycle, run stage by
stage). What this walk covers is the *interface over it*: rows, state lines,
buttons and the halt-unconfirmed behaviour. Everything below is expected
behaviour; anything that deviates is a finding.

## Prerequisites

As of 2026-09-07, all present on this machine — a failure here is a change,
not a mystery:

| | |
|---|---|
| `smpmgr` | `0.19.0`, at `~/.local/bin/smpmgr` |
| reference project | `~/dev/iot/zephyr_projects/esp32c3-round-display` |
| `[ota]` | `address = "192.168.1.177"`, `transport = "udp"`, `method = "mcumgr"` |
| `VERSION` | `0.3.0` |
| board answer | `xiao_esp32c3` + shield `seeed_xiao_round_display`, in the registry |
| signed image | `build/esp32c3-round-display/zephyr/zephyr.signed.bin`, 1.3 MB |
| slots | `Found` in `build/` and `build_ota/` |
| transport | `Enabled` in both — `CONFIG_MCUMGR_TRANSPORT_UDP=y` in the app domain |

Two things to know before starting.

**The board is at `0.3.0` confirmed with `0.2.0` in slot 1.** An update needs
a *different* image or Verify cannot tell a swap from a no-op: bump `VERSION`
to `0.4.0` and rebuild pristine before the cycle means anything. The modal's
own `Rebuild (pristine)` button does that.

**The modal may open on `Prepare`, not `Update`.** If the managed Kconfig
block on disk no longer matches what the current template renders, the
`board Kconfig fragment` row reads `Pending`. That is correct: accepting
Prepare rewrites the block in place with everything outside the markers
byte-identical. It is also the first thing worth watching.

## The walk

Reach the modal all three ways at least once — `o`, the `Zephyr Actions`
menu's last row, and `?` → type `ota` → `Enter`.

1. **The two read-back rows.** `slot0/slot1` reads `✓ found in …`;
   `transport` reads `✓ CONFIG_MCUMGR_TRANSPORT_UDP=y in …`. The transport
   row reads the built `.config` back (the app domain's, never the sysbuild
   top level's) and says whether Kconfig kept the transport symbol.
2. **Prepare.** The confirm names the project and board; accepting rewrites
   the fragment. Check with `git diff` that only the managed region moved.
3. **`s` twice.** Adds the net shell block, then takes it back out — the row
   should go `⚠ skipped` → `□ pending` → `✓ done` → `□ pending` →
   `⚠ skipped`, and the file should come back byte-identical.
4. **`t`.** Open the transport picker, `Esc` out without changing anything.
   Changing it rewrites the Kconfig block, so only do that deliberately.
5. **`p` — the one to try first.** A one-second `os echo` against
   `192.168.1.177`. If the board answers, the address is good and the whole
   rest of the cycle is worth starting. **Also try it with the board off**:
   the failure should be a named timeout on the `Probe` row, not a hang.
6. **`Rebuild (pristine)`** after bumping `VERSION` — the modal closes, the
   build streams into the Monitor with `Stop` reachable, and reopening should
   show `Update`.
7. **The cycle.** `Update` → the confirm quotes `smpmgr … image upload …` →
   watch the settle countdown (90 s, measured against the real swap) → it
   must **halt** at `✓ Confirm image` with the unconfirmed warning shown in
   the warning colour. (`[ota] auto_confirm = false` keeps this halt; the
   default `true` confirms after a verified swap.)
8. **The reopen.** At the halt, press `Esc`, then `o` again. The button must
   still read `✓ Confirm image` and the state line must still say the image
   is unconfirmed — closing the window must not spend the safety net.
9. **`Stop` mid-upload**, and **`Stop` during the settle** — the latter must
   read as a stop ("ended early --- the board may still be swapping") and
   resume at `Verify`, never as "ready".
10. **`Confirm image`**, and the button lands on `✓ Done`.

## What to bring back

For anything that misbehaves: what the row/state line said verbatim, what the
button read, and the Output pane's tail. The state line is the half most
likely to be wrong — it owns only the footer's left half, and wording that
fits a test assertion can still truncate in a rendered frame.
