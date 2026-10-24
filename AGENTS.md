## Skills

- Use the `rust` skill for project work.
- Use the `unslop` skill for all writing, including comments and user-facing
  text.
- Use the `test-quality` skill when writing or reviewing tests.
- Use the `qa` skill when verifying user-visible behavior, packaged artifacts,
  supported platforms, the frontend, or a release candidate.
- Use the `show-me` skill when ownership, data flow, control flow, state
  transitions, or dependencies are easier to understand as a graph.
- Use the `explaining-code` skill when explaining how code works or what a
  change does.

## Project rules

- After each step of work, add to `docs/book.md` what got built, how it works,
  and why it was built that way. Write each entry with the `show-me` and
  `explaining-code` skills. Keep decision records next to it in `docs/`.
- Treat `shrek` as the permanent default branch. Do not rename or replace it.
- Write each commit with a simple, well-written subject line and an explanatory
  body. The body must explain why the change was needed instead of restating the
  diff.
- Preserve user work and avoid unrelated edits.
- Only add a test if its failure would tell you something is actually broken.
  Assertions on styling values, colors, or internal structure fail on harmless
  changes and pass on real bugs, so leave them out.
- Test actual implementation, do not duplicate logic into tests.
- Before adding complexity for a speculative or vanishingly unlikely race or security edge case, explain the concrete failure mode, likelihood, and complexity cost to the user and get their buy-in. Do not silently expand scope for theoretical robustness.
- Do not spawn sub-agents unless the current work or user request requires it.
