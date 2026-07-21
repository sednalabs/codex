# Model-visible context

When reviewing changes that assemble or inject model-visible context, check
that:

1. History is built incrementally; no history rewrite is introduced.
2. Frequent context changes do not cause avoidable cache misses.
3. Every injected item has a bounded size and a hard cap.
4. No item exceeds 10K tokens.
5. New individual items that can cross 1K tokens receive the required P0
   review attention.
6. Every injected fragment is defined as a struct in `core/context` and
   implements the `ContextualUserFragment` trait.
