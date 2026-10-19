"use client";

/**
 * useStellarTransaction — React integration for safe Stellar submission.
 *
 * Wraps {@link runTransactionWithSequenceRecovery} so a component can:
 *   • run an economic action and let definite failures (stale sequence,
 *     pre-submit errors) recover automatically, while
 *   • pausing on an ambiguous outcome — `requiresUserConfirmation` becomes
 *     true and the caller must render a prompt and call `confirmRetry()`.
 *
 * The status maps directly onto the existing `TransactionProgress` states
 * (`idle` | `running` → submitted/processing | `confirmed` | `failed`), and
 * `error` is a `WalletError` ready for `WalletErrorBanner`.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  refreshAccountSequence,
  runTransactionWithSequenceRecovery,
  type AccountState,
  type SubmitFn,
  type TransactionIntent,
  type TransactionRunResult,
} from "@/lib/stellarTransaction";
import type { WalletError } from "@/lib/walletErrors";

export type StellarTxStatus =
  | "idle"
  | "running"
  | "confirmed"
  | "ambiguous"
  | "failed";

export interface UseStellarTransactionOptions<TParams> {
  /** Connected source account, or null when disconnected. */
  address: string | null;
  /** Build, sign and broadcast the intent. Throw `TransactionSubmitError`. */
  submit: SubmitFn<TParams>;
  /** Override account-state refresh (tests / custom RPC). */
  refreshAccountState?: (address: string) => Promise<AccountState>;
  /** Automatic retries for definite failures. Default 1. */
  maxAutoRetries?: number;
  /** Build the retained intent for a run. Defaults to a generated id. */
  createIntent?: (params: TParams) => TransactionIntent<TParams>;
}

export interface UseStellarTransactionResult<TParams> {
  status: StellarTxStatus;
  /** Transaction hash, retained across ambiguity. */
  hash?: string;
  error?: WalletError;
  /** The original intent, retained for a confirmed retry. */
  intent: TransactionIntent<TParams> | null;
  /** True when the caller must ask the user before retrying. */
  requiresUserConfirmation: boolean;
  accountState: AccountState | null;
  /** Run the action, creating a fresh intent. */
  run: (params: TParams) => Promise<TransactionRunResult<TParams>>;
  /** Retry an ambiguous outcome after explicit user confirmation. */
  confirmRetry: () => Promise<TransactionRunResult<TParams> | null>;
  reset: () => void;
}

let intentCounter = 0;

function defaultIntent<TParams>(params: TParams): TransactionIntent<TParams> {
  intentCounter += 1;
  return {
    id: `stellar-tx-${Date.now()}-${intentCounter}`,
    params,
    createdAt: Date.now(),
  };
}

export function useStellarTransaction<TParams>(
  options: UseStellarTransactionOptions<TParams>
): UseStellarTransactionResult<TParams> {
  const { address, submit, maxAutoRetries = 1, createIntent } = options;
  const refreshAccountState =
    options.refreshAccountState ?? refreshAccountSequence;

  const [status, setStatus] = useState<StellarTxStatus>("idle");
  const [hash, setHash] = useState<string>();
  const [error, setError] = useState<WalletError>();
  const [intent, setIntent] = useState<TransactionIntent<TParams> | null>(null);
  const [requiresUserConfirmation, setRequiresUserConfirmation] =
    useState(false);
  const [accountState, setAccountState] = useState<AccountState | null>(null);

  const intentRef = useRef<TransactionIntent<TParams> | null>(null);
  const mountedRef = useRef(true);
  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const execute = useCallback(
    async (
      txIntent: TransactionIntent<TParams>,
      userConfirmedRetry: boolean
    ): Promise<TransactionRunResult<TParams>> => {
      intentRef.current = txIntent;
      setStatus("running");
      setError(undefined);
      setRequiresUserConfirmation(false);

      const result = await runTransactionWithSequenceRecovery({
        intent: txIntent,
        submit,
        address,
        refreshAccountState,
        maxAutoRetries,
        userConfirmedRetry,
      });

      if (!mountedRef.current) return result;

      setIntent(result.intent);
      setHash(result.hash);
      setAccountState(result.accountState);

      if (result.status === "confirmed") {
        setStatus("confirmed");
        setRequiresUserConfirmation(false);
      } else if (result.status === "ambiguous") {
        setStatus("ambiguous");
        setError(result.error);
        setRequiresUserConfirmation(true);
      } else {
        setStatus("failed");
        setError(result.error);
        setRequiresUserConfirmation(false);
      }

      return result;
    },
    [address, submit, refreshAccountState, maxAutoRetries]
  );

  const run = useCallback(
    async (params: TParams): Promise<TransactionRunResult<TParams>> => {
      const txIntent = (createIntent ?? defaultIntent)(params);
      return execute(txIntent, false);
    },
    [createIntent, execute]
  );

  const confirmRetry = useCallback(async () => {
    const txIntent = intentRef.current ?? intent;
    if (!txIntent) return null;
    return execute(txIntent, true);
  }, [execute, intent]);

  const reset = useCallback(() => {
    intentRef.current = null;
    setStatus("idle");
    setHash(undefined);
    setError(undefined);
    setIntent(null);
    setRequiresUserConfirmation(false);
    setAccountState(null);
  }, []);

  return useMemo(
    () => ({
      status,
      hash,
      error,
      intent,
      requiresUserConfirmation,
      accountState,
      run,
      confirmRetry,
      reset,
    }),
    [
      status,
      hash,
      error,
      intent,
      requiresUserConfirmation,
      accountState,
      run,
      confirmRetry,
      reset,
    ]
  );
}
