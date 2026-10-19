/**
 * Stellar sequence-number recovery.
 *
 * A Stellar transaction embeds a sequence number that must equal the source
 * account's current sequence + 1. If another transaction from the same account
 * lands first, a transaction built with the old sequence is rejected with
 * `tx_bad_seq`. Recovering from that is only safe when we can *prove* the
 * failed attempt never applied on-chain.
 *
 * This module classifies a submission failure into one of three buckets:
 *
 *   • retryable — a DEFINITE failure. Either the transaction never reached the
 *     network (build/simulate/sign error, pre-submit timeout), or the network
 *     refused the whole transaction (`tx_bad_seq`). Account state is refreshed
 *     and the action may be rebuilt safely.
 *   • ambiguous — the transaction may or may not have applied (post-submit
 *     timeout, dropped connection, unknown RPC result). A retry could DUPLICATE
 *     an economic action, so it is NEVER automatic: the original intent and
 *     transaction hash are retained and the caller must ask the user.
 *   • terminal — a user rejection or an invalid, unrecoverable request. No retry.
 */

import {
  classifyWalletError,
  isStaleSequenceError,
  type WalletError,
} from "@/lib/walletErrors";

// ── Stages & errors ────────────────────────────────────────────────────────────

/**
 * How far a submission got before it failed.
 *
 * `unknown` is deliberately conservative: if the caller cannot prove the
 * transaction never reached the network, a retry is treated as ambiguous.
 */
export type SubmissionStage = "pre_submit" | "post_submit" | "unknown";

/**
 * Thrown by a submit function to report *where* a transaction failed.
 * Attach `hash` whenever the transaction was already broadcast, so the caller
 * can retain it for reconciliation even when the outcome is ambiguous.
 */
export class TransactionSubmitError extends Error {
  readonly stage: SubmissionStage;
  /** Hash of the broadcast transaction, when one exists. */
  readonly hash?: string;
  /** The original error, kept for classification and diagnostics. */
  readonly raw: unknown;

  constructor(options: {
    message?: string;
    stage: SubmissionStage;
    hash?: string;
    cause?: unknown;
  }) {
    super(options.message ?? "Transaction submission failed");
    this.name = "TransactionSubmitError";
    this.stage = options.stage;
    this.hash = options.hash;
    this.raw = options.cause;
  }
}

// ── Failure classification ─────────────────────────────────────────────────────

export type RetryClass = "retryable" | "ambiguous" | "terminal";

export interface FailureClassification {
  retryClass: RetryClass;
  stage: SubmissionStage;
  /** Definite stale-sequence rejection — refresh account state before retry. */
  isStaleSequence: boolean;
  walletError: WalletError;
  /** Hash retained from the failed attempt, when known. */
  hash?: string;
}

function stageOf(err: unknown): SubmissionStage {
  return err instanceof TransactionSubmitError ? err.stage : "unknown";
}

function hashOf(err: unknown): string | undefined {
  return err instanceof TransactionSubmitError ? err.hash : undefined;
}

/**
 * Decide whether a failed submission is safe to retry.
 *
 * Order matters: a stale-sequence rejection is definitive regardless of stage,
 * so it wins before the stage-based rules.
 */
export function classifySubmissionFailure(err: unknown): FailureClassification {
  const stage = stageOf(err);
  const hash = hashOf(err);
  // Classify the underlying error so wrapped Stellar result codes are seen.
  const source =
    err instanceof TransactionSubmitError && err.raw ? err.raw : err;
  const walletError = classifyWalletError(source);
  const isStaleSequence = isStaleSequenceError(source);

  // `tx_bad_seq` means the network rejected the whole transaction, so it did
  // not apply — safe to refresh the sequence and rebuild.
  if (isStaleSequence) {
    return { retryClass: "retryable", stage, isStaleSequence, walletError, hash };
  }

  // The user or the network definitively refused the request.
  if (
    walletError.code === "USER_REJECTED" ||
    walletError.code === "INSUFFICIENT_BALANCE" ||
    walletError.code === "NETWORK_MISMATCH" ||
    walletError.code === "EXTENSION_NOT_INSTALLED"
  ) {
    return { retryClass: "terminal", stage, isStaleSequence, walletError, hash };
  }

  // Nothing was sent → definite failure, safe to retry.
  if (stage === "pre_submit") {
    return { retryClass: "retryable", stage, isStaleSequence, walletError, hash };
  }

  // Broadcast but unresolved (or stage unknown) → ambiguous. Never auto-retry:
  // the transaction may have applied, and a retry could duplicate the action.
  return { retryClass: "ambiguous", stage, isStaleSequence, walletError, hash };
}

