import { setupServer } from "msw/node";
import { handlers } from "./handlers";

/** Used by Vitest (src/test/setup.ts) to mock the admin API in unit/component tests. */
export const server = setupServer(...handlers);
