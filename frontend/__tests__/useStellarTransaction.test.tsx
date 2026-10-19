import { act, renderHook } from "@testing-library/react";

import { useStellarTransaction } from "@/lib/hooks/useStellarTransaction";
import {
  TransactionSubmitError,
  type AccountState,
  type TransactionIntent,
} from "@/lib/stellarTransaction";

type Params = { amount: string };

const ADDRESS = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF";

const ACCOUNT_STATE: AccountState = {
  address: ADDRESS,
  sequence: "4294967296",
  refreshedAt: 1,
};

function fixedIntent(params: Params): TransactionIntent<Params> {
  return { id: "intent-1", params, createdAt: 1 };
}

describe("useStellarTransaction", () => {
  it("reports confirmed with the transaction hash on success", async () => {
    const { result } = renderHook(() =>
      useStellarTransaction<Params>({
        address: ADDRESS,
        createIntent: fixedIntent,
        submit: jest.fn(async () => ({ hash: "hash-1" })),
      })
    );

    await act(async () => {
      await result.current.run({ amount: "10" });
    });

    expect(result.current.status).toBe("confirmed");
    expect(result.current.hash).toBe("hash-1");
    expect(result.current.requiresUserConfirmation).toBe(false);
  });

  it("pauses on an ambiguous outcome without duplicating the action", async () => {
    const submit = jest.fn(async () => {
      throw new TransactionSubmitError({
        stage: "post_submit",
        hash: "abc123",
        cause: new Error("Request timed out"),
      });
    });

    const { result } = renderHook(() =>
      useStellarTransaction<Params>({
        address: ADDRESS,
        createIntent: fixedIntent,
        refreshAccountState: jest.fn(async () => ACCOUNT_STATE),
        submit,
      })
    );

    await act(async () => {
      await result.current.run({ amount: "10" });
    });

    expect(result.current.status).toBe("ambiguous");
    expect(result.current.requiresUserConfirmation).toBe(true);
    expect(result.current.error?.code).toBe("TRANSACTION_TIMEOUT");
    // Intent + hash retained for reconciliation.
    expect(result.current.intent?.id).toBe("intent-1");
    expect(result.current.hash).toBe("abc123");
    // Never automatically repeated.
    expect(submit).toHaveBeenCalledTimes(1);
  });

  it("retries an ambiguous outcome only when the user confirms", async () => {
    let calls = 0;
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

    const { result } = renderHook(() =>
      useStellarTransaction<Params>({
        address: ADDRESS,
        createIntent: fixedIntent,
        refreshAccountState: jest.fn(async () => ACCOUNT_STATE),
        submit,
      })
    );

    await act(async () => {
      await result.current.run({ amount: "10" });
    });

    expect(result.current.status).toBe("ambiguous");

    await act(async () => {
      await result.current.confirmRetry();
    });

    expect(result.current.status).toBe("confirmed");
    expect(result.current.hash).toBe("hash-2");
    expect(result.current.requiresUserConfirmation).toBe(false);
    expect(submit).toHaveBeenCalledTimes(2);
  });

  it("resets back to idle", async () => {
    const { result } = renderHook(() =>
      useStellarTransaction<Params>({
        address: ADDRESS,
        createIntent: fixedIntent,
        submit: jest.fn(async () => ({ hash: "hash-1" })),
      })
    );

    await act(async () => {
      await result.current.run({ amount: "10" });
    });
    expect(result.current.status).toBe("confirmed");

    act(() => {
      result.current.reset();
    });

    expect(result.current.status).toBe("idle");
    expect(result.current.hash).toBeUndefined();
    expect(result.current.intent).toBeNull();
  });
});
