"use client";

import { preloadStage } from "@/lib/preload-diagnostic";
import { listen } from "@tauri-apps/api/event";
import { CirclePause, Play, Search } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AccountSetupDialog, AddLinks, Confirmation } from "@/components/dialogs";
import { CatalogSearch } from "@/components/catalog-search";
import { SpeedControl } from "@/components/connection-controls";
import { AddLinkButton, Bookmarks, Downloads, filters, matches, type Filter } from "@/components/library";
import { accountLabels, SettingsPage, type Section } from "@/components/settings";
import { Button, ValidationButton } from "@/components/ui/primitives";
import { Toast, type ToastMessage, type ToastType } from "@/components/toast";
import { SuoneloLogo } from "@/components/SuoneloLogo";
import { ShellIcon } from "@/components/ui/shell-icon";
import {
  desktop, errorMessage, type BookmarkScope, type Destination, type FileItem, type LargePlan, type MetadataField,
  type SubmissionReport,
} from "@/lib/desktop";
import { canRetryKeyring, isMac } from "@/lib/platform";
import { useAccount, type Prompt } from "@/lib/use-account";
import { useQueue } from "@/lib/use-queue";
import { useSettings } from "@/lib/use-settings";

type View = "Downloads" | "Bookmarks" | "Settings";

const previewNotice = "Qobuz offers MP3 previews for this account. Audio downloads will save previews where available.";

