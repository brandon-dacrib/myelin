/**
 * The shared "answer honestly" treatment for non-2xx `/api/v1` responses
 * (docs/status/16-management-web-interface.md, "Degrade honestly").
 *
 * Against a real `hs serve`, most of the 142 operations answer RFC 9457
 * `501 not-implemented` (the handler doesn't exist yet) or `503` (the
 * handler exists but isn't wired to a data source). Both carry the same
 * `Problem` body (`components["schemas"]["Problem"]` in `./schema`,
 * `crates/hs-admin/openapi/openapi.yaml`'s closed catalog, RFC 0004 section
 * 3.5) as every other error, with a required `status` field — so
 * classification here reads `problem.status`, not the raw `Response`, and
 * needs no extra plumbing through every call site.
 *
 * Every `api/*.ts` query/mutation function calls `unwrap()` instead of the
 * `if (error) throw error` idiom, so a thrown error is always either an
 * `ApiProblemError` (a real, parsed RFC 9457 body) or something else
 * (a network failure, a parse failure) that `classifyError` treats as a
 * generic error. Pages render the result through
 * `src/components/QueryProblemState.tsx`, never a bare `ErrorState`, so a
 * 501/503 is never presented as a fault, a forever-spinner, or a table that
 * implies zero rows.
 */
import type { components } from "./schema";

export type Problem = components["schemas"]["Problem"];

/** Thrown by {@link unwrap} for a non-2xx response that carried a parsed RFC 9457 `Problem` body. */
export class ApiProblemError extends Error {
  readonly problem: Problem;

  constructor(problem: Problem) {
    super(problem.title || `Request failed with status ${problem.status}`);
    this.name = "ApiProblemError";
    this.problem = problem;
  }
}

/**
 * The shape every `openapi-fetch` call returns: exactly one of `data`/`error` is set. Matches
 * `openapi-fetch`'s own return type structurally, so `unwrap(await api.GET(...))` infers `T`
 * without needing the call site to destructure (destructuring into separate `const data =`/
 * `const error =` bindings loses the correlation between them).
 */
type FetchResult<T> =
  | { data: T; error?: never; response: Response }
  | { data?: never; error: Problem; response: Response };

/** Unwraps an `openapi-fetch` result, throwing an {@link ApiProblemError} on failure. */
export function unwrap<T>(result: FetchResult<T>): T {
  if (result.error !== undefined) {
    throw new ApiProblemError(result.error);
  }
  return result.data;
}

export type ProblemKind =
  "not-implemented" | "unavailable" | "forbidden" | "unauthorized" | "not-found" | "error";

export interface ClassifiedProblem {
  kind: ProblemKind;
  problem?: Problem;
}

/**
 * Classifies a thrown query/mutation error into what the UI should say. `not-implemented` (501)
 * and `unavailable` (503) are the two statuses this track's brief calls out by name; `forbidden`
 * (403), `unauthorized` (401) and `not-found` (404) get their own existing treatments
 * (`ForbiddenState`, a re-sign-in prompt, "not found"); everything else is a generic fault.
 */
export function classifyError(err: unknown): ClassifiedProblem {
  if (err instanceof ApiProblemError) {
    const { problem } = err;
    switch (problem.status) {
      case 501:
        return { kind: "not-implemented", problem };
      case 503:
        return { kind: "unavailable", problem };
      case 403:
        return { kind: "forbidden", problem };
      case 401:
        return { kind: "unauthorized", problem };
      case 404:
        return { kind: "not-found", problem };
      default:
        return { kind: "error", problem };
    }
  }
  return { kind: "error" };
}

/**
 * Whether a classified error is worth a `useQuery` retry. `not-implemented`, `forbidden`,
 * `unauthorized` and `not-found` are permanent for the lifetime of the page (the server isn't
 * going to grow a handler mid-session); retrying just delays the honest answer and risks the
 * "spinner forever" failure mode this track's brief explicitly rules out. `unavailable` and a
 * generic/network error may be transient, so a couple of retries are worthwhile.
 */
export function isRetryableError(err: unknown): boolean {
  return classifyError(err).kind === "unavailable" || classifyError(err).kind === "error";
}
