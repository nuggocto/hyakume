## Skills

- Use the `rust` skill for project work.
- Use the `unslop` skill for all writing, including comments and user-facing
  text.
- Use the `test-quality` skill when writing or reviewing tests.
- Use the `security` skill when work touches a trust boundary, such as
  invitations, peer messages, received files, paths, local IPC, secure storage,
  terminal output, mobile bridges, CI, releases, or website deployment.
- Use the `qa` skill when verifying user-visible behavior, packaged artifacts,
  supported platforms, the frontend, or a release candidate.
- Use the `show-me` skill when ownership, data flow, control flow, state
  transitions, or dependencies are easier to understand as a graph.
- Use the `explaining-code` skill when explaining how code works or what a
  change does.

## Project rules

- Never mention phase numbers anywhere outside `PROJECT.md`, including source
  code, comments, tests, command output, filenames, and other documentation.
  Track progress only by marking completed checkboxes in `PROJECT.md`.
- After each step of work, add to `docs/book.md` what got built, how it works,
  and why it was built that way. Write each entry with the `show-me` and
  `explaining-code` skills. Keep decision records next to it in `docs/`.
- Treat `shrek` as the permanent default branch. Do not rename or replace it.
- Write each commit with a simple, well-written subject line and an explanatory
  body. The body must explain why the change was needed instead of restating the
  diff.
- Preserve user work and avoid unrelated edits.
- Keep the website in the separate `hyakume-front` repository, using static
  Astro, pnpm, and Cloudflare Pages at the acquired domain `hyakume.org`.
- Do not edit `../hyakume-front` unless the current work or user request includes
  it.
- Do not spawn sub-agents unless the current work or user request requires it.
- Only add a test if its failure would tell you something is actually broken.
  Assertions on styling values, colors, or internal structure fail on harmless
  changes and pass on real bugs, so leave them out.
