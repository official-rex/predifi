import fc from "fast-check";
import {
  I128_MAX,
  formatBaseUnits,
  parseAmountToBaseUnits,
  requireAmountToBaseUnits,
} from "@/lib/amount";

/**
 * Property tests for issue #129 — prove user-entered decimal amounts
 * round-trip to the intended Stellar base units without floating-point loss or
 * silent truncation.
 *
 * These tests generate values across the supported precision range and assert:
 *  - parse → format → parse is an identity on exact base units;
 *  - base units → format → parse is an identity;
 *  - excess precision is rejected, never truncated;
 *  - out-of-range values are rejected before submission;
 *  - arbitrary strings never throw.
 */

const STROOP = 7;

/** A decimal string with at most `decimals` fractional digits. */
function amountWithPrecision(decimals: number): fc.Arbitrary<string> {
  return fc
    .tuple(
      fc.nat({ max: 1_000_000_000 }),
      fc.array(fc.integer({ min: 0, max: 9 }), { maxLength: decimals }),
    )
    .map(([whole, fraction]) =>
      fraction.length === 0 ? `${whole}` : `${whole}.${fraction.join("")}`,
    );
}

describe("parseAmountToBaseUnits / formatBaseUnits (issue #129)", () => {
  it("round-trips exact base units across every supported precision", () => {
    for (let decimals = 0; decimals <= STROOP; decimals += 1) {
      fc.assert(
        fc.property(
          fc.bigInt({ min: 0n, max: 10n ** 30n }),
          (baseUnits) => {
            const formatted = formatBaseUnits(baseUnits, { decimals });
            const parsed = parseAmountToBaseUnits(formatted, { decimals });
            expect(parsed).toEqual({ ok: true, baseUnits, decimals });
          },
        ),
        { numRuns: 200 },
      );
    }
  });

  it("parse → format → parse preserves the value for generated inputs", () => {
    fc.assert(
      fc.property(amountWithPrecision(STROOP), (input) => {
        const first = parseAmountToBaseUnits(input, { decimals: STROOP });
        expect(first.ok).toBe(true);
        if (!first.ok) return;

        const formatted = formatBaseUnits(first.baseUnits, { decimals: STROOP });
        const second = parseAmountToBaseUnits(formatted, { decimals: STROOP });
        expect(second).toEqual(first);
      }),
      { numRuns: 500 },
    );
  });

  it("is exact above Number.MAX_SAFE_INTEGER (no float drift)", () => {
    // 2^53 + 1 whole units, plus one stroop of precision.
    const input = "9007199254740993.0000001";
    const parsed = parseAmountToBaseUnits(input, { decimals: STROOP });

    expect(parsed).toEqual({
      ok: true,
      baseUnits: 90_071_992_547_409_930_000_001n,
      decimals: STROOP,
    });

    // A float round-trip would have lost the low digits.
    expect(Number.isSafeInteger(Number(input.replace(".", "")))).toBe(false);

    if (parsed.ok) {
      expect(formatBaseUnits(parsed.baseUnits, { decimals: STROOP })).toBe(
        "9007199254740993.0000001",
      );
    }
  });

  it("rejects excess precision instead of truncating it", () => {
    fc.assert(
      fc.property(
        fc.nat({ max: 1_000_000 }),
        fc.integer({ min: 0, max: 9 }),
        fc.array(fc.integer({ min: 0, max: 9 }), {
          minLength: STROOP,
          maxLength: STROOP,
        }),
        (whole, firstDigit, rest) => {
          const fraction = [firstDigit, ...rest].join(""); // length STROOP + 1
          const input = `${whole}.${fraction}`;
          const parsed = parseAmountToBaseUnits(input, { decimals: STROOP });

          expect(parsed).toEqual({ ok: false, error: "excess_precision" });
        },
      ),
      { numRuns: 300 },
    );
  });

  it("rejects out-of-range base units before submission", () => {
    fc.assert(
      fc.property(
        fc.bigInt({ min: I128_MAX + 1n, max: I128_MAX + 10n ** 30n }),
        (baseUnits) => {
          const formatted = formatBaseUnits(baseUnits, { decimals: STROOP });
          expect(parseAmountToBaseUnits(formatted, { decimals: STROOP })).toEqual(
            { ok: false, error: "out_of_range" },
          );
        },
      ),
      { numRuns: 200 },
    );

    // Custom bound.
    expect(
      parseAmountToBaseUnits("1.5", { decimals: 1, maxBaseUnits: 10n }),
    ).toEqual({ ok: false, error: "out_of_range" });
  });

  it("rejects malformed, empty and negative inputs explicitly", () => {
    const cases: Array<[string, string]> = [
      ["", "empty"],
      ["   ", "empty"],
      ["-1", "negative"],
      ["-0.5", "negative"],
      ["1,000", "invalid_format"],
      ["1.2.3", "invalid_format"],
      ["1e7", "invalid_format"],
      ["abc", "invalid_format"],
      ["1 2", "invalid_format"],
      ["+1", "invalid_format"],
      ["0x10", "invalid_format"],
      [".", "invalid_format"],
    ];

    for (const [input, error] of cases) {
      expect(parseAmountToBaseUnits(input, { decimals: STROOP })).toEqual({
        ok: false,
        error,
      });
    }
  });

  it("never throws on arbitrary input", () => {
    fc.assert(
      fc.property(fc.string(), (input) => {
        expect(() => parseAmountToBaseUnits(input, { decimals: STROOP })).not.toThrow();
      }),
      { numRuns: 500 },
    );
  });

  it("accepts the documented grammar (leading dot / trailing dot / zero)", () => {
    expect(parseAmountToBaseUnits(".5", { decimals: 1 })).toEqual({
      ok: true,
      baseUnits: 5n,
      decimals: 1,
    });
    expect(parseAmountToBaseUnits("1.", { decimals: 2 })).toEqual({
      ok: true,
      baseUnits: 100n,
      decimals: 2,
    });
    expect(parseAmountToBaseUnits("0", { decimals: 7 })).toEqual({
      ok: true,
      baseUnits: 0n,
      decimals: 7,
    });
  });

  it("throws with the rejection reason at the submission boundary", () => {
    expect(() => requireAmountToBaseUnits("1.23456789")).toThrow(
      "amount_excess_precision",
    );
    expect(() => requireAmountToBaseUnits("nope")).toThrow(
      "amount_invalid_format",
    );
    expect(requireAmountToBaseUnits("1.2345678")).toBe(12_345_678n);
  });
});
