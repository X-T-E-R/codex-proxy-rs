import { createPublicKey, createVerify } from "node:crypto";

export function createRemoteJWKSet(url) {
  let keys = null;
  return async () => {
    if (!keys) {
      const response = await fetch(url);
      if (!response.ok) throw new Error("jwks fetch failed");
      const body = await response.json();
      keys = (body.keys ?? []).map((jwk) => createPublicKey({ key: jwk, format: "jwk" }));
    }
    return keys;
  };
}

export async function jwtVerify(token, getKey, options = {}) {
  const parts = String(token).split(".");
  if (parts.length !== 3) throw new Error("invalid jwt");
  const [encodedHeader, encodedPayload, encodedSignature] = parts;
  const header = JSON.parse(Buffer.from(encodedHeader, "base64url").toString("utf8"));
  const payload = JSON.parse(Buffer.from(encodedPayload, "base64url").toString("utf8"));
  if (options.algorithms && !options.algorithms.includes(header.alg)) {
    throw new Error("unexpected jwt algorithm");
  }
  const data = `${encodedHeader}.${encodedPayload}`;
  const signature = Buffer.from(encodedSignature, "base64url");
  const keys = await getKey();
  const signed = keys.some((key) => {
    const verify = createVerify("RSA-SHA256");
    verify.update(data);
    verify.end();
    try {
      return verify.verify(key, signature);
    } catch {
      return false;
    }
  });
  if (!signed) throw new Error("invalid jwt signature");
  if (options.issuer && payload.iss !== options.issuer) throw new Error("unexpected jwt issuer");
  const audience = payload.aud;
  if (options.audience && audience !== options.audience && !(Array.isArray(audience) && audience.includes(options.audience))) {
    throw new Error("unexpected jwt audience");
  }
  for (const claim of options.requiredClaims ?? []) {
    if (payload[claim] === undefined || payload[claim] === null || payload[claim] === "") {
      throw new Error(`jwt missing ${claim}`);
    }
  }
  if (typeof payload.exp === "number" && payload.exp * 1000 <= Date.now()) throw new Error("jwt expired");
  return { payload };
}
