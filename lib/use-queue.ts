import { preloadStage } from "@/lib/preload-diagnostic";
import { useCallback, useRef, useState } from "react";
import { desktop, type Bookmark, type QueueEvent, type QueueSnapshot } from "./desktop";
import type { Notify } from "@/components/toast";

export const emptyQueue: QueueSnapshot = {
  revision: -1,
  jobs: [],
  confirmations: [],
  canClear: false,
  protectedCount: 0,
  bytesPerSecond: 0,
  effectiveMode: "sequential",
  requestedMode: "sequential",
  liveLimit: 1,
  paused: false,
  canPause: false,
};

function toggleSetId(setter: React.Dispatch<React.SetStateAction<Set<number>>>, id: number) {
  setter(previous => {
    const next = new Set(previous);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    return next;
  });
}

export function useQueue(notify: Notify) {
  const [queue, setQueue] = useState(emptyQueue);
  const [bookmarks, setBookmarks] = useState<Bookmark[]>([]);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [expanded, setExpanded] = useState<Set<number>>(new Set());

  const queueRevision = useRef(-1);
  const bookmarkRevision = useRef(-1);
  const gapFetching = useRef(false);
  const speedRef = useRef(0);
  const speedListeners = useRef(new Set<() => void>());

  const emitSpeed = useCallback((bytesPerSecond: number) => {
    if (speedRef.current === bytesPerSecond) return;
    speedRef.current = bytesPerSecond;
    for (const listener of speedListeners.current) listener();
  }, []);

  const subscribeSpeed = useCallback((listener: () => void) => {
    speedListeners.current.add(listener);
    return () => { speedListeners.current.delete(listener); };
  }, []);

  const getSpeed = useCallback(() => speedRef.current, []);

  const applyBookmarks = useCallback((incoming: Bookmark[]) => {
    setBookmarks(incoming);
    const ids = new Set(incoming.map(bookmark => bookmark.id));
    setSelected(previous => {
      let changed = false;
      for (const id of previous) {
        if (!ids.has(id)) {
          changed = true;
          break;
        }
      }
      return changed ? new Set([...previous].filter(id => ids.has(id))) : previous;
    });
  }, []);

  const acceptQueue = useCallback((incoming: QueueEvent) => {
    if (incoming.revision <= queueRevision.current) return;
    if ("jobs" in incoming) {
      queueRevision.current = incoming.revision;
      emitSpeed(incoming.bytesPerSecond);
      setQueue(incoming);
      return;
    }
    if (incoming.revision !== queueRevision.current + 1) {
      if (!gapFetching.current) {
        gapFetching.current = true;
        void desktop.queueSnapshot().then(acceptQueue).catch(() => undefined).finally(() => {
          gapFetching.current = false;
        });
      }
      return;
    }
    queueRevision.current = incoming.revision;
    if (!("jobId" in incoming)) {
      emitSpeed(incoming.bytesPerSecond);
      return;
    }
    setQueue(previous => ({
      ...previous,
      revision: incoming.revision,
      jobs: previous.jobs.map(job => job.id === incoming.jobId && job.generation === incoming.generation &&
        job.files.some(file => file.id === incoming.fileId && incoming.attempt >= file.attempt) ? {
        ...job,
        progress: incoming.jobProgress,
        files: job.files.map(file => file.id === incoming.fileId
          ? {
            ...file,
            status: "downloading",
            generation: incoming.generation,
            attempt: incoming.attempt,
            progress: incoming.progress,
            phase: incoming.phase,
            receivedBytes: incoming.receivedBytes,
            totalBytes: incoming.totalBytes,
            finalSizeBytes: incoming.finalSizeBytes,
          }
          : file),
      } : job),
    }));
  }, [emitSpeed]);

  const subscribeChannels = useCallback((isActive: () => boolean) => {
    let recovery: ReturnType<typeof setInterval> | undefined;
    let polling = false;
    const recover = () => {
      if (!isActive() || recovery) return;
      notify("Live updates disconnected. Refreshing the lists periodically.", "warning");
      recovery = setInterval(() => {
        if (polling || !isActive()) return;
        polling = true;
        void Promise.all([
          desktop.queueSnapshot().then(snapshot => { if (isActive()) acceptQueue(snapshot); }).catch(() => undefined),
          desktop.listBookmarks().then(items => { if (isActive()) applyBookmarks(items); }).catch(() => undefined),
        ]).finally(() => {
          polling = false;
        });
      }, 1500);
    };

    void desktop.subscribeQueue(snapshot => { if (isActive()) acceptQueue(snapshot); })
      .then(() => preloadStage("queue"))
      .catch(recover);
    void desktop.subscribeBookmarks(snapshot => {
      if (!isActive() || snapshot.revision <= bookmarkRevision.current) return;
      bookmarkRevision.current = snapshot.revision;
      applyBookmarks(snapshot.bookmarks);
    })
      .then(() => preloadStage("bookmarks"))
      .catch(recover);

    return () => { clearInterval(recovery); };
  }, [acceptQueue, applyBookmarks, notify]);

  const toggleSelected = useCallback((id: number) => toggleSetId(setSelected, id), []);
  const deselectBookmarks = useCallback(() => setSelected(new Set()), []);
  const toggleExpanded = useCallback((id: number) => toggleSetId(setExpanded, id), []);
  const clearExpanded = useCallback(() => setExpanded(new Set()), []);

  return {
    queue,
    acceptQueue,
    bookmarks,
    applyBookmarks,
    selected,
    toggleSelected,
    deselectBookmarks,
    expanded,
    toggleExpanded,
    clearExpanded,
    subscribeSpeed,
    getSpeed,
    subscribeChannels,
  };
}