// ── Account state refresh ──────────────────────────────────────────────────────

/** Horizon base URL, configurable per environment. */
export const STELLAR_HORIZON_URL =
  process.env.NEXT_PUBLIC_STELLAR_HORIZON_URL ??
  "https://horizon-testnet.stellar.org";

/** Account state needed to rebuild a transaction against a fresh sequence. */
export interface AccountState {
  address: string;
  /** The account's current sequence number (i64 serialized as a string). */
  sequence: string;
  refreshedAt: number;
}

/** Minimal fetch signature so tests can inject a deterministic transport. */
export type FetchLike = (
  input: string,
  init?: RequestInit
) => Promise<Response>;

export interface RefreshAccountStateOptions {
  /** Inject a fetch implementation (tests). Defaults to global fetch. */
  fetchImpl?: FetchLike;
  /** Override the Horizon base URL. */
  horizonUrl?: string;
}

/**
 * Re-read an account's current sequence number from Horizon.
 *
 * Throws when the account cannot be read or the response is malformed — the
 * caller must not blindly rebuild a transaction from an unknown sequence.
 */
export async function refreshAccountSequence(
  address: string,
  options: RefreshAccountStateOptions = {}
): Promise<AccountState> {
  const fetchImpl = options.fetchImpl ?? fetch;
  const horizonUrl = (options.horizonUrl ?? STELLAR_HORIZON_URL).replace(
    /\/$/,
    ""
  );

  const res = await fetchImpl(
    `${horizonUrl}/accounts/${encodeURIComponent(address)}`,
    { headers: { Accept: "application/json" } }
  );

  if (!res.ok) {
    throw new Error(`Failed to refresh account state (HTTP ${res.status})`);
  }

  const body = (await res.json()) as { sequence?: unknown };
  if (typeof body?.sequence !== "string" || body.sequence.length === 0) {
    throw new Error("Account response is missing a sequence number");
  }

  return { address, sequence: body.sequence, refreshedAt: Date.now() };
}

// ── Recovery coordinator ───────────────────────────────────────────────────────

/** The user's original economic intent, retained across every retry. */
export interface TransactionIntent<TParams = unknown> {
  /** Stable id for this intent (e.g. `pool-42-outcome-1`). */
  id: string;
  /** The original parameters, reused verbatim on retry. */
  params: TParams;
  /** Epoch ms when the intent was first created. */
  createdAt: number;
}

export interface SubmitContext {
  /** 1-based attempt counter across the whole recovery run. */
  attempt: number;
  /** Freshest account state available, when a refresh has happened. */
  accountState: AccountState | null;
}

/** Build, sign and broadcast the intent. Throw {@link TransactionSubmitError}. */
export type SubmitFn<TParams> = (
  intent: TransactionIntent<TParams>,
  context: SubmitContext
) => Promise<{ hash: string }>;

export interface RunTransactionOptions<TParams> {
  intent: TransactionIntent<TParams>;
  submit: SubmitFn<TParams>;
  /** Source account whose sequence may go stale. */
  address?: string | null;
  /** Re-read account state before rebuilding. Required for stale-sequence recovery. */
  refreshAccountState?: (address: string) => Promise<AccountState>;
  /** Automatic retries allowed for DEFINITE failures. Default 1. */
  maxAutoRetries?: number;
  /**
   * Set true only after the user explicitly confirms a retry of an ambiguous
   * outcome. Never set this from an automatic path.
   */
  userConfirmedRetry?: boolean;
}

export type RunStatus = "confirmed" | "ambiguous" | "failed";

