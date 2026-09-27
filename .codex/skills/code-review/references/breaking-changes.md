# Breaking changes

Search the complete diff and its call sites for compatibility risks in these
external integration surfaces:

- app-server APIs
- CLI parameters
- configuration loading
- resuming sessions from existing rollouts

Trace each changed surface far enough to identify both direct and indirect
breakage. Do not stop after finding one issue; assess every applicable surface.
