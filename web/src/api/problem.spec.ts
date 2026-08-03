import { AxiosError, AxiosHeaders } from "axios";
import { describe, expect, it } from "vitest";
import type { Problem } from "./generated/types.gen";
import { failureOf } from "./problem";

function refusal(status: number, problem?: Partial<Problem>): AxiosError {
  const error = new AxiosError("refused");
  error.response = {
    status,
    statusText: "",
    headers: new AxiosHeaders(),
    config: { headers: new AxiosHeaders() },
    data: problem,
  };
  return error;
}

describe("failureOf", () => {
  it("words a refusal from its code, which is the field §8 promises", () => {
    const failure = failureOf(
      refusal(403, {
        code: "scope.missing",
        title: "the caller lacks a scope",
        status: 403,
        type: "about:blank",
      }),
    );

    expect(failure.code).toBe("scope.missing");
    expect(failure.message).toContain("droits");
  });

  it("falls back to the server's own words for a code it has never seen", () => {
    // This is what keeps an older frontend usable against a newer server: an
    // unknown code shows the server's title instead of "une erreur est survenue".
    const failure = failureOf(
      refusal(422, {
        code: "revision.too.many",
        title: "cette notification a atteint 1 000 révisions",
        status: 422,
        type: "about:blank",
      }),
    );

    expect(failure.message).toBe(
      "cette notification a atteint 1 000 révisions",
    );
  });

  it("carries the retry delay of a 429", () => {
    const failure = failureOf(
      refusal(429, {
        code: "rate.limited",
        title: "slow down",
        status: 429,
        type: "about:blank",
        retry_after_seconds: 30,
      }),
    );

    expect(failure.retryAfter).toBe(30);
  });

  it("says the server is unreachable rather than showing a status of 0", () => {
    const error = new AxiosError("Network Error");
    expect(failureOf(error).message).toContain("injoignable");
  });

  it("is total: anything thrown becomes a message", () => {
    expect(failureOf(new Error("boom")).message).toBe("boom");
    expect(failureOf("a string").message).toBeTruthy();
    expect(failureOf(undefined).message).toBeTruthy();
  });
});
