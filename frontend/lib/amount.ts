/**
 * amount.ts
 *
 * Strict, exact conversion between user-entered decimal amounts and Stellar
 * base units (stroops).
 *
 * Design goals:
 *  - **No floating point.** Parsing and formatting use strings and `BigInt`
 *    only, so values above `Number.MAX_SAFE_INTEGER` and amounts at the edge of
 *    token precision are exact.
 *  - **No silent truncation.** A value with more fractional digits than the
 *    asset supports is rejected (`excess_precision`), never rounded or sliced.
 *  - **Explicit accepted precision.** Every parse takes the asset's decimal
 *    precision (default 7 for XLM / Stellar SAC stroops) and enforces it.
 *  - **Bounded.** Values outside the configured base-unit range are rejected
 *    (`out_of_range`) before they can reach a wallet.
 *
 * The parser returns a discriminated result instead of throwing so callers can
 * surface the rejection reason in the UI.
 */

/** Stellar stroop precision: 1 XLM = 10_000_000 base units. */
export const STELLAR_DECIMALS = 7;

/** Highest decimal precision this module will convert exactly. */
export const MAX_SUPPORTED_DECIMALS = 18;

/** Maximum value of a signed 128-bit integer, the contract's amount type. */
export const I128_MAX = (1n << 127n) - 1n;

/** Why an amount was rejected. */
export type AmountParseError =
  | "empty"
  | "invalid_format"
  | "negative"
  | "excess_precision"
  | "out_of_range";

/** Result of {@link parseAmountToBaseUnits}. */
export type AmountParseResult =
  | { ok: true; baseUnits: bigint; decimals: number }
  | { ok: false; error: AmountParseError };

/** Options for {@link parseAmountToBaseUnits}. */
export interface ParseAmountOptions {
  /** Accepted fractional digits for this asset. Defaults to {@link STELLAR_DECIMALS}. */
  decimals?: number;
  /** Inclusive upper bound on the resulting base units. Defaults to {@link I128_MAX}. */
  maxBaseUnits?: bigint;
}

/** Options for {@link formatBaseUnits}. */
export interface FormatAmountOptions {
  /** Asset precision to scale by. Defaults to {@link STELLAR_DECIMALS}. */
  decimals?: number;
  /** Trim trailing fractional zeros (and a bare `.`). Defaults to `true`. */
  trimTrailingZeros?: boolean;
}

/** `10n ** BigInt(decimals)`, validated. */
function pow10(decimals: number): bigint {
  if (!Number.isInteger(decimals) || decimals < 0 || decimals > MAX_SUPPORTED_DECIMALS) {
    throw new RangeError(
      `decimals must be an integer in [0, ${MAX_SUPPORTED_DECIMALS}], got ${decimals}`,
    );
  }
  return 10n ** BigInt(decimals);
}

function resolveDecimals(decimals: number | undefined): number {
  const value = decimals ?? STELLAR_DECIMALS;
  if (!Number.isInteger(value) || value < 0 || value > MAX_SUPPORTED_DECIMALS) {
    throw new RangeError(
      `decimals must be an integer in [0, ${MAX_SUPPORTED_DECIMALS}], got ${value}`,
    );
  }
  return value;
}

/**
 * Parse a user-entered decimal amount into exact integer base units.
 *
 * Accepted grammar: `digits` or `digits.digits` or `.digits` (at least one
 * digit overall). No sign, exponent, thousands separator, whitespace inside the
 * number, or second decimal point is accepted.
 *
 * Rejection is explicit: `empty`, `negative`, `invalid_format`,
 * `excess_precision` (more than `decimals` fractional digits) and
 * `out_of_range` (base units above `maxBaseUnits`).
 *
 * @example
 *   parseAmountToBaseUnits("1.2345678")            // { ok: true, baseUnits: 12345678n, decimals: 7 }
 *   parseAmountToBaseUnits("1.23456789")           // { ok: false, error: "excess_precision" }
 *   parseAmountToBaseUnits("0.0000001", { decimals: 6 }) // { ok: false, error: "excess_precision" }
 *   parseAmountToBaseUnits("-1")                   // { ok: false, error: "negative" }
 */
export function parseAmountToBaseUnits(
  input: string,
  options: ParseAmountOptions = {},
): AmountParseResult {
  const decimals = resolveDecimals(options.decimals);
  const maxBaseUnits = options.maxBaseUnits ?? I128_MAX;

  const trimmed = input.trim();
  if (trimmed === "") {
    return { ok: false, error: "empty" };
  }
  if (trimmed.startsWith("-")) {
    return { ok: false, error: "negative" };
  }

  // digits, optional single ".", optional digits; or ".digits".
  const match = /^(?:(\d+)(?:\.(\d*))?|\.(\d+))$/.exec(trimmed);
  if (!match) {
    return { ok: false, error: "invalid_format" };
  }

  const wholePart = match[1] ?? "0";
  const fractionPart = match[2] ?? match[3] ?? "";

  if (fractionPart.length > decimals) {
    return { ok: false, error: "excess_precision" };
  }

  const factor = pow10(decimals);
  const padded = fractionPart.padEnd(decimals, "0");
  const baseUnits = BigInt(wholePart) * factor + BigInt(padded === "" ? "0" : padded);

  if (baseUnits > maxBaseUnits) {
    return { ok: false, error: "out_of_range" };
  }

  return { ok: true, baseUnits, decimals };
}

/**
 * Format exact integer base units as a canonical decimal string.
 *
 * The output always re-parses to the same base units under the same precision.
 * Trailing fractional zeros are trimmed by default (`"1.2300000"` → `"1.23"`).
 *
 * @example
 *   formatBaseUnits(12345678n)                        // "1.2345678"
 *   formatBaseUnits(15000000n)                        // "1.5"
 *   formatBaseUnits(15000000n, { trimTrailingZeros: false }) // "1.5000000"
 *   formatBaseUnits(0n)                               // "0"
 */
export function formatBaseUnits(
  baseUnits: bigint,
  options: FormatAmountOptions = {},
): string {
  const decimals = resolveDecimals(options.decimals);
  const trim = options.trimTrailingZeros ?? true;

  const negative = baseUnits < 0n;
  const abs = negative ? -baseUnits : baseUnits;
  const factor = pow10(decimals);
  const whole = abs / factor;
  const fraction = abs % factor;

  let fractionStr = fraction.toString().padStart(decimals, "0");
  if (trim) {
    fractionStr = fractionStr.replace(/0+$/, "");
  }

  const sign = negative ? "-" : "";
  return fractionStr === ""
    ? `${sign}${whole.toString()}`
    : `${sign}${whole.toString()}.${fractionStr}`;
}

/**
 * Convenience predicate: does `input` parse exactly under `options`?
 */
export function isValidAmount(
  input: string,
  options: ParseAmountOptions = {},
): boolean {
  return parseAmountToBaseUnits(input, options).ok;
}

/**
 * Parse and return base units, or throw with the rejection reason.
 *
 * Use this at the wallet-submission boundary where a rejected amount must abort
 * the flow (the UI should validate first via {@link parseAmountToBaseUnits}).
 *
 * @throws {RangeError} on invalid `decimals`.
 * @throws {Error} `amount_<reason>` when the amount is rejected.
 */
export function requireAmountToBaseUnits(
  input: string,
  options: ParseAmountOptions = {},
): bigint {
  const result = parseAmountToBaseUnits(input, options);
  if (!result.ok) {
    throw new Error(`amount_${result.error}`);
  }
  return result.baseUnits;
}
