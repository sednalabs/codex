// based on event types from codex-rs/exec/src/exec_events.rs

import type { ThreadItem } from "./items";

/** Emitted when a new thread is started as the first event. */
export type ThreadStartedEvent = {
  type: "thread.started";
  /** The identifier of the new thread. Can be used to resume the thread later. */
  thread_id: string;
};

/**
 * Emitted when a turn is started by sending a new prompt to the model.
 * A turn encompasses all events that happen while the agent is processing the prompt.
 */
export type TurnStartedEvent = {
  type: "turn.started";
  thread_id: string;
  turn_id: string;
};

/** Describes the usage of tokens during a turn. */
export type Usage = {
  /** The number of input tokens used during the turn. */
  input_tokens: number;
  /** The number of cached input tokens used during the turn. */
  cached_input_tokens: number;
  /** The number of output tokens used during the turn. */
  output_tokens: number;
  /** The number of reasoning output tokens used during the turn. */
  reasoning_output_tokens: number;
};

/** Emitted when a turn is completed. Typically right after the assistant's response. */
export type TurnCompletedEvent = {
  type: "turn.completed";
  thread_id: string;
  turn_id: string;
  usage: Usage;
};

/** Indicates that a turn failed with an error. */
export type TurnFailedEvent = {
  type: "turn.failed";
  thread_id: string;
  turn_id: string;
  error: ThreadError;
};

/** Emitted when a new item is added to the thread. Typically the item is initially "in progress". */
export type ItemStartedEvent = {
  type: "item.started";
  thread_id?: string;
  turn_id?: string;
  item: ThreadItem;
};

/** Emitted when an item is updated. */
export type ItemUpdatedEvent = {
  type: "item.updated";
  thread_id?: string;
  turn_id?: string;
  item: ThreadItem;
};

/** Signals that an item has reached a terminal state—either success or failure. */
export type ItemCompletedEvent = {
  type: "item.completed";
  thread_id?: string;
  turn_id?: string;
  item: ThreadItem;
};

/** Fatal error emitted by the stream. */
export type ThreadError = {
  message: string;
  thread_id?: string;
  turn_id?: string;
};

/** Represents an unrecoverable error emitted directly by the event stream. */
export type ThreadErrorEvent = {
  type: "error";
  message: string;
  thread_id?: string;
  turn_id?: string;
};

/** Top-level JSONL events emitted by codex exec. */
export type ThreadEvent =
  | ThreadStartedEvent
  | TurnStartedEvent
  | TurnCompletedEvent
  | TurnFailedEvent
  | ItemStartedEvent
  | ItemUpdatedEvent
  | ItemCompletedEvent
  | ThreadErrorEvent;
