# Fork detection fixtures

Screen text used by the fork's Claude account work (epic E9). Every file is the
**bottom buffer** of a pane as `herdr agent read <pane> --source detection
--format text` renders it, so a fixture can be evaluated offline with:

```bash
herdr agent explain --file tests/fixtures/fork/<file> --agent claude --json
```

## Provenance — read this before trusting `claude-usage-limit.txt`

`AGENTS.md` requires screen detection to be **evidence-based**: a manifest rule
is derived from a live capture of the real agent in the target state. One
fixture here does not meet that bar, and the gap is deliberate and recorded
rather than papered over.

| File | Provenance | Live-verified |
| --- | --- | --- |
| `claude-usage-limit.txt` | **Reconstructed, not captured.** Composed 2026-09-07 from Claude Code's known usage-limit wording (the `<N>-hour limit reached ∙ resets <time>` footer segment and the `Claude usage limit reached. Your limit will reset at <time> (<zone>).` transcript sentence), laid out in the prompt-box/footer shape that *is* live-captured — the `─` rules, bare `❯` and `⏵⏵ … · …` status line come from the Claude Code 2.1.251 captures in `src/detect/manifest/tests.rs`. | ❌ **No.** A usage-limit screen needs a rate-limited real Claude account, which no local lab can produce. |
| `claude-usage-limit-with-dialog.txt` | Written to be adversarial the other way: a *real* limit footer with a blocking dialog on top of it, in the live-captured `live_blocked_form` shape. Both of the rule's positive gates match its region; only the `not` gates keep the dialog — priority 980, one step below `usage_limit` — as the reported blocker. | n/a (a negative) |
| `claude-usage-limit-negative.txt` | Written to be adversarial: a transcript that says "Claude usage limit reached" and "resets at 14:00" as prose, plus a user prompt mentioning a usage limit. | n/a (a negative) |
| `claude-idle.txt` | The idle prompt-box shape from the 2.1.251 captures in `src/detect/manifest/tests.rs`. | ✅ upstream capture |
| `claude-working.txt` | The live-turn shape from the same captures. | ✅ upstream capture |
| `claude-permission-prompt.txt` | The Bash-approval shape from the same captures. | ✅ upstream capture |

### What a human must verify against a really rate-limited account

Until these are done, the `usage_limit` rule in
`src/detect/manifests/claude.toml` is **best-effort**: it is proven not to fire
on healthy screens, and it is *not* proven to fire on the real one.

1. Capture the real screen twice — `herdr agent read <pane> --source detection
   --format text` and `--format ansi` — and replace `claude-usage-limit.txt`
   with the text capture.
2. Check the notice's **placement**. The rule's region is
   `after_last_horizontal_rule` — the live status area below the prompt box —
   and inside it the notice must start its own line or follow a `·`/`∙`/`|`
   separator. If the real screen puts it anywhere else (inside the prompt box,
   or above the box's last `─` rule), the rule will not fire and the region or
   the anchors need widening.
3. Check the **wording** of the headline (`5-hour limit reached`, `Weekly limit
   reached`, `Claude usage limit reached`) and of the reset clause (`resets
   3pm`, `resets at 14:00`, `Your limit will reset at …`). Anything else needs
   a new branch.
4. Confirm no permission prompt, no MCP dialog and no `/upgrade` menu is
   reported as `usage_limit` while the account is healthy — and, with the
   account really limited, that a permission prompt raised on top of it is
   still reported as the permission prompt.
5. Confirm the notice is not vetoed by the rule's transcript-marker `not`
   gate: if the real screen puts a `⏺` or `⎿` line below the prompt box's
   last horizontal rule, that gate has to be narrowed.
6. Confirm `/exit` still leaves the session cleanly from the limit screen (that
   is what `herdr agent switch-account` does next), including the typed
   fallback `herdr agent switch-account` uses when `agent.prompt` is refused
   with `agent_blocked`.
