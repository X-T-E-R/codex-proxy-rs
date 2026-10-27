import assert from "node:assert/strict";
import test from "node:test";
import { piFailureMessage } from "../pi.mjs";

test("pi forwards only the known subscription-sharing eligibility fact", () => {
  assert.equal(
    piFailureMessage({ error: { errorMessage: "OpenAI API error: The ChatGPT user is not eligible for subscription sharing; private marker" } }),
    "The ChatGPT user is not eligible for subscription sharing",
  );
  assert.equal(piFailureMessage({ error: { errorMessage: "secret token and arbitrary upstream body" } }), "pi responses stream failed");
  assert.equal(piFailureMessage({ error: { errorMessage: { nested: "secret" } } }), "pi responses stream failed");
  assert.equal(piFailureMessage({}), "pi responses stream failed");
});
