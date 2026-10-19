import {
  TransactionSubmitError,
  classifySubmissionFailure,
  refreshAccountSequence,
  runTransactionWithSequenceRecovery,
  type AccountState,
  type TransactionIntent,
} from "@/lib/stellarTransaction";

// ── Helpers ────────────────────────────────────────────────────────────────────

function jsonResponse(body: unknown, status = 200): Response {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => body,
  } as unknown as Response;
}

/** A Horizon `tx_bad_seq` rejection, nested exactly as the API returns it. */
function badSeqError(): unknown {
  return {
    message: "Transaction submission failed",
    extras: { result_codes: { transaction: "tx_bad_seq" } },
  };
}

type PredictionParams = { poolId: number; outcome: number; amount: string };

const INTENT: TransactionIntent<PredictionParams> = {
  id: "pool-42-outcome-1",
  params: { poolId: 42, outcome: 1, amount: "10" },
  createdAt: 1_700_000_000_000,
};

const ACCOUNT_STATE: AccountState = {
  address: "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF",
  sequence: "4294967296",
  refreshedAt: 1,
};

// ── classifySubmissionFailure ──────────────────────────────────────────────────

describe("classifySubmissionFailure", () => {
  it("treats a definite pre-submit failure as retryable", () => {
    const error = new TransactionSubmitError({
      stage: "pre_submit",
      cause: new Error("simulation failed"),
    });

    const result = classifySubmissionFailure(error);

    expect(result.retryClass).toBe("retryable");
    expect(result.stage).toBe("pre_submit");
    expect(result.isStaleSequence).toBe(false);
  });

  it("treats a stale sequence (`tx_bad_seq`) as retryable regardless of stage", () => {
    const error = new TransactionSubmitError({
      stage: "post_submit",
      cause: badSeqError(),
    });

    const result = classifySubmissionFailure(error);

    expect(result.retryClass).toBe("retryable");
    expect(result.isStaleSequence).toBe(true);
    expect(result.walletError.code).toBe("STALE_SEQUENCE");
  });

  it("treats a pre-submit timeout as retryable (nothing was sent)", () => {
    const error = new TransactionSubmitError({
      stage: "pre_submit",
      cause: new Error("Request timed out"),
    });

    expect(classifySubmissionFailure(error).retryClass).toBe("retryable");
  });

  it("treats a post-submit timeout as ambiguous (never auto-retry)", () => {
    const error = new TransactionSubmitError({
      stage: "post_submit",
      hash: "abc123",
      cause: new Error("Request timed out"),
    });

    const result = classifySubmissionFailure(error);

    expect(result.retryClass).toBe("ambiguous");
    expect(result.hash).toBe("abc123");
    expect(result.walletError.code).toBe("TRANSACTION_TIMEOUT");
  });

  it("treats an unknown-stage, unknown error as ambiguous (conservative)", () => {
    expect(classifySubmissionFailure(new Error("kaboom")).retryClass).toBe(
      "ambiguous"
    );
  });

  it("treats user rejection and insufficient balance as terminal", () => {
    expect(
      classifySubmissionFailure(
        new TransactionSubmitError({
          stage: "pre_submit",
          cause: new Error("User rejected the request"),
        })
      ).retryClass
    ).toBe("terminal");

    expect(
      classifySubmissionFailure(
        new TransactionSubmitError({
          stage: "pre_submit",
          cause: new Error("insufficient balance for fee"),
        })
      ).retryClass
    ).toBe("terminal");
  });
});

// ── refreshAccountSequence ─────────────────────────────────────────────────────

describe("refreshAccountSequence", () => {
  it("reads the current sequence number from Horizon", async () => {
    const fetchImpl = jest
      .fn()
      .mockResolvedValue(jsonResponse({ sequence: "12345" }));

    const state = await refreshAccountSequence(ACCOUNT_STATE.address, {
      fetchImpl,
      horizonUrl: "https://horizon.example.org/",
    });

    expect(state.sequence).toBe("12345");
    expect(state.address).toBe(ACCOUNT_STATE.address);
    expect(fetchImpl).toHaveBeenCalledWith(
      `https://horizon.example.org/accounts/${ACCOUNT_STATE.address}`,
      expect.objectContaining({ headers: { Accept: "application/json" } })
    );
  });

  it("throws when Horizon responds with a non-2xx status", async () => {
    const fetchImpl = jest.fn().mockResolvedValue(jsonResponse({}, 404));

    await expect(
      refreshAccountSequence(ACCOUNT_STATE.address, { fetchImpl })
    ).rejects.toThrow(/Failed to refresh account state/);
  });

  it("throws when the response has no sequence number", async () => {
    const fetchImpl = jest.fn().mockResolvedValue(jsonResponse({}));

    await expect(
      refreshAccountSequence(ACCOUNT_STATE.address, { fetchImpl })
    ).rejects.toThrow(/missing a sequence number/);
  });
});