export default function App() {
  const [view, setView] = useState<View>("Downloads");
  const [filter, setFilter] = useState<Filter>("Show all");
  const [section, setSection] = useState<Section>("Account");
  const [pauseBusy, setPauseBusy] = useState(false);
  const [metadata, setMetadata] = useState<MetadataField[]>([]);
  const [systemMode, setSystemMode] = useState<"light" | "dark">("light");
  const [prompt, setPrompt] = useState<Prompt | null>(null);
  const [searchOpen, setSearchOpen] = useState(false);
  const [deciding, setDeciding] = useState<string[]>([]);
  const [toast, setToast] = useState<ToastMessage | null>(null);
  const [startupError, setStartupError] = useState("");

  const appRoot = useRef<HTMLDivElement>(null);
  const searchLauncher = useRef<HTMLButtonElement>(null);
  const addLinkLauncher = useRef<HTMLButtonElement>(null);
  const toastTimer = useRef<ReturnType<typeof setTimeout>>(undefined);
  const closeBusy = useRef(false);

  const notify = useCallback((message: string, type: ToastType = "info") => {
    clearTimeout(toastTimer.current);
    setToast({ message, type });
    toastTimer.current = setTimeout(() => setToast(null), type === "error" ? 8000 : 4200);
  }, []);

  const dismiss = useCallback(() => setPrompt(null), []);
  const accountSettings = useCallback(() => {
    setSection("Account");
    setView("Settings");
  }, []);

  const { settings, theme: effectiveTheme, initialize, patch, flush, reset, resetVersion, saveStatus, saveLimit } = useSettings(notify);
  const {
    queue, acceptQueue, bookmarks, applyBookmarks, selected, toggleSelected, deselectBookmarks,
    expanded, toggleExpanded, clearExpanded, subscribeSpeed, getSpeed, subscribeChannels,
  } = useQueue(notify);

  const handleAccountSwitch = useCallback(() => {
    clearExpanded();
    setDeciding([]);
    setFilter("Show all");
  }, [clearExpanded]);

  const {
    account, validation, accountMessage, accountErrorCode, accountCommitRevision, clearAccountMessage,
    listenAccountChanges, applyBootstrapAccount, cancelValidation, validate, retryKeyringAccess, removeCredentials,
  } = useAccount({
    notify,
    setPrompt,
    dismiss,
    onAccountSwitch: handleAccountSwitch,
    openAccountSettings: accountSettings,
  });

  useEffect(() => {
    let active = true;
    let stopChannels: (() => void) | undefined;
    const isActive = () => active;
    const preference = matchMedia("(prefers-color-scheme: dark)");
    const syncSystem = () => setSystemMode(preference.matches ? "dark" : "light");
    syncSystem();
    preference.addEventListener("change", syncSystem);
    const accountListener = listenAccountChanges(isActive);

    void accountListener.ready.then(() => desktop.bootstrap()).then(initial => {
      if (!active) return;
      initialize(initial.settings, preference.matches);
      if (initial.settings.theme == null) void flush().catch(() => undefined);
      acceptQueue(initial.queue);
      applyBookmarks(initial.bookmarks);
      applyBootstrapAccount(initial.account);
      void desktop.metadataCatalog().then(catalog => {
        if (active) setMetadata(catalog);
      }).catch(error => { if (active) notify(errorMessage(error), "error"); });
      stopChannels = subscribeChannels(isActive);
    }).catch(error => {
      if (active) setStartupError(errorMessage(error));
    });

    return () => {
      active = false;
      accountListener.unlisten();
      preference.removeEventListener("change", syncSystem);
      stopChannels?.();
      clearTimeout(toastTimer.current);
    };
  }, [acceptQueue, applyBookmarks, applyBootstrapAccount, flush, initialize, listenAccountChanges, notify, subscribeChannels]);

  const run = useCallback(async (action: () => Promise<unknown>, success?: string) => {
    try { await action(); if (success) notify(success, "success"); }
    catch (error) { notify(errorMessage(error), "error"); }
  }, [notify]);

  useEffect(() => {
    if (view !== "Settings" || section !== "Artwork & Metadata") return;
    let current = true;
    void desktop.metadataCatalog().then(catalog => { if (current) setMetadata(catalog); }).catch(() => undefined);
    return () => { current = false; };
  }, [view, section]);

  const requestClose = useCallback(async () => {
    if (closeBusy.current) return;
    closeBusy.current = true;
    try {
      await flush();
      const decision = await desktop.requestClose();
      if (decision.requiresConfirmation) {
        setPrompt(previous => ({
          kind: "confirm",
          title: "Close this session?",
          description: "Active and queued work will stop. Session rows and staging files will be cleared. Finalized files will be kept.",
          label: "Close session",
          action: async () => { await flush(); await desktop.shutdownSession(); },
          restore: previous?.kind === "confirm" && previous.title === "Close this session?" ? previous.restore : previous,
        }));
      } else {
        await desktop.shutdownSession();
      }
    } catch (error) {
      notify(errorMessage(error), "error");
    } finally {
      closeBusy.current = false;
    }
  }, [flush, notify]);

  useEffect(() => {
    const registration = listen("close-requested", () => void requestClose());
    void registration.then(() => preloadStage("close")).catch(() => undefined);
    return () => { void registration.then(unlisten => unlisten()).catch(() => undefined); };
  }, [requestClose]);

  const plan = queue.confirmations.find(item => !deciding.includes(`${item.id}:${item.planRevision}`));
  const modalOpen = Boolean(prompt || plan || searchOpen);
  const hasSettings = settings != null;
  useEffect(() => {
    function shortcut(event: KeyboardEvent) {
      const primaryModifier = isMac ? event.metaKey && !event.ctrlKey : event.ctrlKey && !event.metaKey;
      if (!hasSettings || modalOpen || event.repeat || event.defaultPrevented || !primaryModifier || event.altKey || event.shiftKey) return;
      switch (event.key.toLowerCase()) {
        case "n":
          event.preventDefault();
          addLinkLauncher.current?.focus();
          setPrompt({ kind: "add" });
          break;
        case "f":
          event.preventDefault();
          searchLauncher.current?.focus();
          setSearchOpen(true);
          break;
      }
    }
    window.addEventListener("keydown", shortcut);
    return () => window.removeEventListener("keydown", shortcut);
  }, [hasSettings, modalOpen]);

  useEffect(() => {
    if ((validation?.origin === "settings" && (view !== "Settings" || section !== "Account")) ||
        (validation?.origin === "notice" && view === "Settings")) {
      void cancelValidation();
    }
  }, [view, section, validation?.origin, cancelValidation]);

  const resetSettings = useCallback(() => {
    setPrompt({
      kind: "confirm",
      title: "Reset settings?",
      description: "This restores default preferences and keeps credentials, bookmarks, and current jobs.",
      label: "Reset settings",
      action: async () => { await reset(); notify("Settings restored to defaults.", "success"); },
    });
  }, [notify, reset]);

  const clearDownloads = useCallback(() => {
    setPrompt({
      kind: "confirm",
      title: "Clear download list?",
      description: "Queued entries and inactive history will be removed. Active albums will continue. Files on disk will be kept.",
      label: "Clear all",
      action: async () => {
        const result = await desktop.clearDownloads();
        notify(`${result.removed} entries cleared. ${result.protected} active albums retained.`, "success");
      },
    });
  }, [notify]);

  const submission = useCallback((report: SubmissionReport, destination: Destination) => {
    if (!report.accepted.length && !report.invalid.length && !report.duplicates.length) return;
    if (report.accepted.length) {
      setView(destination === "downloads" ? "Downloads" : "Bookmarks");
      if (destination === "downloads") setFilter("Show all");
    }
    const summary = `${report.accepted.length} ${destination === "downloads" ? "queued" : "saved"} · ${report.duplicates.length} duplicate · ${report.invalid.length} invalid`;
    notify(summary, report.invalid.length || report.duplicates.length ? "warning" : report.accepted.length ? "success" : "info");
  }, [notify]);

  const decide = useCallback(async (item: LargePlan, decision: boolean) => {
    await desktop.decideLargeSubmission(item, decision);
    const key = `${item.id}:${item.planRevision}`;
    setDeciding(previous => [...previous, key]);
    void desktop.queueSnapshot().then(acceptQueue).catch(() => undefined);
  }, [acceptQueue]);

  const bookmarkDownload = useCallback((scope: BookmarkScope) => {
    void run(async () => { await flush(); submission(await desktop.downloadBookmarks(scope), "downloads"); });
  }, [flush, run, submission]);

  const downloadsActions = useMemo(() => ({
    cancel: (id: number) => run(() => desktop.cancelJob(id)),
    remove: (id: number) => run(() => desktop.removeJob(id)),
    retry: (id: number) => run(async () => { await flush(); await desktop.retryJob(id); }),
    retryFile: (id: number, fileId: number) => run(async () => { await flush(); await desktop.retryFile(id, fileId); }),
    replaceFile: (id: number, file: FileItem) => {
      const token = file.replacementToken;
      if (!token || !file.conflictPath) return;
      setPrompt({
        kind: "confirm",
        title: "Replace saved audio?",
        description: `Replace ${file.title} at ${file.conflictPath} with this download?`,
        label: "Replace audio",
        action: async () => { await flush(); await desktop.replaceSearchAudio(id, file.id, token); },
      });
    },
    openFolder: (id: number, fileId: number) => run(() => desktop.openContainingFolder(id, fileId)),
    openReleaseFolder: (id: number) => run(() => desktop.openReleaseFolder(id)),
  }), [flush, run]);

  const handleSearchSubmit = useCallback((report: { accepted: unknown[]; skipped: unknown[]; conflicts: unknown[] }, destination: Destination) => {
    const target = destination === "downloads" ? "Downloads" : "Bookmarks";
    const skipped = report.skipped.length + report.conflicts.length;
    const message = `${report.accepted.length} added to ${target}${skipped ? ` · ${skipped} skipped` : ""}.`;
    notify(message, report.accepted.length ? "success" : "info");
  }, [notify]);

  const closeSearch = useCallback(() => setSearchOpen(false), []);

  const { pending, transfers, filterCounts } = useMemo(() => {
    const queuedIds: number[] = [];
    let activeTransfers = 0;
    const counts = {
      "Show all": queue.jobs.length,
      Complete: 0,
      Downloading: 0,
      Failed: 0,
      Incomplete: 0,
      Queued: 0,
    } satisfies Record<Filter, number>;
    for (const job of queue.jobs) {
      if (job.status === "queued") {
        queuedIds.push(job.id);
        counts.Queued += 1;
      } else if (job.status === "completed") {
        counts.Complete += 1;
      } else if (job.status === "downloading" || job.status === "retrying") {
        counts.Downloading += 1;
      } else if (job.status === "failed") {
        counts.Failed += 1;
      } else if (matches(job, "Incomplete")) {
        counts.Incomplete += 1;
      }
      for (const file of job.files) {
        if (file.status === "downloading") activeTransfers += 1;
      }
    }
    return { pending: queuedIds, transfers: activeTransfers, filterCounts: counts };
  }, [queue.jobs]);

  const effectiveMode = settings?.appearance === "system" ? systemMode : (settings?.appearance ?? systemMode);
  const ready = Boolean(settings && account);
  const subtitle = view === "Downloads" ? (filter === "Show all" ? "Your current download session" : filter)
    : view === "Bookmarks" ? "Saved releases and links" : saveStatus;

  return (
    <div ref={appRoot} id="app" data-mode={effectiveMode} data-theme={effectiveTheme} data-loading={!ready || undefined}>
      <header className="top">
        <div className="brand"><SuoneloLogo /></div>
        {ready && <button ref={searchLauncher} type="button" className="search-launcher" aria-haspopup="dialog" aria-keyshortcuts={isMac ? "Meta+F" : "Control+F"}
          disabled={Boolean(prompt || plan)} onClick={() => setSearchOpen(true)}>
          <Search className="icon" aria-hidden /><span>Search Qobuz</span><kbd>{isMac ? "⌘F" : "Ctrl+F"}</kbd>
        </button>}
        <div className="topright">
          <Button className="ghost with-icon" onClick={() => void requestClose()}><ShellIcon name="exit" /> Exit</Button>
        </div>
      </header>
      <aside aria-label="Application navigation">
        <div className="section-label">Library</div>
        <Button aria-current={view === "Downloads"} onClick={() => setView("Downloads")}>
          <ShellIcon name="downloads" /> Downloads <span className="count">{queue.jobs.length}</span>
        </Button>
        <Button aria-current={view === "Bookmarks"} onClick={() => setView("Bookmarks")}>
          <ShellIcon name="bookmarks" /> Bookmarks <span className="count">{bookmarks.length}</span>
        </Button>
        {view === "Downloads" && (
          <>
            <div className="section-label">Queue status</div>
            {filters.map(name => (
              <Button key={name} aria-current={filter === name} onClick={() => setFilter(name)}>
                {name} <span className="count">{filterCounts[name]}</span>
              </Button>
            ))}
          </>
        )}
        <div className="sidebar-footer">
          <div className="section-label">Application</div>
          <Button aria-current={view === "Settings"} onClick={() => setView("Settings")}><ShellIcon name="settings" /> Settings</Button>
        </div>
      </aside>
      <main>
        <header className="pagehead">
          <div><h1>{view}</h1><p className="subtitle" role={view === "Settings" ? "status" : undefined}>{subtitle}</p></div>
          {ready && <div className="actions">
            {view === "Settings" ? <Button className="danger" data-action="reset-settings" onClick={resetSettings}>Reset settings</Button>
              : <>
                {view === "Downloads" && queue.jobs.length > 0 && (
                  <Button className="danger with-icon" disabled={!queue.canClear} onClick={clearDownloads}>Clear all</Button>
                )}
                {view === "Downloads" && queue.canPause && <Button className="with-icon" disabled={pauseBusy} onClick={() => {
                  setPauseBusy(true);
                  void run(async () => acceptQueue(await desktop.setQueuePaused(!queue.paused)))
                    .finally(() => setPauseBusy(false));
                }}>
                  {queue.paused ? <Play className="icon" aria-hidden /> : <CirclePause className="icon" aria-hidden />}
                  {queue.paused ? "Resume" : "Pause"}
                </Button>}
                <AddLinkButton ref={addLinkLauncher} onClick={() => setPrompt({ kind: "add" })} />
              </>}
          </div>}
        </header>
        {startupError && (
          <section className="panel config" role="alert">
            <h2>Desktop connection unavailable</h2>
            <p>{startupError}</p>
            <Button onClick={() => window.location.reload()}>Retry</Button>
          </section>
        )}
        {!ready && !startupError && <div className="panel empty" role="status">Loading the desktop session…</div>}
        {ready && account && settings && (
          <>
            {view !== "Settings" && account.state !== "ready" && (
              <div className="session-notice notice">
                <span>{accountMessage || account.reason || `${accountLabels[account.state]}. Login to your account to get started.`}</span>
                {account.state === "unvalidated" ? (
                  <ValidationButton label="Retry validation" validating={validation?.origin === "notice" && validation.processing}
                    disabled={!!validation} onClick={() => void validate(undefined, "notice")} />
                ) : account.state !== "validating" && (
                  <Button onClick={() => setPrompt({ kind: "setup" })}>
                    {account.state === "rejected" ? "Replace token" : "Set up account"}
                  </Button>
                )}
              </div>
            )}
            {view === "Downloads" && account.state === "ready" && account.accessMode === "preview" && (
              <div className="session-notice notice" role="status">
                {isMac ? <span>{previewNotice}</span> : previewNotice}
              </div>
            )}
            {view !== "Settings" && settings.outputMissing && (
              <div className="session-notice notice">
                <span>The download destination is unavailable.</span>
                <Button onClick={() => { setSection("Output"); setView("Settings"); }}>Choose a destination</Button>
              </div>
            )}
            {view === "Downloads" && (
              <>
                {queue.recovery?.retryConnection ? (
                  <div className="session-notice notice">
                    <span role="status">Qobuz requests are waiting for a connection check.</span>
                    <Button onClick={() => void run(() => desktop.retryConnection())}>Retry connection</Button>
                  </div>
                ) : queue.recovery?.cooldownSeconds ? (
                  <p className="session-notice notice" role="status">
                    Qobuz rate limit. New API requests resume in {queue.recovery.cooldownSeconds} s.
                  </p>
                ) : queue.recovery?.workerCap ? (
                  <p className="session-notice notice" role="status">
                    Connection recovery limits the app to {queue.recovery.workerCap} active {queue.recovery.workerCap === 1 ? "operation" : "operations"}.
                  </p>
                ) : null}
                <Downloads jobs={queue.jobs} paused={queue.paused} filter={filter} expanded={expanded} toggle={toggleExpanded}
                  tooltipContainer={appRoot} actions={downloadsActions} />
              </>
            )}
            {view === "Bookmarks" && (
              <Bookmarks
                bookmarks={bookmarks} selected={selected} toggle={toggleSelected} deselect={deselectBookmarks}
                download={bookmarkDownload}
                remove={ids => void run(() => desktop.removeBookmarks(ids), "Selected bookmarks removed.")}
                retry={id => void run(() => desktop.retryBookmark(id))}
                clear={() => setPrompt({
                  kind: "confirm", title: "Clear bookmarks?", description: "This removes every saved bookmark.",
                  label: "Clear all", action: () => desktop.clearBookmarks(),
                })}
              />
            )}
            {view === "Settings" && (
              <SettingsPage key={resetVersion} section={section} select={setSection} settings={settings} patch={patch} theme={effectiveTheme}
                modePending={queue.requestedMode !== queue.effectiveMode}
                retryTheme={settings.theme == null && saveStatus === "Settings could not be saved." ? () => void flush().catch(() => undefined) : undefined}
                metadata={metadata} account={account} notify={notify}
                pending={Boolean(pending.length)} apply={() => void run(async () => {
                  await flush();
                  await desktop.applySettingsToPending(pending);
                }, "Applied current settings to queued jobs.")}
                accountActions={{
                  validate, signIn: () => void validate(undefined, "settings", "email"),
                  retryKeyring: canRetryKeyring(account.reasonCode) ? retryKeyringAccess : undefined,
                  remove: removeCredentials, cancel: () => void cancelValidation(), clearMessage: clearAccountMessage,
                  busy: !!validation, validating: validation?.origin === "settings" && validation.processing, message: accountMessage, errorCode: accountErrorCode ?? account.reasonCode, commitRevision: accountCommitRevision,
                }} />
            )}
          </>
        )}
      </main>
      <footer className="foot">
        <span>{account?.state === "ready" && account.accessMode === "preview" ? "MP3 previews only" : account ? accountLabels[account.state] : "Starting"} · {transfers} active {transfers === 1 ? "transfer" : "transfers"}</span>
        <SpeedControl queue={queue} saved={settings?.maxConnections ?? 1} disabled={!ready || modalOpen}
          subscribeSpeed={subscribeSpeed} getSpeed={getSpeed}
          save={async value => {
            try {
              await saveLimit(value, acceptQueue);
              notify("Max connections saved.", "success");
            } catch (error) {
              notify(errorMessage(error), "error");
              throw error;
            }
          }} />
      </footer>
      <Toast toast={toast} dismiss={() => { clearTimeout(toastTimer.current); setToast(null); }} />
      {ready && <CatalogSearch key={account?.epoch ?? 0} open={searchOpen && !prompt} close={closeSearch}
        account={account} beforeQueue={flush} onSubmit={handleSearchSubmit} />}
      {prompt?.kind === "add" && settings && <AddLinks close={dismiss} onSuccess={submission} beforeQueue={flush}
        initialQuality={settings.quality} allowFallback={settings.allowFallback} previewOnly={account?.accessMode === "preview"} tokenForm={isMac && account?.accessMode === "unknown"} mode={`${effectiveMode}:${effectiveTheme}`} />}
      {(prompt?.kind === "setup" || (isMac && prompt?.kind === "confirm" && prompt.restore?.kind === "setup")) && <AccountSetupDialog finish={input => void validate(input, "setup")}
        signIn={() => void validate(undefined, "setup", "email")} cancel={() => void cancelValidation()}
        close={() => { void cancelValidation(); dismiss(); }}
        busy={validation?.origin === "setup"} validating={validation?.origin === "setup" && validation.processing}
        signingIn={validation?.origin === "setup" && validation.method === "email"}
        reason={account?.reason} message={accountMessage} errorCode={accountErrorCode ?? account?.reasonCode} hasSavedToken={account?.hasToken}
        retryKeyring={canRetryKeyring(account?.reasonCode) ? retryKeyringAccess : undefined} />}
      {prompt?.kind === "confirm" && (
        <Confirmation key={prompt.title} title={prompt.title} description={prompt.description} label={prompt.label}
          validateAccount={prompt.accountValidation}
          confirm={async () => { await prompt.action(); if (!prompt.accountValidation) dismiss(); }}
          close={async () => { await prompt.decline?.(); setPrompt(prompt.restore ?? null); }} />
      )}
      {!prompt && !searchOpen && plan && (
        <Confirmation key={`${plan.id}:${plan.planRevision}`} title={`Download ${plan.trackCount} ${plan.unit === "pdfs" ? "booklets" : "tracks"}?`}
          description="Transfers will wait for your confirmation."
          label="Start downloads" confirm={() => decide(plan, true)} close={() => decide(plan, false)} />
      )}
    </div>
  );
}
