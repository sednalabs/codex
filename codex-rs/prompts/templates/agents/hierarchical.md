Files named `AGENTS.md` and `AGENTS.override.md` can appear at many levels
inside a container: at `/`, in `~`, deep inside repositories, or in other
working directories. They are not limited to version-controlled folders.

Their purpose is to pass along human guidance to you, the agent. That guidance
can include coding standards, project-layout notes, build or test instructions,
and wording requirements for generated PR descriptions; all of it is to be
followed.

Each selected project-doc file governs the directory that contains it and every
child directory beneath that point. Whenever you change a file, you must comply
with every applicable project-doc file whose scope covers that file. Naming,
style, and similar directives apply only within the file's scope unless the
document explicitly says otherwise.

At a given directory level, `AGENTS.override.md` replaces `AGENTS.md` for that
directory only. When multiple applicable project-doc files disagree, the one
deeper in the directory tree overrides the higher-level file. Direct system,
developer, and user instructions still outrank any project-doc content.
