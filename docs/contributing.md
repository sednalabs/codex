## Contributing

We welcome community contributions through the [openai/codex issue tracker](https://github.com/openai/codex/issues). Bug reports, root-cause analyses, and feature requests help us understand what matters most and improve Codex.

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
At this time, this fork does not accept unsolicited code contributions.
=======
**We do not accept external code contributions or pull requests.**
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360

### Why we do not accept external code contributions

Effective changes to Codex require architectural context, an understanding of system-level constraints, and visibility into the project's roadmap. External pull requests often focus on issues that are lower priority, affect a small number of users, or need substantial changes to fit the broader system. Reviewing and iterating on those changes can take more time than implementing a fix directly, diverting attention from higher-priority work.

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
Fork maintainers may invite an external contributor to submit a pull request when:
=======
Community expertise is most valuable when shared through detailed bug reports, reproduction steps, logs, root-cause analysis, and design discussions in issues. Understanding the problem, identifying the right solution, and prioritizing the work are typically the hard parts; implementation is comparatively straightforward with the help of Codex itself.
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360

For these reasons, we focus community contributions on issue reports, analysis, and feedback, while the Codex team handles code changes.

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
Pull requests that have not been explicitly invited by a fork maintainer will be closed without review.
=======
### Reporting bugs
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360

Before opening a new issue, search the issue tracker to see whether the problem has already been reported. If it has, add any new information to the existing issue.

When reporting a bug, include as much relevant detail as possible:

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
Many contributions were made without full visibility into the architectural context, system-level constraints, or near-term roadmap considerations that guide this fork's development. Others focused on issues that were low priority or affected a very small subset of users. Reviewing and iterating on these PRs often took more time than implementing the fix directly, and diverted attention from higher-priority work.
=======
- Clear, detailed steps to reproduce the problem.
- Expected and actual behavior.
- Your Codex version, operating system, and other relevant environment details.
- Logs, error messages, or other diagnostic information, with sensitive information removed.
- Root-cause analysis, technical observations, or potential approaches to a fix, if available.
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360

### Requesting features

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
For these reasons, we focus external contributions on discussion, analysis, and feedback, and reserve code changes for cases where a targeted invitation makes sense.

### Development workflow

If you are invited by a fork maintainer to contribute a PR, here is the recommended development workflow.

- Create a _topic branch_ from `main` - e.g. `feat/interactive-prompt`.
- `main` is the maintained downstream branch and the only public PR target in this fork.
- `upstream-main` is a read-only upstream mirror and is not a PR target.
- Upstream sync for `main` is merge-based (`upstream-main` -> `main`), not rebase-based.
- Keep your changes focused. Multiple unrelated fixes should be opened as separate PRs.
- Ensure your change is free of lint warnings and test failures.
- When branch work is complete, commit it, push the topic branch, and open a PR targeting `main`; do not leave completed work as local-only changes.

### Guidance for invited code contributions

1. **Start with an issue.** Open a new one or comment on an existing discussion so we can agree on the solution before code is written.
2. **Add or update tests.** A bug fix should generally come with test coverage that fails before your change and passes afterwards. 100% coverage is not required, but aim for meaningful assertions.
3. **Document behavior.** If your change affects user-facing behavior, update the README, inline help (`codex --help`), or relevant example projects.
4. **Keep commits atomic.** Each commit should compile and the tests should pass. This makes reviews and potential rollbacks easier.

### Model metadata updates

When a change updates model catalogs or model metadata (`/models` payloads, presets, or fixtures):

- Set `input_modalities` explicitly for any model that does not support images.
- Keep compatibility defaults in mind: omitted `input_modalities` currently implies text + image support.
- Ensure client surfaces that accept images (for example, TUI paste/attach) consume the same capability signal.
- Add/update tests that cover unsupported-image behavior and warning paths.

### Opening a pull request (by invitation only)

- Fill in the PR template (or include similar information) - **What? Why? How?**
- Include a link to a bug report or enhancement request in the issue tracker
- Run the smallest relevant local checks first. Use the root `just` helpers so you stay consistent with the rest of the workspace: `just fmt`, `just fix -p <crate>` for the crate you touched, and the relevant tests (for example, `just test -p codex-tui`). Heavy sweeps and release-mode validation should be offloaded to GitHub Actions after the branch is pushed.
- Make sure your branch is up-to-date with `main` and that you have resolved merge conflicts.
- Open a PR for every completed topic branch. Branch work is not considered handed off until a PR exists.
- Mark the PR as **Ready for review** only when you believe it is in a merge-able state.

### Review process

1. One maintainer will be assigned as a primary reviewer.
2. If your invited PR introduces scope or behavior that was not previously discussed and approved, we may close the PR.
3. We may ask for changes. Please do not take this personally. We value the work, but we also value consistency and long-term maintainability.
4. When there is consensus that the PR meets the bar, a maintainer will squash-and-merge.
=======
Open a feature request in the issue tracker, or upvote an existing request that describes the same need. Explain your use case, the behavior you would like, and why it would improve your workflow.
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360

### Community values

- **Be kind and inclusive.** Treat others with respect; we follow the [Contributor Covenant](https://www.contributor-covenant.org/).
- **Assume good intent.** Written communication is hard, so err on the side of generosity.
- **Share what you learn.** Reproduction details, logs, and analysis help the entire community.

### Security

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
If you run into problems setting up the project, would like feedback on an idea, or just want to say _hi_ - please open a Discussion topic or jump into the relevant issue. We are happy to help.

### Security & responsible AI

If you discover a security issue in this fork, open a private security report via GitHub Security Advisories for this repository.
=======
If you discover a security vulnerability, follow the [security policy](../SECURITY.md) instead of reporting it in a public issue.
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
