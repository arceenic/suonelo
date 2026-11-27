import { listen } from "@tauri-apps/api/event";
import { useCallback, useRef, useState } from "react";
import { desktop, errorCode, errorMessage, type Account, type Credentials } from "./desktop";
import { isMac } from "./platform";
import type { Notify } from "@/components/toast";

export type ValidationOrigin = "settings" | "setup" | "notice";
export type SignInMethod = "token" | "email";
export type Prompt =
  | { kind: "add" | "setup" }
  | {
    kind: "confirm";
    title: string;
    description: string;
    label: string;
    action: () => Promise<void>;
    decline?: () => Promise<void>;
    restore?: Prompt | null;
    accountValidation?: boolean;
  };

export function useAccount({
  notify,
  setPrompt,
  dismiss,
  onAccountSwitch,
  openAccountSettings,
}: {
  notify: Notify;
  setPrompt: React.Dispatch<React.SetStateAction<Prompt | null>>;
  dismiss: () => void;
  onAccountSwitch: () => void;
  openAccountSettings: () => void;
}) {
  const [account, setAccount] = useState<Account>();
  const [validation, setValidation] = useState<{ origin: ValidationOrigin; method: SignInMethod; processing: boolean } | null>(null);
  const [accountMessage, setAccountMessage] = useState("");
  const [accountErrorCode, setAccountErrorCode] = useState<string>();
  const [accountCommitRevision, setAccountCommitRevision] = useState(0);

  const startupSetupShown = useRef(false);
  const keyringRetrying = useRef(false);
  const lastAccountEvent = useRef<Account>(null);
  const manualValidation = useRef<string | null>(null);
  const cancellingValidation = useRef(false);

  const clearAccountMessage = useCallback(() => {
    setAccountMessage("");
    setAccountErrorCode(undefined);
  }, []);

  const listenAccountChanges = useCallback((isActive: () => boolean) => {
    const registration = listen<Account>("account-changed", ({ payload }) => {
      if (!isActive()) return;
      lastAccountEvent.current = payload;
      setAccount(payload);
      if (!manualValidation.current) {
        setAccountMessage("");
        setAccountErrorCode(undefined);
      }
      const needsStartupSetup = payload.state === "missing" && payload.epoch === 0 && !startupSetupShown.current;
      if ((payload.state === "rejected" || needsStartupSetup) && !manualValidation.current) {
        if (needsStartupSetup) startupSetupShown.current = true;
        setPrompt(current => current ?? { kind: "setup" });
      }
    });
    return {
      ready: registration.catch(() => undefined),
      unlisten: () => { void registration.then(fn => fn()).catch(() => undefined); },
    };
  }, [setPrompt]);

  const applyBootstrapAccount = useCallback((initialAccount: Account) => {
    const resolved = lastAccountEvent.current ?? initialAccount;
    setAccount(resolved);
    if ((resolved.state === "missing" || resolved.state === "rejected") && !manualValidation.current) {
      startupSetupShown.current = true;
      setPrompt(current => current ?? { kind: "setup" });
    }
  }, [setPrompt]);

  const cancelValidation = useCallback(async () => {
    const attempt = manualValidation.current;
    if (!attempt) return;
    manualValidation.current = null;
    cancellingValidation.current = true;
    setValidation(null);
    setAccountErrorCode(undefined);
    setAccountMessage("Sign-in cancelled.");
    try {
      await desktop.cancelAccountValidation(attempt);
    } catch (error) {
      setAccountMessage(errorMessage(error));
    } finally {
      cancellingValidation.current = false;
    }
  }, []);

  const validate = useCallback(async (
    input?: Credentials,
    origin: ValidationOrigin = "settings",
    method: SignInMethod = "token",
  ) => {
    if (manualValidation.current || cancellingValidation.current || account?.state === "validating") return false;
    // WKWebView's packaged origin may not expose secure-context-only randomUUID().
    const attempt = Array.from(crypto.getRandomValues(new Uint8Array(16)), byte =>
      byte.toString(16).padStart(2, "0"),
    ).join("");
    manualValidation.current = attempt;
    setValidation({ origin, method, processing: true });
    setAccountErrorCode(undefined);
    setAccountMessage(method === "email" ? "Opening Qobuz sign-in…" : "Validating credentials…");
    let awaitingConfirmation = false;

    function finish(nextAccount: Account) {
      setAccount(nextAccount);
      if (isMac) setAccountCommitRevision(revision => revision + 1);
      setAccountMessage(nextAccount.accessMode === "preview" ? "Signed in for MP3 previews." : "Signed in.");
      notify(nextAccount.accessMode === "preview" ? "Signed in to Qobuz for MP3 previews." : "Signed in to Qobuz.", "success");
    }

    function settle() {
      if (manualValidation.current === attempt) {
        manualValidation.current = null;
        setValidation(null);
      }
    }

    try {
      const result = method === "email"
        ? await desktop.startEmailSignIn(attempt, phase => {
          if (manualValidation.current !== attempt || awaitingConfirmation) return;
          setValidation({ origin, method, processing: phase === "validating" });
          setAccountMessage(phase === "waiting"
            ? "Complete sign-in in the Qobuz window. Select Back if Qobuz prompts you."
            : "Validating credentials…");
        })
        : await desktop.validateCredentials(attempt, input);
      if (manualValidation.current !== attempt) return false;
      if (result.account) {
        finish(result.account);
        if (origin === "setup") dismiss();
        return true;
      }
      if (!result.confirmation) throw Error("Qobuz credentials could not be validated.");
      const switching = result.confirmation === "switch";
      const restore = origin === "setup" ? { kind: "setup" } as const : null;
      awaitingConfirmation = true;
      setValidation({ origin, method, processing: false });
      setAccountMessage("Waiting for confirmation.");
      setPrompt({
        kind: "confirm",
        title: switching ? "Switch Qobuz accounts?" : "Stop active downloads?",
        description: switching
          ? "This will stop active downloads and clear the current Downloads queue. Saved files, bookmarks, and search ranking history will be kept."
          : "Active downloads will stop. Downloads rows and saved files will be kept.",
        label: switching ? "Switch accounts" : "Continue",
        restore,
        accountValidation: true,
        action: async () => {
          setValidation({ origin, method, processing: true });
          try {
            const committed = await desktop.confirmAccountValidation(attempt);
            if (manualValidation.current === attempt) {
              finish(committed);
              if (switching) onAccountSwitch();
            }
            dismiss();
          } catch (error) {
            setAccountMessage(errorMessage(error));
            setPrompt(restore);
          } finally {
            settle();
          }
        },
        decline: cancelValidation,
      });
      return false;
    } catch (error) {
      if (manualValidation.current === attempt) {
        setAccountMessage(errorMessage(error));
        if (method === "token") setAccountErrorCode(errorCode(error));
      }
      return false;
    } finally {
      if (!awaitingConfirmation) settle();
    }
  }, [account?.state, cancelValidation, dismiss, notify, onAccountSwitch, setPrompt]);

  const retryKeyringAccess = useCallback(async () => {
    if (keyringRetrying.current) return;
    keyringRetrying.current = true;
    setAccountMessage("");
    setAccountErrorCode(undefined);
    try {
      setAccount(await desktop.retryVaultAccess());
    } catch (error) {
      setAccountMessage(errorMessage(error));
      setAccountErrorCode(errorCode(error));
    } finally {
      keyringRetrying.current = false;
    }
  }, []);

  const removeCredentials = useCallback(() => {
    setPrompt({
      kind: "confirm",
      title: "Remove saved credentials?",
      description: "Active attempts will stop. Settings, bookmarks, and finalized files will be kept.",
      label: "Remove credentials",
      action: async () => {
        setAccount(await desktop.removeCredentials());
        setAccountMessage("");
        if (isMac) setAccountErrorCode(undefined);
        openAccountSettings();
      },
    });
  }, [openAccountSettings, setPrompt]);

  return {
    account,
    validation,
    accountMessage,
    accountErrorCode,
    accountCommitRevision,
    clearAccountMessage,
    listenAccountChanges,
    applyBootstrapAccount,
    cancelValidation,
    validate,
    retryKeyringAccess,
    removeCredentials,
  };
}