export interface TransactionRunResult<TParams> {
  status: RunStatus;
  /** The original intent — always retained, including on failure. */
  intent: TransactionIntent<TParams>;
  /** Transaction hash when known (retained even for an ambiguous outcome). */
  hash?: string;
  error?: WalletError;
  classification?: FailureClassification;
  /** True when the caller MUST ask the user before any further retry. */
  requiresUserConfirmation: boolean;
  /** Number of submit attempts made to the wallet / network. */
  attempts: number;
  accountState: AccountState | null;
}

/**
 * Run an intent with bounded, safe recovery.
 *
 * Guarantees:
 *   • A definite pre-submit (or `tx_bad_seq`) failure may be retried
 *     automatically, refreshing account state first for stale sequences.
 *   • An ambiguous outcome is NEVER retried automatically. The run stops with
 *     `status: "ambiguous"` and `requiresUserConfirmation: true`, retaining the
 *     intent and hash.
 *   • Passing `userConfirmedRetry: true` permits exactly one retry of an
 *     ambiguous outcome (after a best-effort account refresh).
 */
export async function runTransactionWithSequenceRecovery<TParams>(
  options: RunTransactionOptions<TParams>
): Promise<TransactionRunResult<TParams>> {
  const {
    intent,
    submit,
    address = null,
    refreshAccountState,
    maxAutoRetries = 1,
  } = options;

  let accountState: AccountState | null = null;
  let attempts = 0;
  let autoRetries = 0;
  // One-shot: consumed by the first ambiguous retry we are allowed to perform.
  let allowAmbiguousRetry = options.userConfirmedRetry === true;

  const buildResult = (
    status: RunStatus,
    failure: FailureClassification | undefined,
    extra: {
      hash?: string;
      requiresUserConfirmation: boolean;
    }
  ): TransactionRunResult<TParams> => ({
    status,
    intent,
    hash: extra.hash,
    error: failure?.walletError,
    classification: failure,
    requiresUserConfirmation: extra.requiresUserConfirmation,
    attempts,
    accountState,
  });

  // Initial attempt + up to `maxAutoRetries` definite retries + one confirmed
  // ambiguous retry.
  const maxAttempts = maxAutoRetries + 2;
  let lastFailure: FailureClassification | undefined;

  for (let index = 0; index < maxAttempts; index++) {
    attempts += 1;

    try {
      const { hash } = await submit(intent, { attempt: attempts, accountState });
      return buildResult("confirmed", undefined, {
        hash,
        requiresUserConfirmation: false,
      });
    } catch (caught) {
      lastFailure = classifySubmissionFailure(caught);
    }

    // Terminal: user or network refused. No retry — but never a duplicate.
    if (lastFailure.retryClass === "terminal") {
      return buildResult("failed", lastFailure, {
        hash: lastFailure.hash,
        requiresUserConfirmation: false,
      });
    }

    // Ambiguous: a retry could duplicate an economic action.
    if (lastFailure.retryClass === "ambiguous") {
      if (!allowAmbiguousRetry) {
        // Stop and ask the user; keep intent + hash for reconciliation.
        return buildResult("ambiguous", lastFailure, {
          hash: lastFailure.hash,
          requiresUserConfirmation: true,
        });
      }
      allowAmbiguousRetry = false;
      // Best effort: if the earlier attempt applied, the sequence advanced.
      if (refreshAccountState && address) {
        try {
          accountState = await refreshAccountState(address);
        } catch {
          // Keep the last-known state; the retry will surface any failure.
        }
      }
      continue;
    }

    // Retryable (definite).
    if (autoRetries >= maxAutoRetries) {
      return buildResult("failed", lastFailure, {
        hash: lastFailure.hash,
        requiresUserConfirmation: false,
      });
    }

    if (lastFailure.isStaleSequence) {
      // Without a fresh sequence a rebuild would fail identically.
      if (!refreshAccountState || !address) {
        return buildResult("failed", lastFailure, {
          hash: lastFailure.hash,
          requiresUserConfirmation: false,
        });
      }
      try {
        accountState = await refreshAccountState(address);
      } catch {
        return buildResult("failed", lastFailure, {
          hash: lastFailure.hash,
          requiresUserConfirmation: false,
        });
      }
    }

    autoRetries += 1;
  }

  // Bounded loop exhausted without a definitive result.
  return buildResult("failed", lastFailure, {
    hash: lastFailure?.hash,
    requiresUserConfirmation: false,
  });
}
