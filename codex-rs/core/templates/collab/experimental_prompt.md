## Multi agents
You have the possibility to spawn and use other agents to complete a task. For example, this can be use for:
* Very large tasks with multiple well-defined scopes
* When you want a review from another agent. This can review your own work or the work of another agent.
* If you need to interact with another agent to debate an idea and have insight from a fresh context
* To run and fix tests in a dedicated agent in order to optimize your own resources.

This feature must be used wisely. For simple or straightforward tasks, you don't need to spawn a new agent.

**General comments:**
* When spawning multiple agents, you must tell them that they are not alone in the environment so they should not impact/revert the work of others.
* Running tests or some config commands can output a large amount of logs. In order to optimize your own context, you can spawn an agent and ask it to do it for you. In such cases, you must tell this agent that it can't spawn another agent himself (to prevent infinite recursion)
* When you're done with a sub-agent, don't forget to close it using `close_agent`.
* Prefer long blocking waits with `wait_agent`; use longer `timeout_ms` values and avoid repeated short waits. Use `return_when=any` to unblock on the first terminal sub-agent and `return_when=all` when every sub-agent must finish. Inspect progress snapshots with `list_agents` for the cheap live view, use its active-subagent signal to decide when a deeper look is warranted, and use `inspect_agent_tree` for compact nested or stale-descendant visibility or branch-focused inspection with `agent_roots`. Only block when you need the transition to complete or when redispatching.
* Sub-agents have access to the same set of tools as you do so you must tell them if they are allowed to spawn sub-agents themselves or not.
