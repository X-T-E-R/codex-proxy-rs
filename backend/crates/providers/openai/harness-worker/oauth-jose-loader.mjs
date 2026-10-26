export async function resolve(specifier, context, nextResolve) {
  if (specifier === "jose") {
    return {
      url: new URL("./oauth-jose-shim.mjs", import.meta.url).href,
      shortCircuit: true,
    };
  }
  return nextResolve(specifier, context);
}
