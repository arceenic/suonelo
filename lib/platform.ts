import type { Credentials } from "./desktop";

export const isMac = process.env.NEXT_PUBLIC_SUONELO_PLATFORM === "darwin";
export const isLinux = process.env.NEXT_PUBLIC_SUONELO_PLATFORM === "linux";

export function canRetryKeyring(code?: string | null): boolean {
  return isLinux && ["vault_access", "vault_cancelled", "vault_changed", "vault_invalid", "vault_locked"]
    .includes(code ?? "");
}

export function automaticSetupRetry(token: string): Credentials {
  return isMac
    ? { token, forceDiscovery: true }
    : { token, appId: "", secret: "", forceDiscovery: true };
}
