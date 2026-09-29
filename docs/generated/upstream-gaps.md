# Generated upstream gaps

> `status: generated` · `authority: evidence` · `captured: 2026-09-24`

This is a bounded next-train queue, not a claim that every newer upstream
commit is safe to apply to the frozen P7 candidate.

| Candidate | Decision | Owner | Reason |
| --- | --- | --- | --- |
| `b19cebecc0169097bda7539af03c886e03bdeafe` live upstream/main | `track-next-train` | root | Moving tip is recorded only for later harvest; frozen cut remains `392f56a6`. |
| `f747d23d4bc8a167207fb1c411e221022391fdbb` Bazel lock-check wording | `track` | root | Coupled shell-script change is outside P7 scope. |
| `a16381c4457e23191d4786968011434c37a04041` Cargo/Bazel debug defaults | `track` | root | Requires MODULE/patch/source changes outside the P7 cutline. |
| `9c77996cd1c28f683fa800891d502d9bb017e692` release workflow removal | `track` | root | Release-high-consequence behavior is not required here. |
| `6824dabe0393337a38cb257d5fe75ae5ca168470` release channel guard | `track` | root | Release-channel outcome requires separate protected review. |
| `1d87af5faa75c2c09785cd088353d3236333673f` network policy source/lock change | `ignore-for-p7` | root | Source and lock are inseparable; no partial lock-only adoption. |
| `f5960fcc22b918e658bc486e53a80031d64fd41e` shared crate coupling | `ignore-for-p7` | root | Source contract is outside P7 scope. |
| `5babf441c179fa8f4f36ebabc5233d0630ce9658` sednalabs PR #854 | `preserve-track` | root | Exact old-base head remains preserved; current-base successor is a separate outcome. |
| gnullvm symlink removal, legacy unknown-event handling, code-mode exception | `deferred-no-equivalent` | root | Focused harvest found no newer upstream equivalent; programme-owned repair is bounded to exact hosted failures and coupled tests. |

No gap is a blocker for the frozen P7 cutline unless a current acceptance
contract is contradicted. A later train must refresh exact refs and rerun the
bounded harvest before adopting any tracked item.
