import contract from "./protocol-contract.json";

export function apiPath(path) {
  if (!path.startsWith("/") || path.startsWith("//")) {
    throw new Error("API path must be an absolute application path");
  }
  return `${contract.api_prefix}${path}`;
}
