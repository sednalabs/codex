# Upgrading the fork and retiring carries

Use this procedure when bringing a deliberate upstream revision into the fork.
The [carry divergence index](divergences/index.yaml) describes known
downstream behavior; it is not a substitute for reviewing the selected source
and the resulting changes.

1. Select and record an exact upstream commit for the upgrade. Do not chase a
   moving branch head during integration. Compare that commit with the current
   fork base and review the complete resulting diff.
2. Prefer upstream behavior or configuration, including an available MCP
   integration, over carrying a local code change when it meets the need.
   Preserve downstream capabilities that are still required; do not remove a
   guardrail solely because adjacent upstream code changed.
3. Integrate the exact source through the repository's protected review path.
   Require protected-current-head checks for applicable changes. Reuse earlier
   evidence only when its source, configuration, environment, and other relevant
   dependencies are unchanged and its applicability is recorded; a check on an
   unrelated revision does not qualify this integration.
4. Package and install progressively using only currently documented platform
   paths. Keep each host update independent: retain its existing profile and
   state, verify the installed version, and preserve a compatible prior binary
   plus a recoverable state or migration route. A successful build or package
   is not proof of installation or acceptance on a host.
5. For each carry, record one disposition: retain with its required behavior,
   retire because the selected upstream source provides an equivalent
   capability, the behavior is demonstrably unused or redundant, or its
   requirement was explicitly removed, or defer with the unmet condition
   stated. Retire only after checking the affected consumer and required hosted
   evidence. Keep the rollback route until the replacement is accepted; if it
   fails, restore the prior binary and compatible state/migration route or
   retain the carry and investigate before retrying.

Verify the selected revision's actual workflow inputs and platform installation
instructions before relying on them. Do not assume an undocumented helper,
approval step, or platform is supported.
