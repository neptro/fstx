import type { BeginOptions, WriteOptions, RecoveryReport, Inspection, TxInspection } from './binding'

export type { BeginOptions, WriteOptions, RecoveryReport, Inspection, TxInspection }

/** Stable error codes, available as `err.code`. */
export type ErrorCode =
  | 'INVALID_PATH'
  | 'NOT_FOUND'
  | 'ALREADY_EXISTS'
  | 'NOT_A_DIRECTORY'
  | 'IS_A_DIRECTORY'
  | 'DIRECTORY_NOT_EMPTY'
  | 'UNSUPPORTED_FILE_TYPE'
  | 'INVALID_MOVE'
  | 'CASE_COLLISION'
  | 'CONFLICT'
  | 'UNSUPPORTED_FILESYSTEM'
  | 'UNSUPPORTED_PLATFORM'
  | 'RECOVERY_REQUIRED'
  | 'COMMIT_OUTCOME_UNKNOWN'
  | 'ROLLBACK_FAILED'
  | 'TRANSACTION_FINISHED'
  | 'IO'

/** Errors thrown by fstx. */
export interface FstxError extends Error {
  code: ErrorCode
}

/**
 * A set of changes to a directory tree that becomes visible all at once, or not at all.
 * Nothing changes on disk until `commit()`. Holds the directory's lock until committed
 * or discarded.
 */
export declare class Transaction {
  /** Starts a transaction on `root`; recovers interrupted transactions first. */
  static begin(root: string, options?: BeginOptions): Transaction
  /** Creates or replaces a file. A replaced file keeps its permissions unless `mode` is given. */
  write(path: string, data: string | Uint8Array, options?: WriteOptions): void
  /** Reads a file as this transaction sees it (including staged writes). */
  read(path: string): Buffer
  exists(path: string): boolean
  createDirAll(path: string): void
  /** Moves a file or directory. Fails if `to` exists. */
  rename(from: string, to: string): void
  /** Removes a file or an empty directory. */
  remove(path: string): void
  removeDirAll(path: string): void
  /** Applies every change atomically; returns once the commit is durable. */
  commit(): void
  /** Like `commit()`, on a worker thread. */
  commitAsync(): Promise<void>
  /** Discards all staged changes and releases the lock. */
  discard(): void
  /** True after `commit()`, `commitAsync()` or `discard()`. */
  get finished(): boolean
}

/** Runs `fn(tx)`, then commits; discards if `fn` throws or its promise rejects. */
export declare function transaction<T>(
  root: string,
  fn: (tx: Transaction) => T,
  options?: BeginOptions,
): T extends PromiseLike<infer U> ? Promise<U> : T

/** Finishes or rolls back transactions interrupted by a crash. */
export declare function recover(root: string, options?: BeginOptions): RecoveryReport

/** Read-only report of interrupted transactions and what recovery would do. */
export declare function inspect(root: string): Inspection