// ── runTransactionWithSequenceRecovery ─────────────────────────────────────────

describe("runTransactionWithSequenceRecovery", () => {
  it("recovers a stale sequence by refreshing account state, creating exactly one prediction", async () => {
    const predictions: string[] = [];
    let calls = 0;
    const refresh = jest.fn(async () => ACCOUNT_STATE);

    const submit = jest.fn(
      async (intent: TransactionIntent<PredictionParams>) => {
        calls += 1;
        if (calls === 1) {
          // Definite: the network rejected the stale sequence, nothing applied.
          throw new TransactionSubmitError({
            stage: "pre_submit",
            cause: badSeqError(),
          });
        }
        predictions.push(intent.id);
        return { hash: "hash-2" };
      }
    );

    const result = await runTransactionWithSequenceRecovery({
      intent: INTENT,
      submit,
      address: ACCOUNT_STATE.address,
      refreshAccountState: refresh,
    });

    expect(result.status).toBe("confirmed");
    expect(result.hash).toBe("hash-2");
    expect(result.attempts).toBe(2);
    expect(refresh).toHaveBeenCalledWith(ACCOUNT_STATE.address);
    expect(submit).toHaveBeenCalledTimes(2);
    // The stale attempt never applied, so exactly one prediction lands.
    expect(predictions).toEqual([INTENT.id]);
  });

  it("does NOT duplicate a prediction on an unknown post-submit outcome", async () => {
    let predictionsCreated = 0;
    const refresh = jest.fn(async () => ACCOUNT_STATE);

    const submit = jest.fn(async () => {
      // The transaction was broadcast — it may have applied.
      predictionsCreated += 1;
      throw new TransactionSubmitError({
        stage: "post_submit",
        hash: "abc123",
        cause: new Error("Request timed out"),
      });
    });

    const result = await runTransactionWithSequenceRecovery({
      intent: INTENT,
      submit,
      address: ACCOUNT_STATE.address,
      refreshAccountState: refresh,
    });

    expect(result.status).toBe("ambiguous");
    expect(result.requiresUserConfirmation).toBe(true);
    // Intent and hash are retained for reconciliation.
    expect(result.intent).toBe(INTENT);
    expect(result.hash).toBe("abc123");
    expect(result.attempts).toBe(1);
    // Never automatically repeated.
    expect(submit).toHaveBeenCalledTimes(1);
    expect(predictionsCreated).toBe(1);
    expect(refresh).not.toHaveBeenCalled();
  });

  it("retries an ambiguous outcome only after explicit user confirmation", async () => {
    let calls = 0;
    const refresh = jest.fn(async () => ACCOUNT_STATE);

    const submit = jest.fn(async () => {
      calls += 1;
      if (calls === 1) {
        throw new TransactionSubmitError({
          stage: "post_submit",
          hash: "hash-1",
          cause: new Error("Request timed out"),
        });
      }
      return { hash: "hash-2" };
    });

    const result = await runTransactionWithSequenceRecovery({
      intent: INTENT,
      submit,
      address: ACCOUNT_STATE.address,
      refreshAccountState: refresh,
      userConfirmedRetry: true,
    });

    expect(result.status).toBe("confirmed");
    expect(result.hash).toBe("hash-2");
    expect(submit).toHaveBeenCalledTimes(2);
    // The sequence may have advanced if the first attempt applied.
    expect(refresh).toHaveBeenCalledWith(ACCOUNT_STATE.address);
  });

  it("does not retry a terminal failure", async () => {
    const submit = jest.fn(async () => {
      throw new TransactionSubmitError({
        stage: "pre_submit",
        cause: new Error("User rejected the request"),
      });
    });

    const result = await runTransactionWithSequenceRecovery({
      intent: INTENT,
      submit,
      refreshAccountState: jest.fn(),
      address: ACCOUNT_STATE.address,
    });

    expect(result.status).toBe("failed");
    expect(result.requiresUserConfirmation).toBe(false);
    expect(submit).toHaveBeenCalledTimes(1);
  });

  it("fails safely when a stale sequence cannot be refreshed", async () => {
    const submit = jest.fn(async () => {
      throw new TransactionSubmitError({
        stage: "pre_submit",
        cause: badSeqError(),
      });
    });

    const result = await runTransactionWithSequenceRecovery({
      intent: INTENT,
      submit,
      // No refreshAccountState / address → cannot rebuild safely.
    });

    expect(result.status).toBe("failed");
    expect(submit).toHaveBeenCalledTimes(1);
  });
});
